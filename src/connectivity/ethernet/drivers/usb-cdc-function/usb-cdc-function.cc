// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/ethernet/drivers/usb-cdc-function/usb-cdc-function.h"

#include <endian.h>
#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.network/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.endpoint/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.function/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/driver/compat/cpp/banjo_client.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/metadata/cpp/metadata_server.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fit/defer.h>
#include <lib/trace/event.h>

#include <cstdint>
#include <cstring>
#include <mutex>
#include <vector>

#include <usb-endpoint/usb-endpoint-client.h>
#include <usb/request-fidl.h>

namespace {

// TODO(https://fxbug.dev/436378683): Remove once usb-cdc-function no longer
// hangs while initializing.
enum class InitState {
  kWaitingForAllocInterface1 = 0,
  kWaitingForAllocInterface2 = 1,
  kWaitingForAllocEp1 = 2,
  kWaitingForAllocEp2 = 3,
  kWaitingForAllocEp3 = 4,
  kWaitingForMacAddress = 5,
  kWaitingForInit1 = 6,
  kWaitingForAddRequests1 = 7,
  kWaitingForInit2 = 8,
  kWaitingForAddRequests2 = 9,
  kWaitingForInit3 = 10,
  kWaitingForAddRequests3 = 11,
  kWaitingForSetInterface = 12
};

}  // namespace

