// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-bus/usb-device.h"

#include <fidl/fuchsia.hardware.usb.device/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.hci/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/fit/function.h>

#include <thread>

#include <fbl/ref_counted.h>
#include <usb/descriptors.h>
#include <usb/request-cpp.h>
#include <zxtest/zxtest.h>

#include "src/devices/testing/mock-ddk/mock-device.h"
#include "src/devices/usb/drivers/usb-bus/tests/common.h"
#include "src/devices/usb/drivers/usb-bus/usb-bus.h"
#include "src/lib/utf_conversion/utf_conversion.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_bus {

class DeviceTest : public zxtest::Test {
 public:
  auto& get_fidl() { return fidl_; }
  auto& get_usb_fidl() { return usb_fidl_; }

  auto& get_device() { return *device_; }

  void SetUp() override {
    auto runtime = fdf_testing::DriverRuntime::GetInstance();
    dispatcher_ =
        std::make_unique<fdf::UnownedSynchronizedDispatcher>(runtime->StartBackgroundDispatcher());
    hci_ = std::make_unique<FakeHci>((*dispatcher_)->async_dispatcher());

    timer_ = fbl::MakeRefCounted<FakeTimer>();
    timer_->set_timeout_handler([=](sync_completion_t* completion, zx_duration_t duration) {
      return sync_completion_wait(completion, duration);
    });

    auto hci_endpoints = fidl::Endpoints<fuchsia_hardware_usb_hci::UsbHci>::Create();
    auto usb_endpoints = fidl::Endpoints<fuchsia_hardware_usb::Usb>::Create();
    auto result = fdf::RunOnDispatcherSync((*dispatcher_)->async_dispatcher(), [&]() {
      auto device = fbl::MakeRefCounted<UsbDevice>(
          root_.get(), ddk::UsbHciProtocolClient(hci_->proto()), std::move(hci_endpoints.client),
          kDeviceId, kHubId, kDeviceSpeed, timer_, (*dispatcher_)->async_dispatcher());
      ASSERT_OK(device->Init((*dispatcher_)->async_dispatcher()));
      device->BindUsbServer((*dispatcher_)->async_dispatcher(), std::move(usb_endpoints.server));
      device_ = device.get();
    });
    EXPECT_TRUE(result.is_ok());
    auto endpoints = fidl::Endpoints<fuchsia_hardware_usb_device::Device>::Create();
    fidl::BindServer((*dispatcher_)->async_dispatcher(), std::move(endpoints.server), device_);
    fidl_.Bind(std::move(endpoints.client));
    usb_fidl_.Bind(std::move(usb_endpoints.client));
  }

  void TearDown() override {
    auto result = fdf::RunOnDispatcherSync((*dispatcher_)->async_dispatcher(), [&]() {
      device_->DdkAsyncRemove();
      mock_ddk::ReleaseFlaggedDevices(root_.get());
    });
    EXPECT_TRUE(result.is_ok());
    hci_.reset();
    dispatcher_.reset();
  }

  void CancelAll() { ASSERT_OK(device_->UsbCancelAll(1)); }

  size_t get_parent_request_size() { return device_->UsbGetRequestSize(); }

  ddk::UsbProtocolClient get_usb_protocol() {
    usb_protocol_t usb;
    device_->DdkGetProtocol(ZX_PROTOCOL_USB, &usb);
    return ddk::UsbProtocolClient(&usb);
  }

  ddk::UsbBusProtocolClient get_usb_bus_protocol() {
    usb_bus_protocol_t usb;
    device_->DdkGetProtocol(ZX_PROTOCOL_USB_BUS, &usb);
    return ddk::UsbBusProtocolClient(&usb);
  }

  void set_custom_control_handling(bool enabled) { hci_->set_custom_control_handling(enabled); }
  usb::BorrowedRequestQueue<void> get_pending_requests() { return hci_->pending_requests(); }
  uint8_t get_configuration() { return hci_->configuration(); }

  void set_enable_endpoint_hook(
      fit::function<zx_status_t(uint32_t device_id, const usb_endpoint_descriptor_t* ep_desc,
                                const usb_ss_ep_comp_descriptor_t* ss_com_desc, bool enable)>
          hook) {
    hci_->set_enable_endpoint_hook(std::move(hook));
  }

  void set_timeout_handler(fit::function<zx_status_t(sync_completion_t*, zx_duration_t)> handler) {
    timer_->set_timeout_handler(std::move(handler));
  }

  bool get_device_reset() { return hci_->device_reset(); }
  uint8_t get_reset_endpoint() { return hci_->reset_endpoint(); }
  void SetEmptyState(bool should_return_empty) { hci_->SetEmptyState(should_return_empty); }
  void SetErrorState(zx_status_t error) { hci_->SetErrorState(error); }

 protected:
  std::shared_ptr<MockDevice> root_{MockDevice::FakeRootParent()};
  fdf_testing::DriverRuntime* runtime() { return fdf_testing::DriverRuntime::GetInstance(); }
  std::unique_ptr<fdf::UnownedSynchronizedDispatcher> dispatcher_;

 private:
  fbl::RefPtr<FakeTimer> timer_;
  fidl::WireSyncClient<fuchsia_hardware_usb_device::Device> fidl_;
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb> usb_fidl_;
  std::unique_ptr<FakeHci> hci_;
  // UsbDevice context pointer owned by us through MockDdk
  UsbDevice* device_;
};

// CancelAll-specific test

TEST_F(DeviceTest, CancelAllCancelsAllRequestsThenReturns) {
  using Request = usb::CallbackRequest<sizeof(max_align_t)>;
  std::atomic<size_t> completed = 0;
  for (size_t i = 0; i < 500; i++) {
    std::optional<Request> request;
    Request::Alloc(&request, 0, 1, get_parent_request_size(),
                   [&](Request request) { completed++; });
    request->Queue(get_usb_protocol());
  }
  CancelAll();
  ASSERT_EQ(completed.load(), 500);
}

// USB protocol tests
TEST_F(DeviceTest, ControlOut) {
  auto usb = get_usb_protocol();
  uint8_t const_data[5];
  for (size_t i = 0; i < 5; i++) {
    const_data[i] = static_cast<uint8_t>(i);
  }
  set_custom_control_handling(true);
  set_timeout_handler([&](sync_completion_t* completion, zx_duration_t duration) {
    EXPECT_EQ(duration, 9001);
    auto requests = get_pending_requests();
    auto request = requests.pop();
    EXPECT_EQ(request->request()->header.ep_address, 0);
    EXPECT_EQ(request->request()->header.length, sizeof(const_data));
    void* mapped_data;
    request->Mmap(&mapped_data);
    EXPECT_EQ(0, memcmp(mapped_data, const_data, sizeof(const_data)));
    EXPECT_EQ(request->request()->setup.bm_request_type, 5);
    EXPECT_EQ(request->request()->setup.b_request, 97);
    EXPECT_EQ(request->request()->setup.w_value, 8);
    EXPECT_EQ(request->request()->setup.w_index, 12);

    request->Complete(ZX_OK, sizeof(const_data));
    return sync_completion_wait(completion, ZX_TIME_INFINITE);
  });
  ASSERT_OK(usb.ControlOut(5, 97, 8, 12, 9001, const_data, sizeof(const_data)));
}

