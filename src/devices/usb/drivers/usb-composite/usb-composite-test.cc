// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-composite/usb-composite.h"

#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/zx/time.h>

#include <string>
#include <vector>

#include <fbl/auto_lock.h>
#include <usb/descriptors.h>
#include <zxtest/zxtest.h>

#include "src/devices/usb/drivers/usb-composite/test-helper.h"
#include "src/devices/usb/drivers/usb-composite/usb-interface.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_composite {

// Serves |usb_| to the driver as its parent's Banjo USB protocol.
class Environment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    compat::DeviceServer::BanjoConfig config{ZX_PROTOCOL_USB};
    config.callbacks[ZX_PROTOCOL_USB] = [this]() {
      return compat::DeviceServer::GenericProtocol{
          .ops = usb_.GetProto()->ops,
          .ctx = usb_.GetProto()->ctx,
      };
    };
    device_server_.Initialize(component::kDefaultInstance, std::nullopt, std::move(config));
    return zx::make_result(
        device_server_.Serve(fdf::Dispatcher::GetCurrent()->async_dispatcher(), &to_driver_vfs));
  }

  MockUsb& usb() { return usb_; }

 private:
  MockUsb usb_;
  compat::DeviceServer device_server_;
};

class TestConfig final {
 public:
  using DriverType = UsbComposite;
  using EnvironmentType = Environment;
};

// The name of the node that UsbComposite parents its interfaces to. It is the device id returned by
// the parent, formatted as "%03d".
constexpr char kCompositeNodeName[] = "010";

// UsbCompositeTest is templated on the configuration descriptor data used for the test,
// UsbCompositeTest<&kDescriptors> will set up a test fixture with a USB protocol client that
// implements GetDescriptors and GetDescriptorsLength for the provided kDescriptors.
template <auto* descriptors>
class UsbCompositeTest : public zxtest::Test {
 public:
  void SetUp() override {
    driver_test_.RunInEnvironmentTypeContext([this](Environment& env) {
      MockUsb& usb = env.usb();
      usb.ExpectGetDeviceDescriptor(usb_device_descriptor_t{
          .b_length = sizeof(usb_device_descriptor_t),
          .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kDevice),
          .bcd_usb = 0,
          .b_device_class = 12,
          .b_device_sub_class = 14,
          .b_device_protocol = 16,
          .b_max_packet_size0 = 5,
          .id_vendor = 9,
          .id_product = 8,
          .bcd_device = 7,
          .i_manufacturer = 6,
          .i_product = 5,
          .i_serial_number = 0x12,
          .b_num_configurations = 1,
      });
      usb.ExpectGetConfiguration(5);
      usb.ExpectGetConfigurationDescriptorLength(ZX_OK, 5, sizeof(*descriptors));
      usb.ExpectGetConfigurationDescriptor(
          ZX_OK, 5,
          std::vector<uint8_t>(
              reinterpret_cast<const uint8_t*>(descriptors),
              reinterpret_cast<const uint8_t*>(descriptors) + sizeof(*descriptors)));
      usb.ExpectGetDeviceId(10);
      ExpectConfigureEndpoints(usb);
    });
    ASSERT_OK(driver_test_.StartDriver());
  }

  void TearDown() override { EXPECT_OK(driver_test_.StopDriver()); }

  struct init_test_expected_values_t {
    std::string name;
    size_t length;
    const uint8_t* start;
  };
  void InitTest(std::vector<init_test_expected_values_t> expected) {
    driver_test_.RunInNodeContext([&](fdf_testing::TestNode& node) {
      ASSERT_EQ(node.children().size(), 1u);
      ASSERT_TRUE(node.children().contains(kCompositeNodeName));
      fdf_testing::TestNode::ChildrenMap& interfaces =
          node.children().at(kCompositeNodeName).children();
      EXPECT_EQ(interfaces.size(), expected.size());
      for (const auto& e : expected) {
        EXPECT_TRUE(interfaces.contains(e.name), "Missing child node %s", e.name.c_str());
      }
    });

    // Interfaces are stored in the order they were published.
    driver_test_.RunInDriverContext([&](UsbComposite& driver) {
      fbl::AutoLock _(&driver.lock_);
      ASSERT_EQ(driver.interfaces_.size(), expected.size());
      for (size_t i = 0; i < expected.size(); i++) {
        UsbInterface* intf = driver.interfaces_[i].get();
        ASSERT_EQ(intf->UsbGetDescriptorsLength(), expected[i].length);
        std::vector<uint8_t> desc(expected[i].length);
        size_t actual;
        intf->UsbGetDescriptors(desc.data(), desc.size(), &actual);
        EXPECT_EQ(actual, desc.size());
        EXPECT_BYTES_EQ(expected[i].start, desc.data(), desc.size());
      }
    });
  }

  void SetClaimed(uint8_t interface_id) {
    driver_test_.RunInDriverContext([interface_id](UsbComposite& driver) {
      fbl::AutoLock _(&driver.lock_);
      driver.interface_statuses_[interface_id] = UsbComposite::InterfaceStatus::CLAIMED;
    });
  }

  // Waits until the child node |name| has been removed and the driver has destroyed the
  // corresponding interface.
  void WaitForInterfaceRemoved(const std::string& name) {
    while (true) {
      bool node_present = driver_test_.RunInNodeContext<bool>([&](fdf_testing::TestNode& node) {
        auto composite = node.children().find(kCompositeNodeName);
        if (composite == node.children().end()) {
          return false;
        }
        return composite->second.children().contains(name);
      });
      bool removal_pending = driver_test_.RunInDriverContext<bool>([](UsbComposite& driver) {
        fbl::AutoLock _(&driver.lock_);
        return !driver.removed_interfaces_.empty();
      });
      if (!node_present && !removal_pending) {
        return;
      }
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
  }

  // Runs |task| with the parent's mock USB protocol.
  void WithUsb(fit::callback<void(MockUsb&)> task) {
    driver_test_.RunInEnvironmentTypeContext(
        [task = std::move(task)](Environment& env) mutable { task(env.usb()); });
  }

 protected:
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test_;

 private:
  void ExpectConfigureEndpoints(MockUsb& usb);
};

