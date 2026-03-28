// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/i2c/drivers/i2c/i2c.h"

#include <fidl/fuchsia.hardware.i2c/cpp/fidl.h>
#include <fidl/fuchsia.hardware.i2cimpl/cpp/fidl.h>
#include <fidl/fuchsia.scheduler/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/metadata/cpp/metadata.h>
#include <lib/trace/event.h>

namespace i2c {

zx::result<> I2cDriver::Start() {
  auto i2cimpl_result = incoming()->Connect<fuchsia_hardware_i2cimpl::Service::Device>();
  if (i2cimpl_result.is_error()) {
    fdf::error("Failed to connect to fuchsia.hardware.i2cimpl service: {}", i2cimpl_result);
    return i2cimpl_result.take_error();
  }
  i2c_.Bind(std::move(*i2cimpl_result));

  fidl::Arena arena;
  zx::result i2c_bus_metadata =
      fdf_metadata::GetMetadata<fuchsia_hardware_i2c_businfo::I2CBusMetadata>(incoming());
  if (i2c_bus_metadata.is_error()) {
    fdf::error("Failed to get i2c_bus_metadata  {}", i2c_bus_metadata);
    return i2c_bus_metadata.take_error();
  }

  if (!i2c_bus_metadata->channels().has_value()) {
    fdf::error("No channels supplied from the metadata");
    return zx::error(ZX_ERR_NO_RESOURCES);
  }

  fdf::Arena i2c_arena('I2CI');
  fdf::WireUnownedResult max_transfer_size = i2c_.buffer(i2c_arena)->GetMaxTransferSize();
  if (!max_transfer_size.ok()) {
    fdf::error("Failed to send GetMaxTransferSize request: {}", max_transfer_size.status_string());
    return zx::error(max_transfer_size.status());
  }
  if (max_transfer_size->is_error()) {
    fdf::error("Failed to get max transfer size: {}",
               zx_status_get_string(max_transfer_size->error_value()));
    return zx::error(max_transfer_size->error_value());
  }
  max_transfer_ = max_transfer_size->value()->size;

  // Add owned i2c node.
  zx::result child = AddOwnedChild("i2c");
  if (child.is_error()) {
    fdf::error("failed to add i2c child node: {}", child);
    return child.take_error();
  }

  i2c_node_ = std::move(child->node_);
  return AddI2cChildren(i2c_bus_metadata.value());
}

zx::result<> I2cDriver::AddI2cChildren(
    const fuchsia_hardware_i2c_businfo::I2CBusMetadata& metadata) {
  if (!metadata.channels()) {
    fdf::error("Failed to find number of channels in metadata: {}",
               zx_status_get_string(ZX_ERR_NOT_FOUND));
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  const auto config = take_config<i2c_config::Config>();

  fdf::debug("Number of i2c channels supplied: {}", metadata.channels()->size());
  const uint32_t bus_id = metadata.bus_id().value_or(0);
  for (const auto& channel : metadata.channels().value()) {
    // Add an i2c child to the owned i2c node.
    auto i2c_child_server = I2cChildServer::CreateAndAddChild(
        fit::bind_member(this, &I2cDriver::Transact), i2c_node_, logger(), bus_id, channel,
        incoming(), outgoing(), node_name(), config);
    if (i2c_child_server.is_error()) {
      fdf::error("Failed to create child server: {}",
                 zx_status_get_string(i2c_child_server.error_value()));
      return i2c_child_server.take_error();
    }
    child_servers_.push_back(std::move(i2c_child_server.value()));
  }

  return zx::ok();
}

void I2cDriver::Transact(uint16_t address, TransferRequestView request,
                         TransferCompleter::Sync& completer) {
  TRACE_DURATION("i2c", "I2cDevice Process Queued Transacts");

  if (request->transactions.size() < 1) {
    completer.ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }
  if (request->transactions.size() > fuchsia_hardware_i2c::wire::kMaxCountTransactions) {
    completer.ReplyError(ZX_ERR_OUT_OF_RANGE);
    return;
  }

  impl_ops_.clear();
  size_t total_transfer_size = 0;
  for (const auto& transaction : request->transactions) {
    if (!transaction.has_data_transfer()) {
      completer.ReplyError(ZX_ERR_INVALID_ARGS);
      return;
    }

    fuchsia_hardware_i2cimpl::wire::I2cImplOp impl_op{
        // Same address for all ops, since there is one address per channel.
        .address = address,
        .stop = transaction.has_stop() && transaction.stop(),
    };

    auto& data_transfer = transaction.data_transfer();
    if (data_transfer.is_read_size()) {
      if (data_transfer.read_size() > max_transfer_) {
        completer.ReplyError(ZX_ERR_INVALID_ARGS);
        return;
      }

      impl_op.type =
          fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithReadSize(data_transfer.read_size());
      total_transfer_size += data_transfer.read_size();
    } else if (data_transfer.is_write_data()) {
      if (data_transfer.write_data().empty()) {
        completer.ReplyError(ZX_ERR_INVALID_ARGS);
        return;
      }

      impl_op.type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithWriteData(
          fidl::ObjectView<fidl::VectorView<uint8_t>>::FromExternal(&data_transfer.write_data()));
      total_transfer_size += data_transfer.write_data().size();
    } else {
      completer.ReplyError(ZX_ERR_INVALID_ARGS);
      return;
    }

    if (total_transfer_size > fuchsia_hardware_i2c::kMaxTransferSize) {
      completer.ReplyError(ZX_ERR_OUT_OF_RANGE);
      return;
    }

    impl_ops_.push_back(impl_op);
  }
  impl_ops_.back().stop = true;

  fdf::Arena arena('I2CI');
  fdf::WireUnownedResult result = i2c_.buffer(arena)->Transact(
      fidl::VectorView<fuchsia_hardware_i2cimpl::wire::I2cImplOp>::FromExternal(impl_ops_));
  impl_ops_.clear();

  if (!result.ok()) {
    fdf::error("Failed to send Transfer request: {}", result.status_string());
    completer.ReplyError(result.status());
    return;
  }
  if (result->is_error()) {
    // Don't log at ERROR severity here, as some I2C devices intentionally NACK to indicate that
    // they are busy.
    fdf::debug("Failed to perform transfer: {}", zx_status_get_string(result->error_value()));
    completer.ReplyError(result->error_value());
    return;
  }

  read_vectors_.clear();
  for (const auto& read : result.value()->read) {
    read_vectors_.emplace_back(read.data);
  }

  completer.ReplySuccess(fidl::VectorView<fidl::VectorView<uint8_t>>::FromExternal(read_vectors_));
  read_vectors_.clear();
}

}  // namespace i2c

FUCHSIA_DRIVER_EXPORT(i2c::I2cDriver);