TEST_F(DeviceTest, ControlIn) {
  auto usb = get_usb_protocol();
  uint8_t const_data[5];
  for (size_t i = 0; i < 5; i++) {
    const_data[i] = static_cast<uint8_t>(i);
  }
  set_custom_control_handling(true);
  set_timeout_handler([&](sync_completion_t* completion, zx_duration_t duration) {
    EXPECT_EQ(duration, 9001);
    auto requests = get_pending_requests();
    auto request = requests.pop();
    EXPECT_EQ(request->request()->header.ep_address, 0);
    EXPECT_EQ(request->request()->header.length, sizeof(const_data));
    void* mapped_data;
    request->Mmap(&mapped_data);
    memcpy(mapped_data, const_data, sizeof(const_data));
    EXPECT_EQ(request->request()->setup.bm_request_type, 5 | fdescriptor::EndpointDirection::kIn);
    EXPECT_EQ(request->request()->setup.b_request, 97);
    EXPECT_EQ(request->request()->setup.w_value, 8);
    EXPECT_EQ(request->request()->setup.w_index, 12);

    request->Complete(ZX_OK, sizeof(const_data));
    return sync_completion_wait(completion, ZX_TIME_INFINITE);
  });
  uint8_t buffer[5];
  size_t actual;
  ASSERT_OK(usb.ControlIn(5 | fdescriptor::EndpointDirection::kIn, 97, 8, 12, 9001, buffer,
                          sizeof(buffer), &actual));
  ASSERT_EQ(0, memcmp(buffer, const_data, sizeof(buffer)));
}

TEST_F(DeviceTest, RequestQueue) {
  auto usb = get_usb_protocol();
  using Request = usb::CallbackRequest<sizeof(max_align_t)>;
  std::optional<Request> request;
  sync_completion_t completion;
  usb_request_t* request_ptr;
  Request::Alloc(&request, 0, 1, get_parent_request_size(), [&](Request owned_request) {
    ASSERT_EQ(owned_request.request(), request_ptr);
    sync_completion_signal(&completion);
  });
  request_ptr = request->request();
  request->Queue(usb);
  auto requests = get_pending_requests();
  auto usb_request = requests.pop();
  usb_request->Complete(ZX_OK, 0);
  sync_completion_wait(&completion, ZX_TIME_INFINITE);
}

TEST_F(DeviceTest, GetSpeed) {
  auto usb = get_usb_protocol();
  ASSERT_EQ(usb.GetSpeed(), kDeviceSpeed);
}

TEST_F(DeviceTest, SetInterface) {
  auto usb = get_usb_protocol();
  set_custom_control_handling(true);
  set_timeout_handler([&](sync_completion_t* completion, zx_duration_t duration) {
    EXPECT_EQ(duration, ZX_TIME_INFINITE);
    auto requests = get_pending_requests();
    auto request = requests.pop();
    EXPECT_EQ(request->request()->header.ep_address, 0);
    EXPECT_EQ(request->request()->setup.bm_request_type, kStandardInterfaceOut);
    EXPECT_EQ(request->request()->setup.b_request, fdescriptor::StandardRequest::kSetInterface);
    EXPECT_EQ(request->request()->setup.w_value, 5);
    EXPECT_EQ(request->request()->setup.w_index, 98);
    request->Complete(ZX_OK, 0);
    return sync_completion_wait(completion, ZX_TIME_INFINITE);
  });
  usb.SetInterface(98, 5);
}

TEST_F(DeviceTest, GetConfiguration) {
  auto usb = get_usb_protocol();
  ASSERT_EQ(usb.GetConfiguration(), 1);
}

TEST_F(DeviceTest, SetConfiguration) {
  auto usb = get_usb_protocol();
  ASSERT_OK(usb.SetConfiguration(2));
  ASSERT_EQ(get_configuration(), 2);
}

TEST_F(DeviceTest, EnableEndpoint) {
  auto usb = get_usb_protocol();
  usb_endpoint_descriptor_t epdesc;
  usb_ss_ep_comp_descriptor_t ss;
  set_enable_endpoint_hook([&](uint32_t device_id, const usb_endpoint_descriptor_t* ep_desc,
                               const usb_ss_ep_comp_descriptor_t* ss_com_desc, bool enable) {
    EXPECT_EQ(device_id, kDeviceId);
    EXPECT_EQ(ep_desc, &epdesc);
    EXPECT_EQ(ss_com_desc, &ss);
    EXPECT_TRUE(enable);
    return ZX_OK;
  });
  ASSERT_OK(usb.EnableEndpoint(&epdesc, &ss, true));
}

TEST_F(DeviceTest, ResetEndpoint) {
  auto usb = get_usb_protocol();
  ASSERT_OK(usb.ResetEndpoint(97));
  ASSERT_EQ(get_reset_endpoint(), 97);
}

TEST_F(DeviceTest, ResetDevice) {
  auto usb = get_usb_protocol();
  ASSERT_OK(usb.ResetDevice());
  ASSERT_TRUE(get_device_reset());
}

TEST_F(DeviceTest, GetMaxTransferSize) {
  auto usb = get_usb_protocol();
  ASSERT_EQ(usb.GetMaxTransferSize(kTransferSizeEndpoint), kMaxTransferSize);
}

TEST_F(DeviceTest, GetDeviceId) {
  auto usb = get_usb_protocol();
  ASSERT_EQ(usb.GetDeviceId(), kDeviceId);
}

TEST_F(DeviceTest, GetDeviceDescriptor) {
  auto usb = get_usb_protocol();
  usb_device_descriptor_t descriptor;
  usb.GetDeviceDescriptor(&descriptor);
  ASSERT_EQ(descriptor.id_vendor, kVendorId);
  ASSERT_EQ(descriptor.id_product, kProductId);
  ASSERT_EQ(descriptor.b_device_class, kDeviceClass);
  ASSERT_EQ(descriptor.b_device_sub_class, kDeviceSubclass);
  ASSERT_EQ(descriptor.b_device_protocol, kDeviceProtocol);
}

TEST_F(DeviceTest, GetConfigurationDescriptorLength) {
  auto usb = get_usb_protocol();
  size_t length;
  ASSERT_OK(usb.GetConfigurationDescriptorLength(1, &length));
  ASSERT_EQ(length, sizeof(usb_configuration_descriptor_t));
}

TEST_F(DeviceTest, GetConfigurationDescriptor) {
  auto usb = get_usb_protocol();
  usb_configuration_descriptor_t descriptor;
  size_t actual;
  ASSERT_OK(usb.GetConfigurationDescriptor(1, reinterpret_cast<uint8_t*>(&descriptor),
                                           sizeof(descriptor), &actual));
  ASSERT_EQ(actual, sizeof(descriptor));
  ASSERT_EQ(descriptor.b_configuration_value, 1);
  ASSERT_EQ(descriptor.w_total_length, sizeof(descriptor));
}

TEST_F(DeviceTest, GetDescriptorsLength) {
  auto usb = get_usb_protocol();
  ASSERT_EQ(usb.GetDescriptorsLength(), sizeof(usb_configuration_descriptor_t));
}

TEST_F(DeviceTest, GetDescriptors) {
  auto usb = get_usb_protocol();
  usb_configuration_descriptor_t descriptor;
  size_t actual;
  usb.GetDescriptors(reinterpret_cast<uint8_t*>(&descriptor), sizeof(descriptor), &actual);
  ASSERT_EQ(actual, sizeof(descriptor));
  ASSERT_EQ(descriptor.b_configuration_value, 1);
  ASSERT_EQ(descriptor.w_total_length, sizeof(descriptor));
}

TEST_F(DeviceTest, GetCurrentFrame) {
  auto usb = get_usb_protocol();
  ASSERT_EQ(usb.GetCurrentFrame(), kCurrentFrame);
}

TEST_F(DeviceTest, GetRequestSize) {
  auto usb = get_usb_protocol();
  ASSERT_EQ(usb.GetRequestSize(), kRequestSize);
}

TEST_F(DeviceTest, FidlGetSpeed) {
  auto& fidl = get_fidl();
  auto result = fidl->GetDeviceSpeed();
  ASSERT_TRUE(result.ok());
  ASSERT_EQ(result.value().speed, kDeviceSpeed);
}

TEST_F(DeviceTest, FidlGetDescriptor) {
  auto& fidl = get_fidl();
  auto result = fidl->GetDeviceDescriptor();
  usb_device_descriptor_t* descriptor =
      reinterpret_cast<usb_device_descriptor_t*>(result.value().desc.data());
  ASSERT_EQ(descriptor->id_vendor, kVendorId);
  ASSERT_EQ(descriptor->id_product, kProductId);
  ASSERT_EQ(descriptor->b_device_class, kDeviceClass);
  ASSERT_EQ(descriptor->b_device_sub_class, kDeviceSubclass);
  ASSERT_EQ(descriptor->b_device_protocol, kDeviceProtocol);
}