constexpr struct interface_config_t {
  usb_configuration_descriptor_t config;
  usb_interface_descriptor_t interface1;
  usb_endpoint_descriptor_t ep1_1;
  usb_ss_ep_comp_descriptor_t ss_companion1;
  usb_endpoint_descriptor_t ep1_2;
  usb_ss_ep_comp_descriptor_t ss_companion2;
  usb_interface_descriptor_t alt1_interface1;
  usb_endpoint_descriptor_t ep1_alt1_1;
  usb_endpoint_descriptor_t ep1_alt1_2;
  usb_interface_descriptor_t alt1_interface2;
  usb_interface_descriptor_t interface2;
  usb_endpoint_descriptor_t ep2_1;
  usb_interface_descriptor_t alt2_interface1;
  usb_endpoint_descriptor_t ep2_alt1_1;
  usb_interface_descriptor_t interface3;
} kTestInterface = {
    .config =
        {
            .b_length = sizeof(usb_configuration_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration),
            .w_total_length = sizeof(interface_config_t),
            .b_num_interfaces = 3,
            .b_configuration_value = 0,
            .i_configuration = 0,
            .bm_attributes =
                fdescriptor::kConfigurationReserved7 | fdescriptor::kConfigurationSelfPowered,
            .b_max_power = 4,
        },
    .interface1 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 0,
            .b_num_endpoints = 2,
            .b_interface_class = 8,
            .b_interface_sub_class = 6,
            .b_interface_protocol = 80,
            .i_interface = 0,
        },
    .ep1_1 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x81,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .ss_companion1 =
        {
            .b_length = sizeof(usb_ss_ep_comp_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion),
            .b_max_burst = 3,
            .bm_attributes = 0,
            .w_bytes_per_interval = 0,
        },
    .ep1_2 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 2,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .ss_companion2 =
        {
            .b_length = sizeof(usb_ss_ep_comp_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion),
            .b_max_burst = 3,
            .bm_attributes = 0,
            .w_bytes_per_interval = 0,
        },
    .alt1_interface1 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 1,
            .b_num_endpoints = 2,
            .b_interface_class = 8,
            .b_interface_sub_class = 6,
            .b_interface_protocol = 80,
            .i_interface = 0,
        },
    .ep1_alt1_1 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x85,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .ep1_alt1_2 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 3,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .alt1_interface2 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 2,
            .b_num_endpoints = 0,
            .b_interface_class = 8,
            .b_interface_sub_class = 6,
            .b_interface_protocol = 80,
            .i_interface = 0,
        },
    .interface2 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 1,
            .b_alternate_setting = 0,
            .b_num_endpoints = 1,
            .b_interface_class = 5,
            .b_interface_sub_class = 7,
            .b_interface_protocol = 8,
            .i_interface = 0,
        },
    .ep2_1 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x82,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .alt2_interface1 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 1,
            .b_alternate_setting = 1,
            .b_num_endpoints = 1,
            .b_interface_class = 5,
            .b_interface_sub_class = 7,
            .b_interface_protocol = 8,
            .i_interface = 0,
        },
    .ep2_alt1_1 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x1,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .interface3 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 2,
            .b_alternate_setting = 0,
            .b_num_endpoints = 0,
            .b_interface_class = 12,
            .b_interface_sub_class = 3,
            .b_interface_protocol = 6,
            .i_interface = 0,
        },
};

