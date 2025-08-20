// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/bootpart/bootpart.h"

#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <fuchsia/hardware/block/driver/cpp/banjo.h>
#include <fuchsia/hardware/block/partition/cpp/banjo.h>
#include <lib/ddk/metadata.h>
#include <lib/driver/compat/cpp/banjo_client.h>
#include <lib/driver/compat/cpp/metadata.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/zbi-format/partition.h>
#include <lib/zbi-format/zbi.h>
#include <stdio.h>
#include <zircon/types.h>

#include "src/devices/block/lib/common/include/common.h"

namespace {

std::string ToGuidString(const uint8_t* src) {
  const struct guid* guid = reinterpret_cast<const struct guid*>(src);
  return std::format(
      "{:#08X}-{:#04X}-{:#04X}-{:#02X}{:#02X}-{:#02X}{:#02X}{:#02X}{:#02X}{:#02X}{:#02X}",
      guid->data1, guid->data2, guid->data3, guid->data4[0], guid->data4[1], guid->data4[2],
      guid->data4[3], guid->data4[4], guid->data4[5], guid->data4[6], guid->data4[7]);
}

}  // namespace

namespace bootpart {

// implement device protocol:

void BootPartition::BlockImplQuery(block_info_t* out_info, uint64_t* out_block_op_size) {
  *out_info = block_info_;
  *out_block_op_size = block_op_size_;
}

void BootPartition::BlockImplQueue(block_op_t* bop, block_impl_queue_callback completion_cb,
                                   void* cookie) {
  switch (bop->command.opcode) {
    case BLOCK_OPCODE_READ:
    case BLOCK_OPCODE_WRITE: {
      if (zx_status_t status =
              block::CheckIoRange(bop->rw, block_info_.block_count, *fdf::Logger::GlobalInstance());
          status != ZX_OK) {
        completion_cb(cookie, status, bop);
        return;
      }

      // Adjust for partition starting block
      bop->rw.offset_dev += partition_.first_block();
      break;
    }
    case BLOCK_OPCODE_FLUSH:
      break;
    default:
      completion_cb(cookie, ZX_ERR_NOT_SUPPORTED, bop);
      return;
  }

  block_impl_client_.Queue(bop, completion_cb, cookie);
}

static_assert(ZBI_PARTITION_GUID_LEN == GUID_LENGTH, "GUID length mismatch");

zx_status_t BootPartition::BlockPartitionGetGuid(guidtype_t guid_type, guid_t* out_guid) {
  switch (guid_type) {
    case GUIDTYPE_TYPE:
      memcpy(out_guid, partition_.type_guid().data(), ZBI_PARTITION_GUID_LEN);
      return ZX_OK;
    case GUIDTYPE_INSTANCE:
      memcpy(out_guid, partition_.unique_guid().data(), ZBI_PARTITION_GUID_LEN);
      return ZX_OK;
    default:
      return ZX_ERR_INVALID_ARGS;
  }
}

static_assert(ZBI_PARTITION_NAME_LEN <= MAX_PARTITION_NAME_LENGTH, "Name length mismatch");

zx_status_t BootPartition::BlockPartitionGetName(char* out_name, size_t capacity) {
  if (capacity < ZBI_PARTITION_NAME_LEN + 1) {
    return ZX_ERR_BUFFER_TOO_SMALL;
  }

  size_t len = strnlen(partition_.name().c_str(), ZBI_PARTITION_NAME_LEN);
  memcpy(out_name, partition_.name().c_str(), len);
  out_name[len] = '\0';

  return ZX_OK;
}

zx_status_t BootPartition::BlockPartitionGetMetadata(partition_metadata_t* out_metadata) {
  if (zx_status_t status = BlockPartitionGetName(out_metadata->name, sizeof(out_metadata->name));
      status != ZX_OK) {
    return status;
  }
  memcpy(&out_metadata->type_guid, partition_.type_guid().data(), ZBI_PARTITION_GUID_LEN);
  memcpy(&out_metadata->instance_guid, partition_.unique_guid().data(), ZBI_PARTITION_GUID_LEN);
  out_metadata->start_block_offset = partition_.first_block();
  out_metadata->num_blocks = partition_.last_block() - partition_.first_block();
  out_metadata->flags = partition_.flags();
  return ZX_OK;
}

zx::result<> Driver::Start() {
  zx::result block_impl_result = compat::ConnectBanjo<ddk::BlockImplProtocolClient>(incoming());
  if (block_impl_result.is_error()) {
    fdf::error("Failed to connect to block-impl banjo protocol: {}", block_impl_result);
    return block_impl_result.take_error();
  }
  ddk::BlockImplProtocolClient block_impl(block_impl_result.value());

  zx::result metadata = compat::GetMetadata<fuchsia_boot_metadata::PartitionMap>(
      incoming(), DEVICE_METADATA_PARTITION_MAP);
  if (metadata.is_error()) {
    fdf::error("Failed to get metadata: {}", metadata);
    return metadata.take_error();
  }
  const auto& partition_map = metadata.value();

  if (!partition_map.partitions().has_value() || partition_map.partitions().value().empty()) {
    fdf::error("Missing partitions");
    return zx::error(ZX_ERR_INTERNAL);
  }
  const auto& partitions = partition_map.partitions().value();

  block_info_t block_info;
  size_t block_op_size;
  block_impl.Query(&block_info, &block_op_size);

  for (size_t i = 0; i < partitions.size(); ++i) {
    std::unique_ptr<BootPartition>& bootpart = boot_partitions_.emplace_back(
        std::make_unique<BootPartition>(block_impl, partitions[i], block_info, block_op_size));
    zx::result result =
        bootpart->Init(node(), node_name(), incoming(), outgoing(), dispatcher(), i);
    if (result.is_error()) {
      fdf::error("Failed to initialize boot partition {}: {}", i, result);
      return result.take_error();
    }
  }
  return zx::ok();
}

zx::result<> BootPartition::Init(fidl::UnownedClientEnd<fuchsia_driver_framework::Node> parent,
                                 const std::optional<std::string>& node_name,
                                 const std::shared_ptr<fdf::Namespace>& incoming,
                                 const std::shared_ptr<fdf::OutgoingDirectory>& outgoing,
                                 async_dispatcher_t* dispatcher, size_t partition_index) {
  const std::string type_guid = ToGuidString(partition_.type_guid().data());
  const std::string uniq_guid = ToGuidString(partition_.unique_guid().data());
  const std::string partition_name = std::format("part-{:03}", partition_index);
  fdf::trace("Partition {} ({}) type={} guid={} name={} first={:#08x} last={:#08x}",
             partition_index, partition_name, type_guid, uniq_guid, partition_.name(),
             partition_.first_block(), partition_.last_block());

  compat::DeviceServer::BanjoConfig banjo_config{.default_proto_id = ZX_PROTOCOL_BLOCK_IMPL};
  banjo_config.callbacks[ZX_PROTOCOL_BLOCK_IMPL] = block_impl_.callback();
  banjo_config.callbacks[ZX_PROTOCOL_BLOCK_PARTITION] = block_partition_.callback();

  zx::result result =
      compat_server_.Initialize(incoming, outgoing, node_name, partition_name,
                                compat::ForwardMetadata::None(), std::move(banjo_config));
  if (result.is_error()) {
    fdf::error("Failed to initialize compat server: {}", result);
    return result.take_error();
  }

  const std::vector<fuchsia_driver_framework::Offer> offers = compat_server_.CreateOffers2();

  const std::vector<fuchsia_driver_framework::NodeProperty2> properties = {
      fdf::MakeProperty2(bind_fuchsia::PROTOCOL, static_cast<uint32_t>(ZX_PROTOCOL_BLOCK_IMPL)),
      fdf::MakeProperty2(bind_fuchsia::PROTOCOL,
                         static_cast<uint32_t>(ZX_PROTOCOL_BLOCK_PARTITION)),
  };

  zx::result child =
      fdf::AddChild(parent, *fdf::Logger::GlobalInstance(), partition_name, properties, offers);
  if (child.is_error()) {
    fdf::error("Failed to add child: {}", child);
    return child.take_error();
  }
  child_ = std::move(child.value());

  return zx::ok();
}

}  // namespace bootpart

FUCHSIA_DRIVER_EXPORT(bootpart::Driver);
