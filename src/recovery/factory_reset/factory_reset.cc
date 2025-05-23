// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "factory_reset.h"

#include <dirent.h>
#include <fcntl.h>
#include <fidl/fuchsia.fshost/cpp/wire.h>
#include <fidl/fuchsia.hardware.block/cpp/wire.h>
#include <lib/component/incoming/cpp/clone.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fdio/cpp/caller.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fdio.h>
#include <lib/fit/defer.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/channel.h>
#include <zircon/errors.h>
#include <zircon/status.h>

#include "src/recovery/factory_reset/factory_reset_config.h"
#include "src/security/lib/kms-stateless/kms-stateless.h"
#include "src/security/lib/zxcrypt/client.h"
#include "src/storage/lib/fs_management/cpp/format.h"
namespace factory_reset {

const char* kBlockPath = "class/block";

zx_status_t ShredZxcryptDevice(fidl::ClientEnd<fuchsia_device::Controller> device,
                               fbl::unique_fd devfs_root_fd) {
  zxcrypt::VolumeManager volume(std::move(device), std::move(devfs_root_fd));

  // Note: the access to /dev/sys/platform from the manifest is load-bearing
  // here, because we can only find the related zxcrypt device for a particular
  // block device via appending "/zxcrypt" to its topological path, and the
  // canonical topological path sits under sys/platform.
  zx::channel driver_chan;
  if (zx_status_t status = volume.OpenClient(zx::sec(5), driver_chan); status != ZX_OK) {
    FX_PLOGS(ERROR, status) << "Couldn't open channel to zxcrypt volume manager";
    return status;
  }

  zxcrypt::EncryptedVolumeClient zxc_manager(std::move(driver_chan));
  if (zx_status_t status = zxc_manager.Shred(); status != ZX_OK) {
    FX_PLOGS(ERROR, status) << "Couldn't shred volume";
    return status;
  }

  return ZX_OK;
}

FactoryReset::FactoryReset(async_dispatcher_t* dispatcher,
                           fidl::ClientEnd<fuchsia_io::Directory> dev,
                           fidl::ClientEnd<fuchsia_hardware_power_statecontrol::Admin> admin,
                           fidl::ClientEnd<fuchsia_fshost::Admin> fshost_admin,
                           factory_reset_config::Config config)
    : dev_(std::move(dev)),
      admin_(std::move(admin), dispatcher),
      fshost_admin_(std::move(fshost_admin), dispatcher),
      config_(config) {}

void FactoryReset::Shred(fit::callback<void(zx_status_t)> callback) const {
  // First try and shred the data volume using fshost.
  auto cb = [this, callback = std::move(callback)](const auto& result) mutable {
    callback([this, &result]() {
      zx_status_t status;
      if (result.ok()) {
        const fit::result response = result.value();
        if (response.is_ok()) {
          FX_LOGS(INFO) << "fshost ShredDataVolume succeeded";
          return ZX_OK;
        }
        if (response.is_error()) {
          if (response.error_value() != ZX_ERR_NOT_SUPPORTED) {
            FX_PLOGS(ERROR, response.error_value()) << "fshost ShredDataVolume failed";
          }
        }
        status = response.error_value();
      } else {
        FX_LOGS(ERROR) << "Failed to call ShredDataVolume: " << result.FormatDescription();
        status = result.status();
      }
      if (config_.use_fxblob()) {
        // We can't fall back to shredding manually for Fxblob, so fail now.
        return status;
      }
      // Fall back to shredding all zxcrypt devices...
      FX_LOGS(INFO) << "Falling back to manually shredding zxcrypt...";
      auto [block_dir, block_dir_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
      if (zx_status_t status = fdio_open3_at(dev_.channel().get(), kBlockPath,
                                             uint64_t{fuchsia_io::wire::kPermReadable},
                                             block_dir_server.TakeChannel().release());
          status != ZX_OK) {
        FX_PLOGS(ERROR, status) << "Failed to open '" << kBlockPath << "'";
        return status;
      }
      int fd;
      if (zx_status_t status = fdio_fd_create(block_dir.TakeChannel().release(), &fd);
          status != ZX_OK) {
        FX_PLOGS(ERROR, status) << "Failed to create fd from '" << kBlockPath << "'";
        return status;
      }
      DIR* const dir = fdopendir(fd);
      auto cleanup = fit::defer([dir]() { closedir(dir); });
      fdio_cpp::UnownedFdioCaller caller(dirfd(dir));
      // Attempts to shred every zxcrypt volume found.
      while (true) {
        dirent* de = readdir(dir);
        if (de == nullptr) {
          return ZX_OK;
        }
        if (std::string_view(de->d_name) == ".") {
          continue;
        }
        zx::result block =
            component::ConnectAt<fuchsia_hardware_block::Block>(caller.directory(), de->d_name);
        if (block.is_error()) {
          FX_PLOGS(ERROR, block.status_value()) << "Error opening " << de->d_name;
          continue;
        }

        std::string controller_path = std::string(de->d_name) + "/device_controller";
        zx::result block_controller =
            component::ConnectAt<fuchsia_device::Controller>(caller.directory(), controller_path);
        if (block_controller.is_error()) {
          FX_PLOGS(ERROR, block_controller.status_value()) << "Error opening " << controller_path;
          continue;
        }
        if (fs_management::DetectDiskFormat(block.value()) == fs_management::kDiskFormatZxcrypt) {
          fbl::unique_fd dev_fd;
          {
            zx::result dev = component::Clone(dev_);
            if (dev.is_error()) {
              FX_PLOGS(ERROR, dev.error_value()) << "Error cloning connection to /dev";
              continue;
            }
            if (zx_status_t status = fdio_fd_create(dev.value().TakeChannel().release(),
                                                    dev_fd.reset_and_get_address());
                status != ZX_OK) {
              FX_PLOGS(ERROR, status) << "Error creating file descriptor from /dev";
              continue;
            }
          }

          zx_status_t status =
              ShredZxcryptDevice(std::move(block_controller.value()), std::move(dev_fd));
          if (status != ZX_OK) {
            FX_PLOGS(ERROR, status) << "Error shredding " << de->d_name;
            return status;
          }
          FX_LOGS(INFO) << "Successfully shredded " << de->d_name;
        }
      }
    }());
  };
  fshost_admin_->ShredDataVolume().ThenExactlyOnce(std::move(cb));
}

void FactoryReset::Reset(fit::callback<void(zx_status_t)> callback) {
  FX_LOGS(INFO) << "Reset called. Starting shred";
  Shred([this, callback = std::move(callback)](zx_status_t status) mutable {
    if (status != ZX_OK) {
      FX_PLOGS(ERROR, status) << "Shred failed";
      callback(status);
      return;
    }
    FX_LOGS(INFO) << "Finished shred";

    uint8_t key_info[kms_stateless::kExpectedKeyInfoSize] = "zxcrypt";
    switch (zx_status_t status = kms_stateless::RotateHardwareDerivedKeyFromService(key_info);
            status) {
      case ZX_OK:
        break;
      case ZX_ERR_NOT_SUPPORTED:
        FX_LOGS(WARNING)
            << "FactoryReset: The device does not support rotatable hardware keys. Ignoring";
        break;
      default:
        FX_PLOGS(ERROR, status) << "FactoryReset: RotateHardwareDerivedKey() failed";
        callback(status);
        return;
    }
    // Reboot to initiate the recovery.
    FX_LOGS(INFO) << "Requesting reboot...";
    auto cb = [callback = std::move(callback)](const auto& result) mutable {
      if (!result.ok()) {
        FX_PLOGS(ERROR, result.status()) << "Reboot call failed";
        callback(result.status());
        return;
      }
      const auto& response = result.value();
      if (response.is_error()) {
        FX_PLOGS(ERROR, response.error_value()) << "Reboot returned error";
        callback(response.error_value());
        return;
      }
      callback(ZX_OK);
    };
    fidl::Arena arena;
    auto builder = fuchsia_hardware_power_statecontrol::wire::RebootOptions::Builder(arena);
    fuchsia_hardware_power_statecontrol::RebootReason2 reasons[1] = {
        fuchsia_hardware_power_statecontrol::RebootReason2::kFactoryDataReset};
    auto vector_view =
        fidl::VectorView<fuchsia_hardware_power_statecontrol::RebootReason2>::FromExternal(reasons);
    builder.reasons(vector_view);
    admin_->PerformReboot(builder.Build()).ThenExactlyOnce(std::move(cb));
  });
}

void FactoryReset::Reset(ResetCompleter::Sync& completer) {
  Reset([completer = completer.ToAsync()](zx_status_t status) mutable { completer.Reply(status); });
}

}  // namespace factory_reset
