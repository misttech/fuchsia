// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ftdi.h"

#include <endian.h>
#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <fuchsia/hardware/usb/cpp/banjo.h>
#include <lib/ddk/driver.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/zx/time.h>
#include <string.h>
#include <zircon/compiler.h>

#include <algorithm>
#include <memory>
#include <mutex>
#include <thread>
#include <utility>
#include <vector>

#include <gtest/gtest.h>
#include <usb/descriptors.h>
#include <usb/usb-request.h>
#include <usb/usb.h>

#include "src/devices/testing/mock-ddk/mock-device.h"
#include "src/lib/testing/predicates/status.h"

namespace ftdi_serial {
namespace {

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

// Spans several write requests but fits in the first batch, so every request is queued directly
// by Write() and none is refilled from WriteComplete().
constexpr size_t kWriteSize = 2 * kUsbBufSize + 904;
static_assert(kWriteSize > kUsbBufSize);
static_assert(kWriteSize <= kWriteReqCount * kUsbBufSize);
constexpr size_t kExpectedWriteRequests = (kWriteSize + kUsbBufSize - 1) / kUsbBufSize;

constexpr uint8_t kBulkInAddr = fidl::ToUnderlying(fdescriptor::EndpointDirection::kIn) | 1;
constexpr uint8_t kBulkOutAddr = fidl::ToUnderlying(fdescriptor::EndpointDirection::kOut) | 2;

// A fake USB parent that holds on to the requests queued by the driver until the test completes
// them. Methods that the driver does not use are stubbed out.
class FakeUsb : public ddk::UsbProtocol<FakeUsb> {
 public:
  struct QueuedRequest {
    usb_request_t* request;
    usb_request_complete_callback_t complete_cb;
  };

  usb_protocol_t GetProto() { return {&usb_protocol_ops_, this}; }

  size_t bulk_out_request_count() {
    std::lock_guard lock(mutex_);
    return bulk_out_requests_.size();
  }

  // Transfers ownership of the queued bulk OUT requests to the caller, who must complete them.
  std::vector<QueuedRequest> TakeBulkOutRequests() {
    std::lock_guard lock(mutex_);
    return std::exchange(bulk_out_requests_, {});
  }

  zx_status_t UsbControlIn(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                           int64_t timeout, uint8_t* out_read_buffer, size_t read_size,
                           size_t* out_read_actual) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx_status_t UsbControlOut(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                            int64_t timeout, const uint8_t* write_buffer, size_t write_size) {
    return ZX_OK;
  }

  void UsbRequestQueue(usb_request_t* request, const usb_request_complete_callback_t* completion) {
    std::lock_guard lock(mutex_);
    if (request->header.ep_address == kBulkOutAddr) {
      bulk_out_requests_.push_back({request, *completion});
    } else {
      bulk_in_requests_.push_back({request, *completion});
    }
  }

  usb_speed_t UsbGetSpeed() { return USB_SPEED_HIGH; }

  zx_status_t UsbSetInterface(uint8_t interface_number, uint8_t alt_setting) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  uint8_t UsbGetConfiguration() { return 0; }

  zx_status_t UsbSetConfiguration(uint8_t configuration) { return ZX_ERR_NOT_SUPPORTED; }

  zx_status_t UsbEnableEndpoint(const usb_endpoint_descriptor_t* ep_desc,
                                const usb_ss_ep_comp_descriptor_t* ss_com_desc, bool enable) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx_status_t UsbResetEndpoint(uint8_t ep_address) { return ZX_ERR_NOT_SUPPORTED; }

  zx_status_t UsbResetDevice() { return ZX_ERR_NOT_SUPPORTED; }

  size_t UsbGetMaxTransferSize(uint8_t ep_address) { return 0; }

  uint32_t UsbGetDeviceId() { return 0; }

  void UsbGetDeviceDescriptor(usb_device_descriptor_t* out_desc) {}

  zx_status_t UsbGetConfigurationDescriptorLength(uint8_t configuration, size_t* out_length) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx_status_t UsbGetConfigurationDescriptor(uint8_t configuration, uint8_t* out_desc_buffer,
                                            size_t desc_size, size_t* out_desc_actual) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  size_t UsbGetDescriptorsLength() { return sizeof(descriptors_); }

  void UsbGetDescriptors(uint8_t* out_descs_buffer, size_t descs_size, size_t* out_descs_actual) {
    *out_descs_actual = std::min(descs_size, sizeof(descriptors_));
    memcpy(out_descs_buffer, &descriptors_, *out_descs_actual);
  }