TEST_F(DeviceTest, FidlGetConfigurationDescriptorSize) {
  auto& fidl = get_fidl();
  auto result = fidl->GetConfigurationDescriptorSize(1);
  ASSERT_TRUE(result.ok());
  ASSERT_OK(result.value().s);
  ASSERT_EQ(result.value().size, sizeof(usb_configuration_descriptor_t));
}

TEST_F(DeviceTest, FidlGetConfigurationDescriptorSize_Nullptr) {
  auto& fidl = get_fidl();
  auto result = fidl->GetConfigurationDescriptorSize(99);  // <-- intentionally bad index.
  ASSERT_TRUE(result.ok());
  ASSERT_EQ(result.value().s, ZX_ERR_INVALID_ARGS);
  ASSERT_EQ(result.value().size, 0);
}

TEST_F(DeviceTest, FidlGetConfigurationDescriptor) {
  auto& fidl = get_fidl();
  const usb_configuration_descriptor_t* descriptor;
  auto result = fidl->GetConfigurationDescriptor(1);
  ASSERT_TRUE(result.ok());
  ASSERT_OK(result.value().s);
  ASSERT_EQ(result.value().desc.size(), sizeof(*descriptor));
  descriptor = reinterpret_cast<const usb_configuration_descriptor_t*>(result.value().desc.data());
  ASSERT_EQ(descriptor->b_configuration_value, 1);
  ASSERT_EQ(descriptor->w_total_length, sizeof(*descriptor));
}

TEST_F(DeviceTest, FidlGetStringDescriptor_Empty) {
  auto& fidl = get_fidl();
  char golden[128];
  size_t dest_len = 128;
  utf16_to_utf8(reinterpret_cast<const uint16_t*>(kStringDescriptors[0][0]), 7,
                reinterpret_cast<uint8_t*>(golden), &dest_len);
  SetEmptyState(true);
  auto result = fidl->GetStringDescriptor(1, MakeConstant<uint16_t, 2>("EN"));
  ASSERT_TRUE(result.value().desc.empty());
  ASSERT_EQ(result.value().s, ZX_ERR_INTERNAL);
}

TEST_F(DeviceTest, FidlGetStringDescriptor) {
  auto& fidl = get_fidl();
  char golden[128];
  {
    size_t dest_len = 128;
    utf16_to_utf8(reinterpret_cast<const uint16_t*>(kStringDescriptors[0][0]), 7,
                  reinterpret_cast<uint8_t*>(golden), &dest_len);
    auto expected = MakeConstant<uint16_t, 2>("EN");
    auto result = fidl->GetStringDescriptor(1, MakeConstant<uint16_t, 2>("EN"));
    ASSERT_TRUE(result.ok());
    ASSERT_OK(result.value().s);
    ASSERT_EQ(result.value().actual_lang_id, expected);
    ASSERT_EQ(result.value().desc.size(), dest_len);  // this is way off for some reason
    ASSERT_EQ(memcmp(result.value().desc.data(), golden, dest_len), 0);
  }

  {
    size_t dest_len = 128;
    utf16_to_utf8(reinterpret_cast<const uint16_t*>(kStringDescriptors[0][1]), 6,
                  reinterpret_cast<uint8_t*>(golden), &dest_len);
    auto expected = MakeConstant<uint16_t, 2>("ES");
    auto result = fidl->GetStringDescriptor(1, MakeConstant<uint16_t, 2>("ES"));
    ASSERT_TRUE(result.ok());
    ASSERT_OK(result.value().s);
    ASSERT_EQ(result.value().actual_lang_id, expected);
    ASSERT_EQ(result.value().desc.size(), dest_len);
    ASSERT_EQ(memcmp(result.value().desc.data(), golden, dest_len), 0);
  }

  {
    size_t dest_len = 128;
    utf16_to_utf8(reinterpret_cast<const uint16_t*>(kStringDescriptors[1][0]), 6,
                  reinterpret_cast<uint8_t*>(golden), &dest_len);
    auto expected = MakeConstant<uint16_t, 2>("EN");
    auto result = fidl->GetStringDescriptor(2, MakeConstant<uint16_t, 2>("EN"));
    ASSERT_TRUE(result.ok());
    ASSERT_OK(result.value().s);
    ASSERT_EQ(result.value().actual_lang_id, expected);
    ASSERT_EQ(result.value().desc.size(), dest_len);
    ASSERT_EQ(memcmp(result.value().desc.data(), golden, dest_len), 0);
  }

  {
    size_t dest_len = 128;
    utf16_to_utf8(reinterpret_cast<const uint16_t*>(kStringDescriptors[1][1]), 11,
                  reinterpret_cast<uint8_t*>(golden), &dest_len);
    auto expected = MakeConstant<uint16_t, 2>("ES");
    auto result = fidl->GetStringDescriptor(2, MakeConstant<uint16_t, 2>("ES"));
    ASSERT_TRUE(result.ok());
    ASSERT_OK(result.value().s);
    ASSERT_EQ(result.value().actual_lang_id, expected);
    ASSERT_EQ(result.value().desc.size(), dest_len);
    ASSERT_EQ(memcmp(result.value().desc.data(), golden, dest_len), 0);
  }
}

TEST_F(DeviceTest, UsbGetStringDescriptor_BufferTooSmall) {
  auto& device = get_device();
  uint16_t lang_id[2];
  uint8_t desc[128];
  size_t actual;

  // The value here is intentionally chosen to be undersized.
  size_t small = 3;

  zx_status_t status = device.UsbGetStringDescriptor(
      1, 1, lang_id, reinterpret_cast<uint8_t*>(&desc), small, &actual);

  EXPECT_EQ(status, ZX_ERR_BUFFER_TOO_SMALL);
  EXPECT_GT(actual, small);
}

TEST_F(DeviceTest, UsbGetStringDescriptor_ErrorHandling) {
  auto& device = get_device();
  uint16_t lang_id[2] = {};
  uint8_t desc[512] = {};
  size_t actual = 0;

  SetErrorState(ZX_ERR_TIMED_OUT);
  zx_status_t status = device.UsbGetStringDescriptor(0, 0, lang_id, desc, sizeof(desc), &actual);
  EXPECT_EQ(status, ZX_ERR_TIMED_OUT);
  EXPECT_EQ(actual, 0u);

  SetErrorState(ZX_OK);
  status = device.UsbGetStringDescriptor(0, 0, lang_id, desc, sizeof(desc), &actual);
  EXPECT_OK(status);
  EXPECT_EQ(actual, 4u);
}

TEST_F(DeviceTest, FidlSetInterface) {
  auto& fidl = get_fidl();
  set_custom_control_handling(true);
  set_timeout_handler([&](sync_completion_t* completion, zx_duration_t duration) {
    EXPECT_EQ(duration, ZX_TIME_INFINITE);
    auto requests = get_pending_requests();
    auto request = requests.pop();
    EXPECT_EQ(request->request()->header.ep_address, 0);
    EXPECT_EQ(request->request()->setup.bm_request_type, kStandardInterfaceOut);
    EXPECT_EQ(request->request()->setup.b_request, fdescriptor::StandardRequest::kSetInterface);
    EXPECT_EQ(request->request()->setup.w_value, 5);
    EXPECT_EQ(request->request()->setup.w_index, 98);
    request->Complete(ZX_OK, 0);
    return sync_completion_wait(completion, ZX_TIME_INFINITE);
  });
  auto result = fidl->SetInterface(98, 5);
  ASSERT_TRUE(result.ok());
  ASSERT_OK(result.value().s);
}

TEST_F(DeviceTest, FidlGetDeviceId) {
  auto& fidl = get_fidl();
  auto result = fidl->GetDeviceId();
  ASSERT_TRUE(result.ok());
  ASSERT_EQ(result.value().device_id, kDeviceId);
}

