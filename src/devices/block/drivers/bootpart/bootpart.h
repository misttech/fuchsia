// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_BOOTPART_BOOTPART_H_
#define SRC_DEVICES_BLOCK_DRIVERS_BOOTPART_BOOTPART_H_

#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <fuchsia/hardware/block/driver/cpp/banjo.h>
#include <fuchsia/hardware/block/partition/cpp/banjo.h>
#include <lib/driver/compat/cpp/banjo_server.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/zbi-format/partition.h>

namespace bootpart {

class BootPartition : public ddk::BlockImplProtocol<BootPartition>,
                      public ddk::BlockPartitionProtocol<BootPartition> {
 public:
  BootPartition(const ddk::BlockImplProtocolClient& block_impl_client,
                fuchsia_boot_metadata::Partition partition, const block_info_t& block_info,
                size_t block_op_size)
      : block_impl_client_(block_impl_client),
        partition_(std::move(partition)),
        block_info_(block_info),
        block_op_size_(block_op_size) {
    // The last LBA is inclusive.
    block_info_.block_count = partition.last_block() - partition.first_block() + 1;
  }
  ~BootPartition() = default;

  zx::result<> Init(fidl::UnownedClientEnd<fuchsia_driver_framework::Node> parent,
                    const std::optional<std::string>& node_name,
                    const std::shared_ptr<fdf::Namespace>& incoming,
                    const std::shared_ptr<fdf::OutgoingDirectory>& outgoing,
                    async_dispatcher_t* dispatcher, size_t partition_index);

  // BlockImplProtocol implementation.
  void BlockImplQuery(block_info_t* out_info, uint64_t* out_block_op_size);
  void BlockImplQueue(block_op_t* bop, block_impl_queue_callback completion_cb, void* cookie);

  // BlockPartitionProtocol implementation.
  zx_status_t BlockPartitionGetGuid(guidtype_t guid_type, guid_t* out_guid);
  zx_status_t BlockPartitionGetName(char* out_name, size_t capacity);
  zx_status_t BlockPartitionGetMetadata(partition_metadata_t* out_metadata);

 private:
  ddk::BlockImplProtocolClient block_impl_client_;
  fuchsia_boot_metadata::Partition partition_;

  block_info_t block_info_;
  size_t block_op_size_;

  compat::BanjoServer block_impl_{ZX_PROTOCOL_BLOCK_IMPL, this, &block_impl_protocol_ops_};
  compat::BanjoServer block_partition_{ZX_PROTOCOL_BLOCK_PARTITION, this,
                                       &block_partition_protocol_ops_};
  compat::SyncInitializedDeviceServer compat_server_;
  fidl::ClientEnd<fuchsia_driver_framework::NodeController> child_;
};

class Driver : public fdf::DriverBase {
 public:
  static constexpr std::string_view kDriverName = "bootpart";

  Driver(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DriverBase(kDriverName, std::move(start_args), std::move(driver_dispatcher)) {}

  // fdf::DriverBase implementation.
  zx::result<> Start() override;

 private:
  std::vector<std::unique_ptr<BootPartition>> boot_partitions_;
};

}  // namespace bootpart

#endif  // SRC_DEVICES_BLOCK_DRIVERS_BOOTPART_BOOTPART_H_
