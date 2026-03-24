// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_VIRTUAL_BUS_TESTER_FUNCTION_PERIPHERAL_BANJO_H_
#define SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_VIRTUAL_BUS_TESTER_FUNCTION_PERIPHERAL_BANJO_H_

#include <fuchsia/hardware/usb/function/cpp/banjo.h>

#include "src/devices/usb/drivers/usb-virtual-bus/tests/virtual-bus-tester-function/peripheral.h"

namespace virtualbus {

class BanjoTestFunction : public TestFunction,
                          public ddk::UsbFunctionInterfaceProtocol<BanjoTestFunction> {
 public:
  BanjoTestFunction(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher dispatcher)
      : TestFunction(std::move(start_args), std::move(dispatcher)) {}

  zx::result<> Start() override;

  // UsbFunctionInterfaceProtocol
  size_t UsbFunctionInterfaceGetDescriptorsSize();
  void UsbFunctionInterfaceGetDescriptors(uint8_t* out_descriptors_buffer, size_t descriptors_size,
                                          size_t* out_descriptors_actual);
  zx_status_t UsbFunctionInterfaceControl(const usb_setup_t* setup, const uint8_t* write_buffer,
                                          size_t write_size, uint8_t* out_read_buffer,
                                          size_t read_size, size_t* out_read_actual);
  zx_status_t UsbFunctionInterfaceSetConfigured(bool configured, usb_speed_t speed);
  zx_status_t UsbFunctionInterfaceSetInterface(uint8_t interface, uint8_t alt_setting);

 private:
  zx::result<> SetFunctionInterface(bool connect) override;
  void QueueOut() override;
  void QueueIn(std::vector<uint8_t> data) override;

  ddk::UsbFunctionProtocolClient function_;
  size_t parent_req_size_ = 0;
};

}  // namespace virtualbus

#endif  // SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_VIRTUAL_BUS_TESTER_FUNCTION_PERIPHERAL_BANJO_H_
