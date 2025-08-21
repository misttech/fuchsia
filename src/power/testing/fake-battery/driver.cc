// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "driver.h"

#include <fidl/fuchsia.power.battery/cpp/natural_types.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/devfs/cpp/connector.h>
#include <lib/fit/function.h>

#include <utility>

namespace fake_battery {

Driver::Driver(fdf::DriverStartArgs start_args,
               fdf::UnownedSynchronizedDispatcher driver_dispatcher)
    : DriverBase("fake-battery", std::move(start_args), std::move(driver_dispatcher)),
      protocol_server_battery_(dispatcher()) {}

zx::result<> Driver::Start() {
  protocol_server_battery_.Init(outgoing());
  return zx::ok();
}

}  // namespace fake_battery

FUCHSIA_DRIVER_EXPORT(fake_battery::Driver);