TEST_F(DeviceTest, FidlGetHubDeviceId) {
  auto& fidl = get_fidl();
  auto result = fidl->GetHubDeviceId();
  ASSERT_TRUE(result.ok());
  ASSERT_EQ(result.value().hub_device_id, kHubId);
}

TEST_F(DeviceTest, FidlGetConfiguration) {
  auto& fidl = get_fidl();
  auto result = fidl->GetConfiguration();
  ASSERT_TRUE(result.ok());
  ASSERT_EQ(result.value().configuration, 1);
}

TEST_F(DeviceTest, FidlSetConfiguration) {
  auto& fidl = get_fidl();
  auto result = fidl->SetConfiguration(2);
  ASSERT_TRUE(result.ok());
  ASSERT_OK(result.value().s);
  ASSERT_EQ(get_configuration(), 2);
}

TEST_F(DeviceTest, ReinitializeWithoutResettingFails) {
  auto& device = get_device();
  EXPECT_EQ(device.Reinitialize(), ZX_ERR_BAD_STATE);
}

namespace {

// NOLINTBEGIN(readability-suspicious-call-argument): zxtest macro expands with swapped argument
// names in comparison lambda.

TEST_F(DeviceTest, UsbServiceGetDeviceSpeed) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  fidl::WireResult<fuchsia_hardware_usb::Usb::GetDeviceSpeed> speed_res =
      usb_fidl->GetDeviceSpeed();
  ASSERT_TRUE(speed_res.ok());
  EXPECT_EQ(speed_res.value().speed, static_cast<fdescriptor::wire::UsbSpeed>(kDeviceSpeed));
}

TEST_F(DeviceTest, UsbServiceGetDeviceId) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  fidl::WireResult<fuchsia_hardware_usb::Usb::GetDeviceId> id_res = usb_fidl->GetDeviceId();
  ASSERT_TRUE(id_res.ok());
  EXPECT_EQ(id_res.value().device_id, kDeviceId);
}

TEST_F(DeviceTest, UsbServiceGetDeviceDescriptor) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  fidl::WireResult<fuchsia_hardware_usb::Usb::GetDeviceDescriptor> desc_res =
      usb_fidl->GetDeviceDescriptor();
  ASSERT_TRUE(desc_res.ok());
  const fdescriptor::wire::UsbDeviceDescriptor& dev_desc = desc_res.value().descriptor;
  EXPECT_EQ(dev_desc.b_length, sizeof(usb_device_descriptor_t));
  EXPECT_EQ(dev_desc.b_descriptor_type, static_cast<uint8_t>(fdescriptor::DescriptorType::kDevice));
  EXPECT_EQ(dev_desc.id_vendor, kVendorId);
  EXPECT_EQ(dev_desc.id_product, kProductId);
  EXPECT_EQ(dev_desc.b_device_class, kDeviceClass);
  EXPECT_EQ(dev_desc.b_device_sub_class, kDeviceSubclass);
  EXPECT_EQ(dev_desc.b_device_protocol, kDeviceProtocol);
}

TEST_F(DeviceTest, UsbServiceGetConfigurationDescriptor) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  fidl::WireResult<fuchsia_hardware_usb::Usb::GetConfigurationDescriptor> cfg_res =
      usb_fidl->GetConfigurationDescriptor(1);
  ASSERT_TRUE(cfg_res.ok());
  ASSERT_TRUE(cfg_res->is_ok());
  ASSERT_EQ(cfg_res->value()->descriptor.size(), sizeof(usb_configuration_descriptor_t));
  const auto* cfg_desc =
      reinterpret_cast<const usb_configuration_descriptor_t*>(cfg_res->value()->descriptor.data());
  EXPECT_EQ(cfg_desc->b_length, sizeof(usb_configuration_descriptor_t));
  EXPECT_EQ(cfg_desc->b_descriptor_type,
            static_cast<uint8_t>(fdescriptor::DescriptorType::kConfiguration));
  EXPECT_EQ(cfg_desc->w_total_length, sizeof(usb_configuration_descriptor_t));
  EXPECT_EQ(cfg_desc->b_configuration_value, 1);
}

TEST_F(DeviceTest, UsbServiceGetConfigurationDescriptorInvalid) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  fidl::WireResult<fuchsia_hardware_usb::Usb::GetConfigurationDescriptor> bad_cfg_res =
      usb_fidl->GetConfigurationDescriptor(99);
  ASSERT_TRUE(bad_cfg_res.ok());
  ASSERT_TRUE(bad_cfg_res->is_error());
  EXPECT_EQ(bad_cfg_res->error_value(), ZX_ERR_INVALID_ARGS);
}

TEST_F(DeviceTest, UsbServiceGetConfiguration) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  fidl::WireResult<fuchsia_hardware_usb::Usb::GetConfiguration> get_cfg_res =
      usb_fidl->GetConfiguration();
  ASSERT_TRUE(get_cfg_res.ok());
  EXPECT_EQ(get_cfg_res.value().configuration, 1);
}

TEST_F(DeviceTest, UsbServiceSetConfiguration) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  fidl::WireResult<fuchsia_hardware_usb::Usb::SetConfiguration> set_cfg_res =
      usb_fidl->SetConfiguration(2);
  ASSERT_TRUE(set_cfg_res.ok());
  EXPECT_TRUE(set_cfg_res->is_ok());
  EXPECT_EQ(get_configuration(), 2);

  fidl::WireResult<fuchsia_hardware_usb::Usb::GetConfiguration> get_cfg_res =
      usb_fidl->GetConfiguration();
  ASSERT_TRUE(get_cfg_res.ok());
  EXPECT_EQ(get_cfg_res.value().configuration, 2);
}

TEST_F(DeviceTest, UsbServiceResetEndpoint) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  fidl::WireResult<fuchsia_hardware_usb::Usb::ResetEndpoint> reset_ep_res =
      usb_fidl->ResetEndpoint(0x81);
  ASSERT_TRUE(reset_ep_res.ok());
  EXPECT_TRUE(reset_ep_res->is_ok());
  EXPECT_EQ(get_reset_endpoint(), 0x81);
}

TEST_F(DeviceTest, UsbServiceSetInterface) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  set_custom_control_handling(true);
  set_timeout_handler([&](sync_completion_t* completion, zx_duration_t duration) {
    usb::BorrowedRequestQueue<void> requests = get_pending_requests();
    std::optional<usb::BorrowedRequest<void>> request = requests.pop();
    EXPECT_EQ(request->request()->setup.b_request, fdescriptor::StandardRequest::kSetInterface);
    EXPECT_EQ(request->request()->setup.w_value, 3);
    EXPECT_EQ(request->request()->setup.w_index, 1);
    request->Complete(ZX_OK, 0);
    return sync_completion_wait(completion, ZX_TIME_INFINITE);
  });
  fidl::WireResult<fuchsia_hardware_usb::Usb::SetInterface> set_if_res =
      usb_fidl->SetInterface(1, 3);
  ASSERT_TRUE(set_if_res.ok());
  EXPECT_TRUE(set_if_res->is_ok());
}

TEST_F(DeviceTest, UsbServiceControlTransferIn) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  fdescriptor::wire::UsbSetup in_setup{
      .bm_request_type = kStandardDeviceIn,
      .b_request = fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor),
      .w_value = usb_descriptor_w_value(fdescriptor::DescriptorType::kDevice, 0),
      .w_index = 0,
      .w_length = sizeof(usb_device_descriptor_t),
  };
  fidl::WireResult<fuchsia_hardware_usb::Usb::ControlTransfer> in_res =
      usb_fidl->ControlTransfer(in_setup, ZX_SEC(5), {});
  ASSERT_TRUE(in_res.ok());
  ASSERT_TRUE(in_res->is_ok());
  ASSERT_EQ(in_res->value()->read_data.size(), sizeof(usb_device_descriptor_t));
  const auto* dev_desc =
      reinterpret_cast<const usb_device_descriptor_t*>(in_res->value()->read_data.data());
  EXPECT_EQ(dev_desc->b_length, sizeof(usb_device_descriptor_t));
  EXPECT_EQ(dev_desc->b_descriptor_type,
            static_cast<uint8_t>(fdescriptor::DescriptorType::kDevice));
  EXPECT_EQ(dev_desc->id_vendor, kVendorId);
  EXPECT_EQ(dev_desc->id_product, kProductId);
}

