// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fpromise/single_threaded_executor.h>
#include <lib/inspect/cpp/hierarchy.h>
#include <lib/inspect/cpp/reader.h>
#include <lib/inspect/testing/cpp/inspect.h>
#include <zircon/compiler.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <usb-inspect/usb-inspect.h>
#include <usb/descriptors.h>

namespace usb_inspect {
namespace fdescriptor = fuchsia_hardware_usb_descriptor;

using namespace inspect::testing;

class UsbInspectTest : public ::testing::Test {
 public:
  inspect::Hierarchy ReadInspect(const inspect::Inspector& inspector) {
    fpromise::result<inspect::Hierarchy> result =
        fpromise::run_single_threaded(inspect::ReadFromInspector(inspector));
    EXPECT_TRUE(result.is_ok());
    return std::move(result.value());
  }
};

TEST_F(UsbInspectTest, TestEndpointInspect) {
  inspect::Inspector inspector;
  EndpointInspect endpoint;

  endpoint.Init(inspector.GetRoot(), "endpoint_test", 3);

  endpoint.UpdateTxQueue(10);
  endpoint.UpdateRxQueue(20);
  endpoint.UpdateRxPendingProcessing(30);
  endpoint.AddTxBytes(1024);
  endpoint.AddRxBytes(2048);

  // Trigger throughput calculation (Total bytes = 1024 Tx + 2048 Rx = 3072 bytes over 1 second)
  endpoint.MeasureThroughput(zx::sec(1));

  endpoint.RecordEvent("event_1");
  endpoint.RecordEvent("event_2");
  endpoint.RecordEvent("event_3");
  endpoint.RecordEvent("event_4");  // should overwrite event_1 (modulo capacity 3)

  auto hierarchy = ReadInspect(inspector);

  auto* endpoint_node = hierarchy.GetByPath({"endpoint_test"});
  ASSERT_THAT(endpoint_node, ::testing::NotNull());

  // Assert basic properties
  EXPECT_THAT(endpoint_node->node(),
              PropertyList(::testing::UnorderedElementsAre(
                  UintIs("tx_pending_requests", 10), UintIs("rx_pending_requests", 20),
                  UintIs("rx_pending_processing", 30), UintIs("total_bytes_tx", 1024),
                  UintIs("total_bytes_rx", 2048), UintIs("max_bytes_per_second", 3072),
                  BoolIs("cancelled", false))));

  // Assert event history circular list size is exactly capacity 3
  auto* history_node = hierarchy.GetByPath({"endpoint_test", "event_history"});
  ASSERT_THAT(history_node, ::testing::NotNull());
  EXPECT_EQ(3u, history_node->children().size());
}

TEST_F(UsbInspectTest, TestDciInspect) {
  inspect::Inspector inspector;
  DciInspect dci;

  dci.Init(inspector.GetRoot(), "dci_test");
  dci.UpdateState("kPeripheralReady");
  dci.UpdateConnectionStatus(true, USB_SPEED_SUPER);
  dci.UpdateUsbMode(UsbMode::kPeripheral);

  auto hierarchy = ReadInspect(inspector);

  auto* dci_node = hierarchy.GetByPath({"dci_test"});
  ASSERT_THAT(dci_node, ::testing::NotNull());

  EXPECT_THAT(dci_node->node(),
              PropertyList(::testing::UnorderedElementsAre(
                  StringIs("state", "kPeripheralReady"), BoolIs("connected", true),
                  StringIs("speed", "super"), StringIs("usb_mode", "PERIPHERAL"))));
}

TEST_F(UsbInspectTest, TestFunctionInspect) {
  inspect::Inspector inspector;
  FunctionInspect func;

  func.Init(inspector.GetRoot(), "function_test", 2);
  func.UpdateConfiguration(1, true);
  func.UpdateDescriptorInfo(255, 66, 1);

  auto hierarchy = ReadInspect(inspector);

  auto* func_node = hierarchy.GetByPath({"function_test"});
  ASSERT_THAT(func_node, ::testing::NotNull());

  EXPECT_THAT(func_node->node(),
              PropertyList(::testing::UnorderedElementsAre(
                  UintIs("index", 2), UintIs("configuration", 1), BoolIs("configured", true),
                  UintIs("interface_class", 255), UintIs("interface_subclass", 66),
                  UintIs("interface_protocol", 1))));
}

TEST_F(UsbInspectTest, TestDciInspectHistory) {
  inspect::Inspector inspector;
  DciInspect dci;

  // Initialize with control capacity 2 and connection capacity 2 to easily test circular buffer
  // wrap-around
  dci.Init(inspector.GetRoot(), "dci_test", 2);

  // 1. Verify General Event History
  dci.RecordEvent("dci_event_1");
  dci.RecordEvent("dci_event_2");

  // 2. Verify Control Transfer Circular History
  // standard GET_DESCRIPTOR
  dci.RecordControlTransfer({
      .request_type = 0x80,
      .request = 0x06,
      .value = 0x0100,
      .index = 0x0000,
      .length = 18,
      .status = ZX_OK,
      .actual_length = 18,
  });
  // standard SET_CONFIGURATION
  dci.RecordControlTransfer({
      .request_type = 0x00,
      .request = 0x09,
      .value = 0x0001,
      .index = 0x0000,
      .length = 0,
      .status = ZX_OK,
      .actual_length = 0,
  });
  // vendor request that failed
  dci.RecordControlTransfer({
      .request_type = 0x40,
      .request = 0x0A,
      .value = 0x1234,
      .index = 0x5678,
      .length = 8,
      .status = ZX_ERR_IO_REFUSED,
      .actual_length = 0,
  });  // Should overwrite the first entry

  auto hierarchy = ReadInspect(inspector);

  // Check general events
  auto* event_history = hierarchy.GetByPath({"dci_test", "event_history"});
  ASSERT_THAT(event_history, ::testing::NotNull());
  EXPECT_EQ(2u, event_history->children().size());

  // Check control transfers
  auto* control_history = hierarchy.GetByPath({"dci_test", "control_history"});
  ASSERT_THAT(control_history, ::testing::NotNull());
  EXPECT_EQ(2u, control_history->children().size());  // Exactly capacity 2

  // Verify the active items in circular list. Since idx 2 overwrote idx 0:
  // slot "1" should be set to standard SET_CONFIGURATION
  // slot "2" should be vendor request that failed
  auto* slot_1 = hierarchy.GetByPath({"dci_test", "control_history", "1"});
  auto* slot_2 = hierarchy.GetByPath({"dci_test", "control_history", "2"});
  ASSERT_THAT(slot_1, ::testing::NotNull());
  ASSERT_THAT(slot_2, ::testing::NotNull());

  EXPECT_THAT(slot_2->node(), PropertyList(::testing::UnorderedElementsAre(
                                  UintIs("bm_request_type", 0x40), UintIs("b_request", 0x0A),
                                  UintIs("w_value", 0x1234), UintIs("w_index", 0x5678),
                                  UintIs("w_length", 8), IntIs("status", ZX_ERR_IO_REFUSED),
                                  UintIs("response_length", 0), UintIs("@time", ::testing::_))));

  EXPECT_THAT(slot_1->node(), PropertyList(::testing::UnorderedElementsAre(
                                  UintIs("bm_request_type", 0x00), UintIs("b_request", 0x09),
                                  UintIs("w_value", 0x0001), UintIs("w_index", 0x0000),
                                  UintIs("w_length", 0), IntIs("status", ZX_OK),
                                  UintIs("response_length", 0), UintIs("@time", ::testing::_))));
}

TEST_F(UsbInspectTest, TestEventHistoryTrafficSnapshots) {
  inspect::Inspector inspector;
  EndpointInspect endpoint;

  endpoint.Init(inspector.GetRoot(), "endpoint_test", 5);

  endpoint.RecordEvent("state_changed: kStoppingUsb");
  endpoint.AddTxBytes(1024);
  endpoint.AddRxBytes(2048);
  endpoint.MeasureThroughput(zx::sec(1));

  endpoint.RecordEvent("state_changed: kOnline");
  endpoint.AddTxBytes(4096);

  auto hierarchy = ReadInspect(inspector);

  auto* snap_0 = hierarchy.GetByPath({"endpoint_test", "transfer_snapshots", "0"});
  ASSERT_THAT(snap_0, ::testing::NotNull());
  EXPECT_THAT(snap_0->node(), PropertyList(::testing::UnorderedElementsAre(
                                  UintIs("@time", ::testing::_), UintIs("total_bytes_tx", 0),
                                  UintIs("total_bytes_rx", 0), UintIs("max_bytes_per_second", 0))));

  auto* snap_1 = hierarchy.GetByPath({"endpoint_test", "transfer_snapshots", "1"});
  ASSERT_THAT(snap_1, ::testing::NotNull());
  EXPECT_THAT(snap_1->node(),
              PropertyList(::testing::UnorderedElementsAre(
                  UintIs("@time", ::testing::_), UintIs("total_bytes_tx", 1024),
                  UintIs("total_bytes_rx", 2048), UintIs("max_bytes_per_second", 3072))));
}

TEST_F(UsbInspectTest, TestEventHistoryTrafficSnapshotsEviction) {
  inspect::Inspector inspector;
  EndpointInspect endpoint;

  endpoint.Init(inspector.GetRoot(), "endpoint_test", 2, 2);

  endpoint.RecordEvent("event_0");
  endpoint.AddTxBytes(100);
  endpoint.RecordEvent("event_1");
  endpoint.AddTxBytes(200);
  endpoint.RecordEvent("event_2");  // Evicts event_0
  endpoint.AddTxBytes(300);

  auto hierarchy = ReadInspect(inspector);
  EXPECT_THAT(hierarchy.GetByPath({"endpoint_test", "transfer_snapshots", "0"}),
              ::testing::IsNull());
  auto* snap_1 = hierarchy.GetByPath({"endpoint_test", "transfer_snapshots", "1"});
  ASSERT_THAT(snap_1, ::testing::NotNull());
  EXPECT_THAT(snap_1->node(), PropertyList(::testing::UnorderedElementsAre(
                                  UintIs("@time", ::testing::_), UintIs("total_bytes_tx", 100),
                                  UintIs("total_bytes_rx", 0), UintIs("max_bytes_per_second", 0))));

  auto* snap_2 = hierarchy.GetByPath({"endpoint_test", "transfer_snapshots", "2"});
  ASSERT_THAT(snap_2, ::testing::NotNull());
  EXPECT_THAT(snap_2->node(), PropertyList(::testing::UnorderedElementsAre(
                                  UintIs("@time", ::testing::_), UintIs("total_bytes_tx", 300),
                                  UintIs("total_bytes_rx", 0), UintIs("max_bytes_per_second", 0))));
}

TEST_F(UsbInspectTest, TestDirectSnapshotTransferStats) {
  inspect::Inspector inspector;
  EndpointInspect endpoint;

  endpoint.Init(inspector.GetRoot(), "endpoint_test", 5);

  endpoint.AddTxBytes(500);
  endpoint.SnapshotTransferStats();

  endpoint.AddTxBytes(250);
  endpoint.SnapshotTransferStats();

  auto hierarchy = ReadInspect(inspector);
  auto* snap_0 = hierarchy.GetByPath({"endpoint_test", "transfer_snapshots", "0"});
  ASSERT_THAT(snap_0, ::testing::NotNull());
  EXPECT_THAT(snap_0->node(), PropertyList(::testing::UnorderedElementsAre(
                                  UintIs("@time", ::testing::_), UintIs("total_bytes_tx", 500),
                                  UintIs("total_bytes_rx", 0), UintIs("max_bytes_per_second", 0))));

  auto* snap_1 = hierarchy.GetByPath({"endpoint_test", "transfer_snapshots", "1"});
  ASSERT_THAT(snap_1, ::testing::NotNull());
  EXPECT_THAT(snap_1->node(), PropertyList(::testing::UnorderedElementsAre(
                                  UintIs("@time", ::testing::_), UintIs("total_bytes_tx", 750),
                                  UintIs("total_bytes_rx", 0), UintIs("max_bytes_per_second", 0))));
}

TEST_F(UsbInspectTest, TestZeroCapacity) {
  inspect::Inspector inspector;
  EndpointInspect endpoint;

  endpoint.Init(inspector.GetRoot(), "endpoint_test", 0, 0);
  endpoint.RecordEvent("event_0");
  endpoint.SnapshotTransferStats();

  auto hierarchy = ReadInspect(inspector);
  EXPECT_THAT(hierarchy.GetByPath({"endpoint_test", "event_history"}), ::testing::IsNull());
  EXPECT_THAT(hierarchy.GetByPath({"endpoint_test", "transfer_snapshots"}), ::testing::IsNull());
}

TEST_F(UsbInspectTest, TestTruncatedDescriptors) {
  inspect::Inspector inspector;
  FunctionInspect func;

  func.Init(inspector.GetRoot(), "function_test", 1);
  func.UpdateConfiguration(1, true);

  // Provide a truncated interface descriptor (b_length is smaller than
  // sizeof(usb_interface_descriptor_t))
  std::vector<uint8_t> truncated = {
      4,
      fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      0,
      0,  // 4 bytes only, but type is interface (4 == kInterface)
  };
  func.SetDescriptors(std::move(truncated));

  auto hierarchy = ReadInspect(inspector);
  auto* func_node = hierarchy.GetByPath({"function_test"});
  ASSERT_THAT(func_node, ::testing::NotNull());
  // Should not crash and should not have created child nodes for the truncated descriptor.
  EXPECT_THAT(hierarchy.GetByPath({"function_test", "interface-000"}), ::testing::IsNull());
  EXPECT_THAT(func_node->node(),
              PropertyList(::testing::Contains(UintIs("malformed_descriptors", 1))));
}

TEST_F(UsbInspectTest, TestInspectMalformedSsCompanionWithoutEp) {
  inspect::Inspector inspector;
  FunctionInspect func;
  func.Init(inspector.GetRoot(), "function_test", 1);

  struct {
    usb_interface_descriptor_t intf;
    usb_ss_ep_comp_descriptor_t ss_comp;
  } __PACKED malformed = {
      .intf =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
              .b_interface_number = 0,
              .b_alternate_setting = 0,
              .b_num_endpoints = 1,
              .b_interface_class = 8,
              .b_interface_sub_class = 6,
              .b_interface_protocol = 80,
              .i_interface = 0,
          },
      .ss_comp =
          {
              .b_length = sizeof(usb_ss_ep_comp_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion),
              .b_max_burst = 0,
              .bm_attributes = 0,
              .w_bytes_per_interval = 0,
          },
  };

