// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-bus/usb-device.h"

#include <fidl/fuchsia.driver.framework/cpp/natural_types.h>
#include <fidl/fuchsia.hardware.usb.device/cpp/wire.h>
#include <fuchsia/hardware/usb/bus/c/banjo.h>
#include <fuchsia/hardware/usb/c/banjo.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/metadata.h>
#include <lib/zx/time.h>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/usb/cpp/bind.h>
#include <ddktl/fidl.h>
#include <fbl/auto_lock.h>
#include <usb/usb.h>

#include "src/devices/usb/drivers/usb-bus/usb-bus.h"
#include "src/lib/utf_conversion/utf_conversion.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_bus {

class UsbWaiterImpl : public UsbWaiterInterface {
 public:
  zx_status_t Wait(sync_completion_t* completion, zx_duration_t duration) {
    return sync_completion_wait(completion, duration);
  }
};

// By default we create devices for the interfaces on the first configuration.
// This table allows us to specify a different configuration for certain devices
// based on their VID and PID.
//
// TODO(voydanoff) Find a better way of handling this. For example, we could query to see
// if any interfaces on the first configuration have drivers that can bind to them.
// If not, then we could try the other configurations automatically instead of having
// this hard coded list of VID/PID pairs
struct UsbConfigOverride {
  uint16_t vid;
  uint16_t pid;
  uint8_t configuration;
};

static const UsbConfigOverride config_overrides[] = {
    {0x0bda, 0x8153, 2},  // Realtek ethernet dongle has CDC interface on configuration 2
    {0x0BDA, 0x8152, 2},
    {0x13b1, 0x0041, 2},  // Linksys USB3GIGV1 ethernet dongle has CDC interface on configuration 2
    {0, 0, 0},
};

// This thread is for calling the usb request completion callback for requests received from our
// client. We do this on a separate thread because it is unsafe to call out on our own completion
// callback, which is called on the main thread of the USB HCI driver.
int UsbDevice::CallbackThread() {
  bool done = false;

  while (!done) {
    UnownedRequestQueue temp_queue;

    // Wait for new usb requests to complete or for signal to exit this thread.
    sync_completion_wait(&callback_thread_completion_, ZX_TIME_INFINITE);
    sync_completion_reset(&callback_thread_completion_);

    {
      fbl::AutoLock lock(&callback_lock_);

      done = callback_thread_stop_;

      // Copy completed requests to a temp list so we can process them outside of our lock.
      temp_queue = std::move(completed_reqs_);
    }

    // Call completion callbacks outside of the lock.
    for (auto req = temp_queue.pop(); req; req = temp_queue.pop()) {
      if (req->operation()->reset) {
        req->Complete(hci_.ResetEndpoint(device_id_, req->operation()->reset_address), 0,
                      req->private_storage()->silent_completions_count);
        continue;
      }
      const auto& response = req->request()->response;
      req->Complete(response.status, response.actual,
                    req->private_storage()->silent_completions_count);
    }
  }

  return 0;
}

void UsbDevice::StartCallbackThread() {
  // TODO(voydanoff) Once we have a way of knowing when a driver has bound to us, move the thread
  // start there so we don't have to start a thread unless we know we will need it.
  thrd_create_with_name(
      &callback_thread_,
      [](void* arg) -> int { return static_cast<UsbDevice*>(arg)->CallbackThread(); }, this,
      "usb-device-callback-thread");
}

void UsbDevice::StopCallbackThread() {
  bool was_thread_stopped;
  {
    fbl::AutoLock lock(&callback_lock_);
    was_thread_stopped = callback_thread_stop_;
    callback_thread_stop_ = true;
  }

  sync_completion_signal(&callback_thread_completion_);
  if (!was_thread_stopped) {
    thrd_join(callback_thread_, nullptr);
  }
}

UsbDevice::Endpoint* UsbDevice::GetEndpoint(uint8_t ep_address) {
  uint8_t index = 0;
  if (ep_address > 0) {
    index = static_cast<uint8_t>(2 * (ep_address & ~fdescriptor::kEndpointDirectionMask));
    if ((ep_address & fdescriptor::kEndpointDirectionMask) ==
        fidl::ToUnderlying(fdescriptor::EndpointDirection::kOut)) {
      index--;
    }
  }
  return index < USB_MAX_EPS ? &eps_[index] : nullptr;
}

bool UsbDevice::UpdateEndpoint(Endpoint* ep, usb_request_t* completed_req) {
  fbl::AutoLock lock(&ep->lock);

  auto unowned = UnownedRequest(completed_req, parent_req_size_, /* allow_destruct */ false);

  std::optional<size_t> completed_req_idx = ep->pending_reqs.find(&unowned);
  if (!completed_req_idx.has_value()) {
    zxlogf(ERROR, "could not find completed req %p in pending list of endpoint: 0x%x",
           unowned.request(), completed_req->header.ep_address);
    // This should never happen, but we should probably still do a callback.
    return true;
  }

  auto* storage = unowned.private_storage();
  storage->ready_for_client = true;

  auto opt_prev = ep->pending_reqs.prev(&unowned);
  // If all requests in the pending list prior to this one are ready for a callback,
  // then this request has completed in order. Since we do an immediate callback
  // for out of order requests, we just have to check the request before this one.
  bool completed_in_order = !opt_prev.has_value() || opt_prev->private_storage()->ready_for_client;

  if (!storage->require_callback && completed_in_order && completed_req->response.status == ZX_OK) {
    // Skipping unwanted callback since the request completed successfully and in order.
    // Don't remove the request from the list until we do the next callback.
    return false;
  }

  if (completed_in_order) {
    // Remove all requests up to the current request from the pending list.
    auto opt_req = ep->pending_reqs.begin();
    while (opt_req) {
      auto req = *std::move(opt_req);
      auto opt_next = ep->pending_reqs.next(&req);

      ZX_DEBUG_ASSERT(req.private_storage()->ready_for_client);

      ep->pending_reqs.erase(&req);
      if (req.request() == completed_req) {
        break;
      }
      opt_req = std::move(opt_next);
    }
  } else {
    // The request completed out of order. Only remove the single request.
    ep->pending_reqs.erase(&unowned);
    // If this request was supposed to do a callback, make sure the previous request
    // will do a callback.
    ZX_DEBUG_ASSERT(opt_prev.has_value());  // Must be populated if we completed out of order.
    if (unowned.private_storage()->require_callback) {
      opt_prev->private_storage()->require_callback = true;
    }
  }
  unowned.private_storage()->silent_completions_count =
      completed_in_order ? completed_req_idx.value() : 0;
  return true;
}

