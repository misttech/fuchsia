// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "power_driver.h"

#include <fidl/fuchsia.examples/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fidl/cpp/client.h>

#include "examples/drivers/power/cpp/component_config.h"
#include "lib/fidl/cpp/channel.h"

namespace power {
namespace fex = fuchsia_examples;

PowerDriver::PowerDriver(fdf::DriverStartArgs start_args,
                         fdf::UnownedSynchronizedDispatcher driver_dispatcher)
    : DriverBase("power_driver", std::move(start_args), std::move(driver_dispatcher)),
      config_(take_config<component_config::Config>()) {
  fdf::info(
      "PowerDriver constructor invoked. This constructor is only implemented to"
      "demonstrate the driver lifecycle. Drivers are not expected to add implementation in the constructor");
}

PowerDriver::~PowerDriver() {
  fdf::info(
      "PowerDriver destructor invoked after PrepareStop() and Stop() are called. "
      "This is only implemented to demonstrate the driver lifecycle. Drivers should avoid implementing the "
      "destructor and perform clean up in PrepareStop() and Stop() functions");
}

zx::result<> PowerDriver::Start() {
  fdf::info(
      "PowerDriver::Start() invoked. In this function, perform the driver "
      "initialization, such as adding children.");
  zx::result echo_protocol = incoming()->Connect<fex::Echo>();
  if (echo_protocol.is_error()) {
    return echo_protocol.take_error();
  }
  fidl::Call(*echo_protocol)->EchoString({"hello world!"});
  return zx::ok();
}

void PowerDriver::PrepareStop(fdf::PrepareStopCompleter completer) {
  FDF_LOG(INFO,
          "PowerDriver::PrepareStop() invoked. This is called before "
          "the driver dispatchers are shutdown. Only implement this function "
          "if you need to manually clearn up objects (ex/ unique_ptrs) in the driver dispatchers");
  completer(zx::ok());
}

void PowerDriver::Stop() {
  FDF_LOG(INFO,
          "PowerDriver::Stop() invoked. This is called after all driver dispatchers are "
          "shutdown. Use this function to perform any remaining teardowns");
}

void PowerDriver::Suspend(fdf_power::SuspendCompleter cb) {
  FDF_LOG(INFO,
          "PowerDriver::Suspend() invoked. Use this function to perform work required before "
          "going into suspend.");
  cb();
}

void PowerDriver::Resume(fdf_power::ResumeCompleter cb) {
  FDF_LOG(INFO,
          "PowerDriver::Resume() invoked. Use this function to perform any work required "
          "after exiting suspend.");
  cb();
}

bool PowerDriver::SuspendEnabled() { return config_.suspend_enabled(); }

}  // namespace power

FUCHSIA_DRIVER_EXPORT(power::PowerDriver);