  std::vector<uint8_t> descs(reinterpret_cast<uint8_t*>(&malformed),
                             reinterpret_cast<uint8_t*>(&malformed) + sizeof(malformed));
  func.SetDescriptors(std::move(descs));

  auto hierarchy = ReadInspect(inspector);
  ASSERT_THAT(hierarchy.GetByPath({"function_test"}), ::testing::NotNull());
}

TEST_F(UsbInspectTest, TestClassSpecificDescriptors) {
  inspect::Inspector inspector;
  FunctionInspect func;
  func.Init(inspector.GetRoot(), "function_test", 1);

  struct {
    usb_interface_descriptor_t intf;
    uint8_t hid_desc[9];
  } __PACKED cs_test = {
      .intf =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
              .b_interface_number = 0,
              .b_alternate_setting = 0,
              .b_num_endpoints = 0,
              .b_interface_class = 3,
              .b_interface_sub_class = 1,
              .b_interface_protocol = 2,
              .i_interface = 0,
          },
      .hid_desc = {9, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x3f, 0x00},
  };

  std::vector<uint8_t> descs(reinterpret_cast<uint8_t*>(&cs_test),
                             reinterpret_cast<uint8_t*>(&cs_test) + sizeof(cs_test));
  func.SetDescriptors(std::move(descs));

  auto hierarchy = ReadInspect(inspector);
  auto* desc_node = hierarchy.GetByPath({"function_test", "interface-000", "descriptor-0x21"});
  ASSERT_THAT(desc_node, ::testing::NotNull());

  EXPECT_THAT(desc_node->node(), PropertyList(::testing::UnorderedElementsAre(
                                     UintIs("type_code", 0x21), UintIs("length", 9),
                                     StringIs("hex_payload", "11 01 00 01 22 3f 00"))));
}