// usb request completion for the requests passed down to the HCI driver
void UsbDevice::RequestComplete(usb_request_t* req) {
  if (req->reset) {
    QueueCallback(req);
    return;
  }
  auto* ep = GetEndpoint(req->header.ep_address);
  if (!ep) {
    zxlogf(ERROR, "could not find endpoint with address 0x%x", req->header.ep_address);
    // This should never happen, but we should probably still do a callback.
    QueueCallback(req);
    return;
  }

  bool do_callback = UpdateEndpoint(ep, req);
  if (do_callback) {
    QueueCallback(req);
  }
}

void UsbDevice::QueueCallback(usb_request_t* req) {
  {
    fbl::AutoLock lock(&callback_lock_);

    // move original request to completed_reqs list so it can be completed on the callback_thread
    completed_reqs_.push(UnownedRequest(req, parent_req_size_));
  }
  sync_completion_signal(&callback_thread_completion_);
}

void UsbDevice::SetHubInterface(const usb_hub_interface_protocol_t* hub_intf) {
  if (hub_intf) {
    hub_intf_ = hub_intf;
  } else {
    hub_intf_.clear();
  }
}

const usb_configuration_descriptor_t* UsbDevice::GetConfigDesc(uint8_t config) {
  for (auto& desc_array : config_descs_) {
    auto* desc = reinterpret_cast<usb_configuration_descriptor_t*>(desc_array.data());
    if (desc->b_configuration_value == config) {
      return desc;
    }
  }
  return nullptr;
}

zx_status_t UsbDevice::DdkGetProtocol(uint32_t proto_id, void* protocol) {
  zx_status_t status;

  switch (proto_id) {
    case ZX_PROTOCOL_USB: {
      auto* usb_proto = static_cast<usb_protocol_t*>(protocol);
      usb_proto->ctx = this;
      usb_proto->ops = &usb_protocol_ops_;
      status = ZX_OK;
      break;
    }
    case ZX_PROTOCOL_USB_BUS: {
      auto* bus_proto = static_cast<usb_bus_protocol_t*>(protocol);
      bus_.GetProto(bus_proto);
      status = ZX_OK;
      break;
    }
    default:
      status = ZX_ERR_NOT_SUPPORTED;
      break;
  }

  return status;
}

void UsbDevice::DdkRelease() {
  StopCallbackThread();

  if (Release()) {
    delete this;
  }
}

void UsbDevice::ControlComplete(void* ctx, usb_request_t* req) {
  sync_completion_signal(static_cast<sync_completion_t*>(ctx));
}

zx_status_t UsbDevice::Control(uint8_t request_type, uint8_t request, uint16_t value,
                               uint16_t index, zx_duration_t timeout, const void* write_buffer,
                               size_t write_size, void* out_read_buffer, size_t read_size,
                               size_t* out_read_actual) {
  size_t length;
  bool out = usb_request_is_out(request_type);
  if (out) {
    length = write_size;
  } else {
    length = read_size;
  }
  if (length > UINT16_MAX) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  std::optional<Request> req;
  bool use_free_list = (length == 0);
  if (use_free_list) {
    req = free_reqs_.Get(length);
  }

  if (!req.has_value()) {
    auto status = Request::Alloc(&req, length, 0, parent_req_size_);
    if (status != ZX_OK) {
      return status;
    }
  }

  // fill in protocol data
  usb_setup_t* setup = &req->request()->setup;
  setup->bm_request_type = request_type;
  setup->b_request = request;
  setup->w_value = value;
  setup->w_index = index;
  setup->w_length = static_cast<uint16_t>(length);

  if (out) {
    if (length > 0 && write_buffer == nullptr) {
      return ZX_ERR_INVALID_ARGS;
    }
    if (length > write_size) {
      return ZX_ERR_BUFFER_TOO_SMALL;
    }
  } else {
    if (length > 0 && out_read_buffer == nullptr) {
      return ZX_ERR_INVALID_ARGS;
    }
    if (length > read_size) {
      return ZX_ERR_BUFFER_TOO_SMALL;
    }
  }

  if (length > 0 && out) {
    size_t result = req->CopyTo(write_buffer, length, 0);
    ZX_ASSERT(result == length);
  }

  sync_completion_t completion;

  req->request()->header.device_id = device_id_;
  req->request()->header.length = length;
  // We call this directly instead of via hci_queue, as it's safe to call our
  // own completion callback, and prevents clients getting into odd deadlocks.
  usb_request_complete_callback_t complete = {
      .callback = ControlComplete,
      .ctx = &completion,
  };
  // Use request() instead of take() since we keep referring to the request below.
  hci_.RequestQueue(req->request(), &complete);
  auto status = waiter_->Wait(&completion, timeout);

  if (status == ZX_OK) {
    status = req->request()->response.status;
  } else if (status == ZX_ERR_TIMED_OUT) {
    // cancel transactions and wait for request to be completed
    status = hci_.CancelAll(device_id_, 0);
    if (status == ZX_OK) {
      waiter_->Wait(&completion, ZX_TIME_INFINITE);
      status = ZX_ERR_TIMED_OUT;
    }
  }

  if (status == ZX_ERR_IO_REFUSED || status == ZX_ERR_IO_INVALID) {
    zxlogf(ERROR, "Control transfer failed with %s; resetting endpoint 0",
           zx_status_get_string(status));
    zx_status_t reset_status = hci_.ResetEndpoint(device_id_, 0);
    if (reset_status != ZX_OK) {
      zxlogf(ERROR, "Failed to reset endpoint 0: %s", zx_status_get_string(reset_status));
    }
  }
  if (status == ZX_OK && !out) {
    auto actual = req->request()->response.actual;
    if (length > 0) {
      size_t result = req->CopyFrom(out_read_buffer, actual, 0);
      ZX_ASSERT(result == actual);
    }
    if (out_read_actual != nullptr) {
      *out_read_actual = actual;
    }
  }

  if (use_free_list) {
    free_reqs_.Add(std::move(*req));
  } else {
    req->Release();
  }
  return status;
}

zx_status_t UsbDevice::UsbControlOut(uint8_t request_type, uint8_t request, uint16_t value,
                                     uint16_t index, int64_t timeout, const uint8_t* write_buffer,
                                     size_t write_size) {
  if (!usb_request_is_out(request_type)) {
    return ZX_ERR_INVALID_ARGS;
  }
  return Control(request_type, request, value, index, timeout, write_buffer, write_size, nullptr, 0,
                 nullptr);
}

