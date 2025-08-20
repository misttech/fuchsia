// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_METADATA_CPP_TESTS_METADATA_INTEGRATION_TEST_METADATA_FORWARDER_TEST_DRIVER_METADATA_FORWARDER_TEST_DRIVER_H_
#define LIB_DRIVER_METADATA_CPP_TESTS_METADATA_INTEGRATION_TEST_METADATA_FORWARDER_TEST_DRIVER_METADATA_FORWARDER_TEST_DRIVER_H_

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.test/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/metadata/cpp/metadata_server.h>
#include <lib/driver/node/cpp/add_child.h>

namespace fdf_metadata::test {

// This driver's purpose is to forward metadata using `fdf::MetadataServer::ForwardMetadata()`.
class MetadataForwarderTestDriver : public fdf::DriverBase {
 public:
  static constexpr std::string_view kDriverName = "forwarder";
  static constexpr std::string_view kChildNodeName = "forwarder";

  MetadataForwarderTestDriver(fdf::DriverStartArgs start_args,
                              fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DriverBase(kDriverName, std::move(start_args), std::move(driver_dispatcher)) {}

  zx::result<> Start() override;

 private:
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  fdf_metadata::MetadataServer<fuchsia_hardware_test::Metadata> metadata_server_;
#endif

  fidl::ClientEnd<fuchsia_driver_framework::NodeController> child_;
};

}  // namespace fdf_metadata::test

#endif  // LIB_DRIVER_METADATA_CPP_TESTS_METADATA_INTEGRATION_TEST_METADATA_FORWARDER_TEST_DRIVER_METADATA_FORWARDER_TEST_DRIVER_H_