TEST_F(UsbInspectTest, TestInterfaceAssociationDescriptor) {
  inspect::Inspector inspector;
  FunctionInspect func;
  func.Init(inspector.GetRoot(), "function_test", 1);

  usb_interface_assoc_descriptor_t iad = {
      .b_length = sizeof(usb_interface_assoc_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterfaceAssociation),
      .b_first_interface = 2,
      .b_interface_count = 2,
      .b_function_class = 14,     // Video
      .b_function_sub_class = 3,  // Video Interface Collection
      .b_function_protocol = 0,
      .i_function = 0,
  };

  std::vector<uint8_t> descs(reinterpret_cast<uint8_t*>(&iad),
                             reinterpret_cast<uint8_t*>(&iad) + sizeof(iad));
  func.SetDescriptors(std::move(descs));

  auto hierarchy = ReadInspect(inspector);
  auto* iad_node = hierarchy.GetByPath({"function_test", "iad-0x02"});
  ASSERT_THAT(iad_node, ::testing::NotNull());

  EXPECT_THAT(iad_node->node(), PropertyList(::testing::UnorderedElementsAre(
                                    UintIs("first_interface", 2), UintIs("interface_count", 2),
                                    UintIs("function_class", 14), UintIs("function_subclass", 3),
                                    UintIs("function_protocol", 0))));
}

