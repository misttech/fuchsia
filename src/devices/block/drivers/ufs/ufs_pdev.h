// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_PDEV_H_
#define SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_PDEV_H_

#include <fidl/fuchsia.hardware.interconnect/cpp/wire.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/wire.h>
#include <fidl/fuchsia.hardware.ufs.phy/cpp/fidl.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/zircon-internal/thread_annotations.h>

#include <mutex>

#include "src/devices/block/drivers/ufs/ufs.h"

namespace ufs {

// NOLINTNEXTLINE(misc-multiple-inheritance)
class UfsPdev : public Ufs, public fidl::Server<fuchsia_hardware_ufs_phy::Ufshci> {
 public:
  using Ufs::Ufs;

  bool SupportsPowerDownSuspend() const override { return supports_power_down_suspend_; }
  // Assumed when POWER_DOWN suspend is supported until the PHY returns ZX_ERR_NOT_SUPPORTED.
  bool SupportsRuntimePowerManagement() const override {
    return supports_power_down_suspend_ &&
           !runtime_power_state_unsupported_.load(std::memory_order_acquire);
  }

  // Ufshci Server methods
  void DmeSet(DmeSetRequest& request, DmeSetCompleter::Sync& completer) override;
  void DmeGet(DmeGetRequest& request, DmeGetCompleter::Sync& completer) override;
  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_ufs_phy::Ufshci> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    fdf::error("Unknown method in Ufshci server: {}", metadata.method_ordinal);
  }

 protected:
  zx::result<> PdevNotifyEventCallback(NotifyEvent event, uint64_t data) TA_EXCL(power_state_lock_);
  zx::result<> PreLinkStartup();
  zx::result<> PrePowerModeChange();
  zx::result<> PostSuspend() TA_EXCL(power_state_lock_);
  zx::result<> PreResume() TA_EXCL(power_state_lock_);
  zx::result<> SetPhyPowerState(fuchsia_hardware_ufs_phy::wire::PowerState state)
      TA_EXCL(power_state_lock_);
  zx::result<> SetBandwidthVote(bool enable) TA_EXCL(power_state_lock_);

  // Asks the connected PHY driver (UfsPhy.GetProperties) whether it supports POWER_DOWN suspend.
  bool QueryPowerDownSuspendSupport();

  zx::result<fidl::ClientEnd<fuchsia_hardware_ufs_phy::Ufshci>> StartUfshciServer();
  void StopUfshciServer();

 private:
  zx::result<> InitResources() override TA_EXCL(power_state_lock_);
  zx_status_t StopResources() override TA_EXCL(power_state_lock_);
  zx::result<> InitQuirk() override;

  zx::result<> CallPhySetPowerStateLocked(fuchsia_hardware_ufs_phy::wire::PowerState state)
      TA_REQ(power_state_lock_);
  zx::result<> SetBandwidthVoteLocked(bool enable) TA_REQ(power_state_lock_);

  fidl::WireSyncClient<fuchsia_hardware_ufs_phy::UfsPhy> ufs_phy_;
  fidl::WireSyncClient<fuchsia_hardware_interconnect::Path> interconnect_client_;
  // Serializes composite PHY power-state and interconnect bandwidth-vote transitions across
  // link-idle, runtime-sleep, system suspend/resume, link recovery, and driver teardown.
  // Strict leaf lock: must never acquire any other lock while held.
  std::mutex power_state_lock_;
  bool bandwidth_vote_active_ TA_GUARDED(power_state_lock_) = false;
  // Set by QueryPowerDownSuspendSupport() during InitResources().
  bool supports_power_down_suspend_ = false;
  // Set once the PHY rejects LINK_IDLE or HIBERNATE_SLEEP with ZX_ERR_NOT_SUPPORTED, so the idle
  // timers stop asking.
  std::atomic<bool> runtime_power_state_unsupported_{false};

  fdf::Dispatcher ufshci_dispatcher_;
  libsync::Completion ufshci_dispatcher_shutdown_completion_;
};

}  // namespace ufs

#endif  // SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_PDEV_H_