namespace usb_cdc_function {

namespace fendpoint = fuchsia_hardware_usb_endpoint;
namespace ffunction = fuchsia_hardware_usb_function;
namespace frequest = fuchsia_hardware_usb_request;

zx_status_t UsbCdcFunction::cdc_generate_mac_address() {
  zx::result result =
      fdf_metadata::GetMetadataIfExists<fuchsia_boot_metadata::MacAddressMetadata>(incoming());
  if (result.is_error()) {
    fdf::error("Failed to get MAC address metadata: {}", result);
    return result.status_value();
  }
  if (result.value().has_value()) {
    const auto &metadata = result.value().value();
    if (!metadata.mac_address().has_value()) {
      fdf::error("MAC address metadata missing mac_address field");
      return ZX_ERR_INTERNAL;
    }
    mac_addr_ = metadata.mac_address().value().octets();
  } else {
    fdf::info("ethernet MAC metadata not found. Generating random address");

    zx_cprng_draw(mac_addr_.data(), mac_addr_.size());
    mac_addr_[0] = 0x02;
  }

  char buffer[sizeof(mac_addr_) * 3];
  snprintf(buffer, sizeof(buffer), "%02X%02X%02X%02X%02X%02X", mac_addr_[0], mac_addr_[1],
           mac_addr_[2], mac_addr_[3], mac_addr_[4], mac_addr_[5]);

  // Make the host and device addresses different so packets are routed
  // correctly.
  mac_addr_[5] ^= 1;

  return function_.AllocStringDesc(buffer, &descriptors_.cdc_eth.iMACAddress);
}

void UsbCdcFunction::DiscardPendingTxBuffers(zx_status_t status) {
  std::array<fnetdev::wire::TxResult, kTxDepth> results;
  auto results_iter = results.begin();
  {
    std::lock_guard<std::mutex> lock(tx_mutex_);
    while (!tx_completion_queue_.empty()) {
      uint32_t id = tx_completion_queue_.front();
      *results_iter++ = {.id = id, .status = status};
      tx_completion_queue_.pop();
    }
  }
  if (results_iter == results.begin() || !netdevice_ifc_.is_valid()) {
    return;
  }
  fdf::Arena arena(kArenaTag);
  fidl::OneWayStatus fidl_status = netdevice_ifc_.buffer(arena)->CompleteTx(
      fidl::VectorView<fnetdev::wire::TxResult>::FromExternal(
          results.data(), std::distance(results.begin(), results_iter)));
  if (!fidl_status.ok()) {
    fdf::error("Failed to complete tx: {}", fidl_status.FormatDescription());
  }
}

void UsbCdcFunction::ReturnPendingRxSpace() {
  fdf::Arena arena(kArenaTag);

  std::array<fnetdev::wire::RxBuffer, kRxDepth> rx_buffers;
  auto rx_buffers_iter = rx_buffers.begin();

  std::array<fnetdev::wire::RxBufferPart, kRxDepth> rx_buffers_parts;
  auto rx_buffers_parts_iter = rx_buffers_parts.begin();

  {
    std::lock_guard<std::mutex> rx(rx_mutex_);
    while (!rx_space_buffers_.empty()) {
      *rx_buffers_parts_iter = {
          .id = rx_space_buffers_.front().id,
          .offset = 0,
          .length = 0,
      };
      rx_space_buffers_.pop();
      *rx_buffers_iter++ = {
          .meta =
              {
                  .port = kPortId,
                  .frame_type = fuchsia_hardware_network::FrameType::kEthernet,
              },
          .data = fidl::VectorView<fnetdev::wire::RxBufferPart>::FromExternal(
              &*rx_buffers_parts_iter, 1),
      };
      rx_buffers_parts_iter++;
    }
  }

  if (rx_buffers_iter == rx_buffers.begin() || !netdevice_ifc_.is_valid()) {
    return;
  }

  fidl::OneWayStatus fidl_status = netdevice_ifc_.buffer(arena)->CompleteRx(
      fidl::VectorView<fnetdev::wire::RxBuffer>::FromExternal(
          rx_buffers.data(), std::distance(rx_buffers.begin(), rx_buffers_iter)));
  if (!fidl_status.ok()) {
    fdf::error("Failed to complete rx: {}", fidl_status.error());
  }
}

void UsbCdcFunction::CdcIntrComplete(std::vector<fendpoint::Completion> completions) {
  for (auto &completion : completions) {
    intr_ep_.PutRequest(usb::FidlRequest{std::move(completion.request().value())});
  }

  if (unbound_) {
    ContinueStop();
  }
}

void UsbCdcFunction::cdc_send_notifications() {
  std::lock_guard<std::mutex> _(state_mutex_);

  usb_cdc_notification_t network_notification = {
      .bmRequestType = USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE,
      .bNotification = USB_CDC_NC_NETWORK_CONNECTION,
      .wValue = online_,
      .wIndex = descriptors_.cdc_intf_0.b_interface_number,
      .wLength = 0,
  };

  usb_cdc_speed_change_notification_t speed_notification = {
      .notification =
          {
              .bmRequestType = USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE,
              .bNotification = USB_CDC_NC_CONNECTION_SPEED_CHANGE,
              .wValue = 0,
              .wIndex = descriptors_.cdc_intf_0.b_interface_number,
              .wLength = 2 * sizeof(uint32_t),
          },
      .downlink_br = 0,
      .uplink_br = 0,
  };

  if (online_) {
    if (speed_ == USB_SPEED_SUPER) {
      // Claim to be gigabit speed.
      speed_notification.downlink_br = speed_notification.uplink_br = 1000 * 1000 * 1000;
    } else {
      // Claim to be 100 megabit speed.
      speed_notification.downlink_br = speed_notification.uplink_br = 100 * 1000 * 1000;
    }
  } else {
    speed_notification.downlink_br = speed_notification.uplink_br = 0;
  }
  std::optional<usb::FidlRequest> req = intr_ep_.GetRequest();
  if (!req.has_value()) {
    fdf::error("[bug] intr_ep_.GetRequest(): no request available");
    return;
  }

  req->clear_buffers();
  std::vector<size_t> actual =
      req->CopyTo(0, &network_notification, sizeof(network_notification), intr_ep_.GetMapped());

  size_t actual_total = 0;
  for (size_t i = 0; i < actual.size(); i++) {
    req.value()->data()->at(i).size(actual[i]);
    actual_total += actual[i];
  }
  ZX_ASSERT(actual_total == sizeof(network_notification));

  req->CacheFlush(intr_ep_.GetMapped());
  std::optional<usb::FidlRequest> req2 = intr_ep_.GetRequest();
  if (!req2.has_value()) {
    fdf::error("[bug] intr_ep_.GetRequest(): no request available");
    return;
  }

  req2->clear_buffers();
  actual = req2->CopyTo(0, &speed_notification, sizeof(speed_notification), intr_ep_.GetMapped());

  actual_total = 0;
  for (size_t i = 0; i < actual.size(); i++) {
    req2.value()->data()->at(i).size(actual[i]);
    actual_total += actual[i];
  }
  ZX_ASSERT(actual_total == sizeof(speed_notification));

  req2->CacheFlush(intr_ep_.GetMapped());

  std::vector<frequest::Request> requests;
  requests.emplace_back(req->take_request());
  requests.emplace_back(req2->take_request());
  auto result = intr_ep_->QueueRequests({std::move(requests)});
  if (result.is_error()) {
    fdf::error("[bug] intr_ep_->QueueRequests(): {}", result.error_value().FormatDescription());
  }
}

void UsbCdcFunction::CdcRxComplete(std::vector<fendpoint::Completion> completions) {
  if (unbound_) {
    for (auto &completion : completions) {
      bulk_out_ep_.PutRequest(usb::FidlRequest{std::move(completion.request().value())});
    }
    ContinueStop();
    return;
  }

  std::lock_guard<std::mutex> lock(rx_mutex_);
  ProcessRxCompletions(std::move(completions));
}

void UsbCdcFunction::ProcessRxCompletions(std::vector<fendpoint::Completion> completions)
    __TA_REQUIRES(rx_mutex_) {
  FDF_ASSERT_MSG(completions.size() <= kRxDepth, "Too many rx completions {}", completions.size());
  fdf::Arena arena(kArenaTag);

  std::array<frequest::wire::Request, kRxDepth> reqs;
  auto reqs_iter = reqs.begin();

  std::array<fnetdev::wire::RxBuffer, kRxDepth> rx_buffers;
  auto rx_buffers_iter = rx_buffers.begin();

  std::array<fnetdev::wire::RxBufferPart, kRxDepth> rx_buffers_parts;
  auto rx_buffers_parts_iter = rx_buffers_parts.begin();

  auto reset_and_enqueue = [&](usb::FidlRequest req) {
    req.reset_buffers(bulk_out_ep_.GetMappedLocked());
    *reqs_iter++ = fidl::ToWire(arena, req.take_request());
  };

  for (auto &completion : completions) {
    zx_status_t status = *completion.status();
    if (status == ZX_ERR_IO_NOT_PRESENT) {
      bulk_out_ep_.PutRequest(usb::FidlRequest{std::move(completion.request().value())});
      continue;
    }

    if (status != ZX_OK) {
      fdf::error("[bug] rx_completion: {}", zx_status_get_string(status));
      reset_and_enqueue(usb::FidlRequest{std::move(completion.request().value())});
      continue;
    }

    if (rx_space_buffers_.empty()) {
      rx_completion_queue_.push_back(std::move(completion));
      continue;
    }

    usb::FidlRequest req(std::move(completion.request().value()));
    const size_t request_length = completion.transfer_size().value();

    fnetdev::wire::RxSpaceBuffer space = rx_space_buffers_.front();

    status = req.CacheFlushInvalidate(bulk_out_ep_.GetMappedLocked());
    if (status != ZX_OK) {
      fdf::error("[bug] CacheFlushInvalidate(): {}", zx_status_get_string(status));
    }

    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      auto *stored_vmo = vmo_store_.GetVmo(space.region.vmo);
      if (!stored_vmo) {
        fdf::error("rx space with unknown vmo {}", space.region.vmo);
        reset_and_enqueue(std::move(req));
        continue;
      }

      if (request_length > space.region.length) {
        fdf::error("rx buffer too small: {} < {}", space.region.length, request_length);
        reset_and_enqueue(std::move(req));
        continue;
      }

      req.CopyFrom(0, reinterpret_cast<void *>(stored_vmo->data().data() + space.region.offset),
                   request_length, bulk_out_ep_.GetMappedLocked());
    }

    *rx_buffers_parts_iter = fnetdev::wire::RxBufferPart{
        .id = space.id,
        .offset = 0,
        .length = static_cast<uint32_t>(request_length),
    };
    *rx_buffers_iter++ = {
        .meta =
            {
                .port = kPortId,
                .frame_type = fuchsia_hardware_network::FrameType::kEthernet,
            },
        .data =
            fidl::VectorView<fnetdev::wire::RxBufferPart>::FromExternal(&*rx_buffers_parts_iter, 1),
    };

    rx_buffers_parts_iter++;
    rx_space_buffers_.pop();

    reset_and_enqueue(std::move(req));
  }