// NoAssociationCompositeTest tests UsbComposite's ability to process interface descriptors
// that have no association descriptors.
using NoAssociationCompositeTest = UsbCompositeTest<&kTestInterface>;

template <>
void NoAssociationCompositeTest::ExpectConfigureEndpoints(MockUsb& usb) {
  usb.ExpectEnableEndpoint(ZX_OK, kTestInterface.ep1_2, kTestInterface.ss_companion2, true);
  usb.ExpectEnableEndpoint(ZX_OK, kTestInterface.ep1_1, kTestInterface.ss_companion1, true);
  usb.ExpectEnableEndpoint(ZX_OK, kTestInterface.ep2_1, {}, true);
}

TEST_F(NoAssociationCompositeTest, InitTest) {
  InitTest(std::vector<init_test_expected_values_t>{
      init_test_expected_values_t{
          .name = "ifc-000",
          .length = reinterpret_cast<uintptr_t>(&kTestInterface.interface2) -
                    reinterpret_cast<uintptr_t>(&kTestInterface.interface1),
          .start = reinterpret_cast<const uint8_t*>(&kTestInterface.interface1),
      },
      init_test_expected_values_t{
          .name = "ifc-001",
          .length = reinterpret_cast<uintptr_t>(&kTestInterface.interface3) -
                    reinterpret_cast<uintptr_t>(&kTestInterface.interface2),
          .start = reinterpret_cast<const uint8_t*>(&kTestInterface.interface2),
      },
      init_test_expected_values_t{
          .name = "ifc-002",
          .length = sizeof(kTestInterface.interface3),
          .start = reinterpret_cast<const uint8_t*>(&kTestInterface.interface3),
      },
  });
}

TEST_F(NoAssociationCompositeTest, ClaimInterfaceTest) {
  driver_test_.RunInDriverContext([](UsbComposite& driver) {
    // GetInterfaceById failed
    EXPECT_EQ(driver.ClaimInterface(3), ZX_ERR_INVALID_ARGS);

    // Success
    EXPECT_OK(driver.ClaimInterface(2));
  });
  // Claiming an interface that has a child node removes that node, after which the interface is
  // destroyed.
  WaitForInterfaceRemoved("ifc-002");

  // Already claimed.
  SetClaimed(1);
  driver_test_.RunInDriverContext(
      [](UsbComposite& driver) { EXPECT_EQ(driver.ClaimInterface(1), ZX_ERR_ALREADY_BOUND); });
}

