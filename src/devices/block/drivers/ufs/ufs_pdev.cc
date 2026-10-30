// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/ufs/ufs_pdev.h"

#include <fidl/fuchsia.hardware.platform.device/cpp/wire.h>
#include <lib/async/cpp/task.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/platform-device/cpp/pdev.h>
#include <lib/zx/time.h>
#include <zircon/errors.h>

#include "src/devices/block/drivers/ufs/uic/uic_commands.h"

namespace ufs {

namespace {

using fuchsia_hardware_ufs_phy::wire::PowerState;

// TODO(https://fxbug.dev/578184694): Scale the active interconnect bandwidth vote from the
// negotiated UniPro gear/lane rate instead of voting a fixed static ceiling.
constexpr uint64_t kOperationalBandwidthBps = 1'000'000'000;  // 1 GB/s
constexpr uint64_t kInactiveBandwidthBps = 0;
constexpr uint32_t kInterconnectTagUfs = 'UFS ';

}  // namespace

zx::result<> UfsPdev::InitResources() {
  auto pdev = driver_incoming()->Connect<fuchsia_hardware_platform_device::Service::Device>("pdev");
  if (!pdev.is_ok()) {
    fdf::error("Failed to connect to platform device service: {}", pdev);
    return pdev.take_error();
  }

  fdf::PDev dev{std::move(pdev.value())};

  {
    auto mmio_params = dev.GetMmio(0);
    if (mmio_params.is_error()) {
      fdf::error("Failed to get MMIO: {}", mmio_params);
      return mmio_params.take_error();
    }

    mmio_buffer_vmo_ = std::move(mmio_params->vmo);
    mmio_buffer_size_ = mmio_params->size;
  }

  {
    auto bti = dev.GetBti(0);
    if (bti.is_error()) {
      fdf::error("Failed to get BTI: {}", bti);
      return bti.take_error();
    }
    bti_ = std::move(bti.value());
  }

  {
    auto irq_result = dev.GetInterrupt(0);
    if (irq_result.is_error()) {
      fdf::error("Failed to get IRQ: {}", irq_result);
      return irq_result.take_error();
    }
    irq_ = std::move(irq_result.value());
  }

  auto phy_client_end = driver_incoming()->Connect<fuchsia_hardware_ufs_phy::Service::Phy>("phy");
  if (phy_client_end.is_ok()) {
    ufs_phy_.Bind(std::move(phy_client_end.value()));
  } else {
    auto default_phy_client_end =
        driver_incoming()->Connect<fuchsia_hardware_ufs_phy::Service::Phy>();
    if (default_phy_client_end.is_ok()) {
      ufs_phy_.Bind(std::move(default_phy_client_end.value()));
    } else {
      fdf::warn("Could not connect to UFS PHY service: {}", default_phy_client_end);
    }
  }
  if (ufs_phy_.is_valid()) {
    supports_power_down_suspend_ = QueryPowerDownSuspendSupport();
  }

  auto interconnect_result =
      driver_incoming()->Connect<fuchsia_hardware_interconnect::PathService::Path>(
          "ufs-interconnect");
  if (interconnect_result.is_error()) {
    fdf::error("Failed to connect to interconnect: {}", interconnect_result);
    return interconnect_result.take_error();
  }

  interconnect_client_.Bind(std::move(interconnect_result.value()));

  if (zx::result<> result = SetBandwidthVote(true); result.is_error()) {
    return result.take_error();
  }

  SetHostControllerCallback(
      [this](NotifyEvent event, uint64_t data) { return PdevNotifyEventCallback(event, data); });

  return zx::ok();
}

zx_status_t UfsPdev::StopResources() {
  ZX_ASSERT(interconnect_client_.is_valid());

  static_cast<void>(SetBandwidthVote(false));

  StopUfshciServer();
  return ZX_OK;
}

zx::result<> UfsPdev::InitQuirk() { return zx::ok(); }

zx::result<> UfsPdev::PdevNotifyEventCallback(NotifyEvent event, uint64_t data) {
  switch (event) {
    case NotifyEvent::kPreLinkStartup:
      return PreLinkStartup();
    case NotifyEvent::kPrePowerModeChange:
      return PrePowerModeChange();
    case NotifyEvent::kPostSuspend:
      return PostSuspend();
    case NotifyEvent::kPreResume:
      return PreResume();
    case NotifyEvent::kLinkIdle:
      return SetPhyPowerState(PowerState::kLinkIdle);
    case NotifyEvent::kLinkActive:
    case NotifyEvent::kExitSleep:
      return SetPhyPowerState(PowerState::kActive);
    case NotifyEvent::kEnterSleep:
      return SetPhyPowerState(PowerState::kHibernateSleep);
    default:
      return Ufs::NotifyEventCallback(event, data);
  }
}

zx::result<> UfsPdev::SetPhyPowerState(PowerState state) {
  std::scoped_lock lock(power_state_lock_);
  if (state == PowerState::kActive) {
    // Restore the interconnect vote before ungating PHY clocks/rails, and still restore
    // PowerState::kActive on the PHY even if SetBandwidth fails because callers
    // (AcquireLinkActive, ExitRuntimeSleepLocked, ResumePower) proceed directly to
    // UFSHCI MMIO register accesses regardless of the notification status.
    zx::result<> bw_res = SetBandwidthVoteLocked(true);
    zx::result<> phy_res = CallPhySetPowerStateLocked(state);
    return phy_res.is_error() ? phy_res : bw_res;
  }

  zx::result<> phy_res = CallPhySetPowerStateLocked(state);
  const bool phy_transitioned = phy_res.is_ok();
  if (phy_res.is_error() &&
      (state != PowerState::kHibernateSleep || phy_res.status_value() != ZX_ERR_NOT_SUPPORTED)) {
    return phy_res.take_error();
  }
  if (zx::result<> bw_res = SetBandwidthVoteLocked(false); bw_res.is_error()) {
    // Failed runtime idle/sleep notifications leave the controller treating the link as active
    // (e.g. HandleLinkIdleTimer() keeps link_idle_ = false, so subsequent I/O accesses UFSHCI
    // MMIO without issuing Notify(kLinkActive)). Restore PowerState::kActive on the PHY before
    // returning the error so auxiliary clocks and M-PHY rails are not left gated.
    if (phy_transitioned && ufs_phy_.is_valid() &&
        (state == PowerState::kLinkIdle || state == PowerState::kHibernateSleep)) {
      if (zx::result<> rollback_res = CallPhySetPowerStateLocked(PowerState::kActive);
          rollback_res.is_error()) {
        fdf::error("Failed to roll back PHY to ACTIVE after SetBandwidth(false) failure: {}",
                   rollback_res);
      }
    }
    return bw_res.take_error();
  }
  return zx::ok();
}

zx::result<> UfsPdev::CallPhySetPowerStateLocked(PowerState state) {
  if (!ufs_phy_.is_valid()) {
    return zx::ok();
  }
  auto res = ufs_phy_->SetPowerState(state);
  if (!res.ok()) {
    // A PHY built against an older protocol has no SetPowerState at all: treat kActive as a no-op
    // and disable optional runtime PHY power-state voting without failing Sleep bandwidth drops.
    if (res.reason() == fidl::Reason::kUnknownMethod) {
      if (state == PowerState::kActive) {
        return zx::ok();
      }
      if (state == PowerState::kLinkIdle || state == PowerState::kHibernateSleep) {
        if (!runtime_power_state_unsupported_.exchange(true, std::memory_order_acq_rel)) {
          fdf::info("PHY does not implement SetPowerState; disabling runtime link-idle voting");
        }
        return zx::error(ZX_ERR_NOT_SUPPORTED);
      }
    }
    fdf::error("Failed to call SetPowerState({}): {}", fidl::ToUnderlying(state),
               res.status_string());
    return zx::error(res.status());
  }
  if (res->is_error()) {
    if (res->error_value() == ZX_ERR_NOT_SUPPORTED &&
        (state == PowerState::kLinkIdle || state == PowerState::kHibernateSleep)) {
      if (!runtime_power_state_unsupported_.exchange(true, std::memory_order_acq_rel)) {
        fdf::info("PHY does not implement PowerState {}; disabling runtime link-idle voting",
                  fidl::ToUnderlying(state));
      }
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
    fdf::error("SetPowerState({}) returned error status: {}", fidl::ToUnderlying(state),
               zx_status_get_string(res->error_value()));
    return zx::error(res->error_value());
  }
  return zx::ok();
}

zx::result<> UfsPdev::SetBandwidthVote(bool enable) {
  std::scoped_lock lock(power_state_lock_);
  return SetBandwidthVoteLocked(enable);
}

zx::result<> UfsPdev::SetBandwidthVoteLocked(bool enable) {
  if (!interconnect_client_.is_valid() || enable == bandwidth_vote_active_) {
    return zx::ok();
  }
  const uint64_t bw = enable ? kOperationalBandwidthBps : kInactiveBandwidthBps;
  fidl::Arena arena;
  auto request = fuchsia_hardware_interconnect::wire::BandwidthRequest::Builder(arena)
                     .average_bandwidth_bps(bw)
                     .peak_bandwidth_bps(bw)
                     .tag(kInterconnectTagUfs)
                     .Build();
  auto result = interconnect_client_->SetBandwidth(request);
  if (!result.ok()) {
    fdf::error("SetBandwidth({}) failed on interconnect: {}", enable,
               zx_status_get_string(result.status()));
    return zx::error(result.status());
  }
  if (result->is_error()) {
    fdf::error("SetBandwidth({}) returned error on interconnect: {}", enable,
               zx_status_get_string(result->error_value()));
    return zx::error(result->error_value());
  }
  bandwidth_vote_active_ = enable;
  return zx::ok();
}

zx::result<> UfsPdev::PostSuspend() {
  std::scoped_lock lock(power_state_lock_);
  zx::result<> phy_res = zx::ok();
  if (supports_power_down_suspend_) {
    phy_res = CallPhySetPowerStateLocked(PowerState::kPowerDown);
  }
  zx::result<> bw_res = SetBandwidthVoteLocked(false);
  return phy_res.is_error() ? phy_res : bw_res;
}

bool UfsPdev::QueryPowerDownSuspendSupport() {
  // This has to be known up front: SuspendPower() only powers down the PHY after the device has
  // been put in PowerDown and the host controller has been stopped, and discovering there that the
  // PHY cannot power-cycle would leave storage unusable until reboot. A PHY driver that predates
  // GetProperties() answers with an unknown-method error and is treated as unsupported.
  auto res = ufs_phy_->GetProperties();
  if (!res.ok()) {
    if (res.reason() == fidl::Reason::kUnknownMethod) {
      fdf::info("UFS PHY driver does not implement GetProperties; PowerDown suspend disabled");
    } else {
      fdf::warn("UFS PHY GetProperties() failed: {}; PowerDown suspend disabled",
                res.status_string());
    }
    return false;
  }
  if (!res->has_supports_suspend_resume() || !res->supports_suspend_resume()) {
    fdf::info("UFS PHY does not support PowerDown suspend; PowerDown suspend disabled");
    return false;
  }
  return true;
}

zx::result<> UfsPdev::PreResume() {
  std::scoped_lock lock(power_state_lock_);
  if (zx::result<> bw_res = SetBandwidthVoteLocked(true); bw_res.is_error()) {
    return bw_res.take_error();
  }
  if (!supports_power_down_suspend_) {
    return zx::ok();
  }
  return CallPhySetPowerStateLocked(PowerState::kActive);
}

// Triggers dynamic PHY calibration prior to UIC power mode / gear change
// (https://fxbug.dev/505006874).
zx::result<> UfsPdev::PrePowerModeChange() {
  if (!ufs_phy_.is_valid()) {
    return zx::ok();
  }

  auto res = ufs_phy_->PrePowerModeChange();
  if (!res.ok()) {
    fdf::error("Failed to call PrePowerModeChange: {}", res.status_string());
    return zx::error(res.status());
  }
  if (res->is_error()) {
    fdf::error("PrePowerModeChange returned error status: {}",
               zx_status_get_string(res->error_value()));
    return zx::error(res->error_value());
  }

  return zx::ok();
}

zx::result<> UfsPdev::PreLinkStartup() {
  if (!ufs_phy_.is_valid()) {
    return zx::ok();
  }

  auto client_end = StartUfshciServer();
  if (client_end.is_error()) {
    return client_end.take_error();
  }

  auto res = ufs_phy_->Init(std::move(client_end.value()));
  if (!res.ok()) {
    fdf::error("Failed to call Init: {}", res.status_string());
    return zx::error(res.status());
  }
  if (res->is_error()) {
    fdf::error("Init returned error status: {}", zx_status_get_string(res->error_value()));
    return zx::error(res->error_value());
  }

  return zx::ok();
}

zx::result<fidl::ClientEnd<fuchsia_hardware_ufs_phy::Ufshci>> UfsPdev::StartUfshciServer() {
  zx::result endpoints = fidl::CreateEndpoints<fuchsia_hardware_ufs_phy::Ufshci>();
  if (endpoints.is_error()) {
    fdf::error("Failed to create endpoints: {}", endpoints);
    return endpoints.take_error();
  }

  if (!ufshci_dispatcher_.get()) {
    ufshci_dispatcher_shutdown_completion_.Reset();
    auto dispatcher = fdf::SynchronizedDispatcher::Create(
        fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "ufshci-worker",
        [this](fdf_dispatcher_t*) { ufshci_dispatcher_shutdown_completion_.Signal(); });
    if (dispatcher.is_error()) {
      fdf::error("Failed to create Ufshci dispatcher: {}",
                 zx_status_get_string(dispatcher.status_value()));
      return zx::error(dispatcher.status_value());
    }
    ufshci_dispatcher_ = *std::move(dispatcher);
  }

  fidl::BindServer(ufshci_dispatcher_.async_dispatcher(), std::move(endpoints->server), this);

  return zx::ok(std::move(endpoints->client));
}

void UfsPdev::StopUfshciServer() {
  if (ufshci_dispatcher_.get()) {
    ufshci_dispatcher_.ShutdownAsync();
    ufshci_dispatcher_shutdown_completion_.Wait();
    ufshci_dispatcher_ = fdf::Dispatcher();
  }
}

void UfsPdev::DmeSet(DmeSetRequest& request, DmeSetCompleter::Sync& completer) {
  DmeSetUicCommand dme_set(*this, request.mib_attribute(), request.gen_selector_index(),
                           AttrSetType::kVolatile, request.value());
  auto result = dme_set.SendCommand();
  if (result.is_error()) {
    fdf::error("DME_SET 0x{:x} failed: {}", request.mib_attribute(), result);
    completer.Reply(zx::error(result.status_value()));
  } else {
    completer.Reply(zx::ok());
  }
}

void UfsPdev::DmeGet(DmeGetRequest& request, DmeGetCompleter::Sync& completer) {
  DmeGetUicCommand dme_get(*this, request.mib_attribute(), request.gen_selector_index());
  auto result = dme_get.SendCommand();
  if (result.is_error()) {
    fdf::error("DME_GET 0x{:x} failed: {}", request.mib_attribute(), result);
    completer.Reply(zx::error(result.status_value()));
  } else {
    completer.Reply(zx::ok(*result.value()));
  }
}

}  // namespace ufs