TEST_F(DeviceTest, UsbServiceControlTransferSetConfiguration) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  uint8_t out_payload[4] = {0x11, 0x22, 0x33, 0x44};
  fdescriptor::wire::UsbSetup out_setup{
      .bm_request_type = kStandardDeviceOut,
      .b_request = fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration),
      .w_value = 2,
      .w_index = 0,
      .w_length = 0,
  };
  fidl::WireResult<fuchsia_hardware_usb::Usb::ControlTransfer> out_res = usb_fidl->ControlTransfer(
      out_setup, ZX_SEC(5), fidl::VectorView<uint8_t>::FromExternal(out_payload, 0));
  ASSERT_TRUE(out_res.ok());
  EXPECT_TRUE(out_res->is_ok());
  EXPECT_EQ(get_configuration(), 2);
  // Verify usb-bus internal current_config_index_ state was also updated, not bypassed.
  EXPECT_EQ(get_usb_protocol().GetConfiguration(), 2);
}

TEST_F(DeviceTest, UsbServiceControlTransferTimeout) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  fdescriptor::wire::UsbSetup in_setup{
      .bm_request_type = kStandardDeviceIn,
      .b_request = fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor),
      .w_value = usb_descriptor_w_value(fdescriptor::DescriptorType::kDevice, 0),
      .w_index = 0,
      .w_length = sizeof(usb_device_descriptor_t),
  };

  // Verify ControlTransfer honors timeout_ns via waiter_ and cancels EP0 on timeout.
  set_custom_control_handling(true);
  bool timeout_observed = false;
  zx_duration_t observed_timeout = 0;
  set_timeout_handler([&](sync_completion_t* completion, zx_duration_t duration) -> zx_status_t {
    if (!timeout_observed) {
      timeout_observed = true;
      observed_timeout = duration;
      return ZX_ERR_TIMED_OUT;
    }
    return sync_completion_wait(completion, duration);
  });
  fidl::WireResult<fuchsia_hardware_usb::Usb::ControlTransfer> timeout_res =
      usb_fidl->ControlTransfer(in_setup, ZX_MSEC(250), {});
  ASSERT_TRUE(timeout_res.ok());
  EXPECT_TRUE(timeout_observed);
  EXPECT_EQ(observed_timeout, ZX_MSEC(250));
  ASSERT_TRUE(timeout_res->is_error());
  EXPECT_EQ(timeout_res->error_value(), ZX_ERR_TIMED_OUT);
  set_custom_control_handling(false);
}

TEST_F(DeviceTest, UsbServiceControlTransferValidation) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();

  uint8_t out_payload[4] = {0x11, 0x22, 0x33, 0x44};
  fdescriptor::wire::UsbSetup in_setup{
      .bm_request_type = kStandardDeviceIn,
      .b_request = fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor),
      .w_value = usb_descriptor_w_value(fdescriptor::DescriptorType::kDevice, 0),
      .w_index = 0,
      .w_length = sizeof(usb_device_descriptor_t),
  };

  // Mismatched OUT write_data size vs. setup.w_length is rejected.
  fdescriptor::wire::UsbSetup bad_out_setup{
      .bm_request_type = kVendorDeviceOut,
      .b_request = 0x42,
      .w_value = 0,
      .w_index = 0,
      .w_length = 2,
  };
  fidl::WireResult<fuchsia_hardware_usb::Usb::ControlTransfer> bad_out_res =
      usb_fidl->ControlTransfer(bad_out_setup, ZX_SEC(5),
                                fidl::VectorView<uint8_t>::FromExternal(out_payload, 4));
  ASSERT_TRUE(bad_out_res.ok());
  ASSERT_TRUE(bad_out_res->is_error());
  EXPECT_EQ(bad_out_res->error_value(), ZX_ERR_INVALID_ARGS);

  // IN request with non-empty write_data is rejected.
  fidl::WireResult<fuchsia_hardware_usb::Usb::ControlTransfer> bad_in_res =
      usb_fidl->ControlTransfer(in_setup, ZX_SEC(5),
                                fidl::VectorView<uint8_t>::FromExternal(out_payload, 4));
  ASSERT_TRUE(bad_in_res.ok());
  ASSERT_TRUE(bad_in_res->is_error());
  EXPECT_EQ(bad_in_res->error_value(), ZX_ERR_INVALID_ARGS);

  // IN request with w_length exceeding maximum limit is rejected.
  fdescriptor::wire::UsbSetup oversize_in_setup{
      .bm_request_type = kStandardDeviceIn,
      .b_request = fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor),
      .w_value = 0,
      .w_index = 0,
      .w_length = static_cast<uint16_t>(fuchsia_hardware_usb::kMaxControlRequestLen + 10),
  };
  fidl::WireResult<fuchsia_hardware_usb::Usb::ControlTransfer> oversize_in_res =
      usb_fidl->ControlTransfer(oversize_in_setup, ZX_SEC(5), {});
  ASSERT_TRUE(oversize_in_res.ok());
  ASSERT_TRUE(oversize_in_res->is_error());
  EXPECT_EQ(oversize_in_res->error_value(), ZX_ERR_INVALID_ARGS);

  // SET_CONFIGURATION bypass attempts are rejected.
  fdescriptor::wire::UsbSetup bad_cfg_val{
      .bm_request_type = kStandardDeviceOut,
      .b_request = fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration),
      .w_value = 0x0102,
      .w_index = 0,
      .w_length = 0,
  };
  fidl::WireResult<fuchsia_hardware_usb::Usb::ControlTransfer> bad_cfg_res =
      usb_fidl->ControlTransfer(bad_cfg_val, ZX_SEC(5), {});
  ASSERT_TRUE(bad_cfg_res.ok());
  ASSERT_TRUE(bad_cfg_res->is_error());
  EXPECT_EQ(bad_cfg_res->error_value(), ZX_ERR_INVALID_ARGS);

  fdescriptor::wire::UsbSetup bad_cfg_len{
      .bm_request_type = kStandardDeviceOut,
      .b_request = fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration),
      .w_value = 1,
      .w_index = 0,
      .w_length = 1,
  };
  uint8_t dummy_byte = 0;
  fidl::WireResult<fuchsia_hardware_usb::Usb::ControlTransfer> bad_cfg_len_res =
      usb_fidl->ControlTransfer(bad_cfg_len, ZX_SEC(5),
                                fidl::VectorView<uint8_t>::FromExternal(&dummy_byte, 1));
  ASSERT_TRUE(bad_cfg_len_res.ok());
  ASSERT_TRUE(bad_cfg_len_res->is_error());
  EXPECT_EQ(bad_cfg_len_res->error_value(), ZX_ERR_INVALID_ARGS);

  // SET_INTERFACE bypass attempts are rejected.
  fdescriptor::wire::UsbSetup bad_intf_val{
      .bm_request_type = kStandardInterfaceOut,
      .b_request = fidl::ToUnderlying(fdescriptor::StandardRequest::kSetInterface),
      .w_value = 0x0100,
      .w_index = 0,
      .w_length = 0,
  };
  fidl::WireResult<fuchsia_hardware_usb::Usb::ControlTransfer> bad_intf_res =
      usb_fidl->ControlTransfer(bad_intf_val, ZX_SEC(5), {});
  ASSERT_TRUE(bad_intf_res.ok());
  ASSERT_TRUE(bad_intf_res->is_error());
  EXPECT_EQ(bad_intf_res->error_value(), ZX_ERR_INVALID_ARGS);
}

