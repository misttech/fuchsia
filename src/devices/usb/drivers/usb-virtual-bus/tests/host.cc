// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-virtual-bus/tests/host.h"

#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/component/cpp/driver_export.h>

#include <usb/request-cpp.h>

namespace virtualbus {

void Device::PrepareStop(fdf::PrepareStopCompleter completer) {
  usb_client_.CancelAll(bulk_out_addr_);
  completer(zx::ok());
}

void Device::RunShortPacketTest(RunShortPacketTestCompleter::Sync& completer) {
  if (completer_.has_value()) {
    completer.Close(ZX_ERR_BAD_STATE);
    return;
  }
  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* request) {
            usb::Request<> req(request, static_cast<Device*>(ctx)->parent_req_size_);
            constexpr auto kExpected = 20;
            static_cast<Device*>(ctx)->completer_->Reply(req.request()->response.actual ==
                                                         kExpected);
          },
      .ctx = this,
  };
  completer_ = completer.ToAsync();
  std::optional<usb::Request<>> req;
  constexpr auto kUsbBufSize = 100;
  usb::Request<>::Alloc(&req, kUsbBufSize, bulk_out_addr_, parent_req_size_);
  usb_client_.RequestQueue(req->take(), &complete);
}

zx::result<> Device::Start() {
  zx::result<ddk::UsbProtocolClient> usb = compat::ConnectBanjo<ddk::UsbProtocolClient>(incoming());
  if (usb.is_error()) {
    FDF_LOG(ERROR, "Failed to connect function %s", usb.status_string());
    return usb.take_error();
  }
  usb_client_ = *usb;

  // Find our endpoints.
  std::optional<usb::InterfaceList> usb_interface_list;
  zx_status_t status = usb::InterfaceList::Create(usb_client_, true, &usb_interface_list);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  for (auto& interface : *usb_interface_list) {
    for (auto ep_itr : interface.GetEndpointList()) {
      if (usb_ep_direction(ep_itr.descriptor()) == USB_ENDPOINT_OUT) {
        if (usb_ep_type(ep_itr.descriptor()) == USB_ENDPOINT_BULK) {
          bulk_out_addr_ = ep_itr.descriptor()->b_endpoint_address;
        }
      }
    }
  }
  if (!bulk_out_addr_) {
    FDF_LOG(ERROR, "could not find bulk out endpoint");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  parent_req_size_ = usb_client_.GetRequestSize();

  zx::result child = AddOwnedChild(kName);
  if (child.is_error()) {
    FDF_LOG(ERROR, "Failed to add child %s", child.status_string());
    return child.take_error();
  }
  child_ = std::move(*child);

  auto serve_result = outgoing()->AddService<fuchsia_hardware_usb_virtualbustest::Service>(
      fuchsia_hardware_usb_virtualbustest::Service::InstanceHandler({
          .device = bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                            fidl::kIgnoreBindingClosure),
      }));
  if (serve_result.is_error()) {
    zxlogf(ERROR, "Failed to add Device service %s", serve_result.status_string());
    return serve_result.take_error();
  }

  return zx::ok();
}

}  // namespace virtualbus

FUCHSIA_DRIVER_EXPORT(virtualbus::Device);
