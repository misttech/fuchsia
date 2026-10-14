// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/syscalls.h>
#include <zircon/syscalls/clock.h>

#include <usb-inspect/usb-inspect.h>
#include <usb/descriptors.h>
// We include this legacy Banjo header strictly to retrieve the C-struct layouts of standard
// USB descriptors (usb_interface_descriptor_t, usb_endpoint_descriptor_t) to dynamically
// parse and serialize the descriptor tree in our lazy inspect callback.
#include <fuchsia/hardware/usb/c/banjo.h>

#include <algorithm>
#include <format>

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_inspect {

const char* SpeedToString(usb_speed_t speed) {
  switch (static_cast<fdescriptor::UsbSpeed>(speed)) {
    case fdescriptor::UsbSpeed::kFull:
      return "full";
    case fdescriptor::UsbSpeed::kLow:
      return "low";
    case fdescriptor::UsbSpeed::kHigh:
      return "high";
    case fdescriptor::UsbSpeed::kSuper:
      return "super";
    default:
      return "undefined";
  }
}

// ============================================================================
// EndpointInspect Implementation
// ============================================================================

void EndpointInspect::Init(inspect::Node& parent, const std::string& name, size_t event_capacity,
                           size_t transfer_snapshot_capacity) {
  {
    std::lock_guard<std::mutex> _(lock_);
    event_history_capacity_ = event_capacity;
    event_history_.clear();
    event_history_index_ = 0;
    transfer_snapshot_capacity_ = transfer_snapshot_capacity;
    transfer_snapshots_.clear();
    transfer_snapshots_index_ = 0;
    pool_counts_cb_ = nullptr;
  }

  // Initialize atomic values
  tx_pending_requests_ = 0;
  rx_pending_requests_ = 0;
  rx_pending_processing_ = 0;
  total_bytes_tx_ = 0;
  total_bytes_rx_ = 0;
  failed_bytes_tx_ = 0;
  failed_bytes_rx_ = 0;
  max_bytes_per_second_ = 0;
  cancelled_ = false;

  last_total_bytes_val_.store(0, std::memory_order_relaxed);

  // Create the pure lazy node!
  lazy_node_ = parent.CreateLazyNode(name, [this]() {
    inspect::Inspector inspector;
    auto& root = inspector.GetRoot();

    std::scoped_lock _(lock_);

    uint64_t cur_tx = total_bytes_tx_;
    uint64_t cur_rx = total_bytes_rx_;
    uint64_t cur_max_rate = max_bytes_per_second_;

    root.CreateUint("tx_pending_requests", tx_pending_requests_, &inspector);
    root.CreateUint("rx_pending_requests", rx_pending_requests_, &inspector);
    root.CreateUint("rx_pending_processing", rx_pending_processing_, &inspector);
    root.CreateUint("total_bytes_tx", cur_tx, &inspector);
    root.CreateUint("total_bytes_rx", cur_rx, &inspector);
    if (uint64_t failed_tx = failed_bytes_tx_.load(std::memory_order_relaxed); failed_tx > 0) {
      root.CreateUint("failed_bytes_tx", failed_tx, &inspector);
    }
    if (uint64_t failed_rx = failed_bytes_rx_.load(std::memory_order_relaxed); failed_rx > 0) {
      root.CreateUint("failed_bytes_rx", failed_rx, &inspector);
    }
    root.CreateUint("max_bytes_per_second", cur_max_rate, &inspector);
    root.CreateBool("cancelled", cancelled_, &inspector);

    if (pool_counts_cb_) {
      auto [in_flight, total] = pool_counts_cb_();
      root.CreateUint("in_flight_requests", in_flight, &inspector);
      root.CreateUint("total_requests", total, &inspector);
    }

    if (!event_history_.empty()) {
      auto event_node = root.CreateChild("event_history");
      size_t index = event_history_index_ - event_history_.size();
      for (const auto& entry : event_history_) {
        auto node = event_node.CreateChild(std::to_string(index));
        node.CreateUint("@time", entry.timestamp, &inspector);
        node.CreateString("event", entry.event, &inspector);
        inspector.emplace(std::move(node));
        index++;
      }
      inspector.emplace(std::move(event_node));
    }

    if (!transfer_snapshots_.empty()) {
      auto snapshot_parent = root.CreateChild("transfer_snapshots");
      size_t index = transfer_snapshots_index_ - transfer_snapshots_.size();
      for (const auto& entry : transfer_snapshots_) {
        auto node = snapshot_parent.CreateChild(std::to_string(index));
        node.CreateUint("@time", entry.timestamp, &inspector);
        node.CreateUint("total_bytes_tx", entry.stats.total_bytes_tx, &inspector);
        node.CreateUint("total_bytes_rx", entry.stats.total_bytes_rx, &inspector);
        node.CreateUint("max_bytes_per_second", entry.stats.max_bytes_per_second, &inspector);
        inspector.emplace(std::move(node));
        index++;
      }
      inspector.emplace(std::move(snapshot_parent));
    }

    return fpromise::make_ok_promise(std::move(inspector));
  });
}

void EndpointInspect::MeasureThroughput(zx::duration elapsed) {
  uint64_t total_bytes = total_bytes_tx_.load(std::memory_order_relaxed) +
                         total_bytes_rx_.load(std::memory_order_relaxed);
  uint64_t last_val = last_total_bytes_val_.exchange(total_bytes, std::memory_order_relaxed);
  uint64_t delta = total_bytes >= last_val ? total_bytes - last_val : 0;

  int64_t elapsed_ns = elapsed.to_nsecs();
  if (elapsed_ns > 0) {
    uint64_t rate =
        static_cast<uint64_t>((static_cast<unsigned __int128>(delta) * 1'000'000'000) / elapsed_ns);
    uint64_t max_rate = max_bytes_per_second_.load(std::memory_order_relaxed);
    while (rate > max_rate && !max_bytes_per_second_.compare_exchange_weak(
                                  max_rate, rate, std::memory_order_relaxed)) {
    }
  }
}

void EndpointInspect::SnapshotTransferStatsLocked(zx_instant_boot_t raw_time) {
  if (transfer_snapshot_capacity_ == 0) {
    return;
  }
  if (transfer_snapshots_.size() >= transfer_snapshot_capacity_) {
    transfer_snapshots_.pop_front();
  }
  uint64_t tx = total_bytes_tx_;
  uint64_t rx = total_bytes_rx_;
  uint64_t max_rate = max_bytes_per_second_;
  transfer_snapshots_.push_back(TransferSnapshotEntry{
      .timestamp = raw_time,
      .stats =
          TransferStats{
              .total_bytes_tx = tx,
              .total_bytes_rx = rx,
              .max_bytes_per_second = max_rate,
          },
  });
  transfer_snapshots_index_++;
}

void EndpointInspect::SnapshotTransferStats() {
  if (transfer_snapshot_capacity_ == 0) {
    return;
  }
  std::scoped_lock _(lock_);
  SnapshotTransferStatsLocked(zx_clock_get_boot());
}

void EndpointInspect::RecordEvent(std::string_view event_name) {
  if (event_history_capacity_ == 0) {
    return;
  }
  std::scoped_lock _(lock_);
  if (event_history_.size() >= event_history_capacity_) {
    event_history_.pop_front();
  }

  zx_instant_boot_t raw_time = zx_clock_get_boot();
  event_history_.push_back(EventLogEntry{
      .timestamp = raw_time,
      .event = std::string(event_name),
  });
  event_history_index_++;
  SnapshotTransferStatsLocked(raw_time);
}

// ============================================================================
// DciInspect Implementation
// ============================================================================

void DciInspect::Init(inspect::Node& parent, const std::string& name, size_t ctrl_capacity,
                      size_t event_capacity) {
  {
    std::lock_guard<std::mutex> _(lock_);
    event_history_capacity_ = event_capacity;
    event_history_.clear();
    event_history_index_ = 0;

    control_history_capacity_ = ctrl_capacity;
    control_history_.clear();
    control_history_index_ = 0;

    state_ = "unknown";
    connected_ = false;
    speed_ = "undefined";
    usb_mode_ = "unknown";
    connect_count_ = 0;
    disconnect_count_ = 0;
    set_configuration_count_ = 0;
    last_connect_time_ns_ = 0;
    last_disconnect_time_ns_ = 0;
  }

  // Create the pure lazy node!
  lazy_node_ = parent.CreateLazyNode(name, [this]() {
    inspect::Inspector inspector;
    auto& root = inspector.GetRoot();

    std::lock_guard<std::mutex> _(lock_);
    root.CreateString("state", state_, &inspector);
    root.CreateBool("connected", connected_, &inspector);
    root.CreateString("speed", speed_, &inspector);
    root.CreateString("usb_mode", usb_mode_, &inspector);
    root.CreateUint("connect_count", connect_count_, &inspector);
    root.CreateUint("disconnect_count", disconnect_count_, &inspector);
    root.CreateUint("set_configuration_count", set_configuration_count_, &inspector);
    root.CreateInt("last_connect_time_ns", last_connect_time_ns_, &inspector);
    root.CreateInt("last_disconnect_time_ns", last_disconnect_time_ns_, &inspector);

    if (!event_history_.empty()) {
      auto event_node = root.CreateChild("event_history");
      size_t index = event_history_index_ - event_history_.size();
      for (const auto& entry : event_history_) {
        auto node = event_node.CreateChild(std::to_string(index++));
        node.CreateUint("@time", entry.timestamp, &inspector);
        node.CreateString("event", entry.event, &inspector);
        inspector.emplace(std::move(node));
      }
      inspector.emplace(std::move(event_node));
    }

    if (!control_history_.empty()) {
      auto control_node = root.CreateChild("control_history");
      size_t index = control_history_index_ - control_history_.size();
      for (const auto& entry : control_history_) {
        auto node = control_node.CreateChild(std::to_string(index++));
        node.CreateUint("@time", entry.timestamp, &inspector);
        node.CreateUint("bm_request_type", entry.bm_request_type, &inspector);
        node.CreateUint("b_request", entry.b_request, &inspector);
        node.CreateUint("w_value", entry.w_value, &inspector);
        node.CreateUint("w_index", entry.w_index, &inspector);
        node.CreateUint("w_length", entry.w_length, &inspector);
        node.CreateInt("status", entry.status, &inspector);
        node.CreateUint("response_length", entry.response_length, &inspector);
        inspector.emplace(std::move(node));
      }
      inspector.emplace(std::move(control_node));
    }

    return fpromise::make_ok_promise(std::move(inspector));
  });
}

void DciInspect::UpdateState(const std::string& state) {
  std::lock_guard<std::mutex> _(lock_);
  state_ = state;
}

void DciInspect::UpdateSpeed(usb_speed_t speed) {
  std::lock_guard<std::mutex> _(lock_);
  speed_ = SpeedToString(speed);
}

void DciInspect::UpdateConnectionStatus(bool connected, usb_speed_t speed) {
  std::lock_guard<std::mutex> _(lock_);
  if (connected && !connected_) {
    connect_count_++;
    last_connect_time_ns_ = zx_clock_get_boot();
  } else if (!connected && connected_) {
    disconnect_count_++;
    last_disconnect_time_ns_ = zx_clock_get_boot();
  }
  connected_ = connected;
  speed_ = SpeedToString(speed);
}

void DciInspect::RecordSetConfiguration(uint8_t config) {
  const std::string event = std::format("configuration set: {}", config);
  std::lock_guard<std::mutex> _(lock_);
  set_configuration_count_++;
  if (event_history_capacity_ > 0) {
    if (event_history_.size() >= event_history_capacity_) {
      event_history_.pop_front();
    }
    event_history_.push_back(EventLogEntry{zx_clock_get_boot(), event});
    event_history_index_++;
  }
}

void DciInspect::UpdateUsbMode(UsbMode usb_mode) {
  std::lock_guard<std::mutex> _(lock_);
  usb_mode_ = usb_mode_to_string(usb_mode);
}

void DciInspect::RecordEvent(const std::string& event_name) {
  if (event_history_capacity_ == 0) {
    return;
  }
  std::lock_guard<std::mutex> _(lock_);
  if (event_history_.size() >= event_history_capacity_) {
    event_history_.pop_front();
  }

  zx_instant_boot_t raw_time = zx_clock_get_boot();
  event_history_.push_back(EventLogEntry{raw_time, event_name});
  event_history_index_++;
}

void DciInspect::RecordControlTransfer(const ControlTransferInfo& info) {
  if (control_history_capacity_ == 0) {
    return;
  }
  std::lock_guard<std::mutex> _(lock_);
  if (control_history_.size() >= control_history_capacity_) {
    control_history_.pop_front();
  }

  zx_instant_boot_t raw_time = zx_clock_get_boot();
  control_history_.push_back(ControlTransferLogEntry{
      .timestamp = raw_time,
      .bm_request_type = info.request_type,
      .b_request = info.request,
      .w_value = info.value,
      .w_index = info.index,
      .w_length = info.length,
      .status = info.status,
      .response_length = info.actual_length,
  });
  control_history_index_++;
}

// ============================================================================
// FunctionInspect Implementation
// ============================================================================

void FunctionInspect::Init(inspect::Node& parent, const std::string& name, uint8_t index,
                           size_t event_capacity) {
  {
    std::lock_guard<std::mutex> _(lock_);
    event_history_capacity_ = event_capacity;
    event_history_.clear();
    event_history_index_ = 0;

    index_ = index;
    configuration_ = 0;
    configured_ = false;
    interface_class_ = 0;
    interface_subclass_ = 0;
    interface_protocol_ = 0;
    descriptors_.clear();
  }

  lazy_node_ = parent.CreateLazyNode(name, [this]() {
    inspect::Inspector inspector;
    auto& root = inspector.GetRoot();

    std::lock_guard<std::mutex> _(lock_);
    root.CreateUint("index", index_, &inspector);
    root.CreateUint("configuration", configuration_, &inspector);
    root.CreateBool("configured", configured_, &inspector);
    root.CreateUint("interface_class", interface_class_, &inspector);
    root.CreateUint("interface_subclass", interface_subclass_, &inspector);
    root.CreateUint("interface_protocol", interface_protocol_, &inspector);

    if (!descriptors_.empty()) {
      const uint8_t* desc_start = descriptors_.data();
      const uint8_t* ptr = desc_start;
      const uint8_t* end = desc_start + descriptors_.size();
      size_t interface_index = 0;
      size_t malformed_descriptor_count = 0;
      std::string descriptor_error;
      std::optional<inspect::Node> current_intf_node;
      std::optional<inspect::Node> current_ep_node;

      auto record_malformed = [&](std::string_view desc,
                                  std::optional<uint8_t> type_code = std::nullopt,
                                  std::optional<uint8_t> declared_length = std::nullopt,
                                  std::optional<size_t> expected_min_length = std::nullopt) {
        malformed_descriptor_count++;
        size_t offset = ptr - desc_start;
        if (type_code.has_value() && declared_length.has_value() &&
            expected_min_length.has_value()) {
          descriptor_error =
              std::format("Truncated {} at offset {}: type 0x{:02x}, length {} < expected {}", desc,
                          offset, *type_code, *declared_length, *expected_min_length);
        } else if (type_code.has_value() && declared_length.has_value()) {
          descriptor_error = std::format("Malformed {} at offset {}: type 0x{:02x}, length {}",
                                         desc, offset, *type_code, *declared_length);
        } else {
          descriptor_error = std::format("Malformed {} at offset {}", desc, offset);
        }
      };

      while (ptr < end) {
        if (ptr + sizeof(usb_descriptor_header_t) > end) {
          record_malformed("truncated_header");
          break;
        }
        const usb_descriptor_header_t* header =
            reinterpret_cast<const usb_descriptor_header_t*>(ptr);
        if (header->b_length < sizeof(usb_descriptor_header_t)) {
          record_malformed("invalid_header_length", static_cast<uint8_t>(header->b_descriptor_type),
                           header->b_length, sizeof(usb_descriptor_header_t));
          break;
        }
        if (ptr + header->b_length > end) {
          record_malformed("buffer_overflow", static_cast<uint8_t>(header->b_descriptor_type),
                           header->b_length);
          break;
        }

        if (header->b_descriptor_type == fdescriptor::DescriptorType::kInterface) {
          if (header->b_length < sizeof(usb_interface_descriptor_t)) {
            record_malformed("interface", static_cast<uint8_t>(header->b_descriptor_type),
                             header->b_length, sizeof(usb_interface_descriptor_t));
            break;
          }
          if (current_ep_node.has_value()) {
            inspector.emplace(std::move(*current_ep_node));
            current_ep_node.reset();
          }
          if (current_intf_node.has_value()) {
            inspector.emplace(std::move(*current_intf_node));
            current_intf_node.reset();
          }

          const usb_interface_descriptor_t* desc =
              reinterpret_cast<const usb_interface_descriptor_t*>(header);
          std::string intf_name = std::format("interface-{:03d}", interface_index++);

          current_intf_node = root.CreateChild(intf_name);
          current_intf_node->CreateUint("interface_number", desc->b_interface_number, &inspector);
          current_intf_node->CreateUint("alternate_setting", desc->b_alternate_setting, &inspector);
          current_intf_node->CreateUint("num_endpoints", desc->b_num_endpoints, &inspector);
          current_intf_node->CreateUint("interface_class", desc->b_interface_class, &inspector);
          current_intf_node->CreateUint("interface_subclass", desc->b_interface_sub_class,
                                        &inspector);
          current_intf_node->CreateUint("interface_protocol", desc->b_interface_protocol,
                                        &inspector);

        } else if (header->b_descriptor_type == fdescriptor::DescriptorType::kEndpoint) {
          if (header->b_length < sizeof(usb_endpoint_descriptor_t)) {
            record_malformed("endpoint", static_cast<uint8_t>(header->b_descriptor_type),
                             header->b_length, sizeof(usb_endpoint_descriptor_t));
            break;
          }
          if (current_ep_node.has_value()) {
            inspector.emplace(std::move(*current_ep_node));
            current_ep_node.reset();
          }
          const usb_endpoint_descriptor_t* desc =
              reinterpret_cast<const usb_endpoint_descriptor_t*>(header);
          std::string ep_name = std::format("endpoint-0x{:02x}", desc->b_endpoint_address);

          if (!current_intf_node.has_value()) {
            record_malformed("endpoint_without_interface",
                             static_cast<uint8_t>(header->b_descriptor_type), header->b_length);
            break;
          }
          current_ep_node = current_intf_node->CreateChild(ep_name);

          current_ep_node->CreateUint("endpoint_address", desc->b_endpoint_address, &inspector);
          current_ep_node->CreateUint("attributes", desc->bm_attributes, &inspector);
          current_ep_node->CreateUint("max_packet_size", le16toh(desc->w_max_packet_size),
                                      &inspector);
          current_ep_node->CreateUint("interval", desc->b_interval, &inspector);

        } else if (header->b_descriptor_type ==
                   fdescriptor::DescriptorType::kInterfaceAssociation) {
          if (header->b_length < sizeof(usb_interface_assoc_descriptor_t)) {
            record_malformed("interface_association",
                             static_cast<uint8_t>(header->b_descriptor_type), header->b_length,
                             sizeof(usb_interface_assoc_descriptor_t));
            break;
          }
          if (current_ep_node.has_value()) {
            inspector.emplace(std::move(*current_ep_node));
            current_ep_node.reset();
          }
          if (current_intf_node.has_value()) {
            inspector.emplace(std::move(*current_intf_node));
            current_intf_node.reset();
          }

          const usb_interface_assoc_descriptor_t* desc =
              reinterpret_cast<const usb_interface_assoc_descriptor_t*>(header);
          std::string iad_name = std::format("iad-0x{:02x}", desc->b_first_interface);

          inspect::Node iad_node = root.CreateChild(iad_name);
          iad_node.CreateUint("first_interface", desc->b_first_interface, &inspector);
          iad_node.CreateUint("interface_count", desc->b_interface_count, &inspector);
          iad_node.CreateUint("function_class", desc->b_function_class, &inspector);
          iad_node.CreateUint("function_subclass", desc->b_function_sub_class, &inspector);
          iad_node.CreateUint("function_protocol", desc->b_function_protocol, &inspector);

          inspector.emplace(std::move(iad_node));

        } else if (header->b_descriptor_type == fdescriptor::DescriptorType::kSsEpCompanion) {
          if (header->b_length < sizeof(usb_ss_ep_comp_descriptor_t)) {
            record_malformed("ss_companion", static_cast<uint8_t>(header->b_descriptor_type),
                             header->b_length, sizeof(usb_ss_ep_comp_descriptor_t));
            break;
          }
          if (!current_ep_node.has_value()) {
            record_malformed("companion_without_endpoint",
                             static_cast<uint8_t>(header->b_descriptor_type), header->b_length);
            break;
          }
          const usb_ss_ep_comp_descriptor_t* desc =
              reinterpret_cast<const usb_ss_ep_comp_descriptor_t*>(header);

          inspect::Node comp_node = current_ep_node->CreateChild("ss_companion");
          comp_node.CreateUint("max_burst", desc->b_max_burst, &inspector);
          comp_node.CreateUint("attributes", desc->bm_attributes, &inspector);
          comp_node.CreateUint("bytes_per_interval", le16toh(desc->w_bytes_per_interval),
                               &inspector);
          inspector.emplace(std::move(comp_node));
        } else if (header->b_descriptor_type == fdescriptor::DescriptorType::kSsIsochEpCompanion) {
          if (header->b_length < sizeof(usb_ss_isoch_ep_comp_descriptor_t)) {
            record_malformed("ss_isoch_companion", static_cast<uint8_t>(header->b_descriptor_type),
                             header->b_length, sizeof(usb_ss_isoch_ep_comp_descriptor_t));
            break;
          }
          if (!current_ep_node.has_value()) {
            record_malformed("isoch_companion_without_endpoint",
                             static_cast<uint8_t>(header->b_descriptor_type), header->b_length);
            break;
          }
          const usb_ss_isoch_ep_comp_descriptor_t* desc =
              reinterpret_cast<const usb_ss_isoch_ep_comp_descriptor_t*>(header);

          inspect::Node comp_node = current_ep_node->CreateChild("ss_isoch_companion");
          comp_node.CreateUint("bytes_per_interval", le32toh(desc->dw_bytes_per_interval),
                               &inspector);
          inspector.emplace(std::move(comp_node));
        } else {
          inspect::Node* parent_node = &root;
          if (current_ep_node.has_value()) {
            parent_node = &(*current_ep_node);
          } else if (current_intf_node.has_value()) {
            parent_node = &(*current_intf_node);
          }
          std::string node_name =
              std::format("descriptor-0x{:02x}", static_cast<uint8_t>(header->b_descriptor_type));
          inspect::Node desc_node = parent_node->CreateChild(node_name);
          desc_node.CreateUint("type_code", static_cast<uint8_t>(header->b_descriptor_type),
                               &inspector);
          desc_node.CreateUint("length", header->b_length, &inspector);
          std::string payload_hex;
          if (header->b_length > sizeof(usb_descriptor_header_t)) {
            const uint8_t* payload_data =
                reinterpret_cast<const uint8_t*>(header) + sizeof(usb_descriptor_header_t);
            size_t payload_len = header->b_length - sizeof(usb_descriptor_header_t);
            for (size_t i = 0; i < payload_len; ++i) {
              if (i > 0) {
                payload_hex += " ";
              }
              payload_hex += std::format("{:02x}", payload_data[i]);
            }
          }
          desc_node.CreateString("hex_payload", payload_hex, &inspector);
          inspector.emplace(std::move(desc_node));
        }
        ptr += header->b_length;
      }
      if (current_ep_node.has_value()) {
        inspector.emplace(std::move(*current_ep_node));
      }
      if (current_intf_node.has_value()) {
        inspector.emplace(std::move(*current_intf_node));
      }
      if (malformed_descriptor_count > 0) {
        root.CreateUint("malformed_descriptors", malformed_descriptor_count, &inspector);
        if (!descriptor_error.empty()) {
          root.CreateString("descriptor_error", descriptor_error, &inspector);
        }
      }
    }

    if (!event_history_.empty()) {
      auto event_node = root.CreateChild("event_history");
      size_t index = event_history_index_ - event_history_.size();
      for (const auto& entry : event_history_) {
        auto node = event_node.CreateChild(std::to_string(index++));
        node.CreateUint("@time", entry.timestamp, &inspector);
        node.CreateString("event", entry.event, &inspector);
        inspector.emplace(std::move(node));
      }
      inspector.emplace(std::move(event_node));
    }

    return fpromise::make_ok_promise(std::move(inspector));
  });
}

void FunctionInspect::UpdateConfiguration(uint8_t config, bool configured) {
  std::scoped_lock _(lock_);
  configuration_ = config;
  configured_ = configured;
}

void FunctionInspect::UpdateDescriptorInfo(uint8_t intf_class, uint8_t subclass, uint8_t protocol) {
  std::scoped_lock _(lock_);
  interface_class_ = intf_class;
  interface_subclass_ = subclass;
  interface_protocol_ = protocol;
}

void FunctionInspect::SetDescriptors(std::vector<uint8_t> descriptors) {
  std::scoped_lock _(lock_);
  descriptors_ = std::move(descriptors);
}

void FunctionInspect::RecordEvent(const std::string& event_name) {
  if (event_history_capacity_ == 0) {
    return;
  }
  std::scoped_lock _(lock_);
  if (event_history_.size() >= event_history_capacity_) {
    event_history_.pop_front();
  }

  zx_instant_boot_t raw_time = zx_clock_get_boot();
  event_history_.push_back(EventLogEntry{raw_time, event_name});
  event_history_index_++;
}

// ============================================================================
// FunctionDriverInspect Implementation
// ============================================================================

void FunctionDriverInspect::Init(inspect::Node& driver_node,
                                 std::optional<std::string_view> initial_state,
                                 size_t event_capacity) {
  {
    std::scoped_lock _(lock_);
    event_history_capacity_ = event_capacity;
    event_history_.clear();
    event_history_index_ = 0;
    if (initial_state.has_value()) {
      state_ = std::string(*initial_state);
    } else {
      state_.reset();
    }
  }

  online_ = false;
  configured_ = false;
  unconfiguring_ = false;
  set_configured_true_count_ = 0;
  set_configured_false_count_ = 0;
  unconfigure_start_boot_time_ = 0;
  last_unconfigure_duration_ns_ = 0;

  lazy_values_ = driver_node.CreateLazyValues("metrics", [this]() {
    inspect::Inspector inspector;
    auto& root = inspector.GetRoot();

    std::scoped_lock _(lock_);
    root.CreateBool("online", online_, &inspector);
    if (state_.has_value()) {
      root.CreateString("state", *state_, &inspector);
    }
    root.CreateBool("configured", configured_, &inspector);
    root.CreateBool("unconfiguring", unconfiguring_, &inspector);
    root.CreateUint("set_configured_true_count", set_configured_true_count_, &inspector);
    root.CreateUint("set_configured_false_count", set_configured_false_count_, &inspector);
    root.CreateInt("last_unconfigure_duration_ns", last_unconfigure_duration_ns_, &inspector);

    if (!event_history_.empty()) {
      auto event_node = root.CreateChild("event_history");
      size_t index = event_history_index_ - event_history_.size();
      for (const auto& entry : event_history_) {
        auto node = event_node.CreateChild(std::to_string(index++));
        node.CreateUint("@time", entry.timestamp, &inspector);
        node.CreateString("event", entry.event, &inspector);
        inspector.emplace(std::move(node));
      }
      inspector.emplace(std::move(event_node));
    }

    return fpromise::make_ok_promise(std::move(inspector));
  });
}

void FunctionDriverInspect::RecordEventLocked(std::string_view event_name,
                                              zx_instant_boot_t raw_time) {
  if (event_history_capacity_ == 0) {
    return;
  }
  if (event_history_.size() >= event_history_capacity_) {
    event_history_.pop_front();
  }
  event_history_.push_back(EventLogEntry{
      .timestamp = raw_time,
      .event = std::string(event_name),
  });
  event_history_index_++;
}

void FunctionDriverInspect::RecordEvent(std::string_view event_name) {
  std::scoped_lock _(lock_);
  RecordEventLocked(event_name, zx_clock_get_boot());
}

void FunctionDriverInspect::OnConfigured() {
  std::scoped_lock _(lock_);
  unconfiguring_ = false;
  unconfigure_start_boot_time_ = 0;
  configured_ = true;
  ++set_configured_true_count_;
  RecordEventLocked("SetConfigured(true)", zx_clock_get_boot());
}

void FunctionDriverInspect::OnUnconfigureStart() {
  std::scoped_lock _(lock_);
  zx_instant_boot_t now = zx_clock_get_boot();
  if (!unconfiguring_.exchange(true)) {
    unconfigure_start_boot_time_ = now;
  }
  ++set_configured_false_count_;
  RecordEventLocked("SetConfigured(false) start", now);
}

void FunctionDriverInspect::OnUnconfigureComplete(bool immediate) {
  std::scoped_lock _(lock_);
  zx_instant_boot_t now = zx_clock_get_boot();
  unconfiguring_ = false;
  configured_ = false;
  int64_t start = unconfigure_start_boot_time_.exchange(0);
  last_unconfigure_duration_ns_ = (!immediate && start > 0) ? (now - start) : 0;
  RecordEventLocked("SetConfigured(false) reply", now);
}

}  // namespace usb_inspect