TEST_F(DeviceTest, UsbServiceControlTransferOutPayload) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();
  uint8_t out_payload[4] = {0x11, 0x22, 0x33, 0x44};

  // Valid OUT control transfer with non-empty payload.
  set_custom_control_handling(true);
  set_timeout_handler([&](sync_completion_t* completion, zx_duration_t duration) {
    usb::BorrowedRequestQueue<void> requests = get_pending_requests();
    std::optional<usb::BorrowedRequest<void>> request = requests.pop();
    EXPECT_EQ(request->request()->setup.bm_request_type, kVendorDeviceOut);
    EXPECT_EQ(request->request()->setup.b_request, 0x42);
    EXPECT_EQ(request->request()->header.length, 4u);
    uint8_t received[4] = {};
    size_t copied = (*request).CopyFrom(received, 4, 0);
    EXPECT_EQ(copied, 4u);
    EXPECT_EQ(received[0], 0x11);
    EXPECT_EQ(received[1], 0x22);
    EXPECT_EQ(received[2], 0x33);
    EXPECT_EQ(received[3], 0x44);
    request->Complete(ZX_OK, 4);
    return sync_completion_wait(completion, ZX_TIME_INFINITE);
  });
  fdescriptor::wire::UsbSetup valid_vendor_out{
      .bm_request_type = kVendorDeviceOut,
      .b_request = 0x42,
      .w_value = 0,
      .w_index = 0,
      .w_length = 4,
  };
  fidl::WireResult<fuchsia_hardware_usb::Usb::ControlTransfer> vendor_out_res =
      usb_fidl->ControlTransfer(valid_vendor_out, ZX_SEC(5),
                                fidl::VectorView<uint8_t>::FromExternal(out_payload, 4));
  ASSERT_TRUE(vendor_out_res.ok());
  EXPECT_TRUE(vendor_out_res->is_ok());
  set_custom_control_handling(false);
}

TEST_F(DeviceTest, UsbServiceControlTransferStallRecovery) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& usb_fidl = get_usb_fidl();
  uint8_t out_payload[4] = {0x11, 0x22, 0x33, 0x44};

  // EP0 STALL recovery: when Control encounters ZX_ERR_IO_REFUSED, it resets EP0.
  set_custom_control_handling(true);
  set_timeout_handler([&](sync_completion_t* completion, zx_duration_t duration) {
    usb::BorrowedRequestQueue<void> requests = get_pending_requests();
    std::optional<usb::BorrowedRequest<void>> request = requests.pop();
    request->Complete(ZX_ERR_IO_REFUSED, 0);
    return sync_completion_wait(completion, ZX_TIME_INFINITE);
  });
  fdescriptor::wire::UsbSetup valid_vendor_out{
      .bm_request_type = kVendorDeviceOut,
      .b_request = 0x42,
      .w_value = 0,
      .w_index = 0,
      .w_length = 4,
  };
  fidl::WireResult<fuchsia_hardware_usb::Usb::ControlTransfer> stall_res =
      usb_fidl->ControlTransfer(valid_vendor_out, ZX_SEC(5),
                                fidl::VectorView<uint8_t>::FromExternal(out_payload, 4));
  ASSERT_TRUE(stall_res.ok());
  ASSERT_TRUE(stall_res->is_error());
  EXPECT_EQ(stall_res->error_value(), ZX_ERR_IO_REFUSED);
  EXPECT_EQ(get_reset_endpoint(), 0);
  set_custom_control_handling(false);
}

