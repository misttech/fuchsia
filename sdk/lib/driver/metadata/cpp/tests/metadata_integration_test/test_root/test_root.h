// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_METADATA_CPP_TESTS_METADATA_INTEGRATION_TEST_TEST_ROOT_TEST_ROOT_H_
#define LIB_DRIVER_METADATA_CPP_TESTS_METADATA_INTEGRATION_TEST_TEST_ROOT_TEST_ROOT_H_

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.test/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/node/cpp/add_child.h>

namespace fdf_metadata::test {

// This driver's purpose is to create two child nodes: one for the "test_parent_expose" driver to
// bind to and one for the "test_parent_no_expose" driver to bind to.
class TestRootDriver : public fdf::DriverBase, public fidl::Server<fuchsia_hardware_test::Root> {
 public:
  static constexpr std::string_view kDriverName = "test_root";

  TestRootDriver(fdf::DriverStartArgs start_args,
                 fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DriverBase(kDriverName, std::move(start_args), std::move(driver_dispatcher)) {}

  // fdf::DriverBase implementation.
  zx::result<> Start() override;

  // fidl::Server<fuchsia_hardware_test::Root> implementation.
  void AddMetadataSenderNode(AddMetadataSenderNodeRequest& request,
                             AddMetadataSenderNodeCompleter::Sync& completer) override;

 private:
  std::vector<fidl::ClientEnd<fuchsia_driver_framework::NodeController>> metadata_senders_;
  fidl::ServerBindingGroup<fuchsia_hardware_test::Root> bindings_;
};

}  // namespace fdf_metadata::test

#endif  // LIB_DRIVER_METADATA_CPP_TESTS_METADATA_INTEGRATION_TEST_TEST_ROOT_TEST_ROOT_H_
