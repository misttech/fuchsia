// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.i2cimpl/cpp/driver/wire.h>
#include <lib/driver/testing/cpp/driver_test.h>

#include <bind/fuchsia/test/cpp/bind.h>
#include <gtest/gtest.h>

// This unit test verifies that the ChildTransportDriver is able to interact with a
// fuchsia.hardware.i2cimpl server.

namespace testing {

namespace {
constexpr uint32_t kTestMaxTransferSize = 0x1234567;
constexpr uint32_t kTestBitrate = 0x5;
}  // namespace

// A fuchsia.hardware.i2cimpl server that the underlying ChildDriver instance in the
// test will connect and interact with.
class TestI2cImplServer : public fdf::WireServer<fuchsia_hardware_i2cimpl::Device> {
 public:
  void GetMaxTransferSize(fdf::Arena& arena,
                          GetMaxTransferSizeCompleter::Sync& completer) override {
    completer.buffer(arena).ReplySuccess(kTestMaxTransferSize);
  }

  void SetBitrate(SetBitrateRequestView request, fdf::Arena& arena,
                  SetBitrateCompleter::Sync& completer) override {
    bitrate = request->bitrate;
    completer.buffer(arena).ReplySuccess();
  }

  void Transact(TransactRequestView request, fdf::Arena& arena,
                TransactCompleter::Sync& completer) override {
    completer.buffer(arena).ReplySuccess({});
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_i2cimpl::Device> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    FDF_LOG(
        ERROR,
        "Unknown method in fuchsia.hardware.i2cimpl Device protocol, closing with ZX_ERR_NOT_SUPPORTED");
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  uint32_t bitrate;
};

class DriverTransportTestEnvironment : public fdf_testing::Environment {
 public:
  // To serve the TestI2cImplServer to the driver-under-test, we need to set up the server
  // bindings and add it to the outgoing directory in Serve().
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    fuchsia_hardware_i2cimpl::Service::InstanceHandler handler({
        .device = server_bindings_.CreateHandler(&server_, fdf::Dispatcher::GetCurrent()->get(),
                                                 fidl::kIgnoreBindingClosure),
    });

    auto result = to_driver_vfs.AddService<fuchsia_hardware_i2cimpl::Service>(std::move(handler));
    EXPECT_EQ(ZX_OK, result.status_value());
    return zx::ok();
  }

  uint32_t GetBitrate() const { return server_.bitrate; }

 private:
  // The test fuchsia.hardware.i2cimpl server that is served to the underlying ChildTransportDriver.
  TestI2cImplServer server_;
  fdf::ServerBindingGroup<fuchsia_hardware_i2cimpl::Device> server_bindings_;
};

class FixtureConfig final {
 public:
  using DriverType = fdf_testing::EmptyDriverType;
  using EnvironmentType = DriverTransportTestEnvironment;
};

class ChildTransportDriverTest : public ::testing::Test {
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
  // Since the test calls the ChildTransportDriver's function directly, it wraps the driver
  // with BackgroundDriverTest.
  fdf_testing::BackgroundDriverTest<FixtureConfig> driver_test_;
};

TEST_F(ChildTransportDriverTest, VerifyQueryValues) {
  // Access the driver's bound node and check that it's parenting one child node that has the
  // test property properly set to the max transfer size we return.
  driver_test().RunInNodeContext([](fdf_testing::TestNode& node) {
    EXPECT_EQ(1u, node.children().size());
    auto child = node.children().find("transport-child");
    EXPECT_NE(child, node.children().end());
    auto props = child->second.GetProperties();
    EXPECT_EQ(1u, props.size());
    auto prop = props.begin();
    EXPECT_EQ(bind_fuchsia_test::TEST_CHILD, prop->key());
    EXPECT_EQ(kTestMaxTransferSize, prop->value().int_value().value());
  });

  // Check that the driver set the bitrate to the one we expect.
  driver_test().RunInEnvironmentTypeContext(
      [&](DriverTransportTestEnvironment& env) { EXPECT_EQ(kTestBitrate, env.GetBitrate()); });
}

}  // namespace testing
