// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/logging/cpp/logger.h>

namespace {

class DriverServeFidl : public fdf::DriverBase {
 public:
  DriverServeFidl(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher dispatcher)
      : fdf::DriverBase("driver_serve_fidl", std::move(start_args), std::move(dispatcher)) {}

  zx::result<> Start() override {
    fdf::info("DriverServeFidl started");

    return zx::ok();
  }
};

}  // namespace

FUCHSIA_DRIVER_EXPORT(DriverServeFidl);
