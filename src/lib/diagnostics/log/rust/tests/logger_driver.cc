// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/logging/cpp/logger.h>

namespace {

class LoggerDriver : public fdf::DriverBase {
 public:
  LoggerDriver(fdf::DriverStartArgs start_args,
               fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DriverBase("logger_driver", std::move(start_args), std::move(driver_dispatcher)) {}

  zx::result<> Start() override {
    FDF_LOG(INFO, "Hello, Archivist!");
    return zx::ok();
  }
};

}  // namespace

FUCHSIA_DRIVER_EXPORT(LoggerDriver);