zx_status_t UsbDevice::UsbControlIn(uint8_t request_type, uint8_t request, uint16_t value,
                                    uint16_t index, int64_t timeout, uint8_t* out_read_buffer,
                                    size_t read_size, size_t* out_read_actual) {
  if (!usb_request_is_in(request_type)) {
    return ZX_ERR_INVALID_ARGS;
  }
  return Control(request_type, request, value, index, timeout, nullptr, 0, out_read_buffer,
                 read_size, out_read_actual);
}

void UsbDevice::UsbRequestQueue(usb_request_t* req,
                                const usb_request_complete_callback_t* complete_cb) {
  req->header.device_id = device_id_;
  if (req->reset) {
    // Save client's callback in private storage.
    UnownedRequest request(req, *complete_cb, parent_req_size_);
    *request.private_storage() = {.ready_for_client = false,
                                  .require_callback = !req->cb_on_error_only,
                                  .silent_completions_count = 0};
    RequestComplete(request.take());
    return;
  }
  if (req->direct) {
    hci_.RequestQueue(req, complete_cb);
    return;
  }

  // Queue to HCI driver with our own completion callback so we can call client's completion
  // on our own thread, to avoid drivers from deadlocking the HCI driver's interrupt thread.
  usb_request_complete_callback_t complete = {
      .callback = [](void* ctx,
                     usb_request_t* req) { static_cast<UsbDevice*>(ctx)->RequestComplete(req); },
      .ctx = this,
  };

  // Save client's callback in private storage.
  UnownedRequest request(req, *complete_cb, parent_req_size_);
  *request.private_storage() = {.ready_for_client = false,
                                .require_callback = !req->cb_on_error_only,
                                .silent_completions_count = 0};

  auto* ep = GetEndpoint(req->header.ep_address);
  if (!ep) {
    zxlogf(ERROR, "could not find endpoint with address 0x%x", req->header.ep_address);
  }

  {
    // RequestQueue may callback before it returns, so make sure to release the endpoint lock.
    fbl::AutoLock lock(&ep->lock);
    ep->pending_reqs.push_back(&request);
  }

  // Queue with our callback instead.
  hci_.RequestQueue(request.take(), &complete);
}

usb_speed_t UsbDevice::UsbGetSpeed() { return speed_; }

zx_status_t UsbDevice::UsbSetInterface(uint8_t interface_number, uint8_t alt_setting) {
  return Control(kStandardInterfaceOut,
                 fidl::ToUnderlying(fdescriptor::StandardRequest::kSetInterface), alt_setting,
                 interface_number, ZX_TIME_INFINITE, nullptr, 0, nullptr, 0, nullptr);
}

uint8_t UsbDevice::UsbGetConfiguration() {
  fbl::AutoLock lock(&state_lock_);
  auto* descriptor = reinterpret_cast<usb_configuration_descriptor_t*>(
      config_descs_[current_config_index_].data());
  return descriptor->b_configuration_value;
}

zx_status_t UsbDevice::UsbSetConfiguration(uint8_t configuration) {
  uint8_t index = 0;
  for (auto& desc_array : config_descs_) {
    auto* descriptor = reinterpret_cast<usb_configuration_descriptor_t*>(desc_array.data());
    if (descriptor->b_configuration_value == configuration) {
      fbl::AutoLock lock(&state_lock_);

      zx_status_t status;
      status = Control(kStandardDeviceOut,
                       fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration),
                       configuration, 0, ZX_TIME_INFINITE, nullptr, 0, nullptr, 0, nullptr);
      if (status == ZX_OK) {
        current_config_index_ = index;
      }
      return status;
    }
    index++;
  }
  return ZX_ERR_INVALID_ARGS;
}

zx_status_t UsbDevice::UsbEnableEndpoint(const usb_endpoint_descriptor_t* ep_desc,
                                         const usb_ss_ep_comp_descriptor_t* ss_com_desc,
                                         bool enable) {
  return hci_.EnableEndpoint(device_id_, ep_desc, ss_com_desc, enable);
}

zx_status_t UsbDevice::UsbResetEndpoint(uint8_t ep_address) {
  return hci_.ResetEndpoint(device_id_, ep_address);
}

zx_status_t UsbDevice::UsbResetDevice() {
  {
    fbl::AutoLock lock(&state_lock_);

    if (resetting_) {
      zxlogf(ERROR, "%s: resetting_ already set", __func__);
      return ZX_ERR_BAD_STATE;
    }
    resetting_ = true;
  }

  return hci_.ResetDevice(hub_id_, device_id_);
}

size_t UsbDevice::UsbGetMaxTransferSize(uint8_t ep_address) {
  return hci_.GetMaxTransferSize(device_id_, ep_address);
}

uint32_t UsbDevice::UsbGetDeviceId() { return device_id_; }

void UsbDevice::UsbGetDeviceDescriptor(usb_device_descriptor_t* out_desc) {
  memcpy(out_desc, &device_desc_, sizeof(usb_device_descriptor_t));
}

zx_status_t UsbDevice::UsbGetConfigurationDescriptorLength(uint8_t configuration,
                                                           size_t* out_length) {
  for (auto& desc_array : config_descs_) {
    auto* config_desc = reinterpret_cast<usb_configuration_descriptor_t*>(desc_array.data());
    if (config_desc->b_configuration_value == configuration) {
      *out_length = le16toh(config_desc->w_total_length);
      return ZX_OK;
    }
  }
  *out_length = 0;
  return ZX_ERR_INVALID_ARGS;
}

zx_status_t UsbDevice::UsbGetConfigurationDescriptor(uint8_t configuration,
                                                     uint8_t* out_desc_buffer, size_t desc_size,
                                                     size_t* out_desc_actual) {
  for (auto& desc_array : config_descs_) {
    auto* config_desc = reinterpret_cast<usb_configuration_descriptor_t*>(desc_array.data());
    if (config_desc->b_configuration_value == configuration) {
      size_t length = le16toh(config_desc->w_total_length);
      if (length > desc_size) {
        length = desc_size;
      }
      memcpy(out_desc_buffer, config_desc, length);
      *out_desc_actual = length;
      return ZX_OK;
    }
  }
  return ZX_ERR_INVALID_ARGS;
}

size_t UsbDevice::UsbGetDescriptorsLength() {
  fbl::AutoLock lock(&state_lock_);
  auto* config_desc = reinterpret_cast<usb_configuration_descriptor_t*>(
      config_descs_[current_config_index_].data());
  return le16toh(config_desc->w_total_length);
}

