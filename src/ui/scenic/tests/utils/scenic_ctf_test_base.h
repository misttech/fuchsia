// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_TESTS_UTILS_SCENIC_CTF_TEST_BASE_H_
#define SRC_UI_SCENIC_TESTS_UTILS_SCENIC_CTF_TEST_BASE_H_

#include <fidl/fuchsia.testing.harness/cpp/fidl.h>
#include <fidl/fuchsia.ui.test.context/cpp/fidl.h>
#include <fuchsia/testing/harness/cpp/fidl.h>
#include <fuchsia/ui/test/context/cpp/fidl.h>
#include <lib/fidl/cpp/interface_handle.h>
#include <lib/fidl/cpp/synchronous_interface_ptr.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/zx/channel.h>

#include <cstdint>
#include <cstdlib>
#include <iostream>

#include <zxtest/zxtest.h>

#include "src/ui/testing/util/logging_event_loop.h"

namespace integration_tests {

/// ScenicCtfTest use realm_proxy to connect scenic test realm.
/// The scenic test realm consists of three components:
///   * Scenic
///   * Fake Cobalt
///   * Fake Display Provider
///
/// topology as follows:
///       test_manager
///            |
///     <test component>
///            |                  <- Test realm
/// ----------------------------  <- realm_proxy
///     /      |     \            <- Scenic realm
///  Scenic  Cobalt  Hdcp
class ScenicCtfTest : public zxtest::Test, public ui_testing::LoggingEventLoop {
 public:
  ScenicCtfTest(fuchsia_ui_test_context::RendererType renderer_type =
                    fuchsia_ui_test_context::RendererType::kVulkan)
      : renderer_type_(renderer_type) {}
  ~ScenicCtfTest() override = default;

  /// SetUp connect test realm so test can use realm_proxy_ to access.
  void SetUp() override;

  const std::shared_ptr<sys::ServiceDirectory>& LocalServiceDirectory() const;

  /// Override DisplayRotation() to provide fuchsia.scenic.DisplayRotation to test realm. By
  /// default, it returns 0.
  virtual uint64_t DisplayRotation() const;

  /// Overrides DisplayDimensions() to provide `active_width_px` and `active_height_px` to
  /// fake-display-stack-host in the test realm. If {0, 0}, the default value will be used.
  /// By default, it returns {0, 0};
  ///
  /// `width` and `height` must be both non-zero or both zero.
  virtual fuchsia_math::SizeU DisplayDimensions() const;

  /// Overrides DisplayRefreshRateMillihertz() to provide `refresh_rate_millihertz` to
  /// fake-display-stack-host. If zero, the default value will be used. By default it returns zero.
  virtual uint32_t DisplayRefreshRateMillihertz() const;

  /// Overrides DisplayMaxLayerCount() to provide `max_layer_count` to
  /// fake-display-stack-host. If zero, the default value will be used. By default it returns zero.
  virtual uint32_t DisplayMaxLayerCount() const;

  /// Override DisplayComposition() to provide fuchsia.scenic.DisplayComposition to test realm. True
  /// by default.
  virtual bool DisplayComposition() const;

  /// Connect to the FIDL protocol which served from the realm proxy use default served path if no
  /// name passed in.
  template <typename Protocol>
  fidl::SyncClient<Protocol> ConnectSyncIntoRealm(
      const std::string& service_path = Protocol::kDiscoverableName) {
    return fidl::SyncClient<Protocol>(ConnectIntoRealm<Protocol>(service_path));
  }

  /// Connect to the FIDL protocol which served from the realm proxy use default served path if no
  /// name passed in.
  template <typename Protocol>
  fidl::Client<Protocol> ConnectAsyncIntoRealm(
      const std::string& service_path = Protocol::kDiscoverableName) {
    return fidl::Client<Protocol>(ConnectIntoRealm<Protocol>(service_path));
  }

  /// Connect to the FIDL protocol which served from the realm proxy use default served path if no
  /// name passed in.
  template <typename Protocol>
  fidl::ClientEnd<Protocol> ConnectIntoRealm(
      const std::string& service_path = Protocol::kDiscoverableName) {
    auto [client_end, server_end] = fidl::CreateEndpoints<Protocol>().value();

    auto result = realm_proxy_->ConnectToNamedProtocol(
        fuchsia_testing_harness::RealmProxyConnectToNamedProtocolRequest(service_path,
                                                                         server_end.TakeChannel()));
    if (result.is_error()) {
      std::cerr << "ConnectToNamedProtocol(" << service_path << ", " << Protocol::kDiscoverableName
                << ") failed." << std::endl;
      std::abort();
    }
    return std::move(client_end);
  }

