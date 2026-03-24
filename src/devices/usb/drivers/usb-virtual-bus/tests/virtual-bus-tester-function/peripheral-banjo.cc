// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-virtual-bus/tests/virtual-bus-tester-function/peripheral-banjo.h"

#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/component/cpp/driver_export.h>

#include <usb/request-cpp.h>

namespace virtualbus {

zx::result<> BanjoTestFunction::SetFunctionInterface(bool connect) {
  connect ? function_.SetInterface(this, &usb_function_interface_protocol_ops_)
          : function_.SetInterface(nullptr, nullptr);
  return zx::ok();
}

zx::result<> BanjoTestFunction::Start() {
  zx::result<ddk::UsbFunctionProtocolClient> function =
      compat::ConnectBanjo<ddk::UsbFunctionProtocolClient>(incoming());
  if (function.is_error()) {
    FDF_LOG(ERROR, "Failed to connect function %s", function.status_string());
    return function.take_error();
  }
  function_ = *function;

  zx_status_t status = function_.AllocInterface(&descriptor_.interface.b_interface_number);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "usb_function_alloc_interface failed");
    return zx::error(status);
  }
  status = function_.AllocEp(USB_DIR_OUT, &descriptor_.bulk_out.b_endpoint_address);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "usb_function_alloc_ep failed");
    return zx::error(status);
  }
  status = function_.AllocEp(USB_DIR_IN, &descriptor_.bulk_in.b_endpoint_address);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "usb_function_alloc_ep failed");
    return zx::error(status);
  }

  parent_req_size_ = function_.GetRequestSize();

  auto result = TestFunction::Start();
  if (result.is_error()) {
    return result.take_error();
  }

  zx::result<> connect_result = SetFunctionInterface(true);
  if (connect_result.is_error()) {
    FDF_LOG(ERROR, "Failed to set function interface %s", connect_result.status_string());
    return connect_result.take_error();
  }

  return zx::ok();
}

size_t BanjoTestFunction::UsbFunctionInterfaceGetDescriptorsSize() { return sizeof(descriptor_); }

void BanjoTestFunction::UsbFunctionInterfaceGetDescriptors(uint8_t* out_descriptors_buffer,
                                                           size_t descriptors_size,
                                                           size_t* out_descriptors_actual) {
  memcpy(out_descriptors_buffer, &descriptor_, std::min(descriptors_size, sizeof(descriptor_)));
  *out_descriptors_actual = sizeof(descriptor_);
}

zx_status_t BanjoTestFunction::UsbFunctionInterfaceControl(
    const usb_setup_t* setup, const uint8_t* write_buffer, size_t write_size,
    uint8_t* out_read_buffer, size_t read_size, size_t* out_read_actual) {
  fuchsia_hardware_usb_descriptor::UsbSetup fidl_setup;
  fidl_setup.bm_request_type(setup->bm_request_type);
  fidl_setup.b_request(setup->b_request);
  fidl_setup.w_value(setup->w_value);
  fidl_setup.w_index(setup->w_index);
  fidl_setup.w_length(setup->w_length);

  std::vector<uint8_t> write_data;
  if (write_buffer && write_size > 0) {
    write_data.assign(write_buffer, write_buffer + write_size);
  }

  auto result = DoControl(fidl_setup, std::move(write_data));
  if (result.is_error()) {
    return result.error_value();
  }

  if (out_read_actual) {
    *out_read_actual = 0;
  }

  if (!result->empty()) {
    size_t size = std::min(read_size, result->size());
    memcpy(out_read_buffer, result->data(), size);
    if (out_read_actual) {
      *out_read_actual = size;
    }
  }

  return ZX_OK;
}

zx_status_t BanjoTestFunction::UsbFunctionInterfaceSetConfigured(bool configured,
                                                                 usb_speed_t speed) {
  if (configured) {
    if (configured_) {
      return ZX_OK;
    }
    configured_ = true;
    usb_endpoint_descriptor_t ep_out_desc;
    ep_out_desc.b_length = descriptor_.bulk_out.b_length;
    ep_out_desc.b_descriptor_type = descriptor_.bulk_out.b_descriptor_type;
    ep_out_desc.b_endpoint_address = descriptor_.bulk_out.b_endpoint_address;
    ep_out_desc.bm_attributes = descriptor_.bulk_out.bm_attributes;
    ep_out_desc.w_max_packet_size = descriptor_.bulk_out.w_max_packet_size;
    ep_out_desc.b_interval = descriptor_.bulk_out.b_interval;
    function_.ConfigEp(&ep_out_desc, nullptr);

    usb_endpoint_descriptor_t ep_in_desc;
    ep_in_desc.b_length = descriptor_.bulk_in.b_length;
    ep_in_desc.b_descriptor_type = descriptor_.bulk_in.b_descriptor_type;
    ep_in_desc.b_endpoint_address = descriptor_.bulk_in.b_endpoint_address;
    ep_in_desc.bm_attributes = descriptor_.bulk_in.bm_attributes;
    ep_in_desc.w_max_packet_size = descriptor_.bulk_in.w_max_packet_size;
    ep_in_desc.b_interval = descriptor_.bulk_in.b_interval;
    function_.ConfigEp(&ep_in_desc, nullptr);
  } else {
    configured_ = false;
  }
  return ZX_OK;
}

zx_status_t BanjoTestFunction::UsbFunctionInterfaceSetInterface(uint8_t interface,
                                                                uint8_t alt_setting) {
  return ZX_OK;
}

void BanjoTestFunction::QueueOut() {
  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* req) {
            usb::Request<> request(req, static_cast<BanjoTestFunction*>(ctx)->parent_req_size_);
            std::vector<uint8_t> data(request.request()->response.actual);
            size_t actual = request.CopyFrom(data.data(), data.size(), 0);
            if (actual != data.size()) {
              static_cast<BanjoTestFunction*>(ctx)->expect_out_->Reply(zx::error(ZX_ERR_BAD_STATE));
              static_cast<BanjoTestFunction*>(ctx)->expect_out_.reset();
              return;
            }

            static_cast<BanjoTestFunction*>(ctx)->expect_out_->Reply(zx::ok(std::move(data)));
            static_cast<BanjoTestFunction*>(ctx)->expect_out_.reset();
          },
      .ctx = this,
  };
  std::optional<usb::Request<>> data_out_req;
  usb::Request<>::Alloc(&data_out_req, kMaxPacketSize, descriptor_.bulk_out.b_endpoint_address,
                        parent_req_size_);
  function_.RequestQueue(data_out_req->take(), &complete);
}

void BanjoTestFunction::QueueIn(std::vector<uint8_t> data) {
  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* req) {
            usb::Request<> request(req, static_cast<BanjoTestFunction*>(ctx)->parent_req_size_);
            static_cast<BanjoTestFunction*>(ctx)->expect_in_->Reply(
                zx::ok(request.request()->response.actual));
            static_cast<BanjoTestFunction*>(ctx)->expect_in_.reset();
          },
      .ctx = this,
  };

  std::optional<usb::Request<>> data_in_req;
  usb::Request<>::Alloc(&data_in_req, data.size(), descriptor_.bulk_in.b_endpoint_address,
                        parent_req_size_);
  size_t actual = data_in_req->CopyTo(data.data(), data.size(), 0);
  if (actual != data.size()) {
    expect_in_->Reply(zx::error(ZX_ERR_BAD_STATE));
    expect_in_.reset();
    return;
  }
  function_.RequestQueue(data_in_req->take(), &complete);
}

}  // namespace virtualbus

FUCHSIA_DRIVER_EXPORT(virtualbus::BanjoTestFunction);