void UsbDevice::UsbGetDescriptors(uint8_t* out_descs_buffer, size_t descs_size,
                                  size_t* out_descs_actual) {
  fbl::AutoLock lock(&state_lock_);
  auto* config_desc = reinterpret_cast<usb_configuration_descriptor_t*>(
      config_descs_[current_config_index_].data());
  size_t length = le16toh(config_desc->w_total_length);
  if (length > descs_size) {
    length = descs_size;
  }

  memcpy(out_descs_buffer, config_desc, length);
  *out_descs_actual = length;
}

zx_status_t UsbDevice::UsbGetStringDescriptor(uint8_t desc_id, uint16_t lang_id,
                                              uint16_t* out_actual_lang_id, uint8_t* buf,
                                              size_t buflen, size_t* out_actual) {
  fbl::AutoLock lock(&state_lock_);
  //  If we have never attempted to load our language ID table, do so now.
  if (!lang_ids_.has_value()) {
    usb_langid_desc_t id_desc = {};
    size_t actual = 0;
    auto result = GetDescriptor(fdescriptor::DescriptorType::kString, 0, 0, &id_desc,
                                sizeof(id_desc), &actual);
    if (result == ZX_ERR_IO_REFUSED || result == ZX_ERR_IO_INVALID) {
      zxlogf(WARNING,
             "Failed to get string descriptor language list due to error %s. Resetting endpoint.",
             zx_status_get_string(result));
      // some devices do not support fetching language list
      // in that case assume US English (0x0409)
      zx_status_t reset_result = hci_.ResetEndpoint(device_id_, 0);
      if (reset_result != ZX_OK) {
        zxlogf(ERROR, "failed to reset endpoint, err: %d", reset_result);
        return result;
      }
      zxlogf(INFO, "Reset endpoint complete with status %s", zx_status_get_string(reset_result));
      id_desc.b_length = 4;
      id_desc.w_lang_ids[0] = htole16(0x0409);
      actual = 4;
    } else if (result != ZX_OK) {
      return result;
    } else if ((actual < 4) || (actual > sizeof(id_desc)) || (actual != id_desc.b_length) ||
               (actual & 0x1)) {
      return ZX_ERR_INTERNAL;
    }

    // So, if we have managed to fetch/synthesize a language ID table,
    // go ahead and perform a bit of fixup.  Redefine b_length to be the
    // valid number of entries in the table, and fixup the endianness of
    // all the entries in the table.  Then, attempt to swap in the new
    // language ID table.
    id_desc.b_length = static_cast<uint8_t>((id_desc.b_length - 2) >> 1);
#if BYTE_ORDER != LITTLE_ENDIAN
    for (uint8_t i = 0; i < id_desc.b_length; ++i) {
      id_desc.w_lang_ids[i] = letoh16(id_desc.w_lang_ids[i]);
    }
#endif
    lang_ids_ = id_desc;
  }

  // Handle the special case that the user asked for the language ID table.
  if (desc_id == 0) {
    size_t table_sz = (lang_ids_->b_length << 1);
    buflen &= ~1;
    size_t actual = (table_sz < buflen ? table_sz : buflen);
    memcpy(buf, lang_ids_->w_lang_ids, actual);
    *out_actual = actual;
    return ZX_OK;
  }
  // Search for the requested language ID.
  uint32_t lang_ndx;
  for (lang_ndx = 0; lang_ndx < lang_ids_->b_length; ++lang_ndx) {
    if (lang_id == lang_ids_->w_lang_ids[lang_ndx]) {
      break;
    }
  }

  // If we didn't find it, default to the first entry in the table.
  if (lang_ndx >= lang_ids_->b_length) {
    ZX_DEBUG_ASSERT(lang_ids_->b_length >= 1);
    lang_id = lang_ids_->w_lang_ids[0];
  }

  usb_string_desc_t string_desc = {};
  zxlogf(DEBUG, "Fetching string descriptor with lang_id %u", lang_id);
  size_t actual = 0;
  auto result = GetDescriptor(fdescriptor::DescriptorType::kString, desc_id, le16toh(lang_id),
                              &string_desc, sizeof(string_desc), &actual);

  if (result == ZX_ERR_IO_REFUSED || result == ZX_ERR_IO_INVALID) {
    zxlogf(WARNING, "Fetching string descriptor failed with error %s",
           zx_status_get_string(result));
    zx_status_t reset_result = hci_.ResetEndpoint(device_id_, 0);
    if (reset_result != ZX_OK) {
      zxlogf(ERROR, "failed to reset endpoint, err: %d", reset_result);
      return result;
    }
    result = GetDescriptor(fdescriptor::DescriptorType::kString, desc_id, le16toh(lang_id),
                           &string_desc, sizeof(string_desc), &actual);
    if (result == ZX_ERR_IO_REFUSED || result == ZX_ERR_IO_INVALID) {
      zxlogf(WARNING, "Fetching string descriptor after reset failed with error %s",
             zx_status_get_string(result));
      reset_result = hci_.ResetEndpoint(device_id_, 0);
      if (reset_result != ZX_OK) {
        zxlogf(ERROR, "failed to reset endpoint, err: %d", reset_result);
        return result;
      }
    }
  }

  if (result != ZX_OK) {
    return result;
  }

  if ((actual < 2) || (actual > sizeof(string_desc)) || (actual != string_desc.b_length)) {
    result = ZX_ERR_INTERNAL;
  } else {
    // Success! Convert this result from UTF16LE to UTF8 and store the
    // language ID we actually fetched (if it was not what the user
    // requested).  Note that the conversion may take more buffer space
    // than what was provided by the user.  This is a checked error, and
    // in this situation, the minimum required buffersize will be
    // communicated back via out_actual.
    size_t utf8_actual = buflen;
    *out_actual_lang_id = lang_id;
    result = utf16_to_utf8(string_desc.code_points, (string_desc.b_length >> 1) - 1,
                           static_cast<uint8_t*>(buf), &utf8_actual,
                           UTF_CONVERT_FLAG_FORCE_LITTLE_ENDIAN);

    *out_actual = utf8_actual;

    if (utf8_actual > buflen) {
      zxlogf(ERROR, "insufficient space for UTF8 conversion");
      return ZX_ERR_BUFFER_TOO_SMALL;
    }
  }

  return result;
}