TEST_F(NoAssociationCompositeTest, SetInterfaceTest) {
  // Success
  WithUsb([](MockUsb& usb) {
    usb.ExpectEnableEndpoint(ZX_OK, kTestInterface.ep2_alt1_1, {}, true);
    usb.ExpectEnableEndpoint(ZX_OK, kTestInterface.ep2_1, {}, false);
    usb.ExpectControlOut(ZX_OK, kStandardInterfaceOut,
                         fidl::ToUnderlying(fdescriptor::StandardRequest::kSetInterface), 1, 1,
                         ZX_TIME_INFINITE, std::vector<uint8_t>{});
  });
  driver_test_.RunInDriverContext([](UsbComposite& driver) {
    EXPECT_OK(driver.SetInterface(1, 1));

    // interface_id not found
    EXPECT_EQ(driver.SetInterface(3, 1), ZX_ERR_INVALID_ARGS);
  });
}

TEST_F(NoAssociationCompositeTest, GetAdditionalDescriptorListTest) {
  driver_test_.RunInDriverContext([](UsbComposite& driver) {
    uint8_t desc_list[sizeof(kTestInterface.interface3)];
    size_t actual;
    EXPECT_OK(driver.GetAdditionalDescriptorList(1, desc_list, sizeof(desc_list), &actual));
    EXPECT_EQ(actual, sizeof(desc_list));
    EXPECT_BYTES_EQ(desc_list, &kTestInterface.interface3, actual);
  });
}

constexpr struct assoc_config_t {
  usb_configuration_descriptor_t config;
  usb_interface_assoc_descriptor_t association;
  usb_interface_descriptor_t interface1;
  usb_endpoint_descriptor_t ep1_1;
  usb_ss_ep_comp_descriptor_t ss_companion1;
  usb_endpoint_descriptor_t ep1_2;
  usb_ss_ep_comp_descriptor_t ss_companion2;
  usb_interface_descriptor_t alt1_interface1;
  usb_endpoint_descriptor_t ep1_alt1_1;
  usb_endpoint_descriptor_t ep1_alt1_2;
  usb_interface_descriptor_t alt1_interface2;
  usb_interface_descriptor_t interface2;
  usb_endpoint_descriptor_t ep2_1;
  usb_interface_descriptor_t alt2_interface1;
  usb_endpoint_descriptor_t ep2_alt1_1;
  usb_interface_descriptor_t interface3;
} kTestAssociation = {
    .config =
        {
            .b_length = sizeof(usb_configuration_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration),
            .w_total_length = sizeof(assoc_config_t),
            .b_num_interfaces = 3,
            .b_configuration_value = 0,
            .i_configuration = 0,
            .bm_attributes =
                fdescriptor::kConfigurationReserved7 | fdescriptor::kConfigurationSelfPowered,
            .b_max_power = 4,
        },
    .association =
        {
            .b_length = sizeof(usb_interface_assoc_descriptor_t),
            .b_descriptor_type =
                fidl::ToUnderlying(fdescriptor::DescriptorType::kInterfaceAssociation),
            .b_first_interface = 0,
            .b_interface_count = 2,
            .b_function_class = 5,
            .b_function_sub_class = 3,
            .b_function_protocol = 2,
            .i_function = 1,
        },
    .interface1 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 0,
            .b_num_endpoints = 2,
            .b_interface_class = 8,
            .b_interface_sub_class = 6,
            .b_interface_protocol = 80,
            .i_interface = 0,
        },
    .ep1_1 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x81,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .ss_companion1 =
        {
            .b_length = sizeof(usb_ss_ep_comp_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion),
            .b_max_burst = 3,
            .bm_attributes = 0,
            .w_bytes_per_interval = 0,
        },
    .ep1_2 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 2,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .ss_companion2 =
        {
            .b_length = sizeof(usb_ss_ep_comp_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion),
            .b_max_burst = 3,
            .bm_attributes = 0,
            .w_bytes_per_interval = 0,
        },
    .alt1_interface1 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 1,
            .b_num_endpoints = 2,
            .b_interface_class = 8,
            .b_interface_sub_class = 6,
            .b_interface_protocol = 80,
            .i_interface = 0,
        },
    .ep1_alt1_1 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x85,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .ep1_alt1_2 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 3,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .alt1_interface2 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 2,
            .b_num_endpoints = 0,
            .b_interface_class = 8,
            .b_interface_sub_class = 6,
            .b_interface_protocol = 80,
            .i_interface = 0,
        },
    .interface2 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 1,
            .b_alternate_setting = 0,
            .b_num_endpoints = 1,
            .b_interface_class = 5,
            .b_interface_sub_class = 7,
            .b_interface_protocol = 8,
            .i_interface = 0,
        },
    .ep2_1 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x82,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .alt2_interface1 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 1,
            .b_alternate_setting = 1,
            .b_num_endpoints = 1,
            .b_interface_class = 5,
            .b_interface_sub_class = 7,
            .b_interface_protocol = 8,
            .i_interface = 0,
        },
    .ep2_alt1_1 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x1,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .interface3 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 2,
            .b_alternate_setting = 0,
            .b_num_endpoints = 0,
            .b_interface_class = 12,
            .b_interface_sub_class = 3,
            .b_interface_protocol = 6,
            .i_interface = 0,
        },
};

