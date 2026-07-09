// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "adb-function.h"

#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/logging/cpp/structured_logger.h>
#include <lib/zx/vmar.h>
#include <zircon/assert.h>

#include <cstdint>
#include <optional>

#include <usb/peripheral.h>
#include <usb/request-cpp.h>

#include "zircon/status.h"

namespace usb_adb_function {

namespace fdescriptor = fuchsia_hardware_usb_descriptor;
namespace ffunction = fuchsia_hardware_usb_function;

namespace {

// CompleterType follows fidl::internal::WireCompleter<RequestType>::Async
template <typename CompleterType>
void CompleteTxn(CompleterType& completer, zx_status_t status) {
  if (status == ZX_OK) {
    completer.Reply(fit::ok());
  } else {
    completer.Reply(fit::error(status));
  }
}

}  // namespace

void UsbAdbDevice::StartAdb(StartAdbRequestView request, StartAdbCompleter::Sync& completer) {
  if (adb_binding_.has_value()) {
    zxlogf(WARNING, "ADB already connected");
    completer.ReplyError(ZX_ERR_ALREADY_BOUND);
    return;
  }

  switch (state_) {
    case State::kStoppingUsb:
      zxlogf(WARNING, "ADB connected while stopping");
      completer.ReplyError(ZX_ERR_BAD_STATE);
      return;
    case State::kOnline:
      // We're already online, so send the status change immediately.
      if (auto result =
              fidl::WireSendEvent(request->interface)->OnStatusChanged(fadb::StatusFlags::kOnline);
          !result.ok()) {
        zxlogf(ERROR, "Could not call UsbAdbImpl.OnStatusChanged.");
      }
      break;
    case State::kAwaitingUsbConnection:
      break;
  }
  zxlogf(INFO, "ADB client connected");

  adb_binding_.emplace(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                       std::move(request->interface), this, [this](fidl::UnbindInfo info) {
                         zxlogf(INFO, "Device closed with reason '%s'",
                                info.FormatDescription().c_str());
                         ResetOrStopUsb();
                       });
  completer.ReplySuccess();
}

void UsbAdbDevice::StopAdb(StopAdbCompleter::Sync& completer) {
  zxlogf(INFO, "ADB client requested disconnect.");
  stop_completers_.push_back(completer.ToAsync());
  ResetOrStopUsb();
}

void UsbAdbDevice::ResetOrStopUsb() {
  switch (state_) {
    case State::kStoppingUsb:
      zxlogf(INFO, "Stop requested, but already stopping");
      return;
    case State::kOnline:
      zxlogf(INFO, "Stopping USB");
      break;
    case State::kAwaitingUsbConnection:
      zxlogf(INFO, "Stop requested during USB startup");
      break;
  }

  // Purge any requests from internal queues.
  // TODO(b/417808660): Replace logs with Inspect once the bug is fixed.
  zxlogf(INFO, "rx_requests: %ld", rx_requests_.size());
  while (!rx_requests_.empty()) {
    rx_requests_.front().Reply(fit::error(ZX_ERR_BAD_STATE));
    rx_requests_.pop();
  }
  // TODO(b/417808660): Replace logs with Inspect once the bug is fixed.
  zxlogf(INFO, "pending_replies: %ld", pending_replies_.size());
  while (!pending_replies_.empty()) {
    bulk_out_ep_.PutRequest(
        usb::FidlRequest(std::move(pending_replies_.front().request().value())));
    pending_replies_.pop();
  }
  // TODO(b/417808660): Replace logs with Inspect once the bug is fixed.
  zxlogf(INFO, "tx_pending_reqs: %ld", tx_pending_reqs_.size());
  while (!tx_pending_reqs_.empty()) {
    CompleteTxn(tx_pending_reqs_.front().completer, ZX_ERR_CANCELED);
    tx_pending_reqs_.pop();
  }

  // Disconnect USB.
  // TODO(b/417808660): Replace logs with Inspect once the bug is fixed.
  zxlogf(INFO, "Disconnecting from USB by deconfiguring");
  fidl::Result deconfigure = function_->Deconfigure();
  if (deconfigure.is_error()) {
    zxlogf(ERROR, "Failed to deconfigure: %s",
           deconfigure.error_value().FormatDescription().c_str());
  }
  if (usb_function_binding_.has_value()) {
    usb_function_binding_->Unbind();
    usb_function_binding_.reset();
  }

  zxlogf(INFO, "state_ = State::kStoppingUsb");
  state_ = State::kStoppingUsb;
  if (state_property_) {
    state_property_.Set(StateToString(state_));
    RecordEvent("state_changed: kStoppingUsb");
    UpdateQueueStats();
  }

  CheckUsbStopComplete();
}

void UsbAdbDevice::SendQueued() {
  if (state_ != State::kOnline) {
    ZX_PANIC("Unexpected state: %d", state_);
  }
  while (SendQueuedOnce()) {
  }
}

// Returns true if any progress was made. Returns false if we didn't send
// anything, and therefore calling this again won't be useful until something
// changes.
bool UsbAdbDevice::SendQueuedOnce() {
  if (tx_pending_reqs_.empty()) {
    return false;
  }

  auto& current = tx_pending_reqs_.front();
  std::vector<fuchsia_hardware_usb_request::Request> requests;
  while (current.start < current.request.data().size()) {
    auto req = bulk_in_ep_.GetRequest();
    if (!req) {
      break;
    }
    req->clear_buffers();

    size_t to_copy = std::min(current.request.data().size() - current.start, kVmoDataSize);
    auto actual = req->CopyTo(0, current.request.data().data() + current.start, to_copy,
                              bulk_in_ep_.GetMapped());
    size_t actual_total = 0;
    for (size_t i = 0; i < actual.size(); i++) {
      // Fill in size of data.
      (*req)->data()->at(i).size(actual[i]);
      actual_total += actual[i];
    }
    auto status = req->CacheFlush(bulk_in_ep_.GetMapped());
    if (status != ZX_OK) {
      zxlogf(ERROR, "Cache flush failed %s", zx_status_get_string(status));
    }

    requests.emplace_back(req->take_request());
    current.start += actual_total;
  }

  if (requests.empty()) {
    return false;
  }
  auto result = bulk_in_ep_->QueueRequests(std::move(requests));
  if (result.is_error()) {
    zxlogf(ERROR, "Failed to QueueRequests %s", result.error_value().FormatDescription().c_str());
  }

  if (current.start == current.request.data().size()) {
    CompleteTxn(current.completer, ZX_OK);
    tx_pending_reqs_.pop();
    UpdateQueueStats();
  }

  return true;
}

void UsbAdbDevice::ReceiveQueued() {
  if (state_ != State::kOnline) {
    ZX_PANIC("Unexpected state: %d", state_);
  }
  while (ReceiveQueuedOnce()) {
  }
}

bool UsbAdbDevice::ReceiveQueuedOnce() {
  if (pending_replies_.empty() || rx_requests_.empty()) {
    return false;
  }

  auto completion = std::move(pending_replies_.front());
  pending_replies_.pop();

  zx_status_t status = *completion.status();
  auto req = usb::FidlRequest(std::move(completion.request().value()));

  if (status != ZX_OK) {
    zxlogf(ERROR, "RxComplete called with error %s.", zx_status_get_string(status));
    bulk_out_inspect_.AddFailedRxBytes(req.length());
    rx_requests_.front().Reply(fit::error(ZX_ERR_INTERNAL));
  } else {
    // This should always be true because when we registered VMOs, we only registered one per
    // request.
    ZX_ASSERT(req->data()->size() == 1);
    auto addr = bulk_out_ep_.GetMappedAddr(req.request(), 0);
    if (!addr.has_value()) {
      zxlogf(ERROR, "Failed to get mapped");
      rx_requests_.front().Reply(fit::error(ZX_ERR_INTERNAL));
    } else {
      auto status = req.CacheFlushInvalidate(bulk_out_ep_.GetMapped());
      if (status != ZX_OK) {
        zxlogf(ERROR, "Cache flush and invalidate failed %s", zx_status_get_string(status));
      }
      rx_requests_.front().Reply(fit::ok(
          std::vector<uint8_t>(reinterpret_cast<uint8_t*>(*addr),
                               reinterpret_cast<uint8_t*>(*addr) + *completion.transfer_size())));
      bulk_out_inspect_.AddRxBytes(*completion.transfer_size());
    }
  }
  rx_requests_.pop();
  UpdateQueueStats();
  req.reset_buffers(bulk_out_ep_.GetMapped());

  std::vector<fuchsia_hardware_usb_request::Request> requests;
  requests.emplace_back(req.take_request());
  auto result = bulk_out_ep_->QueueRequests(std::move(requests));
  if (result.is_error()) {
    zxlogf(ERROR, "Failed to QueueRequests %s", result.error_value().FormatDescription().c_str());
  }

  return true;
}

void UsbAdbDevice::QueueTx(QueueTxRequest& request, QueueTxCompleter::Sync& completer) {
  size_t length = request.data().size();
  if (length == 0) {
    zxlogf(INFO, "Invalid argument - Length = 0");
    completer.Reply(fit::error(ZX_ERR_INVALID_ARGS));
    return;
  }

  switch (state_) {
    case State::kStoppingUsb:
      // Return early during shutdown.
      completer.Reply(fit::error(ZX_ERR_BAD_STATE));
      return;
    case State::kOnline:
    case State::kAwaitingUsbConnection:
      tx_pending_reqs_.emplace(
          txn_req_t{.request = std::move(request), .start = 0, .completer = completer.ToAsync()});
      UpdateQueueStats();
      SendQueued();
  }
}

void UsbAdbDevice::Receive(ReceiveCompleter::Sync& completer) {
  switch (state_) {
    case State::kStoppingUsb:
      // Return early during shutdown.
      completer.Reply(fit::error(ZX_ERR_BAD_STATE));
      return;
    case State::kAwaitingUsbConnection:
      rx_requests_.emplace(completer.ToAsync());
      UpdateQueueStats();
      break;
    case State::kOnline:
      rx_requests_.emplace(completer.ToAsync());
      UpdateQueueStats();
      ReceiveQueued();
      break;
  }
}

void UsbAdbDevice::RxComplete(std::vector<fendpoint::Completion> completions) {
  for (auto& completion : completions) {
    // This should always be true because when we registered VMOs, we only registered one per
    // request.
    ZX_ASSERT(completion.request()->data()->size() == 1);

    switch (state_) {
      case State::kAwaitingUsbConnection:
        ZX_PANIC("Completion arrived before we sent any requests?");
      case State::kStoppingUsb:
        bulk_out_ep_.PutRequest(usb::FidlRequest(std::move(completion.request().value())));
        CheckUsbStopComplete();
        break;
      case State::kOnline:
        pending_replies_.push(std::move(completion));
        UpdateQueueStats();
        ReceiveQueued();
        break;
    }
  }
}

void UsbAdbDevice::TxComplete(std::vector<fendpoint::Completion> completions) {
  for (auto& completion : completions) {
    switch (state_) {
      case State::kAwaitingUsbConnection:
        ZX_PANIC("Completion arrived before we sent any requests?");
      case State::kStoppingUsb:
        bulk_in_ep_.PutRequest(usb::FidlRequest(std::move(completion.request().value())));
        CheckUsbStopComplete();
        break;
      case State::kOnline: {
        zx_status_t status = *completion.status();
        usb::FidlRequest req(std::move(completion.request().value()));
        size_t size = req.length();
        if (status == ZX_OK) {
          bulk_in_inspect_.AddTxBytes(completion.transfer_size().value_or(0));
        } else {
          bulk_in_inspect_.AddFailedTxBytes(size);
        }
        bulk_in_ep_.PutRequest(std::move(req));

        // Do not queue requests if status is ZX_ERR_IO_NOT_PRESENT, as the underlying connection
        // could be disconnected or USB_RESET is being processed. Calling adb_send_locked in such
        // scenario will deadlock and crash the driver (see https://fxbug.dev/42174506).
        if (status != ZX_ERR_IO_NOT_PRESENT) {
          SendQueued();
        }
        break;
      }
    }
  }
}

void UsbAdbDevice::Control(ControlRequest& request, ControlCompleter::Sync& completer) {
  completer.Reply(zx::ok(std::vector<uint8_t>{}));
}

void UsbAdbDevice::EnableEndpoints() {
  switch (state_) {
    case State::kOnline:
      zxlogf(INFO, "USB endpoints already enabled");
      return;
    case State::kStoppingUsb:
      zxlogf(ERROR, "This is unexpected: UsbFunctionInterface is disconnected while stopping");
      return;
    case State::kAwaitingUsbConnection:
      zxlogf(INFO, "Enabling USB endpoints");
      break;
  }

  fuchsia_hardware_usb_function::EndpointConfiguration ep_config_out;
  {
    fuchsia_hardware_usb_function::EndpointDescriptor desc;
    desc.bm_attributes(descriptors_.bulk_out_ep.bm_attributes);
    desc.w_max_packet_size(le16toh(descriptors_.bulk_out_ep.w_max_packet_size));
    desc.b_interval(descriptors_.bulk_out_ep.b_interval);
    ep_config_out.descriptor(std::move(desc));
  }
  fidl::Result result_out = function_->ConfigureEndpoint(
      {descriptors_.bulk_out_ep.b_endpoint_address, std::move(ep_config_out)});
  if (result_out.is_error()) {
    ZX_PANIC("Failed to Config BULK OUT ep: %s",
             result_out.error_value().FormatDescription().c_str());
  }

  fuchsia_hardware_usb_function::EndpointConfiguration ep_config_in;
  {
    fuchsia_hardware_usb_function::EndpointDescriptor desc;
    desc.bm_attributes(descriptors_.bulk_in_ep.bm_attributes);
    desc.w_max_packet_size(le16toh(descriptors_.bulk_in_ep.w_max_packet_size));
    desc.b_interval(descriptors_.bulk_in_ep.b_interval);
    ep_config_in.descriptor(std::move(desc));
  }
  fidl::Result result_in = function_->ConfigureEndpoint(
      {descriptors_.bulk_in_ep.b_endpoint_address, std::move(ep_config_in)});
  if (result_in.is_error()) {
    ZX_PANIC("Failed to Config BULK IN ep: %s",
             result_in.error_value().FormatDescription().c_str());
  }

  // queue RX requests
  std::vector<fuchsia_hardware_usb_request::Request> requests;
  while (auto req = bulk_out_ep_.GetRequest()) {
    req->reset_buffers(bulk_out_ep_.GetMapped());
    auto status = req->CacheFlushInvalidate(bulk_out_ep_.GetMapped());
    if (status != ZX_OK) {
      ZX_PANIC("Cache flush and invalidate failed %d", status);
    }

    requests.emplace_back(req->take_request());
  }
  auto result = bulk_out_ep_->QueueRequests(std::move(requests));
  if (result.is_error()) {
    ZX_PANIC("Failed to QueueRequests %s", result.error_value().FormatDescription().c_str());
  }

  if (adb_binding_.has_value()) {
    auto result = fidl::WireSendEvent(*adb_binding_)->OnStatusChanged(fadb::StatusFlags::kOnline);
    if (!result.ok()) {
      zxlogf(ERROR, "Could not call UsbAdbImpl.OnStatusChanged.");
    }
  }

  zxlogf(INFO, "state_ = State::kOnline");
  state_ = State::kOnline;
  if (state_property_) {
    state_property_.Set(StateToString(state_));
    RecordEvent("state_changed: kOnline");
  }
}

void UsbAdbDevice::SetConfigured(SetConfiguredRequest& request,
                                 SetConfiguredCompleter::Sync& completer) {
  zxlogf(INFO, "configured? - %d", request.configured());
  if (request.configured()) {
    EnableEndpoints();
  } else {
    switch (state_) {
      case State::kAwaitingUsbConnection:
        // It's normal to receive SetConfigured(false) while the connection is
        // starting up - ignore it.
        break;
      case State::kOnline:
        ResetOrStopUsb();
        break;
      case State::kStoppingUsb:
        zxlogf(
            WARNING,
            "Received SetConfigured(false) while stopping. This is unexpected, but probably fine.");
        break;
    }
  }
  completer.Reply(zx::ok());
}

void UsbAdbDevice::SetInterface(SetInterfaceRequest& request,
                                SetInterfaceCompleter::Sync& completer) {
  zxlogf(INFO, "SetInterface called");
  completer.Reply(zx::ok());
}

void UsbAdbDevice::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_hardware_usb_function::UsbFunctionInterface> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  zxlogf(WARNING, "Unknown method %ld", metadata.method_ordinal);
}