zx_status_t UsbDevice::UsbCancelAll(uint8_t ep_address) {
  zx_status_t status = hci_.CancelAll(device_id_, ep_address);
  if (status != ZX_OK) {
    return status;
  }
  // Stop the callback thread to prevent races.
  StopCallbackThread();
  // Complete all outstanding requests (host controller should have invoked all of the callbacks
  // at this layer in the stack.
  UnownedRequestQueue temp_queue;
  {
    fbl::AutoLock lock(&callback_lock_);
    // Copy completed requests to a temp list so we can process them outside of our lock.
    temp_queue = std::move(completed_reqs_);
  }
  // Call completion callbacks outside of the lock.
  for (auto req = temp_queue.pop(); req; req = temp_queue.pop()) {
    if (req->operation()->reset) {
      req->Complete(hci_.ResetEndpoint(device_id_, req->operation()->reset_address), 0,
                    req->private_storage()->silent_completions_count);
      continue;
    }
    const auto& response = req->request()->response;
    req->Complete(response.status, response.actual,
                  req->private_storage()->silent_completions_count);
  }

  // TODO(jocelyndang): after cancelling, we should check if the ep pending_reqs has any items.
  // We may have to do callbacks now if the requests already completed before the cancel
  // occurred, but the client did not request any callbacks.

  {
    fbl::AutoLock lock(&callback_lock_);
    callback_thread_stop_ = false;
    StartCallbackThread();
  }
  return ZX_OK;
}

uint64_t UsbDevice::UsbGetCurrentFrame() { return hci_.GetCurrentFrame(); }

size_t UsbDevice::UsbGetRequestSize() { return UnownedRequest::RequestSize(parent_req_size_); }

void UsbDevice::GetDeviceSpeed(GetDeviceSpeedCompleter::Sync& completer) {
  completer.Reply(speed_);
}

void UsbDevice::GetDeviceDescriptor(GetDeviceDescriptorCompleter::Sync& completer) {
  fidl::Array<uint8_t, sizeof(device_desc_)> data;
  memcpy(data.data(), &device_desc_, sizeof(device_desc_));
  completer.Reply(std::move(data));
}

void UsbDevice::GetConfigurationDescriptorSize(
    GetConfigurationDescriptorSizeRequestView request,
    GetConfigurationDescriptorSizeCompleter::Sync& completer) {
  auto* descriptor = GetConfigDesc(request->config);
  if (!descriptor) {
    completer.Reply(ZX_ERR_INVALID_ARGS, 0);
    return;
  }

  auto length = le16toh(descriptor->w_total_length);
  completer.Reply(ZX_OK, length);
}

void UsbDevice::GetConfigurationDescriptor(GetConfigurationDescriptorRequestView request,
                                           GetConfigurationDescriptorCompleter::Sync& completer) {
  auto* descriptor = GetConfigDesc(request->config);
  if (!descriptor) {
    completer.Reply(ZX_ERR_INVALID_ARGS, fidl::VectorView<uint8_t>());
    return;
  }

  size_t length = le16toh(descriptor->w_total_length);
  completer.Reply(ZX_OK,
                  fidl::VectorView<uint8_t>::FromExternal(
                      const_cast<uint8_t*>(reinterpret_cast<const uint8_t*>(descriptor)), length));
}

void UsbDevice::GetStringDescriptor(GetStringDescriptorRequestView request,
                                    GetStringDescriptorCompleter::Sync& completer) {
  char buffer[fuchsia_hardware_usb_device::wire::kMaxStringDescSize];
  size_t actual = 0;
  auto status = UsbGetStringDescriptor(request->desc_id, request->lang_id, &request->lang_id,
                                       reinterpret_cast<uint8_t*>(buffer), sizeof(buffer), &actual);
  completer.Reply(status, fidl::StringView::FromExternal(buffer, actual), request->lang_id);
}

void UsbDevice::SetInterface(SetInterfaceRequestView request,
                             SetInterfaceCompleter::Sync& completer) {
  auto status = UsbSetInterface(request->interface_number, request->alt_setting);
  completer.Reply(status);
}

void UsbDevice::GetDeviceId(GetDeviceIdCompleter::Sync& completer) { completer.Reply(device_id_); }

void UsbDevice::GetHubDeviceId(GetHubDeviceIdCompleter::Sync& completer) {
  completer.Reply(hub_id_);
}

void UsbDevice::GetConfiguration(GetConfigurationCompleter::Sync& completer) {
  fbl::AutoLock lock(&state_lock_);

  auto* descriptor = reinterpret_cast<usb_configuration_descriptor_t*>(
      config_descs_[current_config_index_].data());
  completer.Reply(descriptor->b_configuration_value);
}

void UsbDevice::SetConfiguration(SetConfigurationRequestView request,
                                 SetConfigurationCompleter::Sync& completer) {
  auto status = UsbSetConfiguration(request->configuration);
  completer.Reply(status);
}

void UsbDevice::UsbProtocolServer::ConnectToEndpoint(ConnectToEndpointRequest& request,
                                                     ConnectToEndpointCompleter::Sync& completer) {
  constexpr uint32_t kMask = 0x7FU;
  constexpr uint32_t kReservedMask = 0x70U;
  const uint32_t ep_addr = request.ep_addr();
  const uint32_t ep_num = ep_addr & kMask;
  if (ep_num == 0 || ep_num > 15 || (ep_addr & kReservedMask) != 0) {
    completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
    return;
  }
  fidl::WireResult<fuchsia_hardware_usb_hci::UsbHci::ConnectToEndpoint> result =
      owner_->hci_new_->ConnectToEndpoint(owner_->device_id_, request.ep_addr(),
                                          std::move(request.ep()));
  if (!result.ok()) {
    zxlogf(ERROR, "Failed to call ConnectToEndpoint(): %s",
           result.error().FormatDescription().c_str());
    completer.Reply(fit::as_error(result.status()));
    return;
  }
  if (result->is_error()) {
    zxlogf(ERROR, "ConnectToEndpoint() error: %s", zx_status_get_string(result->error_value()));
    completer.Reply(fit::as_error(result->error_value()));
    return;
  }
  completer.Reply(fit::ok());
}

void UsbDevice::UsbProtocolServer::GetDeviceSpeed(GetDeviceSpeedCompleter::Sync& completer) {
  completer.Reply({{.speed = static_cast<fdescriptor::UsbSpeed>(owner_->speed_)}});
}

void UsbDevice::UsbProtocolServer::GetDeviceId(GetDeviceIdCompleter::Sync& completer) {
  completer.Reply({{.device_id = owner_->device_id_}});
}

