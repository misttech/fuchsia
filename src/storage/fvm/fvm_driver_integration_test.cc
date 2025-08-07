// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/device-watcher/cpp/device-watcher.h>
#include <lib/zx/result.h>

#include <zxtest/zxtest.h>

#include "src/storage/fvm/fvm_test_instance.h"

namespace {

using VolumeManagerInfo = fuchsia_hardware_block_volume::wire::VolumeManagerInfo;

class FvmDriverTest : public zxtest::Test {
 protected:
  void SetUp() override {
    instance_ = std::make_unique<fvm::DriverFvmInstance>();
    instance_->SetUp();
  }

  void TearDown() override { instance_->TearDown(); }

  fidl::UnownedClientEnd<fuchsia_device::Controller> ramdisk_controller_interface() const {
    return instance_->GetRamdiskControllerInterface();
  }

  fidl::UnownedClientEnd<fuchsia_hardware_block::Block> ramdisk_block_interface() const {
    return instance_->GetRamdiskPartition();
  }

  void FVMRebind() { instance_->RestartFvm(); }

  void StartFVM() { instance_->StartFvm(); }

  void CreateFVM(uint64_t block_size, uint64_t block_count, uint64_t slice_size) {
    instance_->CreateFvm(block_size, block_count, slice_size);
  }

  void CreateRamdisk(uint64_t block_size, uint64_t block_count) {
    instance_->CreateRamdisk(block_size, block_count);
  }

  void Upgrade(const uuid::Uuid& old_guid, const uuid::Uuid& new_guid, zx_status_t status) const;

  zx::result<std::unique_ptr<fvm::BlockConnector>> OpenPartitionNoWait(
      std::string_view label) const {
    return instance_->OpenPartitionNoWait(label);
  }

  zx::result<std::unique_ptr<fvm::BlockConnector>> WaitForPartition(std::string_view label) const {
    return instance_->OpenPartition(label);
  }

  zx::result<std::unique_ptr<fvm::BlockConnector>> AllocatePartition(
      fvm::AllocatePartitionRequest request) const {
    return instance_->AllocatePartition(request);
  }

