// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_HOST_BANJO_H_
#define SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_HOST_BANJO_H_

#include "src/devices/usb/drivers/usb-virtual-bus/tests/host.h"

namespace virtualbus {

class BanjoDevice : public Device {
 public:
  BanjoDevice(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher dispatcher)
      : Device(std::move(start_args), std::move(dispatcher)) {}

  zx::result<> Start() override;
  void PrepareStop(fdf::PrepareStopCompleter completer) override;

 private:
  void QueueOut(std::vector<uint8_t> data) override;
  void QueueIn(size_t size) override;

  size_t parent_req_size_ = 0;
};

}  // namespace virtualbus

#endif  // SRC_DEVICES_USB_DRIVERS_USB_VIRTUAL_BUS_TESTS_HOST_BANJO_H_