void UsbDevice::UsbProtocolServer::GetDeviceDescriptor(
    GetDeviceDescriptorCompleter::Sync& completer) {
  fdescriptor::UsbDeviceDescriptor desc{{
      .b_length = owner_->device_desc_.b_length,
      .b_descriptor_type = owner_->device_desc_.b_descriptor_type,
      .bcd_usb = le16toh(owner_->device_desc_.bcd_usb),
      .b_device_class = owner_->device_desc_.b_device_class,
      .b_device_sub_class = owner_->device_desc_.b_device_sub_class,
      .b_device_protocol = owner_->device_desc_.b_device_protocol,
      .b_max_packet_size0 = owner_->device_desc_.b_max_packet_size0,
      .id_vendor = le16toh(owner_->device_desc_.id_vendor),
      .id_product = le16toh(owner_->device_desc_.id_product),
      .bcd_device = le16toh(owner_->device_desc_.bcd_device),
      .i_manufacturer = owner_->device_desc_.i_manufacturer,
      .i_product = owner_->device_desc_.i_product,
      .i_serial_number = owner_->device_desc_.i_serial_number,
      .b_num_configurations = owner_->device_desc_.b_num_configurations,
  }};
  completer.Reply({{.descriptor = desc}});
}

void UsbDevice::UsbProtocolServer::GetConfigurationDescriptor(
    GetConfigurationDescriptorRequest& request,
    GetConfigurationDescriptorCompleter::Sync& completer) {
  const usb_configuration_descriptor_t* descriptor = owner_->GetConfigDesc(request.config());
  if (!descriptor) {
    completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
    return;
  }
  size_t length = le16toh(descriptor->w_total_length);
  if (length > fuchsia_hardware_usb::kMaxDescriptorLen) {
    completer.Reply(fit::as_error(ZX_ERR_OUT_OF_RANGE));
    return;
  }
  const auto* raw = reinterpret_cast<const uint8_t*>(descriptor);
  completer.Reply(fit::ok(fuchsia_hardware_usb::UsbGetConfigurationDescriptorResponse{
      {.descriptor = std::vector<uint8_t>(raw, raw + length)}}));
}

void UsbDevice::UsbProtocolServer::GetConfiguration(GetConfigurationCompleter::Sync& completer) {
  completer.Reply({{.configuration = owner_->UsbGetConfiguration()}});
}

void UsbDevice::UsbProtocolServer::SetConfiguration(SetConfigurationRequest& request,
                                                    SetConfigurationCompleter::Sync& completer) {
  zx_status_t status = owner_->UsbSetConfiguration(request.configuration());
  if (status != ZX_OK) {
    completer.Reply(fit::as_error(status));
    return;
  }
  completer.Reply(fit::ok());
}

void UsbDevice::UsbProtocolServer::SetInterface(SetInterfaceRequest& request,
                                                SetInterfaceCompleter::Sync& completer) {
  zx_status_t status = owner_->UsbSetInterface(request.interface_number(), request.alt_setting());
  if (status != ZX_OK) {
    completer.Reply(fit::as_error(status));
    return;
  }
  completer.Reply(fit::ok());
}

void UsbDevice::UsbProtocolServer::EnableEndpoint(EnableEndpointRequest& request,
                                                  EnableEndpointCompleter::Sync& completer) {
  constexpr uint32_t kMask = 0x7FU;
  constexpr uint32_t kReservedMask = 0x70U;
  const uint32_t ep_addr = request.ep_desc().b_endpoint_address();
  const uint32_t ep_num = ep_addr & kMask;
  if (ep_num == 0 || ep_num > 15 || (ep_addr & kReservedMask) != 0 ||
      request.ep_desc().b_length() < sizeof(usb_endpoint_descriptor_t) ||
      request.ep_desc().b_descriptor_type() !=
          static_cast<uint8_t>(fdescriptor::DescriptorType::kEndpoint)) {
    completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
    return;
  }
  const usb_endpoint_descriptor_t ep_desc = {
      .b_length = request.ep_desc().b_length(),
      .b_descriptor_type = request.ep_desc().b_descriptor_type(),
      .b_endpoint_address = request.ep_desc().b_endpoint_address(),
      .bm_attributes = request.ep_desc().bm_attributes(),
      .w_max_packet_size = htole16(request.ep_desc().w_max_packet_size()),
      .b_interval = request.ep_desc().b_interval(),
  };
  const usb_ss_ep_comp_descriptor_t ss_com_desc = {
      .b_length = request.ss_com_desc().b_length(),
      .b_descriptor_type = request.ss_com_desc().b_descriptor_type(),
      .b_max_burst = request.ss_com_desc().b_max_burst(),
      .bm_attributes = request.ss_com_desc().bm_attributes(),
      .w_bytes_per_interval = htole16(request.ss_com_desc().w_bytes_per_interval()),
  };
  const usb_ss_ep_comp_descriptor_t* ss_com_ptr =
      (ss_com_desc.b_length >= sizeof(usb_ss_ep_comp_descriptor_t) &&
       ss_com_desc.b_descriptor_type ==
           static_cast<uint8_t>(fdescriptor::DescriptorType::kSsEpCompanion))
          ? &ss_com_desc
          : nullptr;
  zx_status_t status = owner_->UsbEnableEndpoint(&ep_desc, ss_com_ptr, request.enable());
  if (status != ZX_OK) {
    completer.Reply(fit::as_error(status));
    return;
  }
  completer.Reply(fit::ok());
}

void UsbDevice::UsbProtocolServer::ResetEndpoint(ResetEndpointRequest& request,
                                                 ResetEndpointCompleter::Sync& completer) {
  constexpr uint32_t kMask = 0x7FU;
  constexpr uint32_t kReservedMask = 0x70U;
  const uint32_t ep_addr = request.ep_address();
  const uint32_t ep_num = ep_addr & kMask;
  if ((ep_num == 0 && ep_addr != 0) || ep_num > 15 || (ep_addr & kReservedMask) != 0) {
    completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
    return;
  }
  zx_status_t status = owner_->UsbResetEndpoint(request.ep_address());
  if (status != ZX_OK) {
    completer.Reply(fit::as_error(status));
    return;
  }
  completer.Reply(fit::ok());
}

