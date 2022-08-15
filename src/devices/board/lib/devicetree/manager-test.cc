// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/board/lib/devicetree/manager.h"

#include <fcntl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <lib/driver/legacy-bind-constants/legacy-bind-constants.h>

#include <sstream>
#include <unordered_set>

#include <bind/fuchsia/platform/cpp/bind.h>
#include <gtest/gtest.h>

#include "fidl/fuchsia.driver.framework/cpp/markers.h"
#include "fidl/fuchsia.driver.framework/cpp/natural_types.h"
#include "fidl/fuchsia.hardware.platform.bus/cpp/markers.h"
#include "lib/driver_runtime/testing/loop_fixture/test_loop_fixture.h"

namespace fdf_devicetree {
namespace {

// Helper function to assert two string views are equal.
void AssertStringViewEq(std::string_view lhs, std::string_view rhs) {
  if (lhs == rhs) {
    return;
  }

  ASSERT_TRUE(false) << "String view comparison failed, expected " << lhs << " got " << rhs;
}

#define ASSERT_STRINGVIEW_EQ(lhs, rhs) AssertStringViewEq(lhs, rhs);

std::string DebugStringifyProperty(fuchsia_driver_framework::NodeProperty& prop) {
  std::stringstream ret;
  ret << "Key=";

  if (prop.key() != std::nullopt) {
    switch (prop.key()->Which()) {
      using Tag = fuchsia_driver_framework::NodePropertyKey::Tag;
      case Tag::kIntValue:
        ret << "Int{" << prop.key()->int_value().value() << "}";
        break;
      case Tag::kStringValue:
        ret << "Str{" << prop.key()->string_value().value() << "}";
        break;
      default:
        ret << "Unknown{" << static_cast<int>(prop.key()->Which()) << "}";
        break;
    }
  } else {
    ret << "NULL";
  }

  ret << " Value=";
  if (prop.value() != std::nullopt) {
    switch (prop.value()->Which()) {
      using Tag = fuchsia_driver_framework::NodePropertyValue::Tag;
      case Tag::kBoolValue:
        ret << "Bool{" << prop.value()->bool_value().value() << "}";
        break;
      case Tag::kEnumValue:
        ret << "Enum{" << prop.value()->enum_value().value() << "}";
        break;
      case Tag::kIntValue:
        ret << "Int{" << prop.value()->int_value().value() << "}";
        break;
      case Tag::kStringValue:
        ret << "String{" << prop.value()->string_value().value() << "}";
        break;
      default:
        ret << "Unknown{" << static_cast<int>(prop.value()->Which()) << "}";
        break;
    }
  } else {
    ret << "NULL";
  }

  return ret.str();
}

void AssertHasProperties(std::vector<fuchsia_driver_framework::NodeProperty> expected,
                         fuchsia_driver_framework::DeviceGroupNode node) {
  for (auto& property : node.bind_properties()) {
    auto iter = std::find(expected.begin(), expected.end(), property);
    EXPECT_NE(expected.end(), iter) << "Unexpected property: " << DebugStringifyProperty(property);
    expected.erase(iter);
  }

  ASSERT_TRUE(expected.empty());
}

class FakePlatformBus final : public fdf::Server<fuchsia_hardware_platform_bus::PlatformBus> {
 public:
  void NodeAdd(NodeAddRequest& request, NodeAddCompleter::Sync& completer) override {
    nodes_.emplace_back(std::move(request.node()));
    completer.Reply(zx::ok());
  }
  void ProtocolNodeAdd(ProtocolNodeAddRequest& request,
                       ProtocolNodeAddCompleter::Sync& completer) override {
    completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
  }
  void RegisterProtocol(RegisterProtocolRequest& request,
                        RegisterProtocolCompleter::Sync& completer) override {
    completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
  }

  void GetBoardInfo(GetBoardInfoCompleter::Sync& completer) override {
    completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
  }
  void SetBoardInfo(SetBoardInfoRequest& request, SetBoardInfoCompleter::Sync& completer) override {
    completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
  }

  void SetBootloaderInfo(SetBootloaderInfoRequest& request,
                         SetBootloaderInfoCompleter::Sync& completer) override {
    completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
  }
  void AddComposite(AddCompositeRequest& request, AddCompositeCompleter::Sync& completer) override {
    completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
  }
  void AddCompositeImplicitPbusFragment(
      AddCompositeImplicitPbusFragmentRequest& request,
      AddCompositeImplicitPbusFragmentCompleter::Sync& completer) override {
    completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
  }
  void RegisterSysSuspendCallback(RegisterSysSuspendCallbackRequest& request,
                                  RegisterSysSuspendCallbackCompleter::Sync& completer) override {
    completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
  }

  std::vector<fuchsia_hardware_platform_bus::Node>& nodes() { return nodes_; }

