// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "adb-function.h"

#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/logging/cpp/structured_logger.h>
#include <lib/zx/vmar.h>
#include <zircon/assert.h>

#include <algorithm>
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

void UsbAdbDevice::PurgeOfflineQueues() {
  const size_t rx_count = rx_requests_.size();
  const size_t reply_count = pending_replies_.size();
  const size_t tx_count = tx_pending_reqs_.size();
  if (rx_count > 0 || reply_count > 0 || tx_count > 0) {
    zxlogf(INFO, "Purging queues (rx_requests: %zu, pending_replies: %zu, tx_pending_reqs: %zu)",
           rx_count, reply_count, tx_count);
  } else {
    zxlogf(DEBUG, "Purging empty queues (rx_requests: 0, pending_replies: 0, tx_pending_reqs: 0)");
  }
  while (!rx_requests_.empty()) {
    rx_requests_.front().Reply(fit::error(ZX_ERR_BAD_STATE));
    rx_requests_.pop();
  }
  while (!pending_replies_.empty()) {
    if (pending_replies_.front().request().has_value()) {
      bulk_out_ep_.PutRequest(
          usb::FidlRequest(std::move(pending_replies_.front().request().value())));
    }
    pending_replies_.pop();
  }
  while (!tx_pending_reqs_.empty()) {
    CompleteTxn(tx_pending_reqs_.front().completer, ZX_ERR_CANCELED);
    tx_pending_reqs_.pop_front();
  }
  UpdateQueueStats();
}