  if (reqs_iter != reqs.begin()) {
    fidl::OneWayStatus queue_status = bulk_out_ep_.client().wire()->QueueRequests(
        fidl::VectorView<frequest::wire::Request>::FromExternal(
            reqs.data(), std::distance(reqs.begin(), reqs_iter)));
    if (!queue_status.ok()) {
      fdf::error("failed to queue rx requests: {}", queue_status.FormatDescription());
      for (auto it = reqs.begin(); it != reqs_iter; it++) {
        bulk_out_ep_.PutRequest(usb::FidlRequest(fidl::ToNatural(*it)));
      }
    }
  }

  if (rx_buffers_iter != rx_buffers.begin()) {
    fidl::OneWayStatus queue_status = netdevice_ifc_.buffer(arena)->CompleteRx(
        fidl::VectorView<fnetdev::wire::RxBuffer>::FromExternal(
            rx_buffers.data(), std::distance(rx_buffers.begin(), rx_buffers_iter)));
    if (!queue_status.ok()) {
      fdf::error("failed to complete rx buffers: {}", queue_status.FormatDescription());
    }
  }
}

void UsbCdcFunction::CdcTxComplete(std::vector<fendpoint::Completion> completions) {
  if (unbound_) {
    for (auto &completion : completions) {
      bulk_in_ep_.PutRequest(usb::FidlRequest{std::move(completion.request().value())});
    }
    ContinueStop();
    return;
  }
  std::array<fnetdev::wire::TxResult, kTxDepth> results;
  auto results_iter = results.begin();
  {
    std::lock_guard<std::mutex> lock(tx_mutex_);
    for (auto &completion : completions) {
      zx_status_t status = *completion.status();
      bulk_in_ep_.PutRequest(usb::FidlRequest(std::move(completion.request().value())));
      if (status != ZX_OK) {
        fdf::debug("tx completion error: {}", zx_status_get_string(status));
      }
      if (tx_completion_queue_.empty()) {
        fdf::error("received tx completion without pending tx");
        continue;
      }
      const uint32_t tx_id = tx_completion_queue_.front();
      *results_iter++ = {.id = tx_id, .status = status};
      tx_completion_queue_.pop();
    }
  }
  if (results_iter == results.begin()) {
    return;
  }
  fdf::Arena arena(kArenaTag);
  fidl::OneWayStatus status = netdevice_ifc_.buffer(arena)->CompleteTx(
      fidl::VectorView<fnetdev::wire::TxResult>::FromExternal(
          results.data(), std::distance(results.begin(), results_iter)));
  if (status.status() != ZX_OK) {
    fdf::error("CompleteTx() failed: {}", zx_status_get_string(status.status()));
  }
}

size_t UsbCdcFunction::UsbFunctionInterfaceGetDescriptorsSize() { return sizeof(descriptors_); }

void UsbCdcFunction::UsbFunctionInterfaceGetDescriptors(uint8_t *out_descriptors_buffer,
                                                        size_t descriptors_size,
                                                        size_t *out_descriptors_actual) {
  const size_t length = std::min(sizeof(descriptors_), descriptors_size);
  memcpy(out_descriptors_buffer, &descriptors_, length);
  *out_descriptors_actual = length;
}

zx_status_t UsbCdcFunction::UsbFunctionInterfaceControl(const usb_setup_t *setup,
                                                        const uint8_t *write_buffer,
                                                        size_t write_size, uint8_t *out_read_buffer,
                                                        size_t read_size, size_t *out_read_actual) {
  uint16_t w_value{le16toh(setup->w_value)};
  uint16_t w_index{le16toh(setup->w_index)};
  uint16_t w_length{le16toh(setup->w_length)};

  fdf::debug(
      "bmRequestType={:02x} bRequest={:02x} wValue={:04x} ({}) "
      "wIndex={:04x} ({}) wLength={:04x} ({})",
      setup->bm_request_type, setup->b_request, w_value, w_value, w_index, w_index, w_length,
      w_length);

  TRACE_DURATION("cdc_eth", __func__, "write_size", write_size, "read_size", read_size);
  if (out_read_actual != NULL) {
    *out_read_actual = 0;
  }

  if (setup->bm_request_type == (USB_DIR_OUT | USB_TYPE_CLASS | USB_RECIP_INTERFACE) &&
      setup->b_request == USB_CDC_SET_ETHERNET_PACKET_FILTER) {
    fdf::debug("setting packet filter not supported");
    return ZX_OK;
  }

  if (setup->bm_request_type == (USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_ENDPOINT) &&
      setup->b_request == USB_REQ_CLEAR_FEATURE && setup->w_value == USB_ENDPOINT_HALT) {
    fdf::debug("clearing endpoint-halt not supported");
    return ZX_OK;
  }

  return ZX_ERR_NOT_SUPPORTED;
}

