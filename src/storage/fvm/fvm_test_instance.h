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

  // Get general info about fvm.
  virtual fuchsia_hardware_block_volume::wire::VolumeManagerInfo GetFvmInfo() const = 0;

  // Allocates a new partition.
  virtual zx::result<std::unique_ptr<BlockConnector>> AllocatePartition(
      const AllocatePartitionRequest& request) const = 0;

  // Opens an existing partition. This will wait for it to appear if it doesn't already exist.
  virtual zx::result<std::unique_ptr<BlockConnector>> OpenPartition(
      const fs_management::PartitionMatcher& matcher) const = 0;

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
  fuchsia_hardware_block_volume::wire::VolumeManagerInfo GetFvmInfo() const override;
  zx::result<std::unique_ptr<BlockConnector>> AllocatePartition(
      const AllocatePartitionRequest& request) const override;
  zx::result<std::unique_ptr<BlockConnector>> OpenPartition(
      const fs_management::PartitionMatcher& matcher) const override;
  fidl::UnownedClientEnd<fuchsia_hardware_block::Block> GetRamdiskPartition() const override;

  fidl::UnownedClientEnd<fuchsia_device::Controller> GetRamdiskControllerInterface() const;
  const fbl::unique_fd& devfs_root() const { return devfs_root_; }
  zx::result<fidl::ClientEnd<fuchsia_hardware_block_volume::VolumeManager>> GetVolumeManager()
      const;
  zx::result<std::unique_ptr<BlockConnector>> OpenPartitionNoWait(
      const fs_management::PartitionMatcher& matcher) const;
  std::string GetFvmPath() const;

 private:
  std::unique_ptr<async::Loop> loop_;
  std::unique_ptr<component_testing::RealmRoot> realm_;
  fbl::unique_fd devfs_root_;
  ramdisk_client_t* ramdisk_ = nullptr;
};

}  // namespace fvm

#endif  // SRC_STORAGE_FVM_FVM_TEST_INSTANCE_H_
