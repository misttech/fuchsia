// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.nodegroup.test/cpp/wire.h>
#include <lib/async/cpp/task.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/logging/cpp/logger.h>

namespace fdf {
using namespace fuchsia_driver_framework;
}  // namespace fdf

namespace ft = fuchsia_nodegroup_test;

namespace {

class LeafDriver : public fdf::DriverBase {
 public:
  LeafDriver(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : fdf::DriverBase("leaf", std::move(start_args), std::move(driver_dispatcher)) {}

  zx::result<> Start() override {
    auto result = async::PostTask(dispatcher(), [&]() { RunAsync(); });
    if (result == ZX_OK) {
      return zx::ok();
    }

    return zx::error(result);
  }

  void RunAsync() {
    auto connect_result = incoming()->Connect<ft::Waiter>();
    if (connect_result.is_error()) {
      fdf::error("Failed to start leaf driver: {}", connect_result);
      node().reset();
      return;
    }

    const fidl::WireSharedClient<ft::Waiter> client{std::move(connect_result.value()),
                                                    dispatcher()};
    auto work_result = DoWork(client);
    if (work_result.is_error()) {
      fdf::error("DoWork was not successful: {}", work_result);
      return;
    }

    fdf::info("Completed RunAsync successfully.");
  }

 private:
  zx::result<uint32_t> GetNumber(std::string_view instance) {
    auto device = incoming()->Connect<ft::Service::Device>(instance);
    if (device.status_value() != ZX_OK) {
      fdf::warn("Failed to connect to {}: {}", instance.data(), device);
      return device.take_error();
    }

    auto result = fidl::WireCall(*device)->GetNumber();
    if (result.status() != ZX_OK) {
      fdf::warn("Failed to call number on {}: {}", instance.data(), result.lossy_description());
      return zx::error(result.status());
    }
    return zx::ok(result.value().number);
  }

  zx::result<> DoWork(const fidl::WireSharedClient<ft::Waiter>& waiter) {
    // Check the left device.
    auto number = GetNumber("left");
    if (number.is_error()) {
      [[maybe_unused]] auto result = waiter->Ack(number.error_value());
      return zx::ok();
    }
    if (*number != 1) {
      fdf::error("Wrong number for left: expecting 1, saw {}", *number);
      [[maybe_unused]] auto result = waiter->Ack(ZX_ERR_INTERNAL);
      return zx::ok();
    }

    // Check the right device.
    number = GetNumber("right");
    if (number.is_error()) {
      [[maybe_unused]] auto result = waiter->Ack(number.error_value());
      return zx::ok();
    }
    if (*number != 2) {
      fdf::error("Wrong number for right: expecting 2, saw {}", *number);
      [[maybe_unused]] auto result = waiter->Ack(ZX_ERR_INTERNAL);
      return zx::ok();
    }

    // Check the optional device.
    number = GetNumber("opt");
    if (number.is_error()) {
      fdf::info("No 'opt' parent.");
    } else if (*number != 3) {
      fdf::error("Wrong number for opt: expecting 3, saw {}", *number);
      [[maybe_unused]] auto result = waiter->Ack(ZX_ERR_INTERNAL);
      return zx::ok();
    }

    // Check the default device (which is the left device).
    number = GetNumber("default");
    if (number.is_error()) {
      [[maybe_unused]] auto result = waiter->Ack(number.error_value());
      return zx::ok();
    }
    if (*number != 1) {
      fdf::error("Wrong number for default: expecting 1, saw {}", *number);
      [[maybe_unused]] auto result = waiter->Ack(ZX_ERR_INTERNAL);
      return zx::ok();
    }

    [[maybe_unused]] auto result = waiter->Ack(ZX_OK);
    return zx::ok();
  }
};

}  // namespace

FUCHSIA_DRIVER_EXPORT(LeafDriver);
