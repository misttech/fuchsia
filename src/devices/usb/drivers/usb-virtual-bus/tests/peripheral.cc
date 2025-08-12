// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-virtual-bus/tests/peripheral.h"

#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/component/cpp/driver_export.h>

#include <usb/request-cpp.h>

namespace virtualbus {

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
  return ZX_OK;
}

zx_status_t TestFunction::UsbFunctionInterfaceSetConfigured(bool configured, usb_speed_t speed) {
  if (configured) {
    if (configured_) {
      return ZX_OK;
    }
    configured_ = true;
    function_.ConfigEp(&descriptor_.bulk_out, nullptr);

    // queue first read on OUT endpoint
    usb_request_complete_callback_t complete = {
        .callback =
            [](void* ctx, usb_request_t* req) {
              usb::Request<> request(req, static_cast<TestFunction*>(ctx)->parent_req_size_);
            },
        .ctx = this,
    };
    std::optional<usb::Request<>> data_out_req;
    usb::Request<>::Alloc(&data_out_req, kMaxPacketSize, descriptor_.bulk_out.b_endpoint_address,
                          parent_req_size_);
    function_.RequestQueue(data_out_req->take(), &complete);
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

  zx::result child = AddOwnedChild(kName);
  if (child.is_error()) {
    FDF_LOG(ERROR, "Failed to add child %s", child.status_string());
    return child.take_error();
  }
  child_ = std::move(*child);

  function_.SetInterface(this, &usb_function_interface_protocol_ops_);

  return zx::ok();
}

}  // namespace virtualbus

FUCHSIA_DRIVER_EXPORT(virtualbus::TestFunction);
