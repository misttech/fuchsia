// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/fvm/fvm_test_instance.h"

#include <fidl/fuchsia.driver.test/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/device-watcher/cpp/device-watcher.h>
#include <lib/fdio/cpp/caller.h>
#include <lib/fdio/fd.h>

#include <bind/fuchsia/platform/cpp/bind.h>
#include <zxtest/zxtest.h>

#include "src/storage/lib/fs_management/cpp/fvm.h"

namespace fvm {

class DriverBlockConnector : public BlockConnector {
 public:
  explicit DriverBlockConnector(fidl::ClientEnd<fuchsia_device::Controller> controller)
      : controller_(std::move(controller)) {
    zx::result partition_server = fidl::CreateEndpoints(&partition_);
    EXPECT_OK(partition_server);
    EXPECT_TRUE(
        fidl::WireCall(controller_)->ConnectToDeviceFidl(partition_server->TakeChannel()).ok());
  }
  ~DriverBlockConnector() override = default;

  fidl::ClientEnd<fuchsia_hardware_block::Block> connect_block() const override {
    zx::result endpoints = fidl::CreateEndpoints<fuchsia_hardware_block::Block>();
    EXPECT_OK(endpoints);
    auto [block_client, server] = std::move(endpoints.value());
    EXPECT_TRUE(fidl::WireCall(controller_)->ConnectToDeviceFidl(server.TakeChannel()).ok());
    return std::move(block_client);
  }

  fidl::UnownedClientEnd<fuchsia_hardware_block::Block> as_block() const override {
    return fidl::UnownedClientEnd<fuchsia_hardware_block::Block>(partition_.channel().borrow());
  }

  fidl::UnownedClientEnd<fuchsia_hardware_block_volume::Volume> as_volume() const override {
    return fidl::UnownedClientEnd<fuchsia_hardware_block_volume::Volume>(
        partition_.channel().borrow());
  }

  static zx::result<std::unique_ptr<BlockConnector>> Create(
      fidl::ClientEnd<fuchsia_device::Controller> controller) {
    return zx::ok(std::make_unique<DriverBlockConnector>(std::move(controller)));
  }

