// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <fuchsia/hardware/usb/cpp/banjo.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/zx/channel.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/time.h>
#include <lib/zx/vmo.h>

#include <algorithm>
#include <cstring>
#include <mutex>
#include <vector>

#include <gtest/gtest.h>
#include <usb/usb-request.h>
#include <usb/video.h>

#include "src/camera/drivers/usb_video/usb_video_stream.h"

namespace camera::usb_video {

namespace {

// Streaming configuration reported by the fake device.  These values are what the
// descriptor blob below advertises, and what the probe/commit control exchange
// negotiates.
constexpr uint8_t kVsInterfaceNumber = 1;
constexpr uint8_t kVsAltSetting = 1;
constexpr uint8_t kEndpointAddress = 0x81;
constexpr uint32_t kClockFrequency = 1000000;
constexpr uint16_t kWidth = 640;
constexpr uint16_t kHeight = 480;
constexpr uint32_t kStride = 640;
constexpr uint32_t kFrameInterval = 333333;
constexpr uint16_t kIsocBandwidth = 1024;
// Negotiated max frame size (10KB).
constexpr uint32_t kMaxVideoFrameSize = 10240;

template <typename T>
void AppendDescriptor(std::vector<uint8_t>& blob, const T& descriptor) {
  const auto* bytes = reinterpret_cast<const uint8_t*>(&descriptor);
  blob.insert(blob.end(), bytes, bytes + sizeof(T));
}

// Builds the descriptor blob that `LoadStreamingSettings` parses.  It describes a
// single NV12 640x480 format served over an isochronous endpoint.
std::vector<uint8_t> BuildDescriptors() {
  std::vector<uint8_t> blob;

  // Standard VideoControl interface.  Only its presence matters; it marks the start
  // of the class specific control descriptors.
  AppendDescriptor(
      blob, usb_interface_descriptor_t{
                .b_length = sizeof(usb_interface_descriptor_t),
                .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
                .b_interface_number = 0,
                .b_alternate_setting = 0,
                .b_num_endpoints = 0,
                .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kVideo),
                .b_interface_sub_class = fidl::ToUnderlying(fdescriptor::VideoSubclass::kControl),
                .b_interface_protocol = 0,
                .i_interface = 0,
            });

  // Class specific VideoControl header.  The driver only reads the clock frequency.
  AppendDescriptor(
      blob,
      usb_video_vc_header_desc{
          .bLength = sizeof(usb_video_vc_header_desc),
          .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface),
          .bDescriptorSubtype = fidl::ToUnderlying(fdescriptor::VideoVcDescriptorSubtype::kHeader),
          .bcdUVC = 0x0150,
          .wTotalLength = sizeof(usb_video_vc_header_desc),
          .dwClockFrequency = kClockFrequency,
          .bInCollection = 0,
      });