TEST_F(UsbInspectTest, TestTruncatedInterfaceAssociationDescriptor) {
  inspect::Inspector inspector;
  FunctionInspect func;
  func.Init(inspector.GetRoot(), "function_test", 1);

  // A truncated IAD with length smaller than sizeof(usb_interface_assoc_descriptor_t)
  std::vector<uint8_t> truncated_iad = {
      4,
      fidl::ToUnderlying(fdescriptor::DescriptorType::kInterfaceAssociation),
      2,
      2,
  };
  func.SetDescriptors(std::move(truncated_iad));

  auto hierarchy = ReadInspect(inspector);
  auto* func_node = hierarchy.GetByPath({"function_test"});
  ASSERT_THAT(func_node, ::testing::NotNull());
  EXPECT_THAT(hierarchy.GetByPath({"function_test", "iad-0x02"}), ::testing::IsNull());
  EXPECT_THAT(func_node->node(),
              PropertyList(::testing::IsSupersetOf({
                  UintIs("malformed_descriptors", 1),
                  StringIs("descriptor_error",
                           "Truncated interface_association at offset 0: type 0x0b, length 4 < "
                           "expected 8"),
              })));
}

TEST_F(UsbInspectTest, TestTruncatedEndpointDescriptor) {
  inspect::Inspector inspector;
  FunctionInspect func;
  func.Init(inspector.GetRoot(), "function_test", 1);

  struct {
    usb_interface_descriptor_t intf;
    uint8_t truncated_ep[4];
  } __PACKED bad_ep_test = {
      .intf =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
              .b_interface_number = 0,
              .b_alternate_setting = 0,
              .b_num_endpoints = 1,
              .b_interface_class = 0,
              .b_interface_sub_class = 0,
              .b_interface_protocol = 0,
              .i_interface = 0,
          },
      .truncated_ep =
          {
              4,
              fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
              0x81,
              2,
          },
  };

  std::vector<uint8_t> descs(reinterpret_cast<uint8_t*>(&bad_ep_test),
                             reinterpret_cast<uint8_t*>(&bad_ep_test) + sizeof(bad_ep_test));
  func.SetDescriptors(std::move(descs));

  auto hierarchy = ReadInspect(inspector);
  auto* func_node = hierarchy.GetByPath({"function_test"});
  ASSERT_THAT(func_node, ::testing::NotNull());
  EXPECT_THAT(func_node->node(),
              PropertyList(::testing::IsSupersetOf({
                  UintIs("malformed_descriptors", 1),
                  StringIs("descriptor_error",
                           "Truncated endpoint at offset 9: type 0x05, length 4 < expected 7"),
              })));
}

