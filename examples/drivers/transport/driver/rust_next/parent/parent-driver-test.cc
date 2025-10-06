// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.i2cimpl/cpp/driver/wire.h>
#include <lib/driver/testing/cpp/driver_test.h>

#include <gtest/gtest.h>

// This unit test connects to the ParentTransportDriver over the fuchsia.hardware.i2cimpl FIDL
// protocol and verifies that the driver responds to the FIDL requests as expected.

namespace testing {

namespace {
constexpr uint32_t kTestBitrate = 0x5;
}  // namespace

class TestEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override { return zx::ok(); }
};

class FixtureConfig final {
 public:
  using DriverType = fdf_testing::EmptyDriverType;
  using EnvironmentType = TestEnvironment;
};

class ParentTransportDriverTest : public ::testing::Test {
 public:
  void SetUp() override {
    zx::result<> result = driver_test().StartDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
  }

  void TearDown() override {
    zx::result<> result = driver_test().StopDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
  }

  fdf_testing::BackgroundDriverTest<FixtureConfig>& driver_test() { return driver_test_; }

 private:
  // Since the test mostly interacts with the ParentTransportDriver's function over FIDL calls,
  // it wraps the driver with BackgroundDriverTest.
  fdf_testing::BackgroundDriverTest<FixtureConfig> driver_test_;
};

TEST_F(ParentTransportDriverTest, TestClient) {
  // Connect to ParentTransportDriver through the fuchsia.hardware.i2cimpl protocol.
  zx::result connect_result = driver_test().Connect<fuchsia_hardware_i2cimpl::Service::Device>();
  ASSERT_TRUE(connect_result.is_ok());

  fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device> client(std::move(connect_result.value()));
  fdf::Arena arena('I2CI');

  // Retrieve and verify the max transfer size.
  constexpr uint32_t kExpectedMaxTransferSize = 0x1234ABCD;
  auto result = client.buffer(arena)->GetMaxTransferSize();
  ASSERT_TRUE(result.ok());
  ASSERT_TRUE(result->is_ok());
  EXPECT_EQ(kExpectedMaxTransferSize, result->value()->size);

  // Set the bitrate to a value that should succeed.
  auto bitrate_good_result = client.buffer(arena)->SetBitrate(kTestBitrate);
  ASSERT_TRUE(bitrate_good_result.ok());
  ASSERT_TRUE(bitrate_good_result->is_ok());

  // Set the bitrate to a value that should not succeed.
  auto bitrate_bad_result = client.buffer(arena)->SetBitrate(kTestBitrate + 1);
  ASSERT_TRUE(bitrate_bad_result.ok());
  ASSERT_TRUE(bitrate_bad_result->is_error());

  // Send a Transact() request and verify the read data.
  auto transact_result = client.buffer(arena)->Transact({});
  ASSERT_TRUE(transact_result.ok());
  ASSERT_TRUE(transact_result->is_ok());

  const std::vector<uint8_t> kExpectedReadData = {0, 1, 2};
  ASSERT_EQ(1u, transact_result.value()->read.size());

  auto read_data = transact_result.value()->read[0];
  ASSERT_EQ(kExpectedReadData.size(), read_data.data.size());
  for (size_t i = 0; i < kExpectedReadData.size(); i++) {
    EXPECT_EQ(kExpectedReadData[i], read_data.data[i]);
  }
}

}  // namespace testing
