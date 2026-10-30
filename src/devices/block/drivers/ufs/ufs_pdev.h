// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_PDEV_H_
#define SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_PDEV_H_

#include <fidl/fuchsia.hardware.interconnect/cpp/wire.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/wire.h>
#include <fidl/fuchsia.hardware.ufs.phy/cpp/fidl.h>
#include <lib/driver/logging/cpp/logger.h>

#include "src/devices/block/drivers/ufs/ufs.h"

namespace ufs {

class UfsPdev final : public Ufs, public fidl::Server<fuchsia_hardware_ufs_phy::Ufshci> {
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
  zx::result<> InitResources() override;
  zx_status_t StopResources() override;

  zx::result<> InitQuirk() override;

  zx::result<> PdevNotifyEventCallback(NotifyEvent event, uint64_t data);
  zx::result<> PreLinkStartup();
  zx::result<> PrePowerModeChange();
  zx::result<> PostSuspend();
  zx::result<> PreResume();
  zx::result<> SetPhyPowerState(fuchsia_hardware_ufs_phy::wire::PowerState state);
  zx::result<> SetBandwidthVote(bool enable);

  // Asks the connected PHY driver (UfsPhy.GetProperties) whether it supports POWER_DOWN suspend.
  bool QueryPowerDownSuspendSupport();

  zx::result<fidl::ClientEnd<fuchsia_hardware_ufs_phy::Ufshci>> StartUfshciServer();
  void StopUfshciServer();

 private:
  fidl::WireSyncClient<fuchsia_hardware_ufs_phy::UfsPhy> ufs_phy_;
  fidl::WireSyncClient<fuchsia_hardware_interconnect::Path> interconnect_client_;
  // Set by QueryPowerDownSuspendSupport() during InitResources().
  bool supports_power_down_suspend_ = false;
  // Set once the PHY rejects LINK_IDLE with ZX_ERR_NOT_SUPPORTED, so the idle timer stops asking.
  std::atomic<bool> runtime_power_state_unsupported_{false};

  fdf::Dispatcher ufshci_dispatcher_;
  libsync::Completion ufshci_dispatcher_shutdown_completion_;
};

}  // namespace ufs

#endif  // SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_PDEV_H_