zx_status_t UsbCdcFunction::UsbFunctionInterfaceSetConfigured(bool configured, usb_speed_t speed) {
  TRACE_DURATION("cdc_eth", __func__, "configured", configured, "speed", speed);
  // Prevent a race with teardown, don't do any work if we're going away.
  if (unbound_) {
    return ZX_OK;
  }

  if (configured_ == configured) {
    return ZX_OK;
  }

  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    online_ = false;
    UpdatePortStatus();
  }

  fdf::info("configured = {}", configured);
  if (configured) {
    if (zx_status_t status = function_.ConfigEp(&descriptors_.intr_ep, NULL); status != ZX_OK) {
      fdf::error("[bug] ConfigEp(): {}", zx_status_get_string(status));
      return status;
    }
    speed_ = speed;
    configured_ = configured;
    cdc_send_notifications();
  } else {
    function_.DisableEp(bulk_out_addr_);
    function_.DisableEp(bulk_in_addr_);
    function_.DisableEp(intr_addr_);

    // Everything is disabled, cancel pending transactions if we have any.
    DiscardPendingTxBuffers(ZX_ERR_CANCELED);

    speed_ = USB_SPEED_UNDEFINED;
    configured_ = configured;
  }

  return ZX_OK;
}

zx_status_t UsbCdcFunction::UsbFunctionInterfaceSetInterface(uint8_t interface,
                                                             uint8_t alt_setting) {
  TRACE_DURATION("cdc_eth", __func__, "interface", interface, "alt_setting", alt_setting);
  // Prevent a race with teardown, don't do any work if we're going away.
  if (unbound_) {
    return ZX_OK;
  }

  if (interface != descriptors_.cdc_intf_0.b_interface_number || alt_setting > 1) {
    return ZX_ERR_INVALID_ARGS;
  }

  // TODO(voydanoff) fullspeed and superspeed support
  if (alt_setting) {
    for (const auto *ep : {&descriptors_.bulk_out_ep, &descriptors_.bulk_in_ep}) {
      if (zx_status_t status = function_.ConfigEp(ep, nullptr); status != ZX_OK) {
        fdf::error("[bug] ConfigEp(): {}", zx_status_get_string(status));
        return status;
      }
    }
  } else {
    for (const uint8_t ep : {bulk_out_addr_, bulk_in_addr_}) {
      if (zx_status_t status = function_.DisableEp(ep); status != ZX_OK) {
        fdf::error("[bug] DisableEp(): {}", zx_status_get_string(status));
        return status;
      }
    }
  }

  bool online;
  if (alt_setting) {
    online = true;

    // queue our OUT reqs
    std::vector<frequest::Request> reqs;
    {
      std::lock_guard<std::mutex> lock(rx_mutex_);
      while (!bulk_out_ep_.RequestsEmpty()) {
        std::optional<usb::FidlRequest> req = bulk_out_ep_.GetRequest();
        req->reset_buffers(bulk_out_ep_.GetMappedLocked());
        ZX_ASSERT(req.has_value());  // A given from the loop.
        reqs.emplace_back(req->take_request());
      }
    }
    fit::result<fidl::OneWayError> queue_status = bulk_out_ep_->QueueRequests(std::move(reqs));
    if (queue_status.is_error()) {
      fdf::error("Failed to queue rx requestys: {}",
                 queue_status.error_value().FormatDescription());
    }
  } else {
    online = false;
  }

  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    online_ = online;
    UpdatePortStatus();
  }

  // send status notifications on interrupt endpoint.
  cdc_send_notifications();

  return ZX_OK;
}