 private:
  fidl::SyncClient<fuchsia_ui_test_context::ScenicRealmFactory> realm_factory_;
  fidl::SyncClient<fuchsia_testing_harness::RealmProxy> realm_proxy_;
  std::unique_ptr<sys::ComponentContext> context_;
  const fuchsia_ui_test_context::RendererType renderer_type_;
};

// TODO(https://fxbug.dev/447603809): DO NOT USE THIS TEST BASE CLASS.
// All HLCCP tests, and should be migrated from ScenicCtfHlcppTest to ScenicCtfHlcppTest.
//
/// ScenicCtfHlcppTest use realm_proxy to connect scenic test realm.
/// The scenic test realm consists of three components:
///   * Scenic
///   * Fake Cobalt
///   * Fake Display Provider
///
/// topology as follows:
///       test_manager
///            |
///     <test component>
///            |                  <- Test realm
/// ----------------------------  <- realm_proxy
///     /      |     \            <- Scenic realm
///  Scenic  Cobalt  Hdcp
//
// TODO(https://fxbug.dev/447603809): DO NOT USE THIS TEST BASE CLASS.
// All HLCCP tests, and should be migrated from ScenicCtfHlcppTest to ScenicCtfHlcppTest.
class ScenicCtfHlcppTest : public zxtest::Test, public ui_testing::LoggingEventLoop {
 public:
  ScenicCtfHlcppTest(fuchsia::ui::test::context::RendererType renderer_type =
                         fuchsia::ui::test::context::RendererType::VULKAN)
      : renderer_type_(renderer_type) {}
  ~ScenicCtfHlcppTest() override = default;

  /// SetUp connect test realm so test can use realm_proxy_ to access.
  void SetUp() override;

  const std::shared_ptr<sys::ServiceDirectory>& LocalServiceDirectory() const;

  /// Override DisplayRotation() to provide fuchsia.scenic.DisplayRotation to test realm. By
  /// default, it returns 0.
  virtual uint64_t DisplayRotation() const;

  /// Overrides DisplayDimensions() to provide `active_width_px` and `active_height_px` to
  /// fake-display-stack-host in the test realm. If {0, 0}, the default value will be used.
  /// By default, it returns {0, 0};
  ///
  /// `width` and `height` must be both non-zero or both zero.
  virtual fuchsia::math::SizeU DisplayDimensions() const;

  /// Overrides DisplayRefreshRateMillihertz() to provide `refresh_rate_millihertz` to
  /// fake-display-stack-host. If zero, the default value will be used. By default it returns zero.
  virtual uint32_t DisplayRefreshRateMillihertz() const;

  /// Overrides DisplayMaxLayerCount() to provide `max_layer_count` to
  /// fake-display-stack-host. If zero, the default value will be used. By default it returns zero.
  virtual uint32_t DisplayMaxLayerCount() const;

  /// Override DisplayComposition() to provide fuchsia.scenic.DisplayComposition to test realm. True
  /// by default.
  virtual bool DisplayComposition() const;

  /// Connect to the FIDL protocol which served from the realm proxy use default served path if no
  /// name passed in.
  template <typename Interface>
  fidl::SynchronousInterfacePtr<Interface> ConnectSyncIntoRealm(
      const std::string& service_path = Interface::Name_) {
    fidl::SynchronousInterfacePtr<Interface> ptr;

    fuchsia::testing::harness::RealmProxy_ConnectToNamedProtocol_Result result;
    if (realm_proxy_->ConnectToNamedProtocol(service_path, ptr.NewRequest().TakeChannel(),
                                             &result) != ZX_OK) {
      std::cerr << "ConnectToNamedProtocol(" << service_path << ", " << Interface::Name_
                << ") failed." << std::endl;
      std::abort();
    }
    return std::move(ptr);
  }

  /// Connect to the FIDL protocol which served from the realm proxy use default served path if no
  /// name passed in.
  template <typename Interface>
  fidl::InterfacePtr<Interface> ConnectAsyncIntoRealm(
      const std::string& service_path = Interface::Name_) {
    fidl::InterfacePtr<Interface> ptr;

    fuchsia::testing::harness::RealmProxy_ConnectToNamedProtocol_Result result;
    if (realm_proxy_->ConnectToNamedProtocol(service_path, ptr.NewRequest().TakeChannel(),
                                             &result) != ZX_OK) {
      std::cerr << "ConnectToNamedProtocol(" << service_path << ", " << Interface::Name_
                << ") failed." << std::endl;
      std::abort();
    }
    return std::move(ptr);
  }

 private:
  fuchsia::ui::test::context::ScenicRealmFactorySyncPtr realm_factory_;
  fuchsia::testing::harness::RealmProxySyncPtr realm_proxy_;
  std::unique_ptr<sys::ComponentContext> context_;
  const fuchsia::ui::test::context::RendererType renderer_type_;
};

}  // namespace integration_tests

#endif  // SRC_UI_SCENIC_TESTS_UTILS_SCENIC_CTF_TEST_BASE_H_