void UsbAdbDevice::CheckUsbStopComplete() {
  if (state_ != State::kStoppingUsb) {
    ZX_PANIC("Unexpected state: %d", state_);
  }

  if (!bulk_in_ep_.RequestsFull() || !bulk_out_ep_.RequestsFull()) {
    // Still waiting for outstanding USB requests to return.
    // TODO(b/417808660): Replace logs with Inspect once the bug is fixed.
    zxlogf(INFO, "Not all USB requests complete (in:%d out:%d)", bulk_in_ep_.RequestsFull(),
           bulk_out_ep_.RequestsFull());
    return;
  }

  zxlogf(INFO, "All USB requests complete. Completing USB stop.");

  if (adb_binding_.has_value()) {
    auto result = fidl::WireSendEvent(*adb_binding_)->OnStatusChanged(fadb::StatusFlags(0));
    if (!result.ok()) {
      zxlogf(ERROR, "Could not call UsbAdbImpl.OnStatusChanged.");
    }
  }

  adb_binding_.reset();

  // TODO(b/417808660): Replace logs with Inspect once the bug is fixed.
  zxlogf(INFO, "Calling stop_completers_");
  while (!stop_completers_.empty()) {
    stop_completers_.back().Reply(zx::ok());
    stop_completers_.pop_back();
  }

  // Is this a proper shutdown, or a restart of USB?
  if (shutdown_callback_.has_value()) {
    bulk_out_ep_.Close();
    bulk_in_ep_.Close();
    zxlogf(INFO, "Shutting down driver.");
    shutdown_callback_.value()(zx::ok());
    shutdown_callback_.reset();
  } else {
    zxlogf(INFO, "Restarting USB connection.");
    StartUsb();
  }
}