  // Standard VideoStreaming interface, alternate setting 0 (no bandwidth).
  AppendDescriptor(
      blob, usb_interface_descriptor_t{
                .b_length = sizeof(usb_interface_descriptor_t),
                .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
                .b_interface_number = kVsInterfaceNumber,
                .b_alternate_setting = 0,
                .b_num_endpoints = 0,
                .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kVideo),
                .b_interface_sub_class = fidl::ToUnderlying(fdescriptor::VideoSubclass::kStreaming),
                .b_interface_protocol = 0,
                .i_interface = 0,
            });

  // Class specific VideoStreaming input header, advertising a single format.
  AppendDescriptor(
      blob, usb_video_vs_input_header_desc_short{
                .bLength = sizeof(usb_video_vs_input_header_desc_short),
                .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface),
                .bDescriptorSubtype =
                    fidl::ToUnderlying(fdescriptor::VideoVsDescriptorSubtype::kInputHeader),
                .bNumFormats = 1,
                .wTotalLength = sizeof(usb_video_vs_input_header_desc_short),
                .bEndpointAddress = kEndpointAddress,
                .bmInfo = 0,
                .bTerminalLink = 0,
                .bStillCaptureMethod = 0,
                .bTriggerSupport = 0,
                .bTriggerUsage = 0,
                .bControlSize = 0,
            });

  // Uncompressed NV12 format with a single frame descriptor.
  usb_video_vs_uncompressed_format_desc format_desc{
      .bLength = sizeof(usb_video_vs_uncompressed_format_desc),
      .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface),
      .bDescriptorSubType =
          fidl::ToUnderlying(fdescriptor::VideoVsDescriptorSubtype::kFormatUncompressed),
      .bFormatIndex = 1,
      .bNumFrameDescriptors = 1,
      .guidFormat = {},
      .bBitsPerPixel = 12,
      .bDefaultFrameIndex = 1,
      .bAspectRatioX = 0,
      .bAspectRatioY = 0,
      .bmInterfaceFlags = 0,
      .bCopyProtect = 0,
  };
  memcpy(format_desc.guidFormat, kUsbVideoGuidNv12Value, sizeof(format_desc.guidFormat));
  AppendDescriptor(blob, format_desc);

  // The driver derives the stride from dwMaxVideoFrameBufferSize / wHeight.
  AppendDescriptor(
      blob, usb_video_vs_frame_desc{
                .bLength = sizeof(usb_video_vs_frame_desc),
                .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface),
                .bDescriptorSubType =
                    fidl::ToUnderlying(fdescriptor::VideoVsDescriptorSubtype::kFrameUncompressed),
                .bFrameIndex = 1,
                .bmCapabilities = 0,
                .wWidth = kWidth,
                .wHeight = kHeight,
                .dwMinBitRate = 0,
                .dwMaxBitRate = 0,
                .dwMaxVideoFrameBufferSize = kStride * kHeight,
                .dwDefaultFrameInterval = kFrameInterval,
                .bFrameIntervalType = 0,
            });

  // Standard VideoStreaming interface, alternate setting 1, and its isochronous
  // endpoint.  This is the setting the driver selects for streaming.
  AppendDescriptor(
      blob, usb_interface_descriptor_t{
                .b_length = sizeof(usb_interface_descriptor_t),
                .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
                .b_interface_number = kVsInterfaceNumber,
                .b_alternate_setting = kVsAltSetting,
                .b_num_endpoints = 1,
                .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kVideo),
                .b_interface_sub_class = fidl::ToUnderlying(fdescriptor::VideoSubclass::kStreaming),
                .b_interface_protocol = 0,
                .i_interface = 0,
            });
  AppendDescriptor(
      blob, usb_endpoint_descriptor_t{
                .b_length = sizeof(usb_endpoint_descriptor_t),
                .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
                .b_endpoint_address = kEndpointAddress,
                .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kIsochronous),
                .w_max_packet_size = kIsocBandwidth,
                .b_interval = 1,
            });

  return blob;
}

// A fake banjo server implementing the usb protocol on behalf of the parent device.
//
// Only the calls the driver makes are implemented with meaningful behaviour; the
// remainder return benign defaults.
//
// Banjo calls are synchronous C calls, so this class may be entered from any of the
// driver's threads. Its state is therefore guarded by |lock_|.
class FakeUsbServer : public ddk::UsbProtocol<FakeUsbServer, ddk::internal::base_protocol> {
 public:
  FakeUsbServer() : descriptors_(BuildDescriptors()) {}

  compat::DeviceServer::BanjoConfig GetBanjoConfig() {
    compat::DeviceServer::BanjoConfig config{ZX_PROTOCOL_USB};
    config.callbacks[ZX_PROTOCOL_USB] = banjo_server_.callback();
    return config;
  }

  uint64_t UsbGetDescriptorsLength() { return descriptors_.size(); }

  void UsbGetDescriptors(uint8_t* out_descs_buffer, size_t descs_size, size_t* out_descs_actual) {
    size_t copied = std::min(descs_size, descriptors_.size());
    memcpy(out_descs_buffer, descriptors_.data(), copied);
    *out_descs_actual = copied;
  }

  // Accepts both the PROBE and COMMIT SET_CUR requests of the format negotiation.
  zx_status_t UsbControlOut(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                            zx_time_t timeout, const uint8_t* write_buffer, size_t write_size) {
    return ZX_OK;
  }