TEST_F(DeviceTest, UsbServiceEnableAndConnectToEndpoint) {
  async::Loop hci_loop(&kAsyncLoopConfigNeverAttachToThread);
  ASSERT_OK(hci_loop.StartThread("fake-hci-loop"));
  auto hci = std::make_unique<FakeHci>(hci_loop.dispatcher());
  hci->set_include_endpoints_in_config(true);

  uint32_t enabled_in_count = 0;
  uint32_t enabled_out_count = 0;
  uint16_t last_in_max_packet_size = 0;
  bool last_has_ss_comp = false;
  uint8_t last_max_burst = 0;
  bool last_enable_flag = false;
  zx_status_t next_enable_status = ZX_OK;
  hci->set_enable_endpoint_hook([&](uint32_t device_id, const usb_endpoint_descriptor_t* ep_desc,
                                    const usb_ss_ep_comp_descriptor_t* ss_com_desc, bool enable) {
    EXPECT_EQ(device_id, kDeviceId);
    if (next_enable_status != ZX_OK) {
      return next_enable_status;
    }
    last_enable_flag = enable;
    last_has_ss_comp = (ss_com_desc != nullptr);
    last_max_burst = ss_com_desc ? ss_com_desc->b_max_burst : 0;
    if (ep_desc->b_endpoint_address == 0x81) {
      enabled_in_count++;
      last_in_max_packet_size = usb_ep_max_packet(ep_desc);
    } else if (ep_desc->b_endpoint_address == 0x01) {
      enabled_out_count++;
    }
    return ZX_OK;
  });

  auto timer = fbl::MakeRefCounted<FakeTimer>();
  timer->set_timeout_handler([=](sync_completion_t* completion, zx_duration_t duration) {
    return sync_completion_wait(completion, duration);
  });

  std::shared_ptr<MockDevice> custom_root = MockDevice::FakeRootParent();
  fidl::Endpoints<fuchsia_hardware_usb_hci::UsbHci> hci_endpoints =
      fidl::Endpoints<fuchsia_hardware_usb_hci::UsbHci>::Create();
  fidl::BindServer(hci_loop.dispatcher(), std::move(hci_endpoints.server), hci.get());

  fidl::Endpoints<fuchsia_hardware_usb::Usb> usb_endpoints =
      fidl::Endpoints<fuchsia_hardware_usb::Usb>::Create();
  UsbDevice* custom_dev = nullptr;
  zx::result init_res = fdf::RunOnDispatcherSync((*dispatcher_)->async_dispatcher(), [&] {
    auto dev = fbl::MakeRefCounted<UsbDevice>(
        custom_root.get(), ddk::UsbHciProtocolClient(hci->proto()), std::move(hci_endpoints.client),
        kDeviceId, kHubId, kDeviceSpeed, timer, (*dispatcher_)->async_dispatcher());
    ASSERT_OK(dev->Init((*dispatcher_)->async_dispatcher()));
    dev->BindUsbServer((*dispatcher_)->async_dispatcher(), std::move(usb_endpoints.server));
    custom_dev = dev.get();
  });
  ASSERT_TRUE(init_res.is_ok());

  fidl::WireSyncClient<fuchsia_hardware_usb::Usb> client(std::move(usb_endpoints.client));

  // Invalid endpoints are rejected for ConnectToEndpoint
  fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint> ep0 =
      fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
  fidl::WireResult<fuchsia_hardware_usb::Usb::ConnectToEndpoint> bad_ep =
      client->ConnectToEndpoint(0x00, std::move(ep0.server));
  ASSERT_TRUE(bad_ep.ok());
  ASSERT_TRUE(bad_ep->is_error());
  EXPECT_EQ(bad_ep->error_value(), ZX_ERR_INVALID_ARGS);

  fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint> ep16 =
      fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
  fidl::WireResult<fuchsia_hardware_usb::Usb::ConnectToEndpoint> bad_ep16 =
      client->ConnectToEndpoint(16, std::move(ep16.server));
  ASSERT_TRUE(bad_ep16.ok());
  ASSERT_TRUE(bad_ep16->is_error());
  EXPECT_EQ(bad_ep16->error_value(), ZX_ERR_INVALID_ARGS);

  fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint> ep_res =
      fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
  fidl::WireResult<fuchsia_hardware_usb::Usb::ConnectToEndpoint> bad_ep_res =
      client->ConnectToEndpoint(0x11, std::move(ep_res.server));
  ASSERT_TRUE(bad_ep_res.ok());
  ASSERT_TRUE(bad_ep_res->is_error());
  EXPECT_EQ(bad_ep_res->error_value(), ZX_ERR_INVALID_ARGS);

  // EnableEndpoint configures the endpoint on the host controller.
  fdescriptor::wire::UsbEndpointDescriptor ep_out_desc{
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = static_cast<uint8_t>(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = 0x01,
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 512,
      .b_interval = 0,
  };
  fdescriptor::wire::UsbSsEpCompDescriptor empty_ss_comp{};
  fidl::WireResult<fuchsia_hardware_usb::Usb::EnableEndpoint> enable_out_res =
      client->EnableEndpoint(ep_out_desc, empty_ss_comp, true);
  ASSERT_TRUE(enable_out_res.ok());
  EXPECT_TRUE(enable_out_res->is_ok());
  EXPECT_EQ(enabled_out_count, 1u);
  EXPECT_TRUE(last_enable_flag);
  EXPECT_FALSE(last_has_ss_comp);

  // ConnectToEndpoint(0x01) forwards to UsbHci::ConnectToEndpoint.
  fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint> ep_out =
      fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
  fidl::WireResult<fuchsia_hardware_usb::Usb::ConnectToEndpoint> out_res =
      client->ConnectToEndpoint(0x01, std::move(ep_out.server));
  ASSERT_TRUE(out_res.ok());
  EXPECT_TRUE(out_res->is_ok());

  // EnableEndpoint with SuperSpeed Endpoint Companion descriptor passes ss_com_desc through.
  fdescriptor::wire::UsbEndpointDescriptor ep_in_desc{
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = static_cast<uint8_t>(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = 0x81,
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 1024,
      .b_interval = 0,
  };
  fdescriptor::wire::UsbSsEpCompDescriptor ss_comp{
      .b_length = sizeof(usb_ss_ep_comp_descriptor_t),
      .b_descriptor_type = static_cast<uint8_t>(fdescriptor::DescriptorType::kSsEpCompanion),
      .b_max_burst = 3,
      .bm_attributes = 0,
      .w_bytes_per_interval = 0,
  };
  fidl::WireResult<fuchsia_hardware_usb::Usb::EnableEndpoint> enable_in_res =
      client->EnableEndpoint(ep_in_desc, ss_comp, true);
  ASSERT_TRUE(enable_in_res.ok());
  EXPECT_TRUE(enable_in_res->is_ok());
  EXPECT_EQ(enabled_in_count, 1u);
  EXPECT_EQ(last_in_max_packet_size, 1024u);
  EXPECT_TRUE(last_has_ss_comp);
  EXPECT_EQ(last_max_burst, 3u);

  // If EnableEndpoint fails, it propagates the error.
  next_enable_status = ZX_ERR_IO_REFUSED;
  fidl::WireResult<fuchsia_hardware_usb::Usb::EnableEndpoint> fail_res =
      client->EnableEndpoint(ep_in_desc, empty_ss_comp, true);
  ASSERT_TRUE(fail_res.ok());
  ASSERT_TRUE(fail_res->is_error());
  EXPECT_EQ(fail_res->error_value(), ZX_ERR_IO_REFUSED);
  next_enable_status = ZX_OK;

  zx::result tear_res = fdf::RunOnDispatcherSync((*dispatcher_)->async_dispatcher(), [&] {
    custom_dev->DdkAsyncRemove();
    mock_ddk::ReleaseFlaggedDevices(custom_root.get());
  });
  ASSERT_TRUE(tear_res.is_ok());
  hci_loop.Shutdown();
}

TEST_F(DeviceTest, UsbServiceEndpointValidation) {
  fidl::WireSyncClient<fuchsia_hardware_usb::Usb>& client = get_usb_fidl();
  fdescriptor::wire::UsbSsEpCompDescriptor empty_ss_comp{};

  // Endpoint 0 is rejected.
  fdescriptor::wire::UsbEndpointDescriptor bad_ep_desc{
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = static_cast<uint8_t>(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = 0x00,
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 64,
      .b_interval = 0,
  };
  fidl::WireResult<fuchsia_hardware_usb::Usb::EnableEndpoint> bad_en_ep0 =
      client->EnableEndpoint(bad_ep_desc, empty_ss_comp, true);
  ASSERT_TRUE(bad_en_ep0.ok());
  ASSERT_TRUE(bad_en_ep0->is_error());
  EXPECT_EQ(bad_en_ep0->error_value(), ZX_ERR_INVALID_ARGS);

  // Endpoint 0x80 is rejected.
  bad_ep_desc.b_endpoint_address = 0x80;
  fidl::WireResult<fuchsia_hardware_usb::Usb::EnableEndpoint> bad_en_ep80 =
      client->EnableEndpoint(bad_ep_desc, empty_ss_comp, true);
  ASSERT_TRUE(bad_en_ep80.ok());
  ASSERT_TRUE(bad_en_ep80->is_error());
  EXPECT_EQ(bad_en_ep80->error_value(), ZX_ERR_INVALID_ARGS);

  // Endpoint number > 15 is rejected.
  bad_ep_desc.b_endpoint_address = 16;
  fidl::WireResult<fuchsia_hardware_usb::Usb::EnableEndpoint> bad_en_ep16 =
      client->EnableEndpoint(bad_ep_desc, empty_ss_comp, true);
  ASSERT_TRUE(bad_en_ep16.ok());
  ASSERT_TRUE(bad_en_ep16->is_error());
  EXPECT_EQ(bad_en_ep16->error_value(), ZX_ERR_INVALID_ARGS);

  // Endpoint address with reserved bits (0x70) set is rejected.
  bad_ep_desc.b_endpoint_address = 0x11;
  fidl::WireResult<fuchsia_hardware_usb::Usb::EnableEndpoint> bad_en_res =
      client->EnableEndpoint(bad_ep_desc, empty_ss_comp, true);
  ASSERT_TRUE(bad_en_res.ok());
  ASSERT_TRUE(bad_en_res->is_error());
  EXPECT_EQ(bad_en_res->error_value(), ZX_ERR_INVALID_ARGS);

  // Endpoint descriptor length < sizeof(usb_endpoint_descriptor_t) is rejected.
  bad_ep_desc.b_endpoint_address = 0x01;
  bad_ep_desc.b_length = sizeof(usb_endpoint_descriptor_t) - 1;
  fidl::WireResult<fuchsia_hardware_usb::Usb::EnableEndpoint> bad_en_len =
      client->EnableEndpoint(bad_ep_desc, empty_ss_comp, true);
  ASSERT_TRUE(bad_en_len.ok());
  ASSERT_TRUE(bad_en_len->is_error());
  EXPECT_EQ(bad_en_len->error_value(), ZX_ERR_INVALID_ARGS);

  // Endpoint descriptor type != Endpoint is rejected.
  bad_ep_desc.b_length = sizeof(usb_endpoint_descriptor_t);
  bad_ep_desc.b_descriptor_type = static_cast<uint8_t>(fdescriptor::DescriptorType::kInterface);
  fidl::WireResult<fuchsia_hardware_usb::Usb::EnableEndpoint> bad_en_type =
      client->EnableEndpoint(bad_ep_desc, empty_ss_comp, true);
  ASSERT_TRUE(bad_en_type.ok());
  ASSERT_TRUE(bad_en_type->is_error());
  EXPECT_EQ(bad_en_type->error_value(), ZX_ERR_INVALID_ARGS);

  // ResetEndpoint validation:
  // ep_address == 0x00 is allowed (for EP0 stall recovery)
  fidl::WireResult<fuchsia_hardware_usb::Usb::ResetEndpoint> reset_ep0 =
      client->ResetEndpoint(0x00);
  ASSERT_TRUE(reset_ep0.ok());
  EXPECT_TRUE(reset_ep0->is_ok());

  // ep_address == 0x80 is rejected (EP0 is bidirectional, 0x80 is invalid)
  fidl::WireResult<fuchsia_hardware_usb::Usb::ResetEndpoint> reset_ep80 =
      client->ResetEndpoint(0x80);
  ASSERT_TRUE(reset_ep80.ok());
  ASSERT_TRUE(reset_ep80->is_error());
  EXPECT_EQ(reset_ep80->error_value(), ZX_ERR_INVALID_ARGS);

  // ep_address > 15 is rejected
  fidl::WireResult<fuchsia_hardware_usb::Usb::ResetEndpoint> reset_ep16 = client->ResetEndpoint(16);
  ASSERT_TRUE(reset_ep16.ok());
  ASSERT_TRUE(reset_ep16->is_error());
  EXPECT_EQ(reset_ep16->error_value(), ZX_ERR_INVALID_ARGS);

  // ep_address with reserved bits set is rejected
  fidl::WireResult<fuchsia_hardware_usb::Usb::ResetEndpoint> reset_ep_res =
      client->ResetEndpoint(0x11);
  ASSERT_TRUE(reset_ep_res.ok());
  ASSERT_TRUE(reset_ep_res->is_error());
  EXPECT_EQ(reset_ep_res->error_value(), ZX_ERR_INVALID_ARGS);

  // Valid endpoint resets succeed
  fidl::WireResult<fuchsia_hardware_usb::Usb::ResetEndpoint> reset_ep1 =
      client->ResetEndpoint(0x01);
  ASSERT_TRUE(reset_ep1.ok());
  EXPECT_TRUE(reset_ep1->is_ok());

  fidl::WireResult<fuchsia_hardware_usb::Usb::ResetEndpoint> reset_ep81 =
      client->ResetEndpoint(0x81);
  ASSERT_TRUE(reset_ep81.ok());
  EXPECT_TRUE(reset_ep81->is_ok());
}

// NOLINTEND(readability-suspicious-call-argument)

}  // namespace

// A fake HCI that pretends to be a device that does dodgy things with
// configuration descriptors: namely, changing the size they claim to be
// depending on how many requests for config descriptors have been made
// previously.
class EvilFakeHci : public FakeHci {
 public:
  EvilFakeHci(async_dispatcher_t* dispatcher, uint16_t initial_config_length,
              uint16_t subsequent_config_length)
      : FakeHci(dispatcher),
        initial_config_length_(initial_config_length),
        subsequent_config_length_(subsequent_config_length) {}

  void UsbHciRequestQueue(usb_request_t* usb_request_,
                          const usb_request_complete_callback_t* complete_cb_) override {
    usb::BorrowedRequest<void> request(usb_request_, *complete_cb_, sizeof(usb_request_t));
    EXPECT_EQ(request.request()->header.ep_address, 0);
    EXPECT_EQ(request.request()->setup.bm_request_type, kStandardDeviceIn);
    EXPECT_EQ(request.request()->setup.b_request, fdescriptor::StandardRequest::kGetDescriptor);

    if (request.request()->header.ep_address == 0) {
      if (request.request()->setup.bm_request_type == kStandardDeviceIn &&
          request.request()->setup.b_request ==
              fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor)) {
        auto type = usb_descriptor_type_from_w_value(request.request()->setup.w_value);
        uint8_t index = usb_descriptor_index_from_w_value(request.request()->setup.w_value);
        switch (type) {
          case fdescriptor::DescriptorType::kDevice: {
            usb_device_descriptor_t* descriptor;
            request.Mmap(reinterpret_cast<void**>(&descriptor));
            descriptor->b_num_configurations = 2;
            descriptor->id_vendor = kVendorId;
            descriptor->id_product = kProductId;
            descriptor->b_device_class = kDeviceClass;
            descriptor->b_device_sub_class = kDeviceSubclass;
            descriptor->b_device_protocol = kDeviceProtocol;
            request.Complete(ZX_OK, sizeof(*descriptor));
          }
            return;
          case fdescriptor::DescriptorType::kConfiguration: {
            usb_configuration_descriptor_t* descriptor;
            request.Mmap(reinterpret_cast<void**>(&descriptor));
            // Use the config descriptor lengths described in the constructor
            // arguments.
            descriptor->w_total_length =
                (config_descriptor_request_count_ % 2 == 0 ? initial_config_length_
                                                           : subsequent_config_length_);
            config_descriptor_request_count_++;
            descriptor->b_configuration_value = static_cast<uint8_t>(index + 1);
            request.Complete(ZX_OK, sizeof(*descriptor));
          }
            return;
          default:
            break;
        }
      }

      // The host should not send us any requests (like attempting to set a configuration)
      // after we do questionable things with wTotalLength.
      request.Complete(ZX_ERR_INVALID_ARGS, 0);
      return;
    }
    pending_requests_.push(std::move(request));
  }

 private:
  int config_descriptor_request_count_ = 0;
  uint16_t initial_config_length_;
  uint16_t subsequent_config_length_;
  usb::BorrowedRequestQueue<void> pending_requests_;
};

class EvilDeviceTest : public zxtest::Test {
 public:
  EvilDeviceTest() : hci_(nullptr) {}

  void SetUp() override {
    auto runtime = fdf_testing::DriverRuntime::GetInstance();
    dispatcher_ =
        std::make_unique<fdf::UnownedSynchronizedDispatcher>(runtime->StartBackgroundDispatcher());
  }

  void TearDown() override { dispatcher_.reset(); }

 protected:
  std::shared_ptr<MockDevice> root_{MockDevice::FakeRootParent()};
  fdf_testing::DriverRuntime* runtime() { return fdf_testing::DriverRuntime::GetInstance(); }
  std::unique_ptr<fdf::UnownedSynchronizedDispatcher> dispatcher_;
  std::unique_ptr<EvilFakeHci> hci_;
};

TEST_F(EvilDeviceTest, GetConfigurationDescriptorTooShortRejected) {
  // We expect this device to fail to initialize because wTotalLength is too
  // short -- 1 byte is shorter than the minimal config descriptor length, so
  // such a response is invalid.
  hci_ = std::make_unique<EvilFakeHci>((*dispatcher_)->async_dispatcher(), 1, 1);
  fbl::RefPtr<FakeTimer> timer = fbl::MakeRefCounted<FakeTimer>();
  timer->set_timeout_handler([=](sync_completion_t* completion, zx_duration_t duration) {
    return sync_completion_wait(completion, duration);
  });

  async::Loop loop{&kAsyncLoopConfigNeverAttachToThread};
  auto endpoints = fidl::Endpoints<fuchsia_hardware_usb_hci::UsbHci>::Create();
  auto device = fbl::MakeRefCounted<UsbDevice>(
      root_.get(), ddk::UsbHciProtocolClient(hci_->proto()), std::move(endpoints.client), kDeviceId,
      kHubId, kDeviceSpeed, timer, loop.dispatcher());
  auto result = device->Init(loop.dispatcher());
  ASSERT_EQ(result, ZX_ERR_IO);
}

TEST_F(EvilDeviceTest, GetConfigurationDescriptorDifferentSizesAreRejected) {
  // We expect this device to fail to initialize because when we request its
  // configuration descriptors, the wTotalSize value we get back changes between
  // the first (size-fetching) request and second (full descriptor-fetching) request.
  hci_ = std::make_unique<EvilFakeHci>((*dispatcher_)->async_dispatcher(),
                                       sizeof(usb_configuration_descriptor_t), 65535);
  fbl::RefPtr<FakeTimer> timer = fbl::MakeRefCounted<FakeTimer>();
  timer->set_timeout_handler([=](sync_completion_t* completion, zx_duration_t duration) {
    return sync_completion_wait(completion, duration);
  });

  async::Loop loop{&kAsyncLoopConfigNeverAttachToThread};
  std::shared_ptr<MockDevice> root = MockDevice::FakeRootParent();
  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_hci::UsbHci>();
  ASSERT_OK(endpoints);
  auto device = fbl::MakeRefCounted<UsbDevice>(root.get(), ddk::UsbHciProtocolClient(hci_->proto()),
                                               std::move(endpoints->client), kDeviceId, kHubId,
                                               kDeviceSpeed, timer, loop.dispatcher());
  auto result = device->Init(loop.dispatcher());
  ASSERT_EQ(result, ZX_ERR_IO);
}
}  // namespace usb_bus
