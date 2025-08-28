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

#include "src/storage/lib/block_server/fake_server.h"
#include "src/storage/lib/fs_management/cpp/fvm.h"
#include "src/storage/lib/fs_management/cpp/mount.h"

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
    // We assume if the caller didn't destroy the ramdisk itself it wants a completely new disk.
    vmo_ = {};
  }
  if (!vmo_) {
    ASSERT_OK(zx::vmo::create(block_size * block_count, 0, &vmo_));
  }
  zx::vmo duplicate_vmo;
  ASSERT_OK(vmo_.duplicate(ZX_RIGHT_SAME_RIGHTS, &duplicate_vmo));
  static uint8_t type_guid[16] = {0};
  ASSERT_OK(ramdisk_create_at_from_vmo_with_params(devfs_root().get(), duplicate_vmo.release(),
                                                   block_size, type_guid, sizeof(type_guid),
                                                   &ramdisk_));
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

void DriverFvmInstance::RestartFvmWithNewDiskSize(uint64_t block_size, uint64_t block_count) {
  auto resp = fidl::WireCall(GetRamdiskPartition())->GetInfo();
  ASSERT_OK(resp.status());
  ASSERT_TRUE(resp->is_ok());
  uint32_t found_block_size = resp->value()->info.block_size;
  ASSERT_EQ(found_block_size, block_size);

  ASSERT_OK(ramdisk_destroy(ramdisk_));
  ramdisk_ = nullptr;

  zx::vmo vmo;
  ASSERT_OK(vmo_.create_child(ZX_VMO_CHILD_SNAPSHOT, 0, block_count * block_size, &vmo));
  vmo_ = std::move(vmo);

  CreateRamdisk(block_size, block_count);
  StartFvm();
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

void DriverFvmInstance::DestroyPartition(std::string_view label) const {
  zx::result partition = OpenPartition(label);
  ASSERT_OK(partition);
  fidl::WireResult result = fidl::WireCall(partition->as_volume())->Destroy();
  ASSERT_OK(result.status());
  ASSERT_OK(result->status);
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

class ComponentBlockConnector : public BlockConnector {
 public:
  ~ComponentBlockConnector() = default;

  static std::unique_ptr<BlockConnector> Create(const fs_management::MountedVolume* volume) {
    auto connector = std::make_unique<ComponentBlockConnector>();
    connector->volume_ = volume;
    fidl::ServerEnd server_end =
        fidl::Endpoints<fuchsia_io::Directory>::Create(&connector->svc_dir_);
    EXPECT_TRUE(fidl::WireCall(connector->volume_->ExportRoot())
                    ->Open("svc", fuchsia_io::kPermReadable, {}, server_end.TakeChannel())
                    .ok());
    connector->partition_ = fidl::ClientEnd<fuchsia_hardware_block_volume::Volume>(
        connector->connect_block().TakeChannel());
    return std::move(connector);
  }

  fidl::ClientEnd<fuchsia_hardware_block::Block> connect_block() const override {
    zx::result volume = component::ConnectAt<fuchsia_hardware_block_volume::Volume>(svc_dir_);
    EXPECT_OK(volume);
    return fidl::ClientEnd<fuchsia_hardware_block::Block>(volume->TakeChannel());
  }

  fidl::UnownedClientEnd<fuchsia_hardware_block::Block> as_block() const override {
    return fidl::UnownedClientEnd<fuchsia_hardware_block::Block>(partition_.channel().borrow());
  }

  fidl::UnownedClientEnd<fuchsia_hardware_block_volume::Volume> as_volume() const override {
    return partition_.borrow();
  }

 private:
  const fs_management::MountedVolume* volume_;
  fidl::ClientEnd<fuchsia_io::Directory> svc_dir_;
  fidl::ClientEnd<fuchsia_hardware_block_volume::Volume> partition_;
};

class ComponentFvmInstance : public FvmInstance {
 public:
  void SetUp() override {}

  void TearDown() override {}

  void CreateRamdisk(uint64_t block_size, uint64_t block_count) override {
    // This will also cause the destructor to run for any previous device.
    device_ = std::make_unique<block_server::FakeServer>(block_server::PartitionInfo{
        .block_count = block_count,
        .block_size = static_cast<uint32_t>(block_size),
        .max_transfer_size = 524288,
    });
    auto [block_client, block_server] =
        fidl::Endpoints<fuchsia_hardware_block_volume::Volume>::Create();
    block_ = fidl::ClientEnd<fuchsia_hardware_block::Block>(block_client.TakeChannel());
    device_->Serve(std::move(block_server));
  }

  void CreateFvm(uint64_t block_size, uint64_t block_count, uint64_t slice_size) override {
    CreateRamdisk(block_size, block_count);
    ASSERT_OK(fs_management::FvmInitPreallocated(GetRamdiskPartition(), block_count * block_size,
                                                 block_count * block_size, slice_size));
    StartFvm();
  }

  void StartFvm() override {
    ASSERT_TRUE(device_);
    auto [block_client, block_server] =
        fidl::Endpoints<fuchsia_hardware_block_volume::Volume>::Create();
    device_->Serve(std::move(block_server));
    zx::result fs = fs_management::MountMultiVolume(
        fidl::ClientEnd<fuchsia_hardware_block::Block>(block_client.TakeChannel()), component_, {});
    ASSERT_OK(fs);
    fvm_ = std::make_unique<fs_management::StartedMultiVolumeFilesystem>(std::move(*fs));
    auto info = GetFvmInfo();
    slice_size_ = info.slice_size;
  }

  void RestartFvm() override {
    if (fvm_) {
      ASSERT_OK(fvm_->Unmount());
      ASSERT_OK(component_.DestroyChild());
      fvm_ = {};
    }
    StartFvm();
  }

  void RestartFvmWithNewDiskSize(uint64_t block_size, uint64_t block_count) override {}

  fuchsia_hardware_block_volume::wire::VolumeManagerInfo GetFvmInfo() const override {
    EXPECT_TRUE(fvm_);
    zx::result volumes =
        component::ConnectAt<fuchsia_fs_startup::Volumes>(fvm_->ServiceDirectory());
    EXPECT_OK(volumes);
    fidl::WireResult info = fidl::WireCall(*volumes)->GetInfo();
    EXPECT_TRUE(info.ok());
    EXPECT_TRUE(info->is_ok());
    return *info.value()->info;
  }

  zx::result<std::unique_ptr<BlockConnector>> AllocatePartition(
      const AllocatePartitionRequest& request) const override {
    EXPECT_TRUE(fvm_);
    fidl::Array<uint8_t, 16> type_guid = fidl::Array<uint8_t, 16>{1, 2, 3, 4};
    memcpy(type_guid.data(), request.type.bytes(), 16);
    fidl::Arena arena;
    zx::result volume = fvm_->CreateVolume(request.name,
                                           fuchsia_fs_startup::wire::CreateOptions::Builder(arena)
                                               .type_guid(type_guid)
                                               .initial_size(request.slice_count * slice_size_)
                                               .Build(),
                                           {});
    if (volume.is_error()) {
      return volume.take_error();
    }
    return zx::ok(ComponentBlockConnector::Create(volume.value()));
  }

  zx::result<std::unique_ptr<BlockConnector>> OpenPartition(std::string_view label) const override {
    EXPECT_TRUE(fvm_);
    std::string name = std::string(label);
    const fs_management::MountedVolume* volume = fvm_->GetVolume(name);
    if (volume == nullptr) {
      zx::result opened_volume = fvm_->OpenVolume(name, {});
      if (opened_volume.is_error()) {
        return opened_volume.take_error();
      }
      volume = opened_volume.value();
    }
    return zx::ok(ComponentBlockConnector::Create(volume));
  }

  void DestroyPartition(std::string_view label) const override {
    ASSERT_OK(fvm_->RemoveVolume(label));
  }

  fidl::UnownedClientEnd<fuchsia_hardware_block::Block> GetRamdiskPartition() const override {
    return block_.borrow();
  }

 private:
  std::unique_ptr<block_server::FakeServer> device_;
  fidl::ClientEnd<fuchsia_hardware_block::Block> block_;
  fs_management::FsComponent component_ =
      fs_management::FsComponent::FromDiskFormat(fs_management::kDiskFormatFvm);
  std::unique_ptr<fs_management::StartedMultiVolumeFilesystem> fvm_;
  uint64_t slice_size_;
};

std::unique_ptr<FvmInstance> CreateFvmInstance(FvmImplementation impl) {
  switch (impl) {
    case FvmImplementation::kDriver:
      return std::make_unique<DriverFvmInstance>();
    case FvmImplementation::kComponent:
      return std::make_unique<ComponentFvmInstance>();
  }
}

}  // namespace fvm