  // Answers the PROBE GET_CUR request with the negotiated frame and payload sizes.
  zx_status_t UsbControlIn(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                           zx_time_t timeout, uint8_t* out_read_buffer, size_t read_size,
                           size_t* out_read_actual) {
    usb_video_vc_probe_and_commit_controls result;
    memset(&result, 0, sizeof(result));
    result.bmHint = fdescriptor::kVideoBmHintFrameInterval;
    result.bFormatIndex = 1;
    result.bFrameIndex = 1;
    result.dwFrameInterval = kFrameInterval;
    result.dwMaxVideoFrameSize = kMaxVideoFrameSize;
    result.dwMaxPayloadTransferSize = kIsocBandwidth;

    std::lock_guard guard(lock_);
    size_t copied = std::min({read_size, sizeof(result), probe_len_});
    memcpy(out_read_buffer, &result, copied);
    *out_read_actual = copied;
    return ZX_OK;
  }

  void SetProbeLen(size_t len) {
    std::lock_guard guard(lock_);
    probe_len_ = len;
  }

  uint64_t UsbGetRequestSize() { return sizeof(usb_request_t); }

  zx_status_t UsbSetInterface(uint8_t interface_number, uint8_t alt_setting) { return ZX_OK; }

  // Requests are retained rather than completed inline.  `UsbState::RequestComplete`
  // re-queues requests while holding its lock, so completing synchronously here
  // would recurse indefinitely.
  void UsbRequestQueue(usb_request_t* usb_request,
                       const usb_request_complete_callback_t* complete_cb) {
    std::lock_guard guard(lock_);
    queued_requests_.push_back({usb_request, *complete_cb});
  }

  // Drains the queued requests so the driver can observe that the stream stopped.
  zx_status_t UsbCancelAll(uint8_t ep_address) {
    std::vector<QueuedRequest> to_cancel;
    {
      std::lock_guard guard(lock_);
      to_cancel.swap(queued_requests_);
    }
    // The completion callbacks re-enter the driver, so they run without |lock_| held.
    for (QueuedRequest& queued : to_cancel) {
      queued.request->response.status = ZX_ERR_CANCELED;
      queued.request->response.actual = 0;
      queued.callback.callback(queued.callback.ctx, queued.request);
    }
    return ZX_OK;
  }

  // Unused by the driver under test.
  usb_speed_t UsbGetSpeed() { return 0; }
  uint8_t UsbGetConfiguration() { return 0; }
  zx_status_t UsbSetConfiguration(uint8_t configuration) { return ZX_ERR_NOT_SUPPORTED; }
  zx_status_t UsbEnableEndpoint(const usb_endpoint_descriptor_t* ep_desc,
                                const usb_ss_ep_comp_descriptor_t* ss_com_desc, bool enable) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  zx_status_t UsbResetEndpoint(uint8_t ep_address) { return ZX_ERR_NOT_SUPPORTED; }
  zx_status_t UsbResetDevice() { return ZX_ERR_NOT_SUPPORTED; }
  uint64_t UsbGetMaxTransferSize(uint8_t ep_address) { return 0; }
  uint32_t UsbGetDeviceId() { return 0; }
  void UsbGetDeviceDescriptor(usb_device_descriptor_t* out_desc) {
    memset(out_desc, 0, sizeof(*out_desc));
  }
  zx_status_t UsbGetConfigurationDescriptorLength(uint8_t configuration, uint64_t* out_length) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  zx_status_t UsbGetConfigurationDescriptor(uint8_t configuration, uint8_t* out_desc_buffer,
                                            size_t desc_size, size_t* out_desc_actual) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  zx_status_t UsbGetStringDescriptor(uint8_t desc_id, uint16_t lang_id, uint16_t* out_lang_id,
                                     uint8_t* out_string_buffer, size_t string_size,
                                     size_t* out_string_actual) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  uint64_t UsbGetCurrentFrame() { return 0; }

 private:
  struct QueuedRequest {
    usb_request_t* request;
    usb_request_complete_callback_t callback;
  };

  const std::vector<uint8_t> descriptors_;
  std::mutex lock_;
  std::vector<QueuedRequest> queued_requests_ __TA_GUARDED(lock_);
  size_t probe_len_ __TA_GUARDED(lock_) = sizeof(usb_video_vc_probe_and_commit_controls);
  compat::BanjoServer banjo_server_{ZX_PROTOCOL_USB, this, &usb_protocol_ops_};
};

