// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.dictionaryoffers.test/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/component/cpp/driver_export.h>

namespace ft = fuchsia_dictionaryoffers_test;

namespace {

class ChildDriver : public fdf::DriverBase {
 public:
  ChildDriver(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : fdf::DriverBase("child", std::move(start_args), std::move(driver_dispatcher)) {}

  zx::result<> Start() override {
    auto data = incoming()->Connect<ft::DataService::Data>();
    if (data.is_error()) {
      fdf::error("Failed to connect to DataService: {}", data);
      return zx::error(ZX_ERR_INTERNAL);
    }

    fidl::Result result = fidl::Call(data.value())->DataDo();
    if (result.is_error()) {
      fdf::error("Failed to call DataDo: {}", result.error_value().FormatDescription());
      return zx::error(ZX_ERR_INTERNAL);
    }

    fdf::info("child successfully connected and called the data plane provided.");

    return zx::ok();
  }
};

}  // namespace

FUCHSIA_DRIVER_EXPORT(ChildDriver);
