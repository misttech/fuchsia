// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_POWER_TESTING_FAKE_BATTERY_DRIVER_H_
#define SRC_POWER_TESTING_FAKE_BATTERY_DRIVER_H_

#include <fidl/fuchsia.power.battery/cpp/natural_types.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/devfs/cpp/connector.h>
#include <zircon/types.h>

#include "battery_protocol_server.h"

namespace fake_battery {

class Driver : public fdf::DriverBase {
 public:
  Driver(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher);

  zx::result<> Start() override;

 private:
  BatteryProtocolServer protocol_server_battery_;
};

}  // namespace fake_battery

#endif  // SRC_POWER_TESTING_FAKE_BATTERY_DRIVER_H_