void UsbDevice::UsbProtocolServer::ControlTransfer(ControlTransferRequest& request,
                                                   ControlTransferCompleter::Sync& completer) {
  // USB 2.0 specification, section 9.2.6.4 specifies a 5-second timeout for control transfers.
  const zx_duration_t timeout = request.timeout_ns() > 0 ? request.timeout_ns() : ZX_SEC(5);
  const fdescriptor::UsbSetup& setup = request.setup();
  const bool is_in = usb_request_is_in(setup.bm_request_type());
  if (is_in) {
    if (setup.w_length() > fuchsia_hardware_usb::kMaxControlRequestLen ||
        !request.write_data().empty()) {
      completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
      return;
    }
    std::vector<uint8_t> buf(setup.w_length());
    size_t actual = 0;
    zx_status_t status =
        owner_->Control(setup.bm_request_type(), setup.b_request(), setup.w_value(),
                        setup.w_index(), timeout, nullptr, 0, buf.data(), buf.size(), &actual);
    if (status != ZX_OK) {
      completer.Reply(fit::as_error(status));
      return;
    }
    buf.resize(std::min(actual, buf.size()));
    completer.Reply(
        fit::ok(fuchsia_hardware_usb::UsbControlTransferResponse{{.read_data = std::move(buf)}}));
    return;
  }

  if (request.write_data().size() != setup.w_length()) {
    completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
    return;
  }

  if (setup.bm_request_type() == kStandardDeviceOut &&
      setup.b_request() == fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration)) {
    if (setup.w_length() != 0 || setup.w_index() != 0 || setup.w_value() > UINT8_MAX) {
      completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
      return;
    }
    zx_status_t status = owner_->UsbSetConfiguration(static_cast<uint8_t>(setup.w_value()));
    if (status != ZX_OK) {
      completer.Reply(fit::as_error(status));
      return;
    }
    completer.Reply(fit::ok(fuchsia_hardware_usb::UsbControlTransferResponse{{.read_data = {}}}));
    return;
  }

  if (setup.bm_request_type() == kStandardInterfaceOut &&
      setup.b_request() == fidl::ToUnderlying(fdescriptor::StandardRequest::kSetInterface)) {
    if (setup.w_length() != 0 || setup.w_index() > UINT8_MAX || setup.w_value() > UINT8_MAX) {
      completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
      return;
    }
    zx_status_t status = owner_->UsbSetInterface(static_cast<uint8_t>(setup.w_index()),
                                                 static_cast<uint8_t>(setup.w_value()));
    if (status != ZX_OK) {
      completer.Reply(fit::as_error(status));
      return;
    }
    completer.Reply(fit::ok(fuchsia_hardware_usb::UsbControlTransferResponse{{.read_data = {}}}));
    return;
  }

  zx_status_t status = owner_->Control(setup.bm_request_type(), setup.b_request(), setup.w_value(),
                                       setup.w_index(), timeout, request.write_data().data(),
                                       request.write_data().size(), nullptr, 0, nullptr);
  if (status != ZX_OK) {
    completer.Reply(fit::as_error(status));
    return;
  }
  completer.Reply(fit::ok(fuchsia_hardware_usb::UsbControlTransferResponse{{.read_data = {}}}));
}

void UsbDevice::UsbProtocolServer::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_hardware_usb::Usb> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  zxlogf(WARNING, "usb-bus: unknown fuchsia.hardware.usb/Usb method ordinal %lu",
         metadata.method_ordinal);
}

zx_status_t UsbDevice::HubResetPort(uint32_t port) {
  if (!hub_intf_.is_valid()) {
    zxlogf(ERROR, "hub interface not set in usb_bus_reset_hub_port");
    return ZX_ERR_BAD_STATE;
  }
  return hub_intf_.ResetPort(port);
}

zx_status_t UsbDevice::Create(zx_device_t* parent, const ddk::UsbHciProtocolClient& hci,
                              fidl::ClientEnd<fuchsia_hardware_usb_hci::UsbHci> hci_new,
                              uint32_t device_id, uint32_t hub_id, usb_speed_t speed,
                              async_dispatcher_t* dispatcher, fbl::RefPtr<UsbDevice>* out_device) {
  fbl::AllocChecker ac;
  auto device = fbl::MakeRefCountedChecked<UsbDevice>(
      &ac, parent, hci, std::move(hci_new), device_id, hub_id, speed,
      fbl::MakeRefCounted<UsbWaiterImpl>(), dispatcher);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  // devices_[device_id] must be set before calling DdkAdd().
  *out_device = device;

  auto status = device->Init(dispatcher);
  if (status != ZX_OK) {
    zxlogf(ERROR, "Failed to Init UsbDevice: %s", zx_status_get_string(status));
    out_device->reset();
  }
  return status;
}