TEST_F(UsbInspectTest, TestMultipleInterfacesAndFlushing) {
  inspect::Inspector inspector;
  FunctionInspect func;
  func.Init(inspector.GetRoot(), "function_test", 1);

  struct {
    usb_interface_descriptor_t intf0;
    usb_endpoint_descriptor_t ep0;
    usb_interface_descriptor_t intf1;
    usb_endpoint_descriptor_t ep1;
  } __PACKED multi_test = {
      .intf0 =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
              .b_interface_number = 0,
              .b_alternate_setting = 0,
              .b_num_endpoints = 1,
              .b_interface_class = 0,
              .b_interface_sub_class = 0,
              .b_interface_protocol = 0,
              .i_interface = 0,
          },
      .ep0 =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
              .b_endpoint_address = 0x01,
              .bm_attributes = 2,
              .w_max_packet_size = 64,
              .b_interval = 0,
          },
      .intf1 =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
              .b_interface_number = 1,
              .b_alternate_setting = 0,
              .b_num_endpoints = 1,
              .b_interface_class = 0,
              .b_interface_sub_class = 0,
              .b_interface_protocol = 0,
              .i_interface = 0,
          },
      .ep1 =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
              .b_endpoint_address = 0x82,
              .bm_attributes = 2,
              .w_max_packet_size = 64,
              .b_interval = 0,
          },
  };

  std::vector<uint8_t> descs(reinterpret_cast<uint8_t*>(&multi_test),
                             reinterpret_cast<uint8_t*>(&multi_test) + sizeof(multi_test));
  func.SetDescriptors(std::move(descs));

  auto hierarchy = ReadInspect(inspector);

  ASSERT_THAT(hierarchy.GetByPath({"function_test", "interface-000", "endpoint-0x01"}),
              ::testing::NotNull());
  ASSERT_THAT(hierarchy.GetByPath({"function_test", "interface-001", "endpoint-0x82"}),
              ::testing::NotNull());
}