// AssociationCompositeTest tests UsbComposite's ability to process interface descriptors
// that have association descriptors.
using AssociationCompositeTest = UsbCompositeTest<&kTestAssociation>;

template <>
void AssociationCompositeTest::ExpectConfigureEndpoints(MockUsb& usb) {
  usb.ExpectEnableEndpoint(ZX_OK, kTestAssociation.ep1_2, kTestAssociation.ss_companion2, true);
  usb.ExpectEnableEndpoint(ZX_OK, kTestAssociation.ep1_1, kTestAssociation.ss_companion1, true);
  usb.ExpectEnableEndpoint(ZX_OK, kTestAssociation.ep2_1, {}, true);
}

TEST_F(AssociationCompositeTest, InitTest) {
  InitTest(std::vector<init_test_expected_values_t>{
      init_test_expected_values_t{
          .name = "asc-000",
          .length = reinterpret_cast<uintptr_t>(&kTestAssociation.interface3) -
                    reinterpret_cast<uintptr_t>(&kTestAssociation.association),
          .start = reinterpret_cast<const uint8_t*>(&kTestAssociation.association),
      },
      init_test_expected_values_t{
          .name = "ifc-002",
          .length = sizeof(kTestAssociation.interface3),
          .start = reinterpret_cast<const uint8_t*>(&kTestAssociation.interface3),
      },
  });
}