// Serves `fuchsia.driver.compat.Service` backed by the fake usb banjo server, which
// is how the driver under test reaches its parent's usb protocol.
class UsbVideoTestEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    device_server_.Initialize(component::kDefaultInstance, std::nullopt,
                              usb_server_.GetBanjoConfig());
    zx_status_t status =
        device_server_.Serve(fdf::Dispatcher::GetCurrent()->async_dispatcher(), &to_driver_vfs);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok();
  }

  void SetProbeLen(size_t len) { usb_server_.SetProbeLen(len); }

 private:
  compat::DeviceServer device_server_;
  FakeUsbServer usb_server_;
};

class FixtureConfig final {
 public:
  using DriverType = UsbVideoStream;
  using EnvironmentType = UsbVideoTestEnvironment;
};

// Creates a `BufferCollectionInfo` with one VMO of size `actual_vmo_size`.
// `BufferCollectionInfo::vmo_size` will be set to `declared_vmo_size`.
fuchsia::sysmem::BufferCollectionInfo CreateBufferCollection(uint64_t declared_vmo_size,
                                                             uint64_t actual_vmo_size) {
  fuchsia::sysmem::BufferCollectionInfo buffer_collection;
  buffer_collection.buffer_count = 1;
  buffer_collection.vmo_size = declared_vmo_size;
  buffer_collection.format.image.width = kWidth;
  buffer_collection.format.image.height = kHeight;
  buffer_collection.format.image.layers = 2;
  buffer_collection.format.image.pixel_format.type = fuchsia::sysmem::PixelFormatType::NV12;
  buffer_collection.format.image.planes[0].bytes_per_row = kStride;
  buffer_collection.format.image.planes[1].bytes_per_row = kStride;

  zx::vmo vmo;
  ZX_ASSERT(zx::vmo::create(actual_vmo_size, 0, &vmo) == ZX_OK);
  buffer_collection.vmos[0] = std::move(vmo);
  return buffer_collection;
}

// The driver runs on a background dispatcher so the test thread can drive it with
// synchronous FIDL clients.
class UsbVideoTest : public ::testing::Test {
 public:
  void SetUp() override {
    ASSERT_EQ(driver_test_.StartDriver().status_value(), ZX_OK);
    // Obtain a control channel from the camera device the driver advertises.
    zx::result device = driver_test_.Connect<fuchsia_hardware_camera::Service::Device>();
    ASSERT_EQ(device.status_value(), ZX_OK);

    fidl::InterfaceHandle<fuchsia::camera::Control> control_handle;
    fidl::InterfaceRequest<fuchsia::camera::Control> control_request = control_handle.NewRequest();
    ASSERT_TRUE(fidl::WireCall(*device)->GetChannel(control_request.TakeChannel()).ok());
    control_ = control_handle.BindSync();
  }

  void TearDown() override { ASSERT_EQ(driver_test_.StopDriver().status_value(), ZX_OK); }

 protected:
  // Returns whether or not `channel` is closed. If wait is true, wait for async channel closure.
  static bool IsChannelClosed(const zx::channel& channel, bool wait = false) {
    zx_signals_t observed = 0;

    zx::time deadline = wait ? zx::time::infinite() : zx::time{0};

    zx_status_t status = channel.wait_one(ZX_CHANNEL_PEER_CLOSED, deadline, &observed);
    return status == ZX_OK && (observed & ZX_CHANNEL_PEER_CLOSED);
  }

  // Sends a CreateStream FIDL request containing `buffer_collection`.
  fidl::InterfaceHandle<fuchsia::camera::Stream> CallCreateStream(
      fuchsia::sysmem::BufferCollectionInfo buffer_collection, zx::eventpair stream_token) {
    fuchsia::camera::FrameRate frame_rate{
        .frames_per_sec_numerator = 10000000,
        .frames_per_sec_denominator = kFrameInterval,
    };
    fidl::InterfaceHandle<fuchsia::camera::Stream> stream_handle;
    fidl::InterfaceRequest<fuchsia::camera::Stream> stream_request = stream_handle.NewRequest();

    EXPECT_EQ(control_->CreateStream(std::move(buffer_collection), frame_rate,
                                     std::move(stream_request), std::move(stream_token)),
              ZX_OK);
    return stream_handle;
  }