void UsbAdbDevice::Stop(fdf::StopCompleter completer) {
  if (throughput_tracker_) {
    throughput_tracker_->Stop();
  }
  shutdown_callback_.emplace(std::move(completer));
  ResetOrStopUsb();
}

zx_status_t UsbAdbDevice::InitEndpoint(
    fidl::ClientEnd<fuchsia_hardware_usb_endpoint::Endpoint> endpoint_client,
    usb::EndpointClient<UsbAdbDevice>& ep, uint32_t req_count) {
  zx_status_t status = ep.Init(std::move(endpoint_client), dispatcher());
  if (status != ZX_OK) {
    zxlogf(ERROR, "Failed to init UsbEndpoint %s", zx_status_get_string(status));
    return status;
  }

  // TODO(127854): When we support active pinning of VMOs, adb may want to use VMOs that are not
  // perpetually pinned.
  auto actual =
      ep.AddRequests(req_count, kVmoDataSize, fuchsia_hardware_usb_request::Buffer::Tag::kVmoId);
  if (actual != req_count) {
    zxlogf(ERROR, "Wanted %u requests, only got %zu requests", req_count, actual);
  }
  return actual == 0 ? ZX_ERR_INTERNAL : ZX_OK;
}

zx::result<> UsbAdbDevice::Start(fdf::DriverContext context) {
  auto client =
      context.incoming().Connect<fuchsia_hardware_usb_function::UsbFunctionService::Device>();

  if (client.is_error()) {
    zxlogf(ERROR, "Failed to connect fidl protocol");
    return client.take_error();
  }
  function_.Bind(std::move(*client));

  zx::result bulk_out_endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  if (bulk_out_endpoints.is_error()) {
    return bulk_out_endpoints.take_error();
  }
  zx::result bulk_in_endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  if (bulk_in_endpoints.is_error()) {
    return bulk_in_endpoints.take_error();
  }

  std::vector<ffunction::EndpointResource> resources;
  {
    ffunction::EndpointResource res;
    res.direction(fdescriptor::EndpointDirection::kOut);
    res.endpoint(std::move(bulk_out_endpoints->server));
    resources.emplace_back(std::move(res));
  }
  {
    ffunction::EndpointResource res;
    res.direction(fdescriptor::EndpointDirection::kIn);
    res.endpoint(std::move(bulk_in_endpoints->server));
    resources.emplace_back(std::move(res));
  }

  fidl::Request<ffunction::UsbFunction::AllocResources> alloc_req;
  alloc_req.interface_count(1);
  alloc_req.endpoints(std::move(resources));

  fidl::Result alloc_result = function_->AllocResources(std::move(alloc_req));
  if (alloc_result.is_error()) {
    zxlogf(ERROR, "AllocResources failed: %s",
           alloc_result.error_value().FormatDescription().c_str());
    return zx::error(alloc_result.error_value().is_framework_error()
                         ? alloc_result.error_value().framework_error().status()
                         : alloc_result.error_value().domain_error());
  }

  auto& response = alloc_result.value();
  descriptors_.adb_intf.b_interface_number = response.interface_nums()[0];
  descriptors_.bulk_out_ep.b_endpoint_address = response.endpoint_addrs()[0];
  descriptors_.bulk_in_ep.b_endpoint_address = response.endpoint_addrs()[1];

  zx_status_t status =
      InitEndpoint(std::move(bulk_out_endpoints->client), bulk_out_ep_, kBulkRxCount);
  if (status != ZX_OK) {
    zxlogf(ERROR, "InitEndpoint failed - %s.", zx_status_get_string(status));
    return zx::error(status);
  }
  status = InitEndpoint(std::move(bulk_in_endpoints->client), bulk_in_ep_, kBulkTxCount);
  if (status != ZX_OK) {
    zxlogf(ERROR, "InitEndpoint failed - %s.", zx_status_get_string(status));
    return zx::error(status);
  }
  auto serve_result = outgoing()->AddService<fadb::Service>(fadb::Service::InstanceHandler({
      .adb = device_bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                            fidl::kIgnoreBindingClosure),
  }));
  if (serve_result.is_error()) {
    zxlogf(ERROR, "Failed to add Device service %s", serve_result.status_string());
    return serve_result.take_error();
  }

  component_inspector_ = context.CreateInspector(this);
  if (component_inspector_.has_value()) {
    inspect_node_ = component_inspector_->root().CreateChild("usb-adb-function");
    state_property_ = inspect_node_.CreateString("state", StateToString(state_));
    bulk_in_inspect_.Init(inspect_node_, "bulk_in");
    bulk_out_inspect_.Init(inspect_node_, "bulk_out");

    RecordEvent("driver_started");
    throughput_tracker_.emplace(dispatcher(), [this](zx::duration delta) {
      bulk_in_inspect_.MeasureThroughput(delta);
      bulk_out_inspect_.MeasureThroughput(delta);
      UpdateQueueStats();
    });
    throughput_tracker_->Start();
  } else {
    zxlogf(WARNING, "Failed to initialize inspector");
  }

  StartUsb();
  return zx::ok();
}