TEST_F(UsbInspectTest, TestValidSsEpCompanion) {
  inspect::Inspector inspector;
  FunctionInspect func;
  func.Init(inspector.GetRoot(), "function_test", 1);

  struct {
    usb_interface_descriptor_t intf;
    usb_endpoint_descriptor_t ep;
    usb_ss_ep_comp_descriptor_t ss_comp;
  } __PACKED valid_test = {
      .intf =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
              .b_interface_number = 0,
              .b_alternate_setting = 0,
              .b_num_endpoints = 1,
              .b_interface_class = 0,
              .b_interface_sub_class = 0,
              .b_interface_protocol = 0,
              .i_interface = 0,
          },
      .ep =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
              .b_endpoint_address = 0x81,
              .bm_attributes = 2,
              .w_max_packet_size = 1024,
              .b_interval = 0,
          },
      .ss_comp =
          {
              .b_length = sizeof(usb_ss_ep_comp_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion),
              .b_max_burst = 3,
              .bm_attributes = 0,
              .w_bytes_per_interval = 0,
          },
  };

  std::vector<uint8_t> descs(reinterpret_cast<uint8_t*>(&valid_test),
                             reinterpret_cast<uint8_t*>(&valid_test) + sizeof(valid_test));
  func.SetDescriptors(std::move(descs));

  auto hierarchy = ReadInspect(inspector);

  auto* comp_node =
      hierarchy.GetByPath({"function_test", "interface-000", "endpoint-0x81", "ss_companion"});
  ASSERT_THAT(comp_node, ::testing::NotNull());

  EXPECT_THAT(comp_node->node(), PropertyList(::testing::UnorderedElementsAre(
                                     UintIs("max_burst", 3), UintIs("attributes", 0),
                                     UintIs("bytes_per_interval", 0))));
}

