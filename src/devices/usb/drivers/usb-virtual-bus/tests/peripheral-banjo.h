// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_PERIPHERAL_BANJO_H_
#define SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_PERIPHERAL_BANJO_H_

#include "src/devices/usb/drivers/usb-virtual-bus/tests/peripheral.h"

namespace virtualbus {

class BanjoTestFunction : public TestFunction {
 public:
  BanjoTestFunction(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher dispatcher)
      : TestFunction(std::move(start_args), std::move(dispatcher)) {}

  zx::result<> Start() override;

 private:
  void QueueOut() override;
  void QueueIn(std::vector<uint8_t> data) override;

  size_t parent_req_size_ = 0;
};

}  // namespace virtualbus

#endif  // SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_PERIPHERAL_BANJO_H_