zx_status_t UsbDevice::Init(async_dispatcher_t* dispatcher) {
  // We implement ZX_PROTOCOL_USB, but drivers bind to us as ZX_PROTOCOL_USB_DEVICE.
  // We also need this for the device to appear in /dev/class/usb-device/.
  ddk_proto_id_ = ZX_PROTOCOL_USB_DEVICE;
  parent_req_size_ = hci_.GetRequestSize();

  // read device descriptor
  size_t actual;
  auto status = GetDescriptor(fdescriptor::DescriptorType::kDevice, 0, 0, &device_desc_,
                              sizeof(device_desc_), &actual);
  if (status == ZX_OK && actual != sizeof(device_desc_)) {
    status = ZX_ERR_IO;
  }
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: GetDescriptor(kDevice) failed", __func__);
    return status;
  }

  const uint8_t num_configurations = device_desc_.b_num_configurations;
  fbl::AllocChecker ac;
  config_descs_.reset(new (&ac) fbl::Array<uint8_t>[num_configurations], num_configurations);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  fbl::AutoLock lock(&state_lock_);

  for (uint8_t config = 0; config < num_configurations; config++) {
    // read configuration descriptor header to determine size
    usb_configuration_descriptor_t config_desc_header;
    size_t actual;
    status = GetDescriptor(fdescriptor::DescriptorType::kConfiguration, config, 0,
                           &config_desc_header, sizeof(config_desc_header), &actual);
    if (status == ZX_OK && actual != sizeof(config_desc_header)) {
      status = ZX_ERR_IO;
    }
    if (status != ZX_OK) {
      zxlogf(ERROR, "%s: GetDescriptor(kConfiguration) failed", __func__);
      return status;
    }
    uint16_t config_desc_size = letoh16(config_desc_header.w_total_length);
    if (config_desc_size < sizeof(config_desc_header)) {
      zxlogf(ERROR,
             "%s: GetDescriptor(kConfiguration) gave length shorter than self: "
             "expected at least %lu, got %u\n",
             __func__, sizeof(config_desc_header), config_desc_size);
      return ZX_ERR_IO;
    }
    auto* config_desc = new (&ac) uint8_t[config_desc_size];
    if (!ac.check()) {
      return ZX_ERR_NO_MEMORY;
    }
    config_descs_[config].reset(config_desc, config_desc_size);

    // read full configuration descriptor
    status = GetDescriptor(fdescriptor::DescriptorType::kConfiguration, config, 0, config_desc,
                           config_desc_size, &actual);
    if (status != ZX_OK) {
      zxlogf(ERROR, "%s: GetDescriptor(kConfiguration) failed", __func__);
      return status;
    }

    // Guard against the device being evil in a couple ways here.

    // If the actual number of bytes we read for this descriptor doesn't
    // match the number of bytes the descriptor said we should
    // expect when we first asked for the descriptor header, return an error.
    if (actual != config_desc_size) {
      zxlogf(ERROR, "%s GetDescriptor(kConfiguration) config %u expected %u bytes, got %lu\n",
             __func__, config, config_desc_size, actual);
      return ZX_ERR_IO;
    }

    // Similarly, if the second time we read the descriptor, the field
    // inside the descriptor we just read says that it's a different size from
    // what we expected or what we read the first time, return an error.
    uint16_t config_desc_size_on_second_read =
        letoh16(reinterpret_cast<usb_configuration_descriptor_t*>(config_desc)->w_total_length);
    if (actual != config_desc_size_on_second_read) {
      zxlogf(ERROR,
             "%s GetDescriptor(kConfiguration) config %u length changed between reads: "
             "was %u bytes, then became %u\n",
             __func__, config, config_desc_size, config_desc_size_on_second_read);
      return ZX_ERR_IO;
    }
  }
  // we will create devices for interfaces on the first configuration by default
  uint8_t configuration = 1;
  const UsbConfigOverride* override = config_overrides;
  while (override->configuration) {
    if (override->vid == le16toh(device_desc_.id_vendor) &&
        override->pid == le16toh(device_desc_.id_product)) {
      zxlogf(INFO, "Overriding %x:%x with configuration number %d", override->vid, override->pid,
             override->configuration);
      configuration = override->configuration;
      break;
    }
    override++;
  }
  if (configuration > num_configurations) {
    zxlogf(
        ERROR,
        "usb_device_add: override configuration number (%d) out of range (num_configurations %d)",
        configuration, num_configurations);
    return ZX_ERR_INTERNAL;
  }
  current_config_index_ = static_cast<uint8_t>(configuration - 1);

  // set configuration
  auto* config_desc = reinterpret_cast<usb_configuration_descriptor_t*>(
      config_descs_[current_config_index_].data());
  status = UsbControlOut(kStandardDeviceOut,
                         fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration),
                         config_desc->b_configuration_value, 0, ZX_TIME_INFINITE, nullptr, 0);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: kSetConfiguration failed", __func__);
    return status;
  }
  zxlogf(INFO, "* found USB device (0x%04x:0x%04x, USB %x.%x) config %u", device_desc_.id_vendor,
         device_desc_.id_product, device_desc_.bcd_usb >> 8, device_desc_.bcd_usb & 0xff,
         configuration);

  // Callback thread must be started before device_add() since it will recursively
  // bind other drivers to us before it returns.
  StartCallbackThread();

  zx::result result = outgoing_.AddService<fuchsia_hardware_usb::UsbService>(
      fuchsia_hardware_usb::UsbService::InstanceHandler({
          .device = usb_server_.CreateHandler(dispatcher),
      }));
  if (result.is_error()) {
    zxlogf(ERROR, "Failed to add service");
    return result.status_value();
  }
  auto endpoints = fidl::CreateEndpoints<fuchsia_io::Directory>();
  if (endpoints.is_error()) {
    return endpoints.status_value();
  }
  result = outgoing_.Serve(std::move(endpoints->server));
  if (result.is_error()) {
    zxlogf(ERROR, "Failed to service the outgoing directory");
    return result.status_value();
  }

  std::array offers = {
      fuchsia_hardware_usb::UsbService::Name,
  };

  char name[16];
  snprintf(name, sizeof(name), "%03d", device_id_);

  zx_device_str_prop_t props[] = {
      ddk::MakeStrProperty(bind_fuchsia::USB_VID, static_cast<uint32_t>(device_desc_.id_vendor)),
      ddk::MakeStrProperty(bind_fuchsia::USB_PID, static_cast<uint32_t>(device_desc_.id_product)),
      ddk::MakeStrProperty(bind_fuchsia::USB_CLASS,
                           static_cast<uint32_t>(device_desc_.b_device_class)),
      ddk::MakeStrProperty(bind_fuchsia::USB_SUBCLASS,
                           static_cast<uint32_t>(device_desc_.b_device_sub_class)),
      ddk::MakeStrProperty(bind_fuchsia::USB_PROTOCOL,
                           static_cast<uint32_t>(device_desc_.b_device_protocol)),
  };
  auto bus_info =
      std::make_unique<fuchsia_driver_framework::BusInfo>(fuchsia_driver_framework::BusInfo{{
          .bus = fuchsia_driver_framework::BusType::kUsb,
          .address = fuchsia_driver_framework::DeviceAddress::WithIntValue(
              static_cast<uint8_t>(device_id_)),
          .address_stability =
              fuchsia_driver_framework::DeviceAddressStability::kUnstableBetweenBoot,
      }});

  status = DdkAdd(ddk::DeviceAddArgs(name)
                      .set_str_props(props)
                      .set_proto_id(bind_fuchsia_usb::BIND_PROTOCOL_DEVICE)
                      .set_fidl_service_offers(offers)
                      .set_outgoing_dir(endpoints->client.TakeChannel())
                      .set_bus_info(std::move(bus_info)));
  if (status != ZX_OK) {
    return status;
  }
  // Hold a reference while devmgr has a pointer to this object.
  AddRef();

  return ZX_OK;
}

zx_status_t UsbDevice::Reinitialize() {
  fbl::AutoLock lock(&state_lock_);

  if (!resetting_) {
    zxlogf(ERROR, "%s: resetting_ not set", __func__);
    return ZX_ERR_BAD_STATE;
  }
  resetting_ = false;

  auto* descriptor = reinterpret_cast<usb_configuration_descriptor_t*>(
      config_descs_[current_config_index_].data());
  auto status = UsbControlOut(kStandardDeviceOut,
                              fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration),
                              descriptor->b_configuration_value, 0, ZX_TIME_INFINITE, nullptr, 0);
  if (status != ZX_OK) {
    zxlogf(ERROR, "could not restore configuration to %u, got err: %d",
           descriptor->b_configuration_value, status);
    return status;
  }

  // TODO(jocelyndang): should we notify the interfaces that the device has been reset?
  // TODO(jocelyndang): we should re-enable endpoints and restore alternate settings.
  return ZX_OK;
}

zx_status_t UsbDevice::GetDescriptor(uint16_t type, uint8_t index, uint16_t language, void* data,
                                     size_t length, size_t* out_actual) {
  return UsbControlIn(kStandardDeviceIn,
                      fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor),
                      usb_descriptor_w_value(type, index), language, ZX_TIME_INFINITE,
                      reinterpret_cast<uint8_t*>(data), length, out_actual);
}

}  // namespace usb_bus