  zx_status_t UsbGetStringDescriptor(uint8_t desc_id, uint16_t lang_id, uint16_t* out_lang_id,
                                     uint8_t* out_string_buffer, size_t string_size,
                                     size_t* out_string_actual) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx_status_t UsbCancelAll(uint8_t ep_address) {
    std::vector<QueuedRequest> canceled;
    {
      std::lock_guard lock(mutex_);
      canceled =
          std::exchange(ep_address == kBulkOutAddr ? bulk_out_requests_ : bulk_in_requests_, {});
    }
    for (QueuedRequest& queued : canceled) {
      usb_request_complete(queued.request, ZX_ERR_IO_NOT_PRESENT, 0, &queued.complete_cb);
    }
    return ZX_OK;
  }

  uint64_t UsbGetCurrentFrame() { return 0; }

  size_t UsbGetRequestSize() { return sizeof(usb_request_t); }

 private:
  [[maybe_unused]] struct {
    usb_interface_descriptor_t interface;
    usb_endpoint_descriptor_t bulk_in;
    usb_endpoint_descriptor_t bulk_out;
  } __PACKED descriptors_ = {
      .interface =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
              .b_interface_number = 0,
              .b_alternate_setting = 0,
              .b_num_endpoints = 2,
              .b_interface_class = 0xFF,
              .b_interface_sub_class = 0xFF,
              .b_interface_protocol = 0xFF,
              .i_interface = 0,
          },
      .bulk_in =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
              .b_endpoint_address = kBulkInAddr,
              .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
              .w_max_packet_size = htole16(512),
              .b_interval = 0,
          },
      .bulk_out =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
              .b_endpoint_address = kBulkOutAddr,
              .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
              .w_max_packet_size = htole16(512),
              .b_interval = 0,
          },
  };

  std::mutex mutex_;
  std::vector<QueuedRequest> bulk_in_requests_ __TA_GUARDED(mutex_);
  std::vector<QueuedRequest> bulk_out_requests_ __TA_GUARDED(mutex_);
};

class FtdiDeviceTest : public testing::Test {
 public:
  void SetUp() override {
    fake_parent_ = MockDevice::FakeRootParent();
    usb_protocol_t proto = fake_usb_.GetProto();
    fake_parent_->AddProtocol(ZX_PROTOCOL_USB, proto.ops, proto.ctx);

    auto device = std::make_unique<FtdiDevice>(fake_parent_.get());
    ASSERT_OK(device->Bind());
    // The device is now owned by mock-ddk and is freed by DdkRelease().
    device_ = device.release();
  }

  void TearDown() override {
    if (device_) {
      device_->DdkAsyncRemove();
      EXPECT_OK(mock_ddk::ReleaseFlaggedDevices(fake_parent_.get()));
      device_ = nullptr;
    }
  }

 protected:
  FakeUsb fake_usb_;
  std::shared_ptr<MockDevice> fake_parent_;
  FtdiDevice* device_ = nullptr;
};

// Regression test for https://fxbug.dev/520618993: each bulk OUT request must describe only the
// bytes copied into its buffer, not the whole remaining write.
TEST_F(FtdiDeviceTest, RequestLengthIsBytesCopied) {
  std::vector<uint8_t> write_data(kWriteSize);
  for (size_t i = 0; i < write_data.size(); i++) {
    write_data[i] = static_cast<uint8_t>(i);
  }

  // Write() blocks until all of its USB requests complete, so call it from another thread while
  // this thread runs the dispatcher that serves the driver's serialimpl FIDL server.
  zx_status_t write_status = ZX_ERR_INTERNAL;
  std::thread writer(
      [&]() { write_status = device_->Write(write_data.data(), write_data.size()); });

  mock_ddk::GetDriverRuntime()->RunUntil([&]() { return fake_usb_.bulk_out_request_count() > 0; },
                                         zx::duration::infinite());

  std::vector<FakeUsb::QueuedRequest> requests = fake_usb_.TakeBulkOutRequests();
  EXPECT_EQ(requests.size(), kExpectedWriteRequests);

  std::vector<uint8_t> sent_data;
  for (const FakeUsb::QueuedRequest& queued : requests) {
    usb_request_t* request = queued.request;
    const size_t expected_length =
        std::min<size_t>(write_data.size() - sent_data.size(), request->size);
    EXPECT_LE(request->header.length, request->size);
    EXPECT_EQ(request->header.length, expected_length);

    const size_t offset = sent_data.size();
    sent_data.resize(offset + expected_length);
    EXPECT_EQ(usb_request_copy_from(request, sent_data.data() + offset, expected_length, 0),
              static_cast<ssize_t>(expected_length));
  }
  EXPECT_EQ(sent_data, write_data);

  // Complete the requests so that Write() returns, even if the checks above failed.
  for (FakeUsb::QueuedRequest& queued : requests) {
    usb_request_complete(queued.request, ZX_OK, queued.request->header.length, &queued.complete_cb);
  }
  writer.join();
  EXPECT_OK(write_status);
}

}  // namespace
}  // namespace ftdi_serial
