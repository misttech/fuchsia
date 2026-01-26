// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/component/cpp/driver_export.h>

// [START include]
#include "examples/drivers/config/cpp/example_config_driver_config.h"
// [END include]

namespace {
class Driver : public fdf::DriverBase {
 public:
  Driver(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : fdf::DriverBase("config-driver", std::move(start_args), std::move(driver_dispatcher)) {}

  zx::result<> Start() override {
    // [START use]
    auto config = take_config<example_config_driver_config::Config>();
    fdf::info("My config value is: {}", config.suspend_enabled());
    // [END use]
    return zx::ok();
  }
};
}  // namespace

FUCHSIA_DRIVER_EXPORT(Driver);
