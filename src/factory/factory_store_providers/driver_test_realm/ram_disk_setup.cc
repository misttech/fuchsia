// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.driver.test/cpp/wire.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/device-watcher/cpp/device-watcher.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>

#include <bind/fuchsia/platform/cpp/bind.h>
#include <src/lib/files/file.h>
#include <src/lib/fsl/vmo/file.h>
#include <src/lib/fsl/vmo/sized_vmo.h>
#include <src/storage/lib/block_client/cpp/client.h>
#include <src/storage/lib/block_client/cpp/remote_block_device.h>
#include <src/storage/testing/ram_disk.h>

const int kRamdiskBlockSize = 1024;
constexpr char kExt4FilePath[] = "/pkg/data/factory_ext4.img";

zx::result<storage::RamDisk> MakeRamdisk() {
  fsl::SizedVmo result;
  if (!fsl::VmoFromFilename(kExt4FilePath, &result)) {
    FX_LOG_KV(ERROR, "Failed to read file", FX_KV("path", kExt4FilePath));
    return zx::make_result(ZX_ERR_INTERNAL).take_error();
  }

  auto size = result.size();
  zx::vmo vmo;
  result.vmo().create_child(ZX_VMO_CHILD_SNAPSHOT_AT_LEAST_ON_WRITE, 0, size, &vmo);

  auto ram_disk_or = storage::RamDisk::CreateWithVmo(std::move(vmo), kRamdiskBlockSize);
  if (!ram_disk_or.is_ok()) {
    FX_LOG_KV(ERROR, "Ramdisk failed to be created");
  } else {
    FX_LOG_KV(INFO, "Ramdisk created", FX_KV("path", ram_disk_or.value().path().c_str()));
  }

  return ram_disk_or;
}

int main() {
  fuchsia_logging::LogSettingsBuilder builder;
  builder.WithTags({"factory_driver_test_realm"}).BuildAndInitialize();

  auto client_end = component::Connect<fuchsia_driver_test::Realm>();
  if (!client_end.is_ok()) {
    FX_LOG_KV(ERROR, "Failed to connect to Realm FIDL", FX_KV("error", client_end.error_value()));
    return 1;
  }
  fidl::WireSyncClient client{std::move(*client_end)};

  fidl::Arena arena;
  auto wire_result =
      client->Start(fuchsia_driver_test::wire::RealmArgs::Builder(arena)
                        .root_driver("fuchsia-boot:///platform-bus#meta/platform-bus.cm")
                        .software_devices(std::vector{fuchsia_driver_test::wire::SoftwareDevice{
                            .device_name = "ram-disk",
                            .device_id = bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_RAM_DISK,
                        }})
                        .Build());
  if (wire_result.status() != ZX_OK) {
    FX_LOG_KV(ERROR, "Failed to call to Realm:Start", FX_KV("status", wire_result.status()));
    return 1;
  }
  if (wire_result->is_error()) {
    FX_LOG_KV(ERROR, "Realm:Start failed", FX_KV("error", wire_result->error_value()));
    return 1;
  }

  auto result = MakeRamdisk();
  // Keep the ramdisk until the test finishes.
  exit(0);
  return 0;
}