// NetworkDeviceImpl protocol:
zx::result<> UsbCdcFunction::Start() {
  zx::result function = compat::ConnectBanjo<ddk::UsbFunctionProtocolClient>(incoming());
  if (function.is_error()) {
    fdf::error("Failed to connect function: {}", function);
    return function.take_error();
  }
  function_ = std::move(function.value());

  zx_status_t status = function_.AllocInterface(&descriptors_.comm_intf.b_interface_number);
  if (status != ZX_OK) {
    fdf::error("[bug] AllocInterface(comm_intf): {}", zx_status_get_string(status));
    return zx::error(status);
  }
  status = function_.AllocInterface(&descriptors_.cdc_intf_0.b_interface_number);
  if (status != ZX_OK) {
    fdf::error("[bug] AllocInterface(data_intf): {}", zx_status_get_string(status));
    return zx::error(status);
  }
  descriptors_.cdc_intf_1.b_interface_number = descriptors_.cdc_intf_0.b_interface_number;
  descriptors_.cdc_union.bControlInterface = descriptors_.comm_intf.b_interface_number;
  descriptors_.cdc_union.bSubordinateInterface = descriptors_.cdc_intf_0.b_interface_number;

  status = function_.AllocEp(USB_DIR_OUT, &bulk_out_addr_);
  if (status != ZX_OK) {
    fdf::error("[bug] AllocEp(bulk_out): {}", zx_status_get_string(status));
    return zx::error(status);
  }
  status = function_.AllocEp(USB_DIR_IN, &bulk_in_addr_);
  if (status != ZX_OK) {
    fdf::error("[bug] AllocEp(bulk_in): {}", zx_status_get_string(status));
    return zx::error(status);
  }
  status = function_.AllocEp(USB_DIR_IN, &intr_addr_);
  if (status != ZX_OK) {
    fdf::error("[bug] AllocEp(intr): {}", zx_status_get_string(status));
    return zx::error(status);
  }

  descriptors_.bulk_out_ep.b_endpoint_address = bulk_out_addr_;
  descriptors_.bulk_in_ep.b_endpoint_address = bulk_in_addr_;
  descriptors_.intr_ep.b_endpoint_address = intr_addr_;

  status = cdc_generate_mac_address();
  if (status != ZX_OK) {
    return zx::error(status);
  }

  auto ep_dispatcher =
      fdf::SynchronizedDispatcher::Create({}, "cdc-ep-dispatcher", [this](fdf_dispatcher_t *) {
        async::PostTask(dispatcher(), [this]() {
          dispatcher_shutdown_ = true;
          ContinueStop();
        });
      });
  if (ep_dispatcher.is_error()) {
    fdf::error("[bug] fdf::SynchronizedDispatcher::Create(): {}", ep_dispatcher);
    return ep_dispatcher.take_error();
  }
  dispatcher_ = std::move(ep_dispatcher.value());

  auto result = incoming()->Connect<ffunction::UsbFunctionService::Device>();
  if (result.is_error()) {
    fdf::error("could not connect to UsbFunctionService: {}", result);
    return result.take_error();
  }

  // allocate bulk out usb requests
  status = bulk_out_ep_.Init(bulk_out_addr_, result.value(), dispatcher_.async_dispatcher());
  if (status != ZX_OK) {
    fdf::error("[bug] bulk_out_ep_.Init(): {}", zx_status_get_string(status));
    return zx::error(status);
  }

  size_t actual = bulk_out_ep_.AddRequests(kRxDepth, BULK_REQ_SIZE, frequest::Buffer::Tag::kVmoId);
  if (actual != kRxDepth) {
    fdf::error("[bug] bulk_out_ep_.AddRequests(): want {}, got {}", kRxDepth, actual);
    return zx::error(ZX_ERR_INTERNAL);
  }

  // allocate bulk in usb requests
  status = bulk_in_ep_.Init(bulk_in_addr_, result.value(), dispatcher_.async_dispatcher());
  if (status != ZX_OK) {
    fdf::error("[bug] bulk_in_ep_.Init(): {}", zx_status_get_string(status));
    return zx::error(status);
  }

  actual = bulk_in_ep_.AddRequests(kTxDepth, BULK_REQ_SIZE, frequest::Buffer::Tag::kVmoId);
  if (actual != kTxDepth) {
    fdf::error("[bug] bulk_in_ep_.AddRequests(): want {}, got {}", kTxDepth, actual);
    return zx::error(ZX_ERR_INTERNAL);
  }

  // allocate interrupt requests
  status = intr_ep_.Init(intr_addr_, result.value(), dispatcher_.async_dispatcher());
  if (status != ZX_OK) {
    fdf::error("[bug] intr_ep_.Init(): {}", zx_status_get_string(status));
    return zx::error(status);
  }

  actual = intr_ep_.AddRequests(INTR_COUNT, BULK_REQ_SIZE, frequest::Buffer::Tag::kVmoId);
  if (actual != INTR_COUNT) {
    fdf::error("[bug] intr_ep_.AddRequests(): want {}, got {}", INTR_COUNT, actual);
    return zx::error(ZX_ERR_INTERNAL);
  }

  status = function_.SetInterface(this, &usb_function_interface_protocol_ops_);
  if (status != ZX_OK) {
    fdf::error("[bug] function_.SetInterface(): {}", zx_status_get_string(status));
    return zx::error(status);
  }

  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    if (zx_status_t status = vmo_store_.Reserve(fnetdev::wire::kMaxVmos); status != ZX_OK) {
      fdf::error("failed to initialize vmo store: {}", zx_status_get_string(status));
      return zx::error(status);
    }
  }

  // compat server for banning or other things if needed
  if (zx::result result = child_.Initialize(incoming(), outgoing(), node_name(), "usb-cdc-netdev");
      result.is_error()) {
    fdf::error("Failed to initialize compat server: {}", result);
    return result.take_error();
  }

  // NetworkDeviceImpl service handler
  auto protocol = [this](fdf::ServerEnd<fnetdev::NetworkDeviceImpl> server_end) mutable {
    fdf::BindServer(driver_dispatcher()->get(), std::move(server_end), this);
  };
  fnetdev::Service::InstanceHandler handler({.network_device_impl = std::move(protocol)});

  if (auto status = outgoing()->AddService<fnetdev::Service>(std::move(handler));
      status.is_error()) {
    fdf::error("Failed to add service: {}", status);
    return status.take_error();
  }

  std::vector offers = child_.CreateOffers2();
  offers.push_back(fdf::MakeOffer2<fnetdev::Service>());

  zx::result controller = AddChild(
      "usb-cdc-netdev", cpp20::span<const fuchsia_driver_framework::NodeProperty2>{}, offers);
  if (controller.is_error()) {
    fdf::error("Failed to add child: {}", controller);
    return controller.take_error();
  }
  child_controller_ = std::move(controller.value());

  return zx::ok();
}

