// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/testing/cpp/driver_runtime.h>

#include <gtest/gtest.h>

namespace {

class TestDriver2 : public fdf::DriverBase2 {
 public:
  TestDriver2() : fdf::DriverBase2("test_driver2") { dispatcher_in_constructor_ = dispatcher(); }

  zx::result<> Start(fdf::DriverContext context) override { return zx::ok(); }

  async_dispatcher_t* dispatcher_in_constructor() const { return dispatcher_in_constructor_; }

 private:
  async_dispatcher_t* dispatcher_in_constructor_ = nullptr;
};

TEST(DriverBase2Test, DispatcherInConstructor) {
  fdf_testing::DriverRuntime runtime;

  EXPECT_NE(nullptr, fdf_dispatcher_get_current_dispatcher());

  TestDriver2 driver;
  EXPECT_NE(nullptr, driver.dispatcher_in_constructor());
  EXPECT_EQ(fdf_dispatcher_get_async_dispatcher(fdf_dispatcher_get_current_dispatcher()),
            driver.dispatcher_in_constructor());
}

}  // namespace
