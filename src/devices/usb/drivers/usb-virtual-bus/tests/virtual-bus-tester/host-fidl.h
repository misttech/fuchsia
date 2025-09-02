// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_VIRTUAL_BUS_TESTER_HOST_FIDL_H_
#define SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_VIRTUAL_BUS_TESTER_HOST_FIDL_H_

#include <usb-endpoint/usb-endpoint-client.h>

#include "src/devices/usb/drivers/usb-virtual-bus/tests/virtual-bus-tester/host.h"
#include "src/devices/usb/drivers/usb-virtual-bus/tests/virtual-bus-tester/virtual_bus_tester_fidl_config.h"

namespace virtualbus {

class FidlDevice : public Device {
 public:
  FidlDevice(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher dispatcher)
      : Device(std::move(start_args), std::move(dispatcher)),
        config_(take_config<virtual_bus_tester_fidl_config::Config>()) {}

  zx::result<> Start() override;
  void PrepareStop(fdf::PrepareStopCompleter completer) override;

 private:
  void QueueOut(std::vector<uint8_t> data) override;
  void QueueIn(size_t size) override;

  void OutComplete(fuchsia_hardware_usb_endpoint::Completion completion);
  void InComplete(fuchsia_hardware_usb_endpoint::Completion completion);

  const virtual_bus_tester_fidl_config::Config config_;
  fdf::SynchronizedDispatcher dispatcher_;

  usb::EndpointClient<FidlDevice> bulk_out_ep_{usb::EndpointType::BULK, this,
                                               std::mem_fn(&FidlDevice::OutComplete)};
  usb::EndpointClient<FidlDevice> bulk_in_ep_{usb::EndpointType::BULK, this,
                                              std::mem_fn(&FidlDevice::InComplete)};
};

}  // namespace virtualbus

#endif  // SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_VIRTUAL_BUS_TESTER_HOST_FIDL_H_