TEST_F(UsbInspectTest, TestGenericDescriptorAtRoot) {
  inspect::Inspector inspector;
  FunctionInspect func;
  func.Init(inspector.GetRoot(), "function_test", 1);

  // A generic descriptor before any interface
  std::vector<uint8_t> descs = {
      0x05,              // b_length
      0xFF,              // b_descriptor_type (Vendor Specific)
      0xAA, 0xBB, 0xCC,  // Payload
  };
  func.SetDescriptors(std::move(descs));

  auto hierarchy = ReadInspect(inspector);
  auto* root_desc = hierarchy.GetByPath({"function_test", "descriptor-0xff"});
  ASSERT_THAT(root_desc, ::testing::NotNull());

  EXPECT_THAT(root_desc->node(), PropertyList(::testing::UnorderedElementsAre(
                                     UintIs("type_code", 0xFF), UintIs("length", 5),
                                     StringIs("hex_payload", "aa bb cc"))));
}

TEST_F(UsbInspectTest, TestEndpointInspectPoolAndCancel) {
  inspect::Inspector inspector;
  EndpointInspect endpoint;
  endpoint.Init(inspector.GetRoot(), "endpoint_test");

  // Verify cancelled is visible even without pool counts callback set.
  {
    auto hierarchy = ReadInspect(inspector);
    auto* ep_node = hierarchy.GetByPath({"endpoint_test"});
    ASSERT_THAT(ep_node, ::testing::NotNull());
    EXPECT_THAT(ep_node->node(), PropertyList(::testing::Contains(BoolIs("cancelled", false))));
    EXPECT_THAT(ep_node->node(), PropertyList(::testing::Not(::testing::Contains(
                                     UintIs("in_flight_requests", ::testing::_)))));
  }

  endpoint.SetPoolCountsCallback([]() -> std::pair<size_t, size_t> { return {3u, 16u}; });

  {
    auto hierarchy = ReadInspect(inspector);
    auto* ep_node = hierarchy.GetByPath({"endpoint_test"});
    ASSERT_THAT(ep_node, ::testing::NotNull());
    EXPECT_THAT(ep_node->node(),
                PropertyList(::testing::Contains(UintIs("in_flight_requests", 3u))));
    EXPECT_THAT(ep_node->node(), PropertyList(::testing::Contains(UintIs("total_requests", 16u))));
    EXPECT_THAT(ep_node->node(), PropertyList(::testing::Contains(BoolIs("cancelled", false))));
  }

  endpoint.OnCancelAllComplete();
  {
    auto hierarchy = ReadInspect(inspector);
    auto* ep_node = hierarchy.GetByPath({"endpoint_test"});
    ASSERT_THAT(ep_node, ::testing::NotNull());
    EXPECT_THAT(ep_node->node(), PropertyList(::testing::Contains(BoolIs("cancelled", true))));
    auto* slot_0 = hierarchy.GetByPath({"endpoint_test", "event_history", "0"});
    ASSERT_THAT(slot_0, ::testing::NotNull());
    EXPECT_THAT(slot_0->node(),
                PropertyList(::testing::Contains(StringIs("event", "CancelAll complete"))));
  }

  endpoint.SetCancelled(false);
  {
    auto hierarchy = ReadInspect(inspector);
    auto* ep_node = hierarchy.GetByPath({"endpoint_test"});
    ASSERT_THAT(ep_node, ::testing::NotNull());
    EXPECT_THAT(ep_node->node(), PropertyList(::testing::Contains(BoolIs("cancelled", false))));
  }

  // Re-initialization should clear pool callback and reset cancelled.
  endpoint.Init(inspector.GetRoot(), "endpoint_test");
  {
    auto hierarchy = ReadInspect(inspector);
    auto* ep_node = hierarchy.GetByPath({"endpoint_test"});
    ASSERT_THAT(ep_node, ::testing::NotNull());
    EXPECT_THAT(ep_node->node(), PropertyList(::testing::Contains(BoolIs("cancelled", false))));
    EXPECT_THAT(ep_node->node(), PropertyList(::testing::Not(::testing::Contains(
                                     UintIs("in_flight_requests", ::testing::_)))));
  }
}

