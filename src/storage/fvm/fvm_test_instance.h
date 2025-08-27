// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_FVM_FVM_TEST_INSTANCE_H_
#define SRC_STORAGE_FVM_FVM_TEST_INSTANCE_H_

#include <fidl/fuchsia.device/cpp/wire.h>
#include <fidl/fuchsia.hardware.block.volume/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/driver_test_realm/realm_builder/cpp/lib.h>
#include <lib/zx/result.h>

#include <memory>

#include <fbl/unique_fd.h>
#include <ramdevice-client/ramdisk.h>

#include "src/storage/lib/fs_management/cpp/fvm.h"

namespace fvm {

enum class FvmImplementation : uint8_t { kDriver, kComponent };

constexpr char kFvmDriverLib[] = "fvm.cm";

constexpr uuid::Uuid kTestUniqueGuid1 = {0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
                                         0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f};
constexpr uuid::Uuid kTestUniqueGuid2 = {0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
                                         0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f};

constexpr std::string_view kTestPartDataName = "data";
constexpr uuid::Uuid kTestPartDataGuid = {
    0xAA, 0xFF, 0xBB, 0x00, 0x33, 0x44, 0x88, 0x99, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
};

constexpr std::string_view kTestPartBlobName = "blob";
constexpr uuid::Uuid kTestPartBlobGuid = {
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0xAA, 0xFF, 0xBB, 0x00, 0x33, 0x44, 0x88, 0x99,
};

constexpr std::string_view kTestPartSystemName = "system";
constexpr uuid::Uuid kTestPartSystemGuid = {
    0xEE, 0xFF, 0xBB, 0x00, 0x33, 0x44, 0x88, 0x99, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
};

class BlockConnector {
 public:
  virtual ~BlockConnector() = default;

  virtual fidl::ClientEnd<fuchsia_hardware_block::Block> connect_block() const = 0;

  virtual fidl::UnownedClientEnd<fuchsia_hardware_block::Block> as_block() const = 0;

  virtual fidl::UnownedClientEnd<fuchsia_hardware_block_volume::Volume> as_volume() const = 0;
};

struct AllocatePartitionRequest {
  size_t slice_count = 1;
  const uuid::Uuid& type;
  const uuid::Uuid& guid;
  const std::string_view& name;
  uint32_t flags = 0;
};

class FvmInstance {
 public:
  virtual void SetUp() = 0;
  virtual void TearDown() = 0;

  virtual ~FvmInstance() = default;

  // Creates a ramdisk, destroying and recreating it if it already exists.
  virtual void CreateRamdisk(uint64_t block_size, uint64_t block_count) = 0;

  // Creates a ramdisk and formats it with fvm.
  virtual void CreateFvm(uint64_t block_size, uint64_t block_count, uint64_t slice_size) = 0;

  virtual void StartFvm() = 0;

  // Rebinds or restarts the FVM instance.
  virtual void RestartFvm() = 0;

  // Create a new ramdisk with a new total size. The block size must be the same as the existing
  // block size. This will start the disk and the fvm after recreating the disk.
  virtual void RestartFvmWithNewDiskSize(uint64_t block_size, uint64_t block_count) = 0;

  // Get general info about fvm.
  virtual fuchsia_hardware_block_volume::wire::VolumeManagerInfo GetFvmInfo() const = 0;

  // Allocates a new partition.
  virtual zx::result<std::unique_ptr<BlockConnector>> AllocatePartition(
      const AllocatePartitionRequest& request) const = 0;

  // Opens an existing partition. This will wait for it to appear if it doesn't already exist.
  virtual zx::result<std::unique_ptr<BlockConnector>> OpenPartition(
      std::string_view label) const = 0;

  // Destroys the named partition, removing it from this fvm instance.
  virtual void DestroyPartition(std::string_view label) const = 0;

  // Returns the block interface of the underlying ramdisk.
  virtual fidl::UnownedClientEnd<fuchsia_hardware_block::Block> GetRamdiskPartition() const = 0;
};

class DriverFvmInstance : public FvmInstance {
 public:
  void SetUp() override;
  void TearDown() override;
  void CreateRamdisk(uint64_t block_size, uint64_t block_count) override;
  void CreateFvm(uint64_t block_size, uint64_t block_count, uint64_t slice_size) override;
  void StartFvm() override;
  void RestartFvm() override;
  void RestartFvmWithNewDiskSize(uint64_t block_size, uint64_t block_count) override;
  fuchsia_hardware_block_volume::wire::VolumeManagerInfo GetFvmInfo() const override;
  zx::result<std::unique_ptr<BlockConnector>> AllocatePartition(
      const AllocatePartitionRequest& request) const override;
  zx::result<std::unique_ptr<BlockConnector>> OpenPartition(std::string_view label) const override;
  void DestroyPartition(std::string_view label) const override;
  fidl::UnownedClientEnd<fuchsia_hardware_block::Block> GetRamdiskPartition() const override;

  fidl::UnownedClientEnd<fuchsia_device::Controller> GetRamdiskControllerInterface() const;
  const fbl::unique_fd& devfs_root() const { return devfs_root_; }
  zx::result<fidl::ClientEnd<fuchsia_hardware_block_volume::VolumeManager>> GetVolumeManager()
      const;
  zx::result<std::unique_ptr<BlockConnector>> OpenPartitionNoWait(std::string_view label) const;
  std::string GetFvmPath() const;

 private:
  std::unique_ptr<async::Loop> loop_;
  std::unique_ptr<component_testing::RealmRoot> realm_;
  fbl::unique_fd devfs_root_;
  ramdisk_client_t* ramdisk_ = nullptr;
  zx::vmo vmo_;
};

std::unique_ptr<FvmInstance> CreateFvmInstance(FvmImplementation impl);

}  // namespace fvm

#endif  // SRC_STORAGE_FVM_FVM_TEST_INSTANCE_H_
