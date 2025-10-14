// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.ax88179/cpp/wire.h>
#include <fuchsia/hardware/ethernet/cpp/banjo.h>
#include <fuchsia/hardware/usb/function/cpp/banjo.h>
#include <lib/driver/compat/cpp/banjo_client.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/devfs/cpp/connector.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <lib/zx/result.h>

#include <algorithm>
#include <memory>
#include <optional>
#include <vector>

#include <fbl/auto_lock.h>
#include <fbl/condition_variable.h>
#include <fbl/mutex.h>
#include <sdk/lib/driver/component/cpp/driver_base.h>
#include <usb/cdc.h>
#include <usb/request-cpp.h>
#include <usb/usb-request.h>
#include <usb/usb.h>

#include "asix-88179-regs.h"

namespace fake_usb_ax88179_function {

constexpr int BULK_MAX_PACKET = 512;
constexpr size_t INTR_MAX_PACKET = 64;

// Acts as a fake USB device for asix-88179 tests. Currently only partially
// implemented for initialization order regression test.

class FakeUsbAx88179Function;

class FakeUsbAx88179Function : public fdf::DriverBase,
                               public fidl::WireServer<fuchsia_hardware_ax88179::Hooks>,
                               public ddk::UsbFunctionInterfaceProtocol<FakeUsbAx88179Function> {
 public:
  static constexpr std::string kDriverName = "FakeUsbAx88179Function";

  FakeUsbAx88179Function(fdf::DriverStartArgs start_args,
                         fdf::UnownedSynchronizedDispatcher dispatcher)
      : DriverBase(kDriverName, std::move(start_args), std::move(dispatcher)),
        connector_{fit::bind_member<&FakeUsbAx88179Function::DevfsConnect>(this)} {}

  zx::result<> Start() override;

  // UsbFunctionInterface:
  size_t UsbFunctionInterfaceGetDescriptorsSize();
  void UsbFunctionInterfaceGetDescriptors(uint8_t* out_descriptors_buffer, size_t descriptors_size,
                                          size_t* out_descriptors_actual);
  zx_status_t UsbFunctionInterfaceControl(const usb_setup_t* setup, const uint8_t* write_buffer,
                                          size_t write_size, uint8_t* out_read_buffer,
                                          size_t read_size, size_t* out_read_actual);
  zx_status_t UsbFunctionInterfaceSetConfigured(bool configured, usb_speed_t speed);
  zx_status_t UsbFunctionInterfaceSetInterface(uint8_t interface, uint8_t alt_setting);

  // Hooks:
  void SetOnline(SetOnlineRequestView request, SetOnlineCompleter::Sync& completer) override;

 private:
  void DevfsConnect(fidl::ServerEnd<fuchsia_hardware_ax88179::Hooks> req);

  void RequestQueue(usb_request_t* req, const usb_request_complete_callback_t* completion);

  ddk::UsbFunctionProtocolClient function_;

  struct {
    usb_interface_descriptor_t interface;
    usb_endpoint_descriptor_t bulk_in;
    usb_endpoint_descriptor_t bulk_out;
    usb_endpoint_descriptor_t intr_ep;
  } __PACKED descriptor_;

  size_t descriptor_size_ = 0;
  size_t parent_req_size_ = 0;
  uint8_t intr_addr_ = 0;

  std::optional<usb::Request<>> intr_req_ TA_GUARDED(mtx_);

  fbl::Mutex mtx_;

  bool configured_ = false;

  fidl::ServerBindingGroup<fuchsia_hardware_ax88179::Hooks> bindings_;
  fidl::WireSyncClient<fuchsia_driver_framework::NodeController> child_;
  driver_devfs::Connector<fuchsia_hardware_ax88179::Hooks> connector_;
};

void FakeUsbAx88179Function::SetOnline(SetOnlineRequestView request,
                                       SetOnlineCompleter::Sync& completer) {
  fbl::AutoLock lock(&mtx_);

  constexpr size_t kInterruptRequestSize = 8;
  uint8_t status[kInterruptRequestSize];
  memset(&status, 0, sizeof(status));
  status[2] = request->online;

  usb_request_complete_callback_t complete = {
      .callback = [](void* ctx, usb_request_t* req) {},
      .ctx = nullptr,
  };

  intr_req_->request()->header.length = sizeof(status);
  intr_req_->request()->header.ep_address = intr_addr_;
  size_t copy_result = intr_req_->CopyTo(status, sizeof(status), 0);
  ZX_ASSERT(copy_result == sizeof(status));
  RequestQueue(intr_req_->request(), &complete);

  completer.Reply(ZX_OK);
}

zx::result<> FakeUsbAx88179Function::Start() {
  fbl::AutoLock lock(&mtx_);

  descriptor_size_ = sizeof(descriptor_);
  descriptor_.interface = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = USB_DT_INTERFACE,
      .b_interface_number = 0,
      .b_alternate_setting = 0,
      .b_num_endpoints = 3,
      .b_interface_class = USB_CLASS_COMM,
      .b_interface_sub_class = USB_CDC_SUBCLASS_ETHERNET,
      .b_interface_protocol = 1,
      .i_interface = 0,
  };
  descriptor_.bulk_in = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = USB_DT_ENDPOINT,
      .b_endpoint_address = USB_ENDPOINT_IN,  // set later
      .bm_attributes = USB_ENDPOINT_BULK,
      .w_max_packet_size = htole16(BULK_MAX_PACKET),
      .b_interval = 0,
  };
  descriptor_.bulk_out = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = USB_DT_ENDPOINT,
      .b_endpoint_address = USB_ENDPOINT_OUT,  // set later
      .bm_attributes = USB_ENDPOINT_BULK,
      .w_max_packet_size = htole16(BULK_MAX_PACKET),
      .b_interval = 0,
  };
  descriptor_.intr_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = USB_DT_ENDPOINT,
      .b_endpoint_address = 0,  // set later
      .bm_attributes = USB_ENDPOINT_INTERRUPT,
      .w_max_packet_size = htole16(INTR_MAX_PACKET),
      .b_interval = 8,
  };

  zx::result function = compat::ConnectBanjo<ddk::UsbFunctionProtocolClient>(incoming());
  if (function.is_error()) {
    fdf::error("Could not connect to UsbFunctionProtocol: {}", function);
    return function.take_error();
  }
  function_ = *function;

  parent_req_size_ = function_.GetRequestSize();

  zx_status_t status = function_.AllocInterface(&descriptor_.interface.b_interface_number);
  if (status != ZX_OK) {
    fdf::error("FakeUsbAx88179Function: usb_function_alloc_interface failed");
    return zx::error(status);
  }
  status = function_.AllocEp(USB_DIR_IN, &descriptor_.bulk_in.b_endpoint_address);
  if (status != ZX_OK) {
    fdf::error("FakeUsbAx88179Function: usb_function_alloc_ep failed");
    return zx::error(status);
  }
  status = function_.AllocEp(USB_DIR_OUT, &descriptor_.bulk_out.b_endpoint_address);
  if (status != ZX_OK) {
    fdf::error("FakeUsbAx88179Function: usb_function_alloc_ep failed");
    return zx::error(status);
  }
  status = function_.AllocEp(USB_DIR_IN, &descriptor_.intr_ep.b_endpoint_address);
  if (status != ZX_OK) {
    fdf::error("FakeUsbAx88179Function: usb_function_alloc_ep failed");
    return zx::error(status);
  }

  intr_addr_ = descriptor_.intr_ep.b_endpoint_address;

  status = usb::Request<>::Alloc(&intr_req_, INTR_MAX_PACKET, intr_addr_, parent_req_size_);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  fuchsia_hardware_ax88179::Service::InstanceHandler handler({
      .hooks = bindings_.CreateHandler(this, dispatcher(), fidl::kIgnoreBindingClosure),
  });
  zx::result serve = outgoing()->AddService<fuchsia_hardware_ax88179::Service>(std::move(handler));
  if (serve.is_error()) {
    fdf::error("Failed to serve Hooks service: {}", serve);
    return serve.take_error();
  }

  zx::result connector = connector_.Bind(dispatcher());
  if (connector.is_error()) {
    fdf::error("connector_.Bind(): {}", connector);
    return connector.take_error();
  }

  fuchsia_driver_framework::DevfsAddArgs devfs_args{};
  devfs_args.connector(std::move(*connector));
  devfs_args.class_name("test-asix-function");

  std::vector<fuchsia_driver_framework::NodeProperty> props{};
  std::vector offers{fdf::MakeOffer2<fuchsia_hardware_ax88179::Service>()};

  zx::result child = AddChild(name(), devfs_args, props, offers);
  if (child.is_error()) {
    fdf::error("AddChild: {}", child);
    return child.take_error();
  }
  child_.Bind(std::move(*child));

  status = function_.SetInterface(this, &usb_function_interface_protocol_ops_);
  if (status != ZX_OK) {
    fdf::error("SetInterface(): {}", zx_status_get_string(status));
    return zx::error(status);
  }

  return zx::ok();
}