 private:
  std::unique_ptr<fvm::DriverFvmInstance> instance_;
};

void FvmDriverTest::Upgrade(const uuid::Uuid& old_guid, const uuid::Uuid& new_guid,
                            zx_status_t status) const {
  zx::result fvm = instance_->GetVolumeManager();
  ASSERT_OK(fvm);
  fuchsia_hardware_block_partition::wire::Guid old_guid_fidl;
  std::copy(old_guid.cbegin(), old_guid.cend(), old_guid_fidl.value.begin());
  fuchsia_hardware_block_partition::wire::Guid new_guid_fidl;
  std::copy(new_guid.cbegin(), new_guid.cend(), new_guid_fidl.value.begin());

  const fidl::WireResult result = fidl::WireCall(*fvm)->Activate(old_guid_fidl, new_guid_fidl);
  ASSERT_OK(result.status());
  const fidl::WireResponse response = result.value();
  ASSERT_STATUS(response.status, status);
}

TEST_F(FvmDriverTest, TestVPartitionUpgrade) {
  constexpr uint64_t kBlockSize = 512;
  constexpr uint64_t kBlockCount = 1 << 16;
  constexpr uint64_t kSliceSize = 64 * kBlockSize;
  CreateFVM(kBlockSize, kBlockCount, kSliceSize);

  // Allocate two VParts, one active, and one inactive.
  {
    auto vp_fd_or = AllocatePartition({
        .type = fvm::kTestPartDataGuid,
        .guid = fvm::kTestUniqueGuid1,
        .name = fvm::kTestPartDataName,
        .flags = fuchsia_hardware_block_volume::wire::kAllocatePartitionFlagInactive,
    });
    ASSERT_OK(vp_fd_or, "Couldn't open Volume");
  }

  {
    auto vp_fd_or = AllocatePartition({
        .type = fvm::kTestPartDataGuid,
        .guid = fvm::kTestUniqueGuid2,
        .name = fvm::kTestPartBlobName,
    });
    ASSERT_OK(vp_fd_or, "Couldn't open volume");
  }

  // Release FVM device that we opened earlier
  FVMRebind();

  // The active partition should still exist.
  ASSERT_OK(WaitForPartition(fvm::kTestPartBlobName));
  // The inactive partition should be gone.
  ASSERT_STATUS(OpenPartitionNoWait(fvm::kTestPartDataName).status_value(), ZX_ERR_NOT_FOUND);

  // Reallocate GUID1 as inactive.

  {
    auto vp_fd_or = AllocatePartition({
        .type = fvm::kTestPartDataGuid,
        .guid = fvm::kTestUniqueGuid1,
        .name = fvm::kTestPartDataName,
        .flags = fuchsia_hardware_block_volume::wire::kAllocatePartitionFlagInactive,
    });
    ASSERT_OK(vp_fd_or, "Couldn't open new volume");
  }

  // Atomically set GUID1 as active and GUID2 as inactive.
  Upgrade(fvm::kTestUniqueGuid2, fvm::kTestUniqueGuid1, ZX_OK);
  // After upgrading, we should be able to open both partitions
  ASSERT_OK(WaitForPartition(fvm::kTestPartDataName));
  ASSERT_OK(WaitForPartition(fvm::kTestPartBlobName));

  // Rebind the FVM driver, check that the upgrade has succeeded.
  // The original (GUID2) should be deleted, and the new partition (GUID)
  // should exist.
  FVMRebind();

  ASSERT_OK(WaitForPartition(fvm::kTestPartDataName));
  ASSERT_STATUS(OpenPartitionNoWait(fvm::kTestPartBlobName).status_value(), ZX_ERR_NOT_FOUND);

  // Try upgrading when the "new" version doesn't exist.
  // (It should return an error and have no noticeable effect).
  Upgrade(fvm::kTestUniqueGuid1, fvm::kTestUniqueGuid2, ZX_ERR_NOT_FOUND);

  // Release FVM device that we opened earlier
  FVMRebind();

  ASSERT_OK(WaitForPartition(fvm::kTestPartDataName));
  ASSERT_STATUS(OpenPartitionNoWait(fvm::kTestPartBlobName).status_value(), ZX_ERR_NOT_FOUND);

  // Try upgrading when the "old" version doesn't exist.
  {
    auto vp_fd_or = AllocatePartition({
        .type = fvm::kTestPartDataGuid,
        .guid = fvm::kTestUniqueGuid2,
        .name = fvm::kTestPartBlobName,
        .flags = fuchsia_hardware_block_volume::wire::kAllocatePartitionFlagInactive,
    });
    ASSERT_OK(vp_fd_or, "Couldn't open volume");
  }

  uuid::Uuid fake_guid = {};
  Upgrade(fake_guid, fvm::kTestUniqueGuid2, ZX_OK);

  FVMRebind();

  // We should be able to open both partitions again.
  zx::result vp_or = WaitForPartition(fvm::kTestPartDataName);
  ASSERT_OK(vp_or);
  ASSERT_OK(WaitForPartition(fvm::kTestPartBlobName));

  // Destroy and reallocate the first partition as inactive.
  {
    const fidl::WireResult result = fidl::WireCall(vp_or->as_volume())->Destroy();
    ASSERT_OK(result.status());
    const fidl::WireResponse response = result.value();
    ASSERT_OK(response.status);
  }
  {
    zx::result vp_or = AllocatePartition({
        .type = fvm::kTestPartDataGuid,
        .guid = fvm::kTestUniqueGuid1,
        .name = fvm::kTestPartDataName,
        .flags = fuchsia_hardware_block_volume::wire::kAllocatePartitionFlagInactive,
    });
    ASSERT_OK(vp_or, "Couldn't open volume");
  }

  // Upgrade the partition with old_guid == new_guid.
  // This should activate the partition.
  Upgrade(fvm::kTestUniqueGuid1, fvm::kTestUniqueGuid1, ZX_OK);

  FVMRebind();

  // We should be able to open both partitions again.
  ASSERT_OK(WaitForPartition(fvm::kTestPartDataName));
  ASSERT_OK(WaitForPartition(fvm::kTestPartBlobName));
}

TEST_F(FvmDriverTest, TestAbortDriverLoadSmallDevice) {
  constexpr uint64_t kMB = 1 << 20;
  constexpr uint64_t kGB = 1 << 30;
  constexpr uint64_t kBlockSize = 512;
  constexpr uint64_t kBlockCount = 50 * kMB / kBlockSize;
  constexpr uint64_t kSliceSize = kMB;
  constexpr uint64_t kFvmPartitionSize = 4 * kGB;

  CreateRamdisk(kBlockSize, kBlockCount);

  // Init fvm with a partition bigger than the underlying disk.
  fs_management::FvmInitWithSize(ramdisk_block_interface(), kFvmPartitionSize, kSliceSize);

  // Try to bind an fvm to the disk.
  //
  // Bind should return ZX_ERR_IO when the load of a driver fails.
  auto resp = fidl::WireCall(ramdisk_controller_interface())->Bind(fvm::kFvmDriverLib);
  ASSERT_OK(resp.status());
  ASSERT_FALSE(resp->is_ok());
  ASSERT_EQ(resp->error_value(), ZX_ERR_INTERNAL);

  CreateRamdisk(kBlockSize, kFvmPartitionSize / kBlockSize);
  fs_management::FvmInitWithSize(ramdisk_block_interface(), kFvmPartitionSize, kSliceSize);

  // Make sure it starts successfully. This asserts on failures.
  StartFVM();
}

}  // namespace
