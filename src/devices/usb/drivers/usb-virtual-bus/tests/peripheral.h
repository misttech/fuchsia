// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_PERIPHERAL_H_
#define SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_PERIPHERAL_H_

#include <fidl/fuchsia.hardware.usb.virtualbustest/cpp/fidl.h>
#include <fuchsia/hardware/usb/function/cpp/banjo.h>
#include <lib/driver/component/cpp/driver_base.h>

#include <queue>

#include <usb/descriptors.h>

namespace virtualbus {

class TestFunction : public fdf::DriverBase,
                     public ddk::UsbFunctionInterfaceProtocol<TestFunction>,
                     public fidl::Server<fuchsia_hardware_usb_virtualbustest::ExpectBusTest> {
 protected:
  static constexpr std::string_view kName = "virtual-bus-test-peripheral";
  static constexpr auto kMaxPacketSize = 20;

 public:
  TestFunction(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher dispatcher)
      : fdf::DriverBase(kName, std::move(start_args), std::move(dispatcher)) {}

  zx::result<> Start() override;

  size_t UsbFunctionInterfaceGetDescriptorsSize();
  void UsbFunctionInterfaceGetDescriptors(uint8_t* out_descriptors_buffer, size_t descriptors_size,
                                          size_t* out_descriptors_actual);
  zx_status_t UsbFunctionInterfaceControl(const usb_setup_t* setup, const uint8_t* write_buffer,
                                          size_t write_size, uint8_t* out_read_buffer,
                                          size_t read_size, size_t* out_read_actual);
  zx_status_t UsbFunctionInterfaceSetConfigured(bool configured, usb_speed_t speed);
  zx_status_t UsbFunctionInterfaceSetInterface(uint8_t interface, uint8_t alt_setting);

  ddk::UsbFunctionProtocolClient& function() { return function_; }

 protected:
  ddk::UsbFunctionProtocolClient function_;

  struct VirtualBusTestDescriptor {
    usb_interface_descriptor_t interface;
    usb_endpoint_descriptor_t bulk_out;
    usb_endpoint_descriptor_t bulk_in;
  } __PACKED descriptor_ = {
      .interface =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = USB_DT_INTERFACE,
              .b_interface_number = 0,
              .b_alternate_setting = 0,
              .b_num_endpoints = 1,
              .b_interface_class = 0xFF,
              .b_interface_sub_class = 0xFF,
              .b_interface_protocol = 0xFF,
              .i_interface = 0,
          },
      .bulk_out =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = USB_DT_ENDPOINT,
              .b_endpoint_address = USB_ENDPOINT_OUT,
              .bm_attributes = USB_ENDPOINT_BULK,
              .w_max_packet_size = 512,
              .b_interval = 0,
          },
      .bulk_in =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = USB_DT_ENDPOINT,
              .b_endpoint_address = USB_ENDPOINT_IN,
              .bm_attributes = USB_ENDPOINT_BULK,
              .w_max_packet_size = 512,
              .b_interval = 0,
          },
  };

  std::optional<ExpectOutCompleter::Async> expect_out_;
  std::optional<ExpectInCompleter::Async> expect_in_;

 private:
  void ExpectControl(ExpectControlRequest& request,
                     ExpectControlCompleter::Sync& completer) override;
  void ExpectOut(ExpectOutCompleter::Sync& completer) override;
  void ExpectIn(ExpectInRequest& request, ExpectInCompleter::Sync& completer) override;
  void Sync(SyncCompleter::Sync& completer) override { completer.Reply(); }

  virtual void QueueOut() = 0;
  virtual void QueueIn(std::vector<uint8_t> data) = 0;

  fdf::OwnedChildNode child_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb_virtualbustest::ExpectBusTest> bindings_;

  bool configured_ = false;

  std::vector<uint8_t> expect_control_data_;
  std::optional<ExpectControlCompleter::Async> expect_control_;
};
}  // namespace virtualbus

#endif  // SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_PERIPHERAL_H_
