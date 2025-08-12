// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-virtual-bus/tests/peripheral.h"

#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/component/cpp/driver_export.h>

#include <usb/request-cpp.h>

namespace virtualbus {

void TestFunction::ExpectControl(ExpectControlRequest& request,
                                 ExpectControlCompleter::Sync& completer) {
  if (expect_control_.has_value()) {
    completer.Reply(zx::error(ZX_ERR_BAD_STATE));
    return;
  }

  expect_control_data_ = std::move(request.in_data());
  expect_control_ = completer.ToAsync();
}

void TestFunction::ExpectOut(ExpectOutCompleter::Sync& completer) {
  if (expect_out_.has_value()) {
    completer.Reply(zx::error(ZX_ERR_BAD_STATE));
    return;
  }
  expect_out_ = completer.ToAsync();

  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* req) {
            usb::Request<> request(req, static_cast<TestFunction*>(ctx)->parent_req_size_);
            std::vector<uint8_t> data(request.request()->response.actual);
            size_t actual = request.CopyFrom(data.data(), data.size(), 0);
            if (actual != data.size()) {
              static_cast<TestFunction*>(ctx)->expect_out_->Reply(zx::error(ZX_ERR_BAD_STATE));
              static_cast<TestFunction*>(ctx)->expect_out_.reset();
              return;
            }

            static_cast<TestFunction*>(ctx)->expect_out_->Reply(zx::ok(std::move(data)));
            static_cast<TestFunction*>(ctx)->expect_out_.reset();
          },
      .ctx = this,
  };
  std::optional<usb::Request<>> data_out_req;
  usb::Request<>::Alloc(&data_out_req, kMaxPacketSize, descriptor_.bulk_out.b_endpoint_address,
                        parent_req_size_);
  function_.RequestQueue(data_out_req->take(), &complete);
}

void TestFunction::ExpectIn(ExpectInRequest& request, ExpectInCompleter::Sync& completer) {
  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* req) {
            usb::Request<> request(req, static_cast<TestFunction*>(ctx)->parent_req_size_);
            static_cast<TestFunction*>(ctx)->expect_in_->Reply(
                zx::ok(request.request()->response.actual));
            static_cast<TestFunction*>(ctx)->expect_in_.reset();
          },
      .ctx = this,
  };

  std::optional<usb::Request<>> data_in_req;
  usb::Request<>::Alloc(&data_in_req, request.data().size(), descriptor_.bulk_in.b_endpoint_address,
                        parent_req_size_);
  size_t actual = data_in_req->CopyTo(request.data().data(), request.data().size(), 0);
  if (actual != request.data().size()) {
    completer.Reply(zx::error(ZX_ERR_BAD_STATE));
    return;
  }
  expect_in_ = completer.ToAsync();
  function_.RequestQueue(data_in_req->take(), &complete);
}

size_t TestFunction::UsbFunctionInterfaceGetDescriptorsSize() { return sizeof(descriptor_); }

void TestFunction::UsbFunctionInterfaceGetDescriptors(uint8_t* out_descriptors_buffer,
                                                      size_t descriptors_size,
                                                      size_t* out_descriptors_actual) {
  memcpy(out_descriptors_buffer, &descriptor_, std::min(descriptors_size, sizeof(descriptor_)));
  *out_descriptors_actual = sizeof(descriptor_);
}

zx_status_t TestFunction::UsbFunctionInterfaceControl(const usb_setup_t* setup,
                                                      const uint8_t* write_buffer,
                                                      size_t write_size, uint8_t* out_read_buffer,
                                                      size_t read_size, size_t* out_read_actual) {
  if (out_read_actual) {
    *out_read_actual = 0;
  }

  if (!expect_control_) {
    return ZX_OK;
  }

  if (setup->b_request != 0xFF) {
    expect_control_->Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
    expect_control_.reset();
    return ZX_ERR_NOT_SUPPORTED;
  }

  if (setup->w_value != 0xA) {
    expect_control_->Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
    expect_control_.reset();
    return ZX_ERR_NOT_SUPPORTED;
  }

  if (setup->bm_request_type == (USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_INTERFACE)) {
    memcpy(out_read_buffer, expect_control_data_.data(), expect_control_data_.size());
    *out_read_actual = expect_control_data_.size();
    expect_control_->Reply(zx::ok(std::vector<uint8_t>{}));
    expect_control_.reset();
    return ZX_OK;
  }
  if (setup->bm_request_type == (USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_INTERFACE)) {
    expect_control_->Reply(zx::ok(std::vector<uint8_t>{write_buffer, write_buffer + write_size}));
    expect_control_.reset();
    return ZX_OK;
  }

  expect_control_->Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
  expect_control_.reset();
  return ZX_ERR_NOT_SUPPORTED;
}

zx_status_t TestFunction::UsbFunctionInterfaceSetConfigured(bool configured, usb_speed_t speed) {
  if (configured) {
    if (configured_) {
      return ZX_OK;
    }
    configured_ = true;
    function_.ConfigEp(&descriptor_.bulk_out, nullptr);
    function_.ConfigEp(&descriptor_.bulk_in, nullptr);
  } else {
    configured_ = false;
  }
  return ZX_OK;
}

zx_status_t TestFunction::UsbFunctionInterfaceSetInterface(uint8_t interface, uint8_t alt_setting) {
  return ZX_OK;
}

zx::result<> TestFunction::Start() {
  zx::result<ddk::UsbFunctionProtocolClient> function =
      compat::ConnectBanjo<ddk::UsbFunctionProtocolClient>(incoming());
  if (function.is_error()) {
    FDF_LOG(ERROR, "Failed to connect function %s", function.status_string());
    return function.take_error();
  }
  function_ = *function;

  parent_req_size_ = function_.GetRequestSize();

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
    zxlogf(ERROR, "usb_function_alloc_ep failed");
    return zx::error(status);
  }

  zx::result child = AddOwnedChild(kName);
  if (child.is_error()) {
    FDF_LOG(ERROR, "Failed to add child %s", child.status_string());
    return child.take_error();
  }
  child_ = std::move(*child);

  auto serve_result =
      outgoing()->AddService<fuchsia_hardware_usb_virtualbustest::ExpectBusTestService>(
          fuchsia_hardware_usb_virtualbustest::ExpectBusTestService::InstanceHandler({
              .device =
                  bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                          fidl::kIgnoreBindingClosure),
          }));
  if (serve_result.is_error()) {
    zxlogf(ERROR, "Failed to add Device service %s", serve_result.status_string());
    return serve_result.take_error();
  }

  function_.SetInterface(this, &usb_function_interface_protocol_ops_);

  return zx::ok();
}

}  // namespace virtualbus

FUCHSIA_DRIVER_EXPORT(virtualbus::TestFunction);