void FakeUsbAx88179Function::DevfsConnect(fidl::ServerEnd<fuchsia_hardware_ax88179::Hooks> req) {
  bindings_.AddBinding(dispatcher(), std::move(req), this, fidl::kIgnoreBindingClosure);
}

void FakeUsbAx88179Function::RequestQueue(usb_request_t* req,
                                          const usb_request_complete_callback_t* completion) {
  function_.RequestQueue(req, completion);
}

size_t FakeUsbAx88179Function::UsbFunctionInterfaceGetDescriptorsSize() { return descriptor_size_; }
void FakeUsbAx88179Function::UsbFunctionInterfaceGetDescriptors(uint8_t* out_descriptors_buffer,
                                                                size_t descriptors_size,
                                                                size_t* out_descriptors_actual) {
  memcpy(out_descriptors_buffer, &descriptor_, std::min(descriptors_size, descriptor_size_));
  *out_descriptors_actual = descriptor_size_;
}

zx_status_t FakeUsbAx88179Function::UsbFunctionInterfaceControl(
    const usb_setup_t* setup, const uint8_t* write_buffer, size_t write_size,
    uint8_t* out_read_buffer, size_t read_size, size_t* out_read_actual) {
  if (out_read_actual) {
    *out_read_actual = 0;
  }
  return ZX_OK;
}

zx_status_t FakeUsbAx88179Function::UsbFunctionInterfaceSetConfigured(bool configured,
                                                                      usb_speed_t speed) {
  fbl::AutoLock lock(&mtx_);
  zx_status_t status;

  if (configured) {
    if (configured_) {
      return ZX_OK;
    }
    configured_ = true;

    if ((status = function_.ConfigEp(&descriptor_.intr_ep, nullptr)) != ZX_OK) {
      fdf::error("usb-ax88179-function: usb_function_config_ep failed");
    }
  } else {
    configured_ = false;
  }
  return ZX_OK;
}

zx_status_t FakeUsbAx88179Function::UsbFunctionInterfaceSetInterface(uint8_t interface,
                                                                     uint8_t alt_setting) {
  return ZX_OK;
}

}  // namespace fake_usb_ax88179_function

FUCHSIA_DRIVER_EXPORT(fake_usb_ax88179_function::FakeUsbAx88179Function);