void UsbCdcFunction::PrepareStop(fdf::PrepareStopCompleter completer) {
  unbound_ = true;
  stop_completer_.emplace(std::move(completer));

  {
    std::lock_guard<std::mutex> lock(rx_mutex_);
    for (auto &c : rx_completion_queue_) {
      bulk_out_ep_.PutRequest(usb::FidlRequest(std::move(c.request().value())));
    }
    rx_completion_queue_.clear();
  }

  function_.DisableEp(bulk_out_addr_);
  function_.DisableEp(bulk_in_addr_);
  function_.DisableEp(intr_addr_);

  DiscardPendingTxBuffers(ZX_ERR_CANCELED);
  ReturnPendingRxSpace();
  function_.SetInterface(nullptr, nullptr);

  bool continue_stop = true;

  if (!intr_ep_.RequestsFull()) {
    intr_ep_->CancelAll().Then(
        [this](fidl::Result<fuchsia_hardware_usb_endpoint::Endpoint::CancelAll> &result) mutable {
          if (!result.is_ok()) {
            fdf::warn("CancelAll failed: {}", result.error_value().FormatDescription());
          }
          ContinueStop();
        });
    continue_stop = false;
  }
  if (!bulk_out_ep_.RequestsFull()) {
    bulk_out_ep_->CancelAll().Then(
        [this](fidl::Result<fuchsia_hardware_usb_endpoint::Endpoint::CancelAll> &result) mutable {
          if (!result.is_ok()) {
            fdf::warn("CancelAll failed: {}", result.error_value().FormatDescription());
          }
          ContinueStop();
        });
    continue_stop = false;
  }

  if (!bulk_in_ep_.RequestsFull()) {
    bulk_in_ep_->CancelAll().Then(
        [this](fidl::Result<fuchsia_hardware_usb_endpoint::Endpoint::CancelAll> &result) mutable {
          if (!result.is_ok()) {
            fdf::warn("CancelAll failed: {}", result.error_value().FormatDescription());
          }
          ContinueStop();
        });
    continue_stop = false;
  }

  if (continue_stop) {
    ContinueStop();
  }
}

void UsbCdcFunction::ContinueStop() {
  if (fdf::Dispatcher::GetCurrent()->async_dispatcher() != dispatcher()) {
    async::PostTask(dispatcher(), [this]() { ContinueStop(); });
    return;
  }
  if (!stop_completer_.has_value()) {
    return;
  }

  if (!intr_ep_.RequestsFull()) {
    fdf::info("Waiting for intr ep requests to be returned");
    return;
  }
  if (!bulk_in_ep_.RequestsFull()) {
    fdf::info("Waiting for bulk in ep requests to be returned");
    return;
  }
  if (!bulk_out_ep_.RequestsFull()) {
    fdf::info("Waiting for bulk out ep requests to be returned");
    return;
  }

  bulk_out_ep_.Close();
  bulk_in_ep_.Close();
  intr_ep_.Close();
  if (!dispatcher_shutdown_) {
    dispatcher_.ShutdownAsync();
    return;
  }
  stop_completer_.value()(zx::ok());
  stop_completer_.reset();
}

void UsbCdcFunction::Init(fnetdev::wire::NetworkDeviceImplInitRequest *request, fdf::Arena &arena,
                          InitCompleter::Sync &completer) {
  std::lock_guard<std::mutex> lock(state_mutex_);
  netdevice_ifc_.Bind(std::move(request->iface), dispatcher_.get());

  auto [client, server] = fdf::Endpoints<fnetdev::NetworkPort>::Create();
  fdf::BindServer(dispatcher_.get(), std::move(server), this);

  // Add port 1
  netdevice_ifc_.buffer(arena)
      ->AddPort(kPortId, std::move(client))
      // Then exactly once so we're sure to complete this transaction even if
      // the dispatcher is shut down.
      .ThenExactlyOnce(
          [completer = completer.ToAsync()](
              fdf::WireUnownedResult<fnetdev::NetworkDeviceIfc::AddPort> &result) mutable {
            fdf::Arena arena(kArenaTag);
            if (!result.ok()) {
              fdf::error("AddPort failed: {}", result.FormatDescription());
              completer.buffer(arena).Reply(result.status());
              return;
            }
            completer.buffer(arena).Reply(result->status);
          });
}

void UsbCdcFunction::Start(fdf::Arena &arena, StartCompleter::Sync &completer) {
  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    UpdatePortStatus();
  }
  completer.buffer(arena).Reply(ZX_OK);
}

void UsbCdcFunction::Stop(fdf::Arena &arena, StopCompleter::Sync &completer) {
  DiscardPendingTxBuffers(ZX_ERR_CANCELED);
  ReturnPendingRxSpace();
  completer.buffer(arena).Reply();
}

void UsbCdcFunction::GetInfo(
    fdf::Arena &arena,
    fdf::WireServer<fnetdev::NetworkDeviceImpl>::GetInfoCompleter::Sync &completer) {
  fnetdev::wire::DeviceImplInfo info = fnetdev::wire::DeviceImplInfo::Builder(arena)
                                           .tx_depth(kTxDepth)
                                           .rx_depth(kRxDepth)
                                           .rx_threshold(kRxDepth / 2)
                                           .max_buffer_parts(1)
                                           .max_buffer_length(BULK_REQ_SIZE)
                                           .buffer_alignment(1)
                                           .min_rx_buffer_length(ETH_MTU)
                                           .min_tx_buffer_length(0)
                                           .Build();

  completer.buffer(arena).Reply(info);
}