void UsbAdbDevice::StartUsb() {
  zx::result iface_endpoints =
      fidl::CreateEndpoints<fuchsia_hardware_usb_function::UsbFunctionInterface>();
  if (iface_endpoints.is_error()) {
    ZX_PANIC("CreateEndpoints failed %s", zx_status_get_string(iface_endpoints.error_value()));
  }
  usb_function_binding_ = fidl::BindServer(dispatcher(), std::move(iface_endpoints->server), this);

  std::vector<uint8_t> descriptors_buffer(sizeof(descriptors_));
  memcpy(descriptors_buffer.data(), &descriptors_, sizeof(descriptors_));

  fidl::Request<fuchsia_hardware_usb_function::UsbFunction::Configure> config_req;
  config_req.configuration(std::move(descriptors_buffer));
  config_req.iface(std::move(iface_endpoints->client));

  fidl::Result config_res = function_->Configure(std::move(config_req));
  if (config_res.is_error()) {
    ZX_PANIC("Configure failed: %s", config_res.error_value().FormatDescription().c_str());
  }

  zxlogf(INFO, "state_ = State::kAwaitingUsbConnection");
  state_ = State::kAwaitingUsbConnection;
  if (state_property_) {
    state_property_.Set(StateToString(state_));
    RecordEvent("state_changed: kAwaitingUsbConnection");
  }
}

}  // namespace usb_adb_function

FUCHSIA_DRIVER_EXPORT2(usb_adb_function::UsbAdbDevice);