constexpr struct two_assoc_config_t {
  usb_configuration_descriptor_t config;
  usb_interface_assoc_descriptor_t association1;
  usb_interface_descriptor_t interface1;
  usb_endpoint_descriptor_t ep1_1;
  usb_ss_ep_comp_descriptor_t ss_companion1;
  usb_endpoint_descriptor_t ep1_2;
  usb_ss_ep_comp_descriptor_t ss_companion2;
  usb_interface_descriptor_t alt1_interface1;
  usb_endpoint_descriptor_t ep1_alt1_1;
  usb_endpoint_descriptor_t ep1_alt1_2;
  usb_interface_descriptor_t alt1_interface2;
  usb_interface_descriptor_t interface2;
  usb_endpoint_descriptor_t ep2_1;
  usb_interface_descriptor_t alt2_interface1;
  usb_endpoint_descriptor_t ep2_alt1_1;
  usb_interface_assoc_descriptor_t association2;
  usb_interface_descriptor_t interface3;
} kTestTwoAssociation = {
    .config =
        {
            .b_length = sizeof(usb_configuration_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration),
            .w_total_length = sizeof(two_assoc_config_t),
            .b_num_interfaces = 3,
            .b_configuration_value = 0,
            .i_configuration = 0,
            .bm_attributes =
                fdescriptor::kConfigurationReserved7 | fdescriptor::kConfigurationSelfPowered,
            .b_max_power = 4,
        },
    .association1 =
        {
            .b_length = sizeof(usb_interface_assoc_descriptor_t),
            .b_descriptor_type =
                fidl::ToUnderlying(fdescriptor::DescriptorType::kInterfaceAssociation),
            .b_first_interface = 0,
            .b_interface_count = 2,
            .b_function_class = 5,
            .b_function_sub_class = 3,
            .b_function_protocol = 2,
            .i_function = 1,
        },
    .interface1 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 0,
            .b_num_endpoints = 2,
            .b_interface_class = 8,
            .b_interface_sub_class = 6,
            .b_interface_protocol = 80,
            .i_interface = 0,
        },
    .ep1_1 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x81,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .ss_companion1 =
        {
            .b_length = sizeof(usb_ss_ep_comp_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion),
            .b_max_burst = 3,
            .bm_attributes = 0,
            .w_bytes_per_interval = 0,
        },
    .ep1_2 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 2,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .ss_companion2 =
        {
            .b_length = sizeof(usb_ss_ep_comp_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion),
            .b_max_burst = 3,
            .bm_attributes = 0,
            .w_bytes_per_interval = 0,
        },
    .alt1_interface1 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 1,
            .b_num_endpoints = 2,
            .b_interface_class = 8,
            .b_interface_sub_class = 6,
            .b_interface_protocol = 80,
            .i_interface = 0,
        },
    .ep1_alt1_1 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x85,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .ep1_alt1_2 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 3,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .alt1_interface2 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 2,
            .b_num_endpoints = 0,
            .b_interface_class = 8,
            .b_interface_sub_class = 6,
            .b_interface_protocol = 80,
            .i_interface = 0,
        },
    .interface2 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 1,
            .b_alternate_setting = 0,
            .b_num_endpoints = 1,
            .b_interface_class = 5,
            .b_interface_sub_class = 7,
            .b_interface_protocol = 8,
            .i_interface = 0,
        },
    .ep2_1 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x82,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .alt2_interface1 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 1,
            .b_alternate_setting = 1,
            .b_num_endpoints = 1,
            .b_interface_class = 5,
            .b_interface_sub_class = 7,
            .b_interface_protocol = 8,
            .i_interface = 0,
        },
    .ep2_alt1_1 =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x1,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = 1024,
            .b_interval = 0,
        },
    .association2 =
        {
            .b_length = sizeof(usb_interface_assoc_descriptor_t),
            .b_descriptor_type =
                fidl::ToUnderlying(fdescriptor::DescriptorType::kInterfaceAssociation),
            .b_first_interface = 2,
            .b_interface_count = 1,
            .b_function_class = 5,
            .b_function_sub_class = 3,
            .b_function_protocol = 2,
            .i_function = 1,
        },
    .interface3 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 2,
            .b_alternate_setting = 0,
            .b_num_endpoints = 0,
            .b_interface_class = 12,
            .b_interface_sub_class = 3,
            .b_interface_protocol = 6,
            .i_interface = 0,
        },
};

// TwoAssociationCompositeTest tests UsbComposite's ability to process interface descriptors
// that have consecutive association descriptors.
using TwoAssociationCompositeTest = UsbCompositeTest<&kTestTwoAssociation>;

template <>
void TwoAssociationCompositeTest::ExpectConfigureEndpoints(MockUsb& usb) {
  usb.ExpectEnableEndpoint(ZX_OK, kTestTwoAssociation.ep1_2, kTestTwoAssociation.ss_companion2,
                           true);
  usb.ExpectEnableEndpoint(ZX_OK, kTestTwoAssociation.ep1_1, kTestTwoAssociation.ss_companion1,
                           true);
  usb.ExpectEnableEndpoint(ZX_OK, kTestTwoAssociation.ep2_1, {}, true);
}

TEST_F(TwoAssociationCompositeTest, InitTest) {
  InitTest(std::vector<init_test_expected_values_t>{
      init_test_expected_values_t{
          .name = "asc-000",
          .length = reinterpret_cast<uintptr_t>(&kTestTwoAssociation.association2) -
                    reinterpret_cast<uintptr_t>(&kTestTwoAssociation.association1),
          .start = reinterpret_cast<const uint8_t*>(&kTestTwoAssociation.association1),
      },
      init_test_expected_values_t{
          .name = "asc-002",
          .length =
              sizeof(kTestTwoAssociation.association2) + sizeof(kTestTwoAssociation.interface3),
          .start = reinterpret_cast<const uint8_t*>(&kTestTwoAssociation.association2),
      },
  });
}

