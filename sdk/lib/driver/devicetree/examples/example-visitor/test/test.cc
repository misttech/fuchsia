// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/component/cpp/node_properties.h>
#include <lib/driver/logging/cpp/logger.h>

#include <gtest/gtest.h>

class ExampleVisitorTest : public ::testing::Test {
 protected:
  void SetUp() override {
    logger_ = std::make_unique<fdf::Logger>("example-visitor-test", FUCHSIA_LOG_INFO);
    fdf::Logger::SetGlobalInstance(logger_.get());
  }

  void DummyMakeProperty() {
    [[maybe_unused]] auto property = fdf::MakeProperty("dummy", "property");
  }

  void TearDown() override { fdf::Logger::SetGlobalInstance(nullptr); }

  std::unique_ptr<fdf::Logger> logger_;
};

TEST_F(ExampleVisitorTest, LoggerTest) {
  FDF_LOG(INFO, "Logger works for example visitor test.");
  fdf::info("std::format based logging works too {}", 1234);
}
