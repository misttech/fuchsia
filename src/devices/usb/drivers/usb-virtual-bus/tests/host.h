// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_HOST_H_
#define SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_HOST_H_

#include <fidl/fuchsia.hardware.usb.virtualbustest/cpp/fidl.h>
#include <fuchsia/hardware/usb/cpp/banjo.h>
#include <lib/driver/component/cpp/driver_base.h>

namespace virtualbus {

class Device : public fdf::DriverBase,
               public fidl::Server<fuchsia_hardware_usb_virtualbustest::BusTest> {
 private:
  static constexpr std::string_view kName = "virtual-bus-test";

 public:
  Device(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher dispatcher)
      : fdf::DriverBase(kName, std::move(start_args), std::move(dispatcher)) {}

  zx::result<> Start() override;
  void PrepareStop(fdf::PrepareStopCompleter completer) override;

 private:
  void Control(ControlRequest& request, ControlCompleter::Sync& completer) override;
  void Out(OutRequest& request, OutCompleter::Sync& completer) override;
  void In(InRequest& request, InCompleter::Sync& completer) override;
  fdf::OwnedChildNode child_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb_virtualbustest::BusTest> bindings_;

  ddk::UsbProtocolClient usb_client_ = {};

  std::optional<OutCompleter::Async> out_completer_;
  std::optional<InCompleter::Async> in_completer_;

  size_t parent_req_size_ = 0;
  uint8_t bulk_out_addr_ = 0;
  uint8_t bulk_in_addr_ = 0;
};

}  // namespace virtualbus

#endif  // SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_HOST_H_