void UsbAdbDevice::StartAdb(StartAdbRequestView request, StartAdbCompleter::Sync& completer) {
  if (adb_binding_.has_value()) {
    zxlogf(WARNING, "ADB already connected");
    completer.ReplyError(ZX_ERR_ALREADY_BOUND);
    return;
  }

  switch (state_) {
    case State::kStoppingForUnbind:
      zxlogf(WARNING, "ADB connected while stopping for unbind");
      completer.ReplyError(ZX_ERR_BAD_STATE);
      return;
    case State::kStoppingForReconnect:
      adb_connected_during_stop_ = true;
      RecordEvent("adb_client_connected_during_stop");
      break;
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
  RecordEvent("adb_client_connected");

  // Note: adb_binding_ is a fidl::ServerBinding (owning handle). Destroying or replacing
  // adb_binding_ synchronously cancels any pending unbind tasks in the FIDL runtime, so connection
  // ID tracking is not required here.
  adb_binding_.emplace(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                       std::move(request->interface), this, [this](fidl::UnbindInfo info) {
                         zxlogf(INFO, "Device closed with reason '%s'",
                                info.FormatDescription().c_str());
                         RecordEvent("adb_client_disconnected: unbind");
                         adb_binding_.reset();
                         adb_connected_during_stop_ = false;
                         if (state_ == State::kAwaitingUsbConnection) {
                           PurgeOfflineQueues();
                           return;
                         }
                         if (state_ == State::kOnline && !usb_disconnect_observed_) {
                           reset_usb_on_stop_ = true;
                           RecordEvent("reset_usb_on_stop");
                         }
                         ResetOrStopUsb(State::kStoppingForReconnect);
                       });
  completer.ReplySuccess();
}

void UsbAdbDevice::StopAdb(StopAdbCompleter::Sync& completer) {
  zxlogf(INFO, "ADB client requested disconnect.");
  RecordEvent("adb_client_disconnected: StopAdb");
  if (adb_binding_.has_value()) {
    auto result = fidl::WireSendEvent(*adb_binding_)->OnStatusChanged(fadb::StatusFlags(0));
    if (!result.ok()) {
      zxlogf(ERROR, "Could not call UsbAdbImpl.OnStatusChanged.");
    }
    adb_binding_.reset();
  }
  adb_connected_during_stop_ = false;

  if (state_ == State::kAwaitingUsbConnection) {
    PurgeOfflineQueues();
    completer.ReplySuccess();
    return;
  }

  stop_completers_.push_back(completer.ToAsync());
  if (state_ == State::kOnline && !usb_disconnect_observed_) {
    reset_usb_on_stop_ = true;
    RecordEvent("reset_usb_on_stop");
  }
  ResetOrStopUsb(State::kStoppingForReconnect);
}

void UsbAdbDevice::SetState(State new_state) {
  zxlogf(INFO, "state_ = State::%s", StateToString(new_state).c_str());
  state_ = new_state;
  if (component_inspector_.has_value()) {
    function_inspect_.SetState(StateToString(state_));
    function_inspect_.SetOnline(state_ == State::kOnline);
    RecordEvent(std::string("state_changed: ") + StateToString(state_));
    UpdateQueueStats();
  }
}

void UsbAdbDevice::ResetOrStopUsb(State stop_state) {
  switch (state_) {
    case State::kStoppingForUnbind:
      zxlogf(INFO, "Stop requested, but already stopping for unbind");
      return;
    case State::kStoppingForReconnect:
      if (stop_state == State::kStoppingForUnbind) {
        zxlogf(INFO, "Stop for unbind requested while already stopping for reconnect");
        adb_connected_during_stop_ = false;
        PurgeOfflineQueues();
        // Update state to kStoppingForUnbind so CheckUsbStopComplete will perform unbind
        // teardown once in-flight requests complete, then return immediately to avoid
        // re-issuing duplicate CancelAll or Deconfigure FIDL calls.
        SetState(stop_state);
        CheckUsbStopComplete();
        return;
      }
      zxlogf(INFO, "Stop requested, but already stopping for reconnect");
      PurgeOfflineQueues();
      CheckUsbStopComplete();
      return;
    case State::kOnline:
      zxlogf(INFO, "Stopping USB");
      break;
    case State::kAwaitingUsbConnection:
      zxlogf(INFO, "Stop requested during USB startup");
      break;
  }

  // Purge any requests from internal queues.
  PurgeOfflineQueues();

  bulk_in_cancelled_ = false;
  bulk_out_cancelled_ = false;

  const bool was_online = (state_ == State::kOnline);
  SetState(stop_state);

  // Cancel in-flight bulk endpoint requests.
  zxlogf(INFO, "Canceling all bulk endpoint requests");
  if (bulk_in_ep_.client().is_valid()) {
    bulk_in_ep_->CancelAll().Then([this](auto& result) {
      bulk_in_cancelled_ = true;
      bulk_in_inspect_.OnCancelAllComplete();
      if (result.is_error()) {
        if (result.error_value().is_domain_error() &&
            result.error_value().domain_error() == ZX_ERR_IO_NOT_PRESENT) {
          zxlogf(INFO,
                 "CancelAll on bulk_in_ep_: hardware endpoint is inactive, request cleanup "
                 "complete");
        } else {
          zxlogf(WARNING, "CancelAll on bulk_in_ep_ failed: %s",
                 result.error_value().FormatDescription().c_str());
        }
      }
      CheckUsbStopComplete();
    });
  } else {
    bulk_in_cancelled_ = true;
    bulk_in_inspect_.SetCancelled(true);
  }

  if (bulk_out_ep_.client().is_valid()) {
    bulk_out_ep_->CancelAll().Then([this](auto& result) {
      bulk_out_cancelled_ = true;
      bulk_out_inspect_.OnCancelAllComplete();
      if (result.is_error()) {
        if (result.error_value().is_domain_error() &&
            result.error_value().domain_error() == ZX_ERR_IO_NOT_PRESENT) {
          zxlogf(INFO,
                 "CancelAll on bulk_out_ep_: hardware endpoint is inactive, request cleanup "
                 "complete");
        } else {
          zxlogf(WARNING, "CancelAll on bulk_out_ep_ failed: %s",
                 result.error_value().FormatDescription().c_str());
        }
      }
      CheckUsbStopComplete();
    });
  } else {
    bulk_out_cancelled_ = true;
    bulk_out_inspect_.SetCancelled(true);
  }

  // Disconnect USB by disabling endpoints if previously online.
  // TODO(b/417808660): Replace logs with Inspect once the bug is fixed.
  if (was_online) {
    zxlogf(INFO, "Disconnecting from USB by disabling endpoints");
    for (const uint8_t ep_addr : {bulk_out_addr(), bulk_in_addr()}) {
      fidl::Result result = function_->DisableEndpoint({ep_addr});
      if (result.is_error()) {
        if (result.error_value().is_domain_error() &&
            (result.error_value().domain_error() == ZX_ERR_IO_NOT_PRESENT ||
             result.error_value().domain_error() == ZX_ERR_BAD_STATE)) {
          zxlogf(INFO, "DisableEndpoint on 0x%x: hardware endpoint is inactive during teardown",
                 ep_addr);
        } else {
          zxlogf(WARNING, "Failed to disable endpoint 0x%x: %s", ep_addr,
                 result.error_value().FormatDescription().c_str());
        }
      }
    }
    RecordEvent("DisableEndpoints complete");
  }

  CheckUsbStopComplete();
}

void UsbAdbDevice::SendQueued() {
  // Early return if not online. In kAwaitingUsbConnection (offline), QueueTx calls
  // SendQueued to buffer outgoing transactions in `tx_pending_reqs_`, which will be
  // flushed when transitioning to kOnline in EnableEndpoints.
  if (state_ != State::kOnline) {
    return;
  }

  for (auto it = tx_pending_reqs_.begin(); it != tx_pending_reqs_.end();) {
    if (it->start == it->request.data().size()) {
      ++it;
      continue;
    }

    std::vector<fuchsia_hardware_usb_request::Request> requests;
    std::vector<uint64_t> queued_vmo_ids;
    bool copy_failed = false;
    while (it->start < it->request.data().size()) {
      auto req = bulk_in_ep_.GetRequest();
      if (!req) {
        break;
      }
      // Reset request buffers before copying new payload data.
      req->clear_buffers();

      size_t to_copy = std::min(it->request.data().size() - it->start, kVmoDataSize);
      zx::result<std::vector<size_t>> actual = req->CachedCopyTo(
          0, it->request.data().data() + it->start, to_copy, bulk_in_ep_.GetMapped());
      if (actual.is_error() || actual->size() != (*req)->data()->size()) {
        zxlogf(ERROR, "CachedCopyTo failed or size mismatch");
        bulk_in_ep_.PutRequest(std::move(req.value()));
        copy_failed = true;
        break;
      }
      size_t actual_total = 0;
      for (size_t i = 0; i < actual->size(); i++) {
        // Fill in size of data.
        (*req)->data()->at(i).size((*actual)[i]);
        actual_total += (*actual)[i];
      }

      queued_vmo_ids.push_back((*req)->data()->at(0).buffer()->vmo_id().value());
      requests.emplace_back(req->take_request());
      it->start += actual_total;
    }

    if (requests.empty()) {
      if (copy_failed) {
        it->status = ZX_ERR_INTERNAL;
        it->start = it->request.data().size();
        if (it->in_flight_vmo_ids.empty()) {
          CompleteTxn(it->completer, it->status);
          it = tx_pending_reqs_.erase(it);
          UpdateQueueStats();
          continue;
        }
        ++it;
        continue;
      }
      // Out of hardware request descriptors; return until in-flight completions return descriptors.
      return;
    }

    // QueueRequests transfers ownership of `requests` to the endpoint FIDL client transport.
    // If QueueRequests fails, the endpoint channel is closed/unbound; mark transaction failed and
    // return.
    auto result = bulk_in_ep_->QueueRequests(std::move(requests));
    if (result.is_error()) {
      zxlogf(ERROR, "Failed to QueueRequests %s", result.error_value().FormatDescription().c_str());
      it->status = ZX_ERR_IO;
      it->start = it->request.data().size();
      if (it->in_flight_vmo_ids.empty()) {
        CompleteTxn(it->completer, it->status);
        tx_pending_reqs_.erase(it);
        UpdateQueueStats();
      }
      return;
    }

    it->in_flight_vmo_ids.insert(it->in_flight_vmo_ids.end(), queued_vmo_ids.begin(),
                                 queued_vmo_ids.end());
    if (copy_failed) {
      it->status = ZX_ERR_INTERNAL;
      it->start = it->request.data().size();
      ++it;
      continue;
    }

    if (it->start < it->request.data().size()) {
      // Transaction was partially queued due to descriptor exhaustion.
      // Return and wait for in-flight requests to complete before queueing remaining bytes.
      return;
    }
    ++it;
  }
}

void UsbAdbDevice::ReceiveQueued() {
  // Defensive safeguard to ensure we only process RX completions and pending replies
  // when the driver state is kOnline.
  if (state_ != State::kOnline) {
    return;
  }

  while (!pending_replies_.empty() && !rx_requests_.empty()) {
    auto completion = std::move(pending_replies_.front());
    pending_replies_.pop();

    zx_status_t status = completion.status().value_or(ZX_ERR_INTERNAL);
    auto req = usb::FidlRequest(std::move(completion.request().value()));

    if (status != ZX_OK) {
      zx_status_t reply_status = (status == ZX_ERR_IO_NOT_PRESENT || status == ZX_ERR_CANCELED)
                                     ? ZX_ERR_CANCELED
                                     : ZX_ERR_INTERNAL;
      if (status == ZX_ERR_IO_NOT_PRESENT || status == ZX_ERR_CANCELED) {
        zxlogf(INFO, "RxComplete called with teardown status: %s.", zx_status_get_string(status));
      } else {
        zxlogf(ERROR, "RxComplete called with error %s.", zx_status_get_string(status));
      }
      bulk_out_inspect_.AddFailedRxBytes(req.length());
      rx_requests_.front().Reply(fit::error(reply_status));
    } else {
      // This should always be true because when we registered VMOs, we only registered one per
      // request.
      ZX_ASSERT(req->data()->size() == 1);
      auto addr = bulk_out_ep_.GetMappedAddr(req.request(), 0);
      if (!addr.has_value()) {
        zxlogf(ERROR, "Failed to get mapped");
        rx_requests_.front().Reply(fit::error(ZX_ERR_INTERNAL));
      } else {
        auto flush_status = req.CacheFlushInvalidate(bulk_out_ep_.GetMapped());
        if (flush_status != ZX_OK) {
          zxlogf(ERROR, "Cache flush and invalidate failed %s", zx_status_get_string(flush_status));
        }
        size_t rx_bytes = completion.transfer_size().value_or(0);
        if (rx_bytes > kVmoDataSize) {
          zxlogf(ERROR, "USB completion transfer_size %zu exceeds VMO size %zu", rx_bytes,
                 kVmoDataSize);
        }
        rx_bytes = std::min(rx_bytes, kVmoDataSize);
        rx_requests_.front().Reply(fit::ok(std::vector<uint8_t>(
            reinterpret_cast<uint8_t*>(*addr), reinterpret_cast<uint8_t*>(*addr) + rx_bytes)));
        bulk_out_inspect_.AddRxBytes(rx_bytes);
      }
    }
    rx_requests_.pop();
    UpdateQueueStats();
    req.reset_buffers(bulk_out_ep_.GetMapped());

    // Do not re-queue requests onto the hardware endpoint if the completion failed due to
    // disconnect or cancellation; return the request to the pool instead.
    if (status == ZX_ERR_IO_NOT_PRESENT || status == ZX_ERR_CANCELED) {
      bulk_out_ep_.PutRequest(std::move(req));
      continue;
    }

    // QueueRequests transfers ownership of `requests` via std::move to the endpoint FIDL client,
    // leaving `requests` empty. If QueueRequests returns an error (e.g. channel closed), return
    // early as endpoint teardown is initiated.
    std::vector<fuchsia_hardware_usb_request::Request> requests;
    requests.emplace_back(req.take_request());
    auto result = bulk_out_ep_->QueueRequests(std::move(requests));
    if (result.is_error()) {
      zxlogf(ERROR, "Failed to QueueRequests %s", result.error_value().FormatDescription().c_str());
      return;
    }
  }
}

void UsbAdbDevice::QueueTx(QueueTxRequest& request, QueueTxCompleter::Sync& completer) {
  size_t length = request.data().size();
  if (length == 0) {
    zxlogf(INFO, "Invalid argument - Length = 0");
    completer.Reply(fit::error(ZX_ERR_INVALID_ARGS));
    return;
  }

  switch (state_) {
    case State::kStoppingForUnbind:
      // Return early during shutdown.
      completer.Reply(fit::error(ZX_ERR_BAD_STATE));
      return;
    case State::kStoppingForReconnect:
      if (!adb_connected_during_stop_) {
        completer.Reply(fit::error(ZX_ERR_BAD_STATE));
        return;
      }
      [[fallthrough]];
    case State::kOnline:
    case State::kAwaitingUsbConnection:
      tx_pending_reqs_.push_back(
          txn_req_t{.request = std::move(request), .start = 0, .completer = completer.ToAsync()});
      UpdateQueueStats();
      SendQueued();
  }
}

void UsbAdbDevice::Receive(ReceiveCompleter::Sync& completer) {
  switch (state_) {
    case State::kStoppingForUnbind:
      // Return early during shutdown.
      completer.Reply(fit::error(ZX_ERR_BAD_STATE));
      return;
    case State::kStoppingForReconnect:
      if (!adb_connected_during_stop_) {
        completer.Reply(fit::error(ZX_ERR_BAD_STATE));
        return;
      }
      [[fallthrough]];
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
      case State::kStoppingForUnbind:
      case State::kStoppingForReconnect:
        bulk_out_ep_.PutRequest(usb::FidlRequest(std::move(completion.request().value())));
        CheckUsbStopComplete();
        break;
      case State::kOnline:
        if (completion.status().value_or(ZX_ERR_INTERNAL) == ZX_ERR_IO_NOT_PRESENT) {
          if (!usb_disconnect_observed_) {
            usb_disconnect_observed_ = true;
            RecordEvent("usb_disconnect_observed: RxComplete");
          }
        }
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
      case State::kStoppingForUnbind:
      case State::kStoppingForReconnect:
        bulk_in_ep_.PutRequest(usb::FidlRequest(std::move(completion.request().value())));
        CheckUsbStopComplete();
        break;
      case State::kOnline: {
        zx_status_t status = completion.status().value_or(ZX_ERR_INTERNAL);
        if (status == ZX_ERR_IO_NOT_PRESENT) {
          if (!usb_disconnect_observed_) {
            usb_disconnect_observed_ = true;
            RecordEvent("usb_disconnect_observed: TxComplete");
          }
        }
        usb::FidlRequest req(std::move(completion.request().value()));
        std::optional<uint64_t> completed_vmo_id;
        if (req->data().has_value() && !req->data()->empty() &&
            req->data()->at(0).buffer().has_value() &&
            req->data()->at(0).buffer()->Which() ==
                fuchsia_hardware_usb_request::Buffer::Tag::kVmoId) {
          completed_vmo_id = req->data()->at(0).buffer()->vmo_id().value();
        }
        size_t size = req.length();
        if (status == ZX_OK) {
          bulk_in_inspect_.AddTxBytes(completion.transfer_size().value_or(0));
        } else {
          bulk_in_inspect_.AddFailedTxBytes(size);
        }
        bulk_in_ep_.PutRequest(std::move(req));

        if (completed_vmo_id.has_value()) {
          for (auto it = tx_pending_reqs_.begin(); it != tx_pending_reqs_.end(); ++it) {
            auto vmo_it = std::find(it->in_flight_vmo_ids.begin(), it->in_flight_vmo_ids.end(),
                                    *completed_vmo_id);
            if (vmo_it != it->in_flight_vmo_ids.end()) {
              it->in_flight_vmo_ids.erase(vmo_it);
              if (status != ZX_OK && it->status == ZX_OK) {
                it->status = status;
              }
              if (it->start == it->request.data().size() && it->in_flight_vmo_ids.empty()) {
                CompleteTxn(it->completer, it->status);
                tx_pending_reqs_.erase(it);
              }
              break;
            }
          }
        }
        UpdateQueueStats();

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
  completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
}

zx::result<> UsbAdbDevice::EnableEndpoints() {
  switch (state_) {
    case State::kOnline:
      zxlogf(INFO, "USB endpoints already enabled");
      function_inspect_.OnConfigured();
      return zx::ok();
    case State::kStoppingForUnbind:
      zxlogf(WARNING, "Received request to enable endpoints while unbinding driver");
      return zx::error(ZX_ERR_BAD_STATE);
    case State::kStoppingForReconnect:
      zxlogf(INFO,
             "Enabling endpoints requested while stopping for reconnect; queuing reconfiguration");
      pending_reconfigure_ = true;
      return zx::ok();
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
    zxlogf(WARNING, "Failed to Config BULK OUT ep: %s",
           result_out.error_value().FormatDescription().c_str());
    return zx::error(result_out.error_value().is_domain_error()
                         ? result_out.error_value().domain_error()
                         : result_out.error_value().framework_error().status());
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
    zxlogf(WARNING, "Failed to Config BULK IN ep: %s",
           result_in.error_value().FormatDescription().c_str());
    (void)function_->DisableEndpoint({descriptors_.bulk_out_ep.b_endpoint_address});
    return zx::error(result_in.error_value().is_domain_error()
                         ? result_in.error_value().domain_error()
                         : result_in.error_value().framework_error().status());
  }

  bulk_out_inspect_.SetCancelled(false);
  bulk_in_inspect_.SetCancelled(false);
  bulk_out_inspect_.SnapshotTransferStats();
  bulk_in_inspect_.SnapshotTransferStats();
  function_inspect_.OnConfigured();

  // queue RX requests
  std::vector<fuchsia_hardware_usb_request::Request> requests;
  while (auto req = bulk_out_ep_.GetRequest()) {
    req->reset_buffers(bulk_out_ep_.GetMapped());
    auto status = req->CacheFlushInvalidate(bulk_out_ep_.GetMapped());
    if (status != ZX_OK) {
      zxlogf(ERROR, "Cache flush and invalidate failed %s", zx_status_get_string(status));
      bulk_out_ep_.PutRequest(std::move(req.value()));
      continue;
    }

    requests.emplace_back(req->take_request());
  }

  // QueueRequests transfers ownership of `requests` via std::move to the endpoint FIDL client,
  // leaving `requests` empty. If QueueRequests returns an error (e.g. channel closed), endpoint
  // teardown is initiated.
  auto result = bulk_out_ep_->QueueRequests(std::move(requests));
  if (result.is_error()) {
    zxlogf(ERROR, "Failed to QueueRequests %s", result.error_value().FormatDescription().c_str());
  }

  if (adb_binding_.has_value()) {
    auto result = fidl::WireSendEvent(*adb_binding_)->OnStatusChanged(fadb::StatusFlags::kOnline);
    if (!result.ok()) {
      zxlogf(ERROR, "Could not call UsbAdbImpl.OnStatusChanged.");
    }
  }

  usb_disconnect_observed_ = false;
  adb_connected_during_stop_ = false;
  SetState(State::kOnline);

  SendQueued();
  ReceiveQueued();
  return zx::ok();
}

void UsbAdbDevice::SetConfigured(SetConfiguredRequest& request,
                                 SetConfiguredCompleter::Sync& completer) {
  zxlogf(INFO, "configured? - %d", request.configured());
  if (request.configured()) {
    if (zx::result<> result = EnableEndpoints(); result.is_error()) {
      completer.Reply(result.take_error());
      return;
    }
    completer.Reply(zx::ok());
    return;
  }

  pending_reconfigure_ = false;
  reset_usb_on_stop_ = false;
  switch (state_) {
    case State::kAwaitingUsbConnection:
      // It's normal to receive SetConfigured(false) while the connection is
      // starting up - reply immediately.
      completer.Reply(zx::ok());
      return;
    case State::kOnline:
      RecordEvent("usb_bus_disconnected: SetConfigured(false)");
      function_inspect_.OnUnconfigureStart();
      set_configured_completers_.push_back(completer.ToAsync());
      ResetOrStopUsb(State::kStoppingForReconnect);
      return;
    case State::kStoppingForUnbind:
    case State::kStoppingForReconnect:
      zxlogf(INFO, "Received SetConfigured(false) while already stopping; coalescing completer.");
      set_configured_completers_.push_back(completer.ToAsync());
      CheckUsbStopComplete();
      return;
  }
}

void UsbAdbDevice::SetInterface(SetInterfaceRequest& request,
                                SetInterfaceCompleter::Sync& completer) {
  zxlogf(INFO, "SetInterface called");
  uint8_t interface = request.interface();
  uint8_t alt_setting = request.alt_setting();

  if (interface != descriptors_.adb_intf.b_interface_number || alt_setting != 0) {
    completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
    return;
  }
  completer.Reply(zx::ok());
}

void UsbAdbDevice::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_hardware_usb_function::UsbFunctionInterface> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  zxlogf(WARNING, "Unknown method %ld", metadata.method_ordinal);
}

void UsbAdbDevice::CheckUsbStopComplete() {
  if (state_ != State::kStoppingForUnbind && state_ != State::kStoppingForReconnect) {
    return;
  }

  // Teardown barrier: do not reply to held `SetConfigured(false)` / `StopAdb()` completers or tear
  // down `usb_function_binding_` until DWC3 has returned every in-flight bulk IN/OUT request VMO
  // (`AllRequestsReturned()`) and acknowledged both endpoint `CancelAll()` calls
  // (`CancelAllCompleted()`).
  if (!AllRequestsReturned() || !CancelAllCompleted()) {
    // Still waiting for outstanding USB requests to return and CancelAll calls to complete.
    // TODO(b/417808660): Replace logs with Inspect once the bug is fixed.
    zxlogf(
        INFO,
        "Not all USB requests complete (in_inflight:%zu/%zu out_inflight:%zu/%zu) or CancelAll complete (in:%d out:%d)",
        bulk_in_ep_.GetInFlightCount(), bulk_in_ep_.GetTotalCount(),
        bulk_out_ep_.GetInFlightCount(), bulk_out_ep_.GetTotalCount(), bulk_in_cancelled_,
        bulk_out_cancelled_);
    return;
  }

  zxlogf(INFO, "Completing USB stop.");
  if (function_inspect_.is_unconfiguring()) {
    function_inspect_.OnUnconfigureComplete();
  } else {
    function_inspect_.SetConfigured(false);
  }
  usb_disconnect_observed_ = false;
  // Passive host bus resets and cable disconnects keep `usb_function_binding_` intact in
  // `usb-peripheral` (`should_deconfigure_usb == false`), avoiding controller bounce loops while
  // unplugging. `Deconfigure()` is strictly reserved for explicit daemon stops
  // (`reset_usb_on_stop_`) and driver unbind (`kStoppingForUnbind`).
  const bool should_deconfigure_usb = (state_ == State::kStoppingForUnbind) || reset_usb_on_stop_;
  reset_usb_on_stop_ = false;

  if (adb_binding_.has_value() &&
      (state_ == State::kStoppingForUnbind || !adb_connected_during_stop_)) {
    auto result = fidl::WireSendEvent(*adb_binding_)->OnStatusChanged(fadb::StatusFlags(0));
    if (!result.ok()) {
      zxlogf(ERROR, "Could not call UsbAdbImpl.OnStatusChanged.");
    }
    if (should_deconfigure_usb) {
      adb_binding_.reset();
    }
  }
  adb_connected_during_stop_ = false;

  // Reply to held `SetConfigured(false)` and `StopAdb()` completers BEFORE calling `Deconfigure()`
  // and closing `usb_function_binding_` so `usb-peripheral` observes `ZX_OK` rather than
  // `ZX_ERR_PEER_CLOSED` even during driver unbind.
  // TODO(b/417808660): Replace logs with Inspect once the bug is fixed.
  zxlogf(INFO, "Calling stop_completers_ and set_configured_completers_");
  while (!stop_completers_.empty()) {
    stop_completers_.back().Reply(zx::ok());
    stop_completers_.pop_back();
  }
  while (!set_configured_completers_.empty()) {
    set_configured_completers_.back().Reply(zx::ok());
    set_configured_completers_.pop_back();
  }

  if (should_deconfigure_usb && usb_function_binding_.has_value()) {
    fidl::Result deconfigure = function_->Deconfigure();
    if (deconfigure.is_error()) {
      zxlogf(ERROR, "Failed to deconfigure: %s",
             deconfigure.error_value().FormatDescription().c_str());
    }
    usb_function_binding_.reset();
  }

  // Is this a proper shutdown, an ADB-requested USB reset, or a host/bus unconfigure?
  if (state_ == State::kStoppingForUnbind) {
    bulk_out_ep_.Close();
    bulk_in_ep_.Close();
    zxlogf(INFO, "Shutting down driver.");
    if (shutdown_callback_.has_value()) {
      shutdown_callback_.value()(zx::ok());
      shutdown_callback_.reset();
    }
  } else if (should_deconfigure_usb) {
    // Any pending_reconfigure_ from the old usb_function_binding_ is invalidated by
    // Deconfigure() and the subsequent USB bus reset; the host will issue a fresh
    // SetConfigured(true) once re-enumeration completes.
    pending_reconfigure_ = false;
    zxlogf(INFO, "Restarting USB connection.");
    if (zx::result<> result = StartUsb(); result.is_error()) {
      zxlogf(WARNING, "Restarting USB connection failed: %s", result.status_string());
      RecordEvent("restarting_usb_connection_failed");
      SetState(State::kAwaitingUsbConnection);
    }
  } else {
    SetState(State::kAwaitingUsbConnection);
    if (pending_reconfigure_) {
      pending_reconfigure_ = false;
      zxlogf(INFO, "Applying pending reconfiguration after stop complete");
      (void)EnableEndpoints();
    }
  }
}

void UsbAdbDevice::Stop(fdf::StopCompleter completer) {
  RecordEvent("driver_stopped");
  if (throughput_tracker_) {
    throughput_tracker_->Stop();
  }
  shutdown_callback_.emplace(std::move(completer));
  ResetOrStopUsb(State::kStoppingForUnbind);
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
  resources.push_back(ffunction::EndpointResource(
      fdescriptor::EndpointDirection::kOut, std::move(bulk_out_endpoints->server),
      fuchsia_hardware_usb_endpoint::EndpointInfo::WithBulk({}), kBulkMaxPacket));
  resources.push_back(ffunction::EndpointResource(
      fdescriptor::EndpointDirection::kIn, std::move(bulk_in_endpoints->server),
      fuchsia_hardware_usb_endpoint::EndpointInfo::WithBulk({}), kBulkMaxPacket));

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
    bulk_out_ep_.Close();
    zxlogf(ERROR, "InitEndpoint failed - %s.", zx_status_get_string(status));
    return zx::error(status);
  }
  auto serve_result = outgoing()->AddService<fadb::Service>(fadb::Service::InstanceHandler({
      .adb = device_bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                            fidl::kIgnoreBindingClosure),
  }));
  if (serve_result.is_error()) {
    bulk_out_ep_.Close();
    bulk_in_ep_.Close();
    zxlogf(ERROR, "Failed to add Device service %s", serve_result.status_string());
    return serve_result.take_error();
  }

  component_inspector_ = context.CreateInspector(this);
  if (component_inspector_.has_value()) {
    inspect_node_ = component_inspector_->root().CreateChild("usb-adb-function");
    constexpr size_t kEventCapacity = 64;
    function_inspect_.Init(inspect_node_, StateToString(state_), kEventCapacity);
    bulk_in_inspect_.Init(inspect_node_, "bulk_in", kEventCapacity);
    bulk_in_inspect_.SetPoolCountsCallback([this]() { return bulk_in_ep_.GetPoolCounts(); });
    bulk_out_inspect_.Init(inspect_node_, "bulk_out", kEventCapacity);
    bulk_out_inspect_.SetPoolCountsCallback([this]() { return bulk_out_ep_.GetPoolCounts(); });

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

  zx::result<> start_usb_result = StartUsb();
  if (start_usb_result.is_error()) {
    bulk_out_ep_.Close();
    bulk_in_ep_.Close();
    return start_usb_result.take_error();
  }
  return zx::ok();
}

zx::result<> UsbAdbDevice::StartUsb() {
  zx::result iface_endpoints =
      fidl::CreateEndpoints<fuchsia_hardware_usb_function::UsbFunctionInterface>();
  if (iface_endpoints.is_error()) {
    zxlogf(ERROR, "CreateEndpoints failed %s", iface_endpoints.status_string());
    return iface_endpoints.take_error();
  }
  usb_function_binding_.emplace(
      dispatcher(), std::move(iface_endpoints->server), this, [this](fidl::UnbindInfo info) {
        zxlogf(INFO, "usb_function_binding_ successfully and fully unbound: %s",
               info.FormatDescription().c_str());
        RecordEvent("usb_function_disconnected: unbind");
      });

  std::vector<uint8_t> descriptors_buffer(sizeof(descriptors_));
  memcpy(descriptors_buffer.data(), &descriptors_, sizeof(descriptors_));

  fidl::Request<fuchsia_hardware_usb_function::UsbFunction::Configure> config_req;
  config_req.configuration(std::move(descriptors_buffer));
  config_req.iface(std::move(iface_endpoints->client));

  fidl::Result config_res = function_->Configure(std::move(config_req));
  if (config_res.is_error()) {
    zxlogf(WARNING, "Configure failed: %s", config_res.error_value().FormatDescription().c_str());
    usb_function_binding_.reset();
    return zx::error(config_res.error_value().is_framework_error()
                         ? config_res.error_value().framework_error().status()
                         : config_res.error_value().domain_error());
  }

  SetState(State::kAwaitingUsbConnection);
  bulk_in_cancelled_ = false;
  bulk_out_cancelled_ = false;
  return zx::ok();
}

}  // namespace usb_adb_function

FUCHSIA_DRIVER_EXPORT2(usb_adb_function::UsbAdbDevice);