void UsbCdcFunction::QueueTx(fnetdev::wire::NetworkDeviceImplQueueTxRequest *request,
                             fdf::Arena &arena, QueueTxCompleter::Sync &completer) {
  std::lock_guard<std::mutex> lock(tx_mutex_);
  std::array<frequest::wire::Request, kTxDepth> reqs;
  auto reqs_iter = reqs.begin();
  std::array<fnetdev::wire::TxResult, kTxDepth> results;
  auto results_iter = results.begin();

  FDF_ASSERT_MSG(request->buffers.size() <= kTxDepth, "Too many tx buffers {}",
                 request->buffers.size());

  bool online;
  // We snapshot online here but we don't need to hold on to the state lock
  // for the entire duration of this function. Racing with the online signal
  // is acceptable but we don't want to keep sending data if we're offline.
  {
    std::lock_guard<std::mutex> state_lock(state_mutex_);
    online = online_;
  }

  for (const auto &buffer : request->buffers) {
    if (unbound_ || !online) {
      *results_iter++ = {.id = buffer.id, .status = ZX_ERR_BAD_STATE};
      continue;
    }
    if (buffer.data.size() != 1) {
      fdf::warn("Invalid buffer data size {} for id {}", buffer.data.size(), buffer.id);
      *results_iter++ = {.id = buffer.id, .status = ZX_ERR_INVALID_ARGS};
      continue;
    }
    const auto &region = buffer.data[0];

    std::optional<usb::FidlRequest> tx_req = bulk_in_ep_.GetRequest();

    if (!tx_req.has_value()) {
      // Given we're matching our request depth to the netdevice depth, this
      // shouldn't happen.
      fdf::warn("No USB request available for id {}", buffer.id);
      *results_iter++ = {.id = buffer.id, .status = ZX_ERR_NO_RESOURCES};
      continue;
    }
    auto return_request = fit::defer([&]() { bulk_in_ep_.PutRequest(std::move(tx_req.value())); });

    {
      std::lock_guard<std::mutex> state_lock(state_mutex_);
      auto *stored_vmo = vmo_store_.GetVmo(region.vmo);
      if (!stored_vmo) {
        fdf::warn("No VMO found for id {}", region.vmo);
        *results_iter++ = {.id = buffer.id, .status = ZX_ERR_INVALID_ARGS};
        continue;
      }
      auto data = stored_vmo->data();
      if (region.length == 0) {
        *results_iter++ = {.id = buffer.id, .status = ZX_OK};
        continue;
      }
      if (region.offset + region.length > data.size()) {
        fdf::warn("Invalid VMO region for id {}", region.vmo);
        *results_iter++ = {.id = buffer.id, .status = ZX_ERR_INVALID_ARGS};
        continue;
      }

      tx_req->clear_buffers();
      std::vector<size_t> actual = tx_req->CopyTo(0, data.data() + region.offset, region.length,
                                                  bulk_in_ep_.GetMappedLocked());
      size_t actual_total = 0;
      for (size_t i = 0; i < actual.size(); i++) {
        (*tx_req)->data()->at(i).size(actual[i]);
        actual_total += actual[i];
      }
      if (actual_total != region.length) {
        fdf::warn("failed to copy all data {} {}", actual_total, region.length);
        *results_iter++ = {.id = buffer.id, .status = ZX_ERR_INTERNAL};
        continue;
      }
    }

    return_request.cancel();
    tx_completion_queue_.push(buffer.id);
    FDF_ASSERT_MSG(tx_completion_queue_.size() <= kTxDepth, "tx completion queue too large",
                   tx_completion_queue_.size());
    tx_req->CacheFlush(bulk_in_ep_.GetMappedLocked());
    *reqs_iter++ = fidl::ToWire(arena, tx_req->take_request());
  }

  if (results_iter != results.begin()) {
    fidl::OneWayStatus status = netdevice_ifc_.buffer(arena)->CompleteTx(
        fidl::VectorView<fnetdev::wire::TxResult>::FromExternal(
            results.data(), std::distance(results.begin(), results_iter)));
    if (!status.ok()) {
      fdf::error("failed to complete tx: {}", status.FormatDescription());
    }
  }

  if (reqs_iter != reqs.begin()) {
    fidl::OneWayStatus queue_status = bulk_in_ep_.client().wire()->QueueRequests(
        fidl::VectorView<frequest::wire::Request>::FromExternal(
            reqs.data(), std::distance(reqs.begin(), reqs_iter)));

    if (!queue_status.ok()) {
      fdf::error("failed to queue tx requests: {}", queue_status.FormatDescription());
      for (auto it = reqs.begin(); it != reqs_iter; it++) {
        bulk_in_ep_.PutRequest(usb::FidlRequest(fidl::ToNatural(*it)));
      }
    }
  }
}

void UsbCdcFunction::QueueRxSpace(fnetdev::wire::NetworkDeviceImplQueueRxSpaceRequest *request,
                                  fdf::Arena &arena, QueueRxSpaceCompleter::Sync &completer) {
  std::lock_guard<std::mutex> lock(rx_mutex_);
  for (const auto &buffer : request->buffers) {
    rx_space_buffers_.push(buffer);
  }
  FDF_ASSERT_MSG(rx_space_buffers_.size() <= kRxDepth, "rx space buffers too large",
                 rx_space_buffers_.size());

  if (rx_completion_queue_.empty()) {
    return;
  }
  // Take over all pending completions and process them. We'll re-queue if
  // not enough space available.
  ProcessRxCompletions(std::move(rx_completion_queue_));
}