 private:
  std::vector<fuchsia_hardware_platform_bus::Node> nodes_;
};
class FakeDeviceGroupManager final
    : public fidl::Server<fuchsia_driver_framework::DeviceGroupManager> {
 public:
  void CreateDeviceGroup(CreateDeviceGroupRequest& request,
                         CreateDeviceGroupCompleter::Sync& completer) override {
    requests_.emplace_back(std::move(request));
    completer.Reply(zx::ok());
  }

  std::vector<CreateDeviceGroupRequest> requests() { return requests_; }

 private:
  std::vector<CreateDeviceGroupRequest> requests_;
};

class ManagerTest : public gtest::DriverTestLoopFixture {
 public:
  // Load the file |name| into a vector and return it.
  std::vector<uint8_t> LoadTestBlob(const char* name) {
    int fd = open(name, O_RDONLY);
    if (fd < 0) {
      printf("Open failed: %s\n", strerror(errno));
      return {};
    }

    struct stat stat_out;
    if (fstat(fd, &stat_out) < 0) {
      printf("fstat failed: %s\n", strerror(errno));
      return {};
    }

    std::vector<uint8_t> vec(stat_out.st_size);
    ssize_t bytes_read = read(fd, vec.data(), stat_out.st_size);
    if (bytes_read < 0) {
      printf("read failed: %s\n", strerror(errno));
      return {};
    }
    vec.resize(bytes_read);
    return vec;
  }

  void DoPublish(Manager& manager) {
    auto pbus_endpoints = fdf::CreateEndpoints<fuchsia_hardware_platform_bus::PlatformBus>();
    ASSERT_EQ(ZX_OK, pbus_endpoints.status_value());
    auto mgr_endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::DeviceGroupManager>();
    ASSERT_EQ(ZX_OK, mgr_endpoints.status_value());
    auto node_endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::Node>();
    ASSERT_EQ(ZX_OK, node_endpoints.status_value());

    RunOnDispatcher([&]() {
      fdf::BindServer(fdf::Dispatcher::GetCurrent()->get(), std::move(pbus_endpoints->server),
                      &pbus_);
      fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                       std::move(mgr_endpoints->server), &mgr_);
    });

    ASSERT_EQ(ZX_OK, manager
                         .PublishDevices(std::move(pbus_endpoints->client),
                                         std::move(node_endpoints->client),
                                         std::move(mgr_endpoints->client))
                         .status_value());
    ;
  }

  driver::Logger logger_;
  FakePlatformBus pbus_;
  FakeDeviceGroupManager mgr_;
};

TEST_F(ManagerTest, TestFindsNodes) {
  Manager manager(LoadTestBlob("/pkg/test-data/simple.dtb"), logger_);
  ASSERT_EQ(ZX_OK, manager.Discover().status_value());
  ASSERT_EQ(3lu, manager.nodes().size());

  // Root node is always first, and has no name.
  Node* node = manager.nodes()[0].get();
  ASSERT_STRINGVIEW_EQ("", node->name());

  // example-device node should be next.
  node = manager.nodes()[1].get();
  ASSERT_STRINGVIEW_EQ("example-device", node->name());

  // another-device should be last.
  node = manager.nodes()[2].get();
  ASSERT_STRINGVIEW_EQ("another-device", node->name());
}

TEST_F(ManagerTest, TestPropertyCallback) {
  Manager manager(LoadTestBlob("/pkg/test-data/simple.dtb"), logger_);
  std::unordered_set<std::string> expected{
      "compatible",
      "phandle",
  };
  manager.AddPropertyCallback([&](Node* node, devicetree::Property property) {
    if (node->name() == "example-device") {
      auto iter = expected.find(std::string(property.name));
      EXPECT_NE(expected.end(), iter) << "Property " << property.name << " was unexpected.";
      expected.erase(iter);
    }
  });

  ASSERT_EQ(ZX_OK, manager.Discover().status_value());
  EXPECT_EQ(0lu, expected.size());
}

TEST_F(ManagerTest, TestPublishesSimpleNode) {
  Manager manager(LoadTestBlob("/pkg/test-data/simple.dtb"), logger_);
  ASSERT_EQ(ZX_OK, manager.Discover().status_value());

  DoPublish(manager);
  ASSERT_EQ(2lu, pbus_.nodes().size());

  ASSERT_EQ(2lu, mgr_.requests().size());

  auto example_group = mgr_.requests()[1];
  ASSERT_TRUE(example_group.nodes().has_value());
  ASSERT_TRUE(example_group.topological_path().has_value());
  EXPECT_NE(nullptr, strstr("example-device", example_group.topological_path()->data()));
  // First node is the primary node. In this case, it should be the platform device.
  ASSERT_FALSE(example_group.nodes()->empty());

  auto& pbus_node = example_group.nodes().value()[0];
  AssertHasProperties(
      {{{
           .key = fuchsia_driver_framework::NodePropertyKey::WithStringValue(
               "fuchsia.devicetree.first_compatible"),
           .value = fuchsia_driver_framework::NodePropertyValue::WithStringValue(
               "fuchsia,sample-device"),
       }},
       {{
           .key = fuchsia_driver_framework::NodePropertyKey::WithIntValue(BIND_PROTOCOL),
           .value = fuchsia_driver_framework::NodePropertyValue::WithIntValue(
               bind_fuchsia_platform::BIND_PROTOCOL_DEVICE),
       }}},
      pbus_node);
}

}  // namespace
}  // namespace fdf_devicetree
