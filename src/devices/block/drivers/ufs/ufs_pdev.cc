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
      return SetPhyPowerState(PowerState::kActive);
    default:
      return Ufs::NotifyEventCallback(event, data);
  }
}

zx::result<> UfsPdev::SetPhyPowerState(PowerState state) {
  if (!ufs_phy_.is_valid()) {
    return zx::ok();
  }
  auto res = ufs_phy_->SetPowerState(state);
  if (!res.ok()) {
    // A PHY built against an older protocol has no SetPowerState at all: that only rules out the
    // optional runtime LINK_IDLE voting. PowerDown suspend / resume do need the call to succeed.
    if (res.reason() == fidl::Reason::kUnknownMethod && state == PowerState::kLinkIdle) {
      if (!runtime_power_state_unsupported_.exchange(true, std::memory_order_acq_rel)) {
        fdf::info("PHY does not implement SetPowerState; disabling runtime link-idle voting");
      }
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
    fdf::error("Failed to call SetPowerState({}): {}", static_cast<uint8_t>(state),
               res.status_string());
    return zx::error(res.status());
  }
  if (res->is_error()) {
    if (res->error_value() == ZX_ERR_NOT_SUPPORTED && state == PowerState::kLinkIdle) {
      if (!runtime_power_state_unsupported_.exchange(true, std::memory_order_acq_rel)) {
        fdf::info("PHY does not implement PowerState {}; disabling runtime link-idle voting",
                  static_cast<uint8_t>(state));
      }
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
    fdf::error("SetPowerState({}) returned error status: {}", static_cast<uint8_t>(state),
               zx_status_get_string(res->error_value()));
    return zx::error(res->error_value());
  }
  return zx::ok();
}

zx::result<> UfsPdev::SetBandwidthVote(bool enable) {
  if (!interconnect_client_.is_valid()) {
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
  return zx::ok();
}

zx::result<> UfsPdev::PostSuspend() {
  if (zx::result<> res = SetPhyPowerState(PowerState::kPowerDown); res.is_error()) {
    return res.take_error();
  }
  return SetBandwidthVote(false);
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
  if (zx::result<> bw_res = SetBandwidthVote(true); bw_res.is_error()) {
    return bw_res.take_error();
  }
  return SetPhyPowerState(PowerState::kActive);
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

  // Wait for bind completion, because calls to ufs_phy_ depends on the UFSHCI server working.
  libsync::Completion bind_completion;
  zx_status_t status = async::PostTask(
      ufshci_dispatcher_.async_dispatcher(),
      [this, server_end = std::move(endpoints->server), &bind_completion]() mutable {
        fidl::BindServer(ufshci_dispatcher_.async_dispatcher(), std::move(server_end), this);
        bind_completion.Signal();
      });
  if (status != ZX_OK) {
    fdf::error("Failed to post bind task: {}", zx_status_get_string(status));
    return zx::error(status);
  }
  bind_completion.Wait();

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