constexpr struct __PACKED truncated_assoc_config_t {
  usb_configuration_descriptor_t config;
  uint8_t truncated_association[2];
  usb_interface_descriptor_t interface1;
} kTestTruncatedAssociation = {
    .config =
        {
            .b_length = sizeof(usb_configuration_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration),
            .w_total_length = sizeof(truncated_assoc_config_t),
            .b_num_interfaces = 1,
            .b_configuration_value = 0,
            .i_configuration = 0,
            .bm_attributes =
                fdescriptor::kConfigurationReserved7 | fdescriptor::kConfigurationSelfPowered,
            .b_max_power = 4,
        },
    .truncated_association =
        {
            2,
            fidl::ToUnderlying(fdescriptor::DescriptorType::kInterfaceAssociation),
        },
    .interface1 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 0,
            .b_num_endpoints = 0,
            .b_interface_class = 8,
            .b_interface_sub_class = 6,
            .b_interface_protocol = 80,
            .i_interface = 0,
        },
};

using TruncatedAssociationCompositeTest = UsbCompositeTest<&kTestTruncatedAssociation>;

template <>
void TruncatedAssociationCompositeTest::ExpectConfigureEndpoints() {}

TEST_F(TruncatedAssociationCompositeTest, InitTest) {
  // The association descriptor's b_length (2) is smaller than
  // sizeof(usb_interface_assoc_descriptor_t), so descriptor parsing stops there and no
  // interfaces (including interface1) are published as children.
  InitTest(std::vector<init_test_expected_values_t>{});
}

constexpr struct __PACKED extended_assoc_config_t {
  usb_configuration_descriptor_t config;
  usb_interface_assoc_descriptor_t association;
  uint8_t association_extra[2];
  usb_interface_descriptor_t interface1;
  usb_interface_descriptor_t interface2;
} kTestExtendedAssociation = {
    .config =
        {
            .b_length = sizeof(usb_configuration_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration),
            .w_total_length = sizeof(extended_assoc_config_t),
            .b_num_interfaces = 2,
            .b_configuration_value = 0,
            .i_configuration = 0,
            .bm_attributes =
                fdescriptor::kConfigurationReserved7 | fdescriptor::kConfigurationSelfPowered,
            .b_max_power = 4,
        },
    .association =
        {
            .b_length = sizeof(usb_interface_assoc_descriptor_t) + 2,
            .b_descriptor_type =
                fidl::ToUnderlying(fdescriptor::DescriptorType::kInterfaceAssociation),
            .b_first_interface = 0,
            .b_interface_count = 1,
            .b_function_class = 5,
            .b_function_sub_class = 3,
            .b_function_protocol = 2,
            .i_function = 1,
        },
    .association_extra = {0xaa, 0xbb},
    .interface1 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 0,
            .b_num_endpoints = 0,
            .b_interface_class = 8,
            .b_interface_sub_class = 6,
            .b_interface_protocol = 80,
            .i_interface = 0,
        },
    .interface2 =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 1,
            .b_alternate_setting = 0,
            .b_num_endpoints = 0,
            .b_interface_class = 12,
            .b_interface_sub_class = 3,
            .b_interface_protocol = 6,
            .i_interface = 0,
        },
};

using ExtendedAssociationCompositeTest = UsbCompositeTest<&kTestExtendedAssociation>;

template <>
void ExtendedAssociationCompositeTest::ExpectConfigureEndpoints() {}

TEST_F(ExtendedAssociationCompositeTest, InitTest) {
  InitTest(std::vector<init_test_expected_values_t>{
      init_test_expected_values_t{
          .name = "asc-000",
          .length = reinterpret_cast<uintptr_t>(&kTestExtendedAssociation.interface2) -
                    reinterpret_cast<uintptr_t>(&kTestExtendedAssociation.association),
          .start = reinterpret_cast<const uint8_t*>(&kTestExtendedAssociation.association),
      },
      init_test_expected_values_t{
          .name = "ifc-001",
          .length = sizeof(kTestExtendedAssociation.interface2),
          .start = reinterpret_cast<const uint8_t*>(&kTestExtendedAssociation.interface2),
      },
  });
}

}  // namespace usb_composite
