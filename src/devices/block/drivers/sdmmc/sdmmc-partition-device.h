// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_SDMMC_SDMMC_PARTITION_DEVICE_H_
#define SRC_DEVICES_BLOCK_DRIVERS_SDMMC_SDMMC_PARTITION_DEVICE_H_

#include <fidl/fuchsia.hardware.block.volume/cpp/wire.h>
#include <fidl/fuchsia.hardware.inlineencryption/cpp/wire.h>
#include <fuchsia/hardware/block/driver/cpp/banjo.h>
#include <fuchsia/hardware/block/partition/cpp/banjo.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <zircon/types.h>

#include <fbl/auto_lock.h>

#include "sdmmc-types.h"
#include "src/storage/lib/block_server/block_server.h"

namespace sdmmc {

class SdmmcBlockDevice;

class PartitionDevice : public ddk::BlockImplProtocol<PartitionDevice>,
                        public ddk::BlockPartitionProtocol<PartitionDevice>,
                        public block_server::DriverInterface,
                        public fidl::WireServer<fuchsia_hardware_block_volume::Node> {
 public:
  PartitionDevice(SdmmcBlockDevice* sdmmc_parent, const block_info_t& block_info,
                  EmmcPartition partition);

  zx_status_t AddDevice();

  void BlockImplQuery(block_info_t* info_out, size_t* block_op_size_out);
  void BlockImplQueue(block_op_t* btxn, block_impl_queue_callback completion_cb, void* cookie);

  zx_status_t BlockPartitionGetGuid(guidtype_t guid_type, guid_t* out_guid);
  zx_status_t BlockPartitionGetName(char* out_name, size_t capacity);
  zx_status_t BlockPartitionGetMetadata(partition_metadata_t* out_metadata);

  EmmcPartition partition() const { return partition_; }
  block_info_t block_info() const { return block_info_; }

  // fuchsia.driver.framework.Node
  void AddChild(AddChildRequestView request, AddChildCompleter::Sync& completer) override;

  // Visible for testing.
  const block_impl_protocol_ops_t& block_impl_protocol_ops() const {
    return block_impl_protocol_ops_;
  }

  void SendReply(block_server::RequestId, zx::result<>);

  void StopBlockServer();

  // block_server::DriverInterface
  std::string_view SessionSchedulerRole() const final {
    return "fuchsia.devices.block.drivers.sdmmc.worker";
  }
  void OnRequests(cpp20::span<block_server::Request>) final;
  fdf::Logger& logger() const final;

 private:
  SdmmcBlockDevice* const sdmmc_parent_;
  const block_info_t block_info_;
  const EmmcPartition partition_;

  const char* partition_name_ = nullptr;
  fidl::WireSyncClient<fuchsia_driver_framework::NodeController> controller_;
  fidl::WireSyncClient<fuchsia_driver_framework::Node> node_;
  fidl::ServerBindingGroup<fuchsia_hardware_block_volume::Node> node_bindings_;
  fidl::ServerBindingGroup<fuchsia_hardware_inlineencryption::Device> ice_bindings_;

  fbl::Mutex lock_;
  std::optional<block_server::BlockServer> block_server_ TA_GUARDED(lock_);

  // Legacy DFv1-based protocols.
  // TODO(https://fxbug.dev/394968352): Remove once all clients use Volume service provided by
  // block_server_.
  compat::BanjoServer block_impl_server_{ZX_PROTOCOL_BLOCK_IMPL, this, &block_impl_protocol_ops_};
  std::optional<compat::BanjoServer> block_partition_server_;
  compat::SyncInitializedDeviceServer compat_server_;
};

}  // namespace sdmmc

#endif  // SRC_DEVICES_BLOCK_DRIVERS_SDMMC_SDMMC_PARTITION_DEVICE_H_
