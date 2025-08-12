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
  usb_client_.CancelAll(bulk_in_addr_);
  completer(zx::ok());
}

void Device::Control(ControlRequest& request, ControlCompleter::Sync& completer) {
  if (request.is_in()) {
    static const size_t kMaxControlDataSize = 100;
    size_t actual;
    std::vector<uint8_t> data(kMaxControlDataSize);
    auto status =
        usb_client_.ControlIn(USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_INTERFACE, 0xFF, 0xA, 0,
                              ZX_TIME_INFINITE, data.data(), data.size(), &actual);
    if (status != ZX_OK) {
      completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
      return;
    }
    data.resize(actual);
    completer.Reply(zx::ok(std::move(data)));
    return;
  }

  auto status = usb_client_.ControlOut(USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_INTERFACE, 0xFF,
                                       0xA, 0, ZX_TIME_INFINITE, request.out_data().data(),
                                       request.out_data().size());
  if (status != ZX_OK) {
    completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
    return;
  }
  completer.Reply(zx::ok(std::vector<uint8_t>{}));
}

void Device::Out(OutRequest& request, OutCompleter::Sync& completer) {
  if (out_completer_.has_value()) {
    completer.Close(ZX_ERR_BAD_STATE);
    return;
  }

  std::optional<usb::Request<>> req;
  zx_status_t status =
      usb::Request<>::Alloc(&req, request.data().size(), bulk_out_addr_, parent_req_size_);
  if (status != ZX_OK) {
    completer.Reply(zx::error(status));
    return;
  }
  size_t copy_result = req->CopyTo(request.data().data(), request.data().size(), 0);
  ZX_ASSERT(copy_result == request.data().size());

  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* request) {
            usb::Request<> req(request, static_cast<Device*>(ctx)->parent_req_size_);
            static_cast<Device*>(ctx)->out_completer_->Reply(
                zx::ok(req.request()->response.actual));
          },
      .ctx = this,
  };
  out_completer_ = completer.ToAsync();
  usb_client_.RequestQueue(req->take(), &complete);
}

void Device::In(InRequest& request, InCompleter::Sync& completer) {
  if (in_completer_.has_value()) {
    completer.Close(ZX_ERR_BAD_STATE);
    return;
  }

  std::optional<usb::Request<>> req;
  zx_status_t status = usb::Request<>::Alloc(&req, request.size(), bulk_in_addr_, parent_req_size_);
  if (status != ZX_OK) {
    completer.Reply(zx::error(status));
    return;
  }

  usb_request_complete_callback_t complete = {
      .callback =
          [](void* ctx, usb_request_t* request) {
            usb::Request<> req(request, static_cast<Device*>(ctx)->parent_req_size_);

            std::vector<uint8_t> data(req.request()->response.actual);
            size_t actual = req.CopyFrom(data.data(), data.size(), 0);
            if (actual != req.request()->response.actual) {
              static_cast<Device*>(ctx)->in_completer_->Reply(zx::error(ZX_ERR_BAD_STATE));
              return;
            }

            static_cast<Device*>(ctx)->in_completer_->Reply(zx::ok(std::move(data)));
          },
      .ctx = this,
  };
  in_completer_ = completer.ToAsync();
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
      if (usb_ep_direction(ep_itr.descriptor()) == USB_ENDPOINT_IN) {
        if (usb_ep_type(ep_itr.descriptor()) == USB_ENDPOINT_BULK) {
          bulk_in_addr_ = ep_itr.descriptor()->b_endpoint_address;
        }
      }
    }
  }
  if (!bulk_out_addr_) {
    FDF_LOG(ERROR, "could not find bulk out endpoint");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  if (!bulk_in_addr_) {
    zxlogf(ERROR, "could not find bulk in endpoint");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  parent_req_size_ = usb_client_.GetRequestSize();

  zx::result child = AddOwnedChild(kName);
  if (child.is_error()) {
    FDF_LOG(ERROR, "Failed to add child %s", child.status_string());
    return child.take_error();
  }
  child_ = std::move(*child);

  auto serve_result = outgoing()->AddService<fuchsia_hardware_usb_virtualbustest::BusTestService>(
      fuchsia_hardware_usb_virtualbustest::BusTestService::InstanceHandler({
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