 private:
  fidl::ClientEnd<fuchsia_device::Controller> controller_;
  fidl::ClientEnd<fuchsia_hardware_block_partition::Partition> partition_;
};

void DriverFvmInstance::SetUp() {
  loop_ = std::make_unique<async::Loop>(&kAsyncLoopConfigNoAttachToCurrentThread);
  loop_->StartThread();

  auto realm_builder = component_testing::RealmBuilder::Create();
  driver_test_realm::Setup(realm_builder);
  realm_ = std::make_unique<component_testing::RealmRoot>(realm_builder.Build(loop_->dispatcher()));

  zx::result dtr = realm_->component().Connect<fuchsia_driver_test::Realm>();
  ASSERT_OK(dtr);
  fidl::Arena arena;
  auto args_builder = fuchsia_driver_test::wire::RealmArgs::Builder(arena);
  args_builder.root_driver("fuchsia-boot:///platform-bus#meta/platform-bus.cm");
  args_builder.software_devices(std::vector{
      fuchsia_driver_test::wire::SoftwareDevice{
          .device_name = "ram-disk",
          .device_id = bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_RAM_DISK,
      },
  });
  fidl::WireResult result = fidl::WireCall(*dtr)->Start(args_builder.Build());
  ASSERT_OK(result.status());
  ASSERT_TRUE(result.value().is_ok());

  auto [devfs_client, server] = fidl::Endpoints<fuchsia_io::Node>::Create();
  fidl::UnownedClientEnd<fuchsia_io::Directory> exposed(
      realm_->component().exposed().unowned_channel());
  ASSERT_OK(fidl::WireCall(exposed)
                ->Open("dev-topological", fuchsia_io::kPermReadable, {}, server.TakeChannel())
                .status());
  ASSERT_OK(
      fdio_fd_create(devfs_client.TakeChannel().release(), devfs_root_.reset_and_get_address()));

  ASSERT_OK(
      device_watcher::RecursiveWaitForFile(devfs_root().get(), "sys/platform/ram-disk/ramctl"));
}

void DriverFvmInstance::TearDown() { ASSERT_OK(ramdisk_destroy(ramdisk_)); }

void DriverFvmInstance::CreateRamdisk(uint64_t block_size, uint64_t block_count) {
  if (ramdisk_ != nullptr) {
    ASSERT_OK(ramdisk_destroy(ramdisk_));
  }
  ASSERT_OK(ramdisk_create_at(devfs_root().get(), block_size, block_count, &ramdisk_));
}

void DriverFvmInstance::CreateFvm(uint64_t block_size, uint64_t block_count, uint64_t slice_size) {
  CreateRamdisk(block_size, block_count);

  ASSERT_OK(fs_management::FvmInitPreallocated(GetRamdiskPartition(), block_count * block_size,
                                               block_count * block_size, slice_size));

  StartFvm();
}

void DriverFvmInstance::StartFvm() {
  auto resp = fidl::WireCall(GetRamdiskControllerInterface())->Bind(kFvmDriverLib);
  ASSERT_OK(resp.status());
  ASSERT_TRUE(resp->is_ok());

  ASSERT_OK(device_watcher::RecursiveWaitForFile(devfs_root().get(), GetFvmPath().c_str()));
}

void DriverFvmInstance::RestartFvm() {
  auto resp = fidl::WireCall(GetRamdiskControllerInterface())->Rebind(kFvmDriverLib);
  ASSERT_OK(resp.status());
  ASSERT_TRUE(resp->is_ok());

  ASSERT_OK(device_watcher::RecursiveWaitForFile(devfs_root().get(), GetFvmPath().c_str()));
}

fuchsia_hardware_block_volume::wire::VolumeManagerInfo DriverFvmInstance::GetFvmInfo() const {
  zx::result fvm = GetVolumeManager();
  EXPECT_OK(fvm);
  zx::result info = fs_management::FvmQuery(fvm->borrow());
  EXPECT_OK(info);
  return *info;
}

zx::result<std::unique_ptr<BlockConnector>> DriverFvmInstance::AllocatePartition(
    const AllocatePartitionRequest& request) const {
  zx::result fvm = GetVolumeManager();
  if (fvm.is_error()) {
    return fvm.take_error();
  }
  fdio_cpp::UnownedFdioCaller caller(devfs_root());

  zx::result controller = fs_management::FvmAllocatePartitionWithDevfs(
      caller.directory(), *fvm, request.slice_count, request.type, request.guid, request.name,
      request.flags);
  if (controller.is_error()) {
    return controller.take_error();
  }
  return DriverBlockConnector::Create(std::move(controller.value()));
}

zx::result<std::unique_ptr<BlockConnector>> DriverFvmInstance::OpenPartition(
    std::string_view label) const {
  fdio_cpp::UnownedFdioCaller caller(devfs_root());
  zx::result controller =
      fs_management::OpenPartitionWithDevfs(caller.directory(), {.labels = {label}}, true);
  if (controller.is_error()) {
    return controller.take_error();
  }
  return DriverBlockConnector::Create(std::move(controller.value()));
}

fidl::UnownedClientEnd<fuchsia_hardware_block::Block> DriverFvmInstance::GetRamdiskPartition()
    const {
  return fidl::UnownedClientEnd<fuchsia_hardware_block::Block>(
      ramdisk_get_block_interface(ramdisk_));
}

fidl::UnownedClientEnd<fuchsia_device::Controller>
DriverFvmInstance::GetRamdiskControllerInterface() const {
  return fidl::UnownedClientEnd<fuchsia_device::Controller>(
      ramdisk_get_block_controller_interface(ramdisk_));
}

zx::result<fidl::ClientEnd<fuchsia_hardware_block_volume::VolumeManager>>
DriverFvmInstance::GetVolumeManager() const {
  fdio_cpp::UnownedFdioCaller caller(devfs_root());
  return component::ConnectAt<fuchsia_hardware_block_volume::VolumeManager>(caller.directory(),
                                                                            GetFvmPath());
}

zx::result<std::unique_ptr<BlockConnector>> DriverFvmInstance::OpenPartitionNoWait(
    std::string_view label) const {
  fdio_cpp::UnownedFdioCaller caller(devfs_root());
  zx::result controller =
      fs_management::OpenPartitionWithDevfs(caller.directory(), {.labels = {label}}, false);
  if (controller.is_error()) {
    return controller.take_error();
  }
  return DriverBlockConnector::Create(std::move(controller.value()));
}

std::string DriverFvmInstance::GetFvmPath() const {
  return std::string(ramdisk_get_path(ramdisk_)) + "/fvm";
}

}  // namespace fvm