  void SetProbeLen(size_t len) {
    driver_test_.RunInEnvironmentTypeContext(
        [len](UsbVideoTestEnvironment& env) { env.SetProbeLen(len); });
  }

  fdf_testing::BackgroundDriverTest<FixtureConfig> driver_test_;
  fuchsia::camera::ControlSyncPtr control_;
};

// Verifies that a stream is successfully created and initialized when a valid buffer collection is
// provided.
TEST_F(UsbVideoTest, CreateStream_Success) {
  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
  auto buffer_collection = CreateBufferCollection(kMaxVideoFrameSize, kMaxVideoFrameSize);
  auto stream_handle = CallCreateStream(std::move(buffer_collection), std::move(server_token));

  // Verify the channel is operational.
  EXPECT_FALSE(IsChannelClosed(stream_handle.channel(), false));

  // Explicitly close the channel to trigger shutdown.
  stream_handle = {};

  // Wait for the stream to be fully stopped. The driver will close its end of the stream_token
  // eventpair when shutdown is complete.
  zx_signals_t observed = 0;
  zx_status_t status =
      client_token.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite(), &observed);
  EXPECT_EQ(status, ZX_OK);
  EXPECT_TRUE(observed & ZX_EVENTPAIR_PEER_CLOSED);
}

// Verifies that stream creation fails if the physical size of the VMOs in the buffer collection is
// less than the negotiated max frame size.
TEST_F(UsbVideoTest, CreateStream_FailPhysicalSizeTooSmall) {
  const uint64_t kVmoSize = kMaxVideoFrameSize;
  const uint64_t kPhysicalSize = 4096;
  ASSERT_LT(kPhysicalSize, kVmoSize);

  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);

  // VMO size is declared as 10KB, but physical VMO size is only 4KB. This should fail during
  // physical size check.
  auto buffer_collection = CreateBufferCollection(kVmoSize, kPhysicalSize);
  auto stream_handle = CallCreateStream(std::move(buffer_collection), std::move(server_token));

  // Verify the channel failed to be created.
  EXPECT_TRUE(IsChannelClosed(stream_handle.channel(), true));
}

// Verifies that stream creation fails if the device returns a short read on the control transfer.
TEST_F(UsbVideoTest, ShortControlReadRejection) {
  SetProbeLen(10);

  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
  auto buffer_collection = CreateBufferCollection(kMaxVideoFrameSize, kMaxVideoFrameSize);
  auto stream_handle = CallCreateStream(std::move(buffer_collection), std::move(server_token));

  EXPECT_TRUE(IsChannelClosed(stream_handle.channel(), true));
}

TEST_F(UsbVideoTest, CreateStream_Success26ByteProbe) {
  SetProbeLen(26);

  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
  auto buffer_collection = CreateBufferCollection(kMaxVideoFrameSize, kMaxVideoFrameSize);
  auto stream_handle = CallCreateStream(std::move(buffer_collection), std::move(server_token));

  EXPECT_FALSE(IsChannelClosed(stream_handle.channel(), false));
  stream_handle = {};

  zx_signals_t observed = 0;
  zx_status_t status =
      client_token.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite(), &observed);
  EXPECT_EQ(status, ZX_OK);
  EXPECT_TRUE(observed & ZX_EVENTPAIR_PEER_CLOSED);
}

TEST_F(UsbVideoTest, CreateStream_Success34ByteProbe) {
  SetProbeLen(34);

  zx::eventpair client_token, server_token;
  ASSERT_EQ(zx::eventpair::create(0, &client_token, &server_token), ZX_OK);
  auto buffer_collection = CreateBufferCollection(kMaxVideoFrameSize, kMaxVideoFrameSize);
  auto stream_handle = CallCreateStream(std::move(buffer_collection), std::move(server_token));

  EXPECT_FALSE(IsChannelClosed(stream_handle.channel(), false));
  stream_handle = {};

  zx_signals_t observed = 0;
  zx_status_t status =
      client_token.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite(), &observed);
  EXPECT_EQ(status, ZX_OK);
  EXPECT_TRUE(observed & ZX_EVENTPAIR_PEER_CLOSED);
}

}  // namespace

}  // namespace camera::usb_video