TEST_F(UsbInspectTest, TestFunctionDriverInspect) {
  inspect::Inspector inspector;
  inspect::Node driver_node = inspector.GetRoot().CreateChild("func_drv");
  FunctionDriverInspect func;
  func.Init(driver_node, "kAwaitingUsbConnection");

  {
    auto hierarchy = ReadInspect(inspector);
    auto* node = hierarchy.GetByPath({"func_drv"});
    ASSERT_THAT(node, ::testing::NotNull());
    EXPECT_THAT(
        node->node(),
        PropertyList(::testing::UnorderedElementsAre(
            BoolIs("online", false), StringIs("state", "kAwaitingUsbConnection"),
            BoolIs("configured", false), BoolIs("unconfiguring", false),
            UintIs("set_configured_true_count", 0u), UintIs("set_configured_false_count", 0u),
            IntIs("last_unconfigure_duration_ns", 0))));
    EXPECT_THAT(hierarchy.GetByPath({"func_drv", "event_history"}), ::testing::IsNull());
  }

  func.OnConfigured();
  func.SetOnline(true);
  func.SetState("kOnline");
  {
    auto hierarchy = ReadInspect(inspector);
    auto* node = hierarchy.GetByPath({"func_drv"});
    ASSERT_THAT(node, ::testing::NotNull());
    EXPECT_THAT(node->node(), PropertyList(::testing::Contains(BoolIs("online", true))));
    EXPECT_THAT(node->node(), PropertyList(::testing::Contains(StringIs("state", "kOnline"))));
    EXPECT_THAT(node->node(), PropertyList(::testing::Contains(BoolIs("configured", true))));
    EXPECT_THAT(node->node(),
                PropertyList(::testing::Contains(UintIs("set_configured_true_count", 1u))));
    auto* slot_0 = hierarchy.GetByPath({"func_drv", "event_history", "0"});
    ASSERT_THAT(slot_0, ::testing::NotNull());
    EXPECT_THAT(slot_0->node(),
                PropertyList(::testing::Contains(StringIs("event", "SetConfigured(true)"))));
  }

  func.OnUnconfigureStart();
  {
    auto hierarchy = ReadInspect(inspector);
    auto* node = hierarchy.GetByPath({"func_drv"});
    ASSERT_THAT(node, ::testing::NotNull());
    EXPECT_THAT(node->node(), PropertyList(::testing::Contains(BoolIs("unconfiguring", true))));
    EXPECT_THAT(node->node(),
                PropertyList(::testing::Contains(UintIs("set_configured_false_count", 1u))));
  }

  func.OnUnconfigureComplete();
  {
    auto hierarchy = ReadInspect(inspector);
    auto* node = hierarchy.GetByPath({"func_drv"});
    ASSERT_THAT(node, ::testing::NotNull());
    EXPECT_THAT(node->node(), PropertyList(::testing::Contains(BoolIs("configured", false))));
    EXPECT_THAT(node->node(), PropertyList(::testing::Contains(BoolIs("unconfiguring", false))));
    const auto* dur =
        node->node().get_property<inspect::IntPropertyValue>("last_unconfigure_duration_ns");
    ASSERT_NE(dur, nullptr);
    EXPECT_GE(dur->value(), 0);
  }

  func.OnUnconfigureStart();
  func.OnUnconfigureComplete(/*immediate=*/true);
  {
    auto hierarchy = ReadInspect(inspector);
    auto* node = hierarchy.GetByPath({"func_drv"});
    ASSERT_THAT(node, ::testing::NotNull());
    EXPECT_THAT(node->node(),
                PropertyList(::testing::Contains(IntIs("last_unconfigure_duration_ns", 0))));
  }

  // Verify ResetUnconfiguring clears unconfiguring without altering duration or logging reply.
  func.OnUnconfigureStart();
  EXPECT_TRUE(func.is_unconfiguring());
  func.ResetUnconfiguring();
  EXPECT_FALSE(func.is_unconfiguring());
  {
    auto hierarchy = ReadInspect(inspector);
    auto* node = hierarchy.GetByPath({"func_drv"});
    ASSERT_THAT(node, ::testing::NotNull());
    EXPECT_THAT(node->node(), PropertyList(::testing::Contains(BoolIs("unconfiguring", false))));
  }
}

}  // namespace usb_inspect