void UsbCdcFunction::PrepareVmo(fnetdev::wire::NetworkDeviceImplPrepareVmoRequest *request,
                                fdf::Arena &arena, PrepareVmoCompleter::Sync &completer) {
  std::lock_guard<std::mutex> lock(state_mutex_);
  zx_status_t status = vmo_store_.RegisterWithKey(request->id, std::move(request->vmo));
  if (status != ZX_OK) {
    fdf::error("failed to register vmo {}: {}", request->id, zx_status_get_string(status));
  }
  completer.buffer(arena).Reply(status);
}

void UsbCdcFunction::ReleaseVmo(fnetdev::wire::NetworkDeviceImplReleaseVmoRequest *request,
                                fdf::Arena &arena, ReleaseVmoCompleter::Sync &completer) {
  std::lock_guard<std::mutex> lock(state_mutex_);
  zx::result status = vmo_store_.Unregister(request->id);
  if (!status.is_ok()) {
    fdf::error("failed to unregister vmo {}: {}", request->id, status.status_string());
  }
  completer.buffer(arena).Reply();
}

void UsbCdcFunction::GetInfo(
    fdf::Arena &arena, fdf::WireServer<fnetdev::NetworkPort>::GetInfoCompleter::Sync &completer) {
  static constexpr fuchsia_hardware_network::wire::FrameType kRxTypes[] = {
      fuchsia_hardware_network::wire::FrameType::kEthernet};
  static constexpr fuchsia_hardware_network::wire::FrameTypeSupport kTxTypes[] = {{
      .type = fuchsia_hardware_network::wire::FrameType::kEthernet,
      .features = fuchsia_hardware_network::wire::kFrameFeaturesRaw,
  }};

  fuchsia_hardware_network::wire::PortBaseInfo info =
      fuchsia_hardware_network::wire::PortBaseInfo::Builder(arena)
          .port_class(fuchsia_hardware_network::wire::PortClass::kEthernet)
          .rx_types(fidl::VectorView<fuchsia_hardware_network::wire::FrameType>::FromExternal(
              const_cast<fuchsia_hardware_network::wire::FrameType *>(kRxTypes), 1))
          .tx_types(
              fidl::VectorView<fuchsia_hardware_network::wire::FrameTypeSupport>::FromExternal(
                  const_cast<fuchsia_hardware_network::wire::FrameTypeSupport *>(kTxTypes), 1))
          .Build();

  completer.buffer(arena).Reply(info);
}

void UsbCdcFunction::GetStatus(fdf::Arena &arena, GetStatusCompleter::Sync &completer) {
  std::lock_guard<std::mutex> lock(state_mutex_);
  completer.buffer(arena).Reply(fidl::ToWire(arena, ReadStatus()));
}

void UsbCdcFunction::SetActive(fnetdev::wire::NetworkPortSetActiveRequest *request,
                               fdf::Arena &arena, SetActiveCompleter::Sync &completer) {}

void UsbCdcFunction::GetMac(fdf::Arena &arena, GetMacCompleter::Sync &completer) {
  auto [client, server] = fdf::Endpoints<fnetdev::MacAddr>::Create();
  fdf::BindServer(dispatcher_.get(), std::move(server), this);
  completer.buffer(arena).Reply(std::move(client));
}

void UsbCdcFunction::Removed(fdf::Arena &arena, RemovedCompleter::Sync &completer) {}

void UsbCdcFunction::GetAddress(fdf::Arena &arena, GetAddressCompleter::Sync &completer) {
  fuchsia_net::wire::MacAddress mac;
  memcpy(mac.octets.data(), mac_addr_.data(), mac_addr_.size());
  completer.buffer(arena).Reply(mac);
}

void UsbCdcFunction::GetFeatures(fdf::Arena &arena, GetFeaturesCompleter::Sync &completer) {
  fnetdev::wire::Features features =
      fnetdev::wire::Features::Builder(arena)
          .multicast_filter_count(0)
          .supported_modes(fnetdev::wire::SupportedMacFilterMode::kPromiscuous)
          .Build();
  completer.buffer(arena).Reply(features);
}

void UsbCdcFunction::SetMode(fnetdev::wire::MacAddrSetModeRequest *request, fdf::Arena &arena,
                             SetModeCompleter::Sync &completer) {
  completer.buffer(arena).Reply();
}

fuchsia_hardware_network::PortStatus UsbCdcFunction::ReadStatus() const {
  fuchsia_hardware_network::PortStatus status;

  status.mtu(ETH_MTU);
  fuchsia_hardware_network::StatusFlags flags;
  if (online_) {
    flags |= fuchsia_hardware_network::StatusFlags::kOnline;
  }
  status.flags(flags);
  return status;
}

void UsbCdcFunction::UpdatePortStatus() {
  if (netdevice_ifc_.is_valid()) {
    fdf::Arena arena(kArenaTag);
    fidl::OneWayStatus status =
        netdevice_ifc_.buffer(arena)->PortStatusChanged(kPortId, fidl::ToWire(arena, ReadStatus()));
    if (!status.ok()) {
      fdf::error("Failed to notify port status: {}", status.FormatDescription());
    }
  }
}

bool UsbCdcFunction::HasPendingRxCompletions() {
  std::lock_guard<std::mutex> lock(rx_mutex_);
  return !rx_completion_queue_.empty();
}

}  // namespace usb_cdc_function

FUCHSIA_DRIVER_EXPORT(usb_cdc_function::UsbCdcFunction);
