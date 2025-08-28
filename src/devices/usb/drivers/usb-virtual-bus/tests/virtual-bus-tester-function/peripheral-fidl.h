// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_VIRTUAL_BUS_TESTER_FUNCTION_PERIPHERAL_FIDL_H_
#define SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_VIRTUAL_BUS_TESTER_FUNCTION_PERIPHERAL_FIDL_H_

#include <usb-endpoint/usb-endpoint-client.h>

#include "src/devices/usb/drivers/usb-virtual-bus/tests/virtual-bus-tester-function/peripheral.h"

namespace virtualbus {

class FidlTestFunction : public TestFunction {
 public:
  FidlTestFunction(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher dispatcher)
      : TestFunction(std::move(start_args), std::move(dispatcher)) {}

  zx::result<> Start() override;

 private:
  void QueueOut() override;
  void QueueIn(std::vector<uint8_t> data) override;

  void OutComplete(fuchsia_hardware_usb_endpoint::Completion completion);
  void InComplete(fuchsia_hardware_usb_endpoint::Completion completion);

  fdf::SynchronizedDispatcher dispatcher_;

  usb::EndpointClient<FidlTestFunction> bulk_out_ep_{usb::EndpointType::BULK, this,
                                                     std::mem_fn(&FidlTestFunction::OutComplete)};
  usb::EndpointClient<FidlTestFunction> bulk_in_ep_{usb::EndpointType::BULK, this,
                                                    std::mem_fn(&FidlTestFunction::InComplete)};
};

}  // namespace virtualbus

#endif  // SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_VIRTUAL_BUS_TESTER_FUNCTION_PERIPHERAL_FIDL_H_
