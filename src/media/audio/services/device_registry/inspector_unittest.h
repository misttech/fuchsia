// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_SERVICES_DEVICE_REGISTRY_INSPECTOR_UNITTEST_H_
#define SRC_MEDIA_AUDIO_SERVICES_DEVICE_REGISTRY_INSPECTOR_UNITTEST_H_

#include <fidl/fuchsia.audio.device/cpp/natural_types.h>
#include <lib/inspect/cpp/hierarchy.h>
#include <lib/inspect/cpp/reader.h>

#include <gtest/gtest.h>

#include "src/media/audio/services/common/fidl_thread.h"
#include "src/media/audio/services/device_registry/adr_server_unittest_base.h"
#include "src/media/audio/services/device_registry/common_unittest.h"
#include "src/media/audio/services/device_registry/inspector.h"
#include "src/media/audio/services/device_registry/testing/fake_composite.h"

namespace media_audio {

// This provides unittest functions for Inspector and its child classes.
class InspectorTest : public AudioDeviceRegistryServerTestBase {
 private:
  static inspect::Hierarchy InnerGetHierarchy() {
    auto& component_inspector = Inspector::Singleton()->component_inspector();
    auto& inspector = component_inspector->inspector();

    zx::vmo duplicate = inspector.DuplicateVmo();
    if (duplicate.get() == ZX_HANDLE_INVALID) {
      return inspect::Hierarchy();
    }

    auto ret = inspect::ReadFromVmo(duplicate);
    EXPECT_TRUE(ret.is_ok());
    if (ret.is_ok()) {
      return ret.take_value();
    }

    return inspect::Hierarchy();
  }

 protected:
  static inline const std::string kClassName = "InspectorTest";
  static inline const fuchsia_audio_device::RingBufferOptions kDefaultRingBufferOptions{{
      .format = fuchsia_audio::Format{{.sample_type = fuchsia_audio::SampleType::kInt16,
                                       .channel_count = 2,
                                       .frames_per_second = 22000}},
      .ring_buffer_min_bytes = 2000,
  }};

  static inspect::Hierarchy GetHierarchy() {
    auto h = InnerGetHierarchy();
    h.Sort();
    return h;
  }

  // Use this if you don't need to preconfigure a RingBuffer before adding the device.
  std::shared_ptr<FakeComposite> CreateAndAddFakeComposite() {
    auto fake_driver = CreateFakeComposite();
    adr_service()->AddDevice(Device::Create(
        adr_service(), dispatcher(), "Test composite name",
        fuchsia_audio_device::DeviceType::kComposite,
        fuchsia_audio_device::DriverClient::WithComposite(fake_driver->Enable()), kClassName));
    RunLoopUntilIdle();
    return fake_driver;
  }

  std::optional<TokenId> WaitForAddedDeviceTokenId(
      fidl::Client<fuchsia_audio_device::Registry>& registry_client) {
    std::optional<TokenId> added_device_id;
    registry_client->WatchDevicesAdded().Then(
        [&added_device_id](
            fidl::Result<fuchsia_audio_device::Registry::WatchDevicesAdded>& result) mutable {
          ASSERT_TRUE(result.is_ok()) << result.error_value();
          ASSERT_TRUE(result->devices().has_value());
          ASSERT_EQ(result->devices()->size(), 1u);
          ASSERT_TRUE(result->devices()->at(0).token_id().has_value());
          added_device_id = result->devices()->at(0).token_id();
        });
    RunLoopUntilIdle();
    return added_device_id;
  }

  void CreateControlledDevice() {
    device_ = Device::Create(
        adr_service(), dispatcher(), "Test composite name",
        fuchsia_audio_device::DeviceType::kComposite,
        fuchsia_audio_device::DriverClient::WithComposite(fake_driver()->Enable()), kClassName);
    adr_service()->AddDevice(device());
    RunLoopUntilIdle();
    auto registry = CreateTestRegistryServer();
    std::optional<TokenId> added_device_id = WaitForAddedDeviceTokenId(registry->client());
    ASSERT_EQ(RegistryServer::count(), 1u);
    ASSERT_TRUE(added_device_id.has_value());
    auto [presence, device_to_control] = adr_service()->FindDeviceByTokenId(*added_device_id);
    EXPECT_EQ(presence, AudioDeviceRegistry::DevicePresence::Active);
    ASSERT_EQ(device_, device_to_control);
    control_ = CreateTestControlServer(device_);
    RunLoopUntilIdle();
    ASSERT_EQ(ControlServer::count(), 1u);
  }

  // RingBuffer testcase setup, used in a number of RingBuffer-related unittests
  // Must be saved: fake_driver/device/control/ring_buffer_client/ring_buffer
  void AddDeviceAndCreateRingBuffer() {
    fake_driver_ = CreateFakeComposite();
    element_id_ = FakeComposite::kMaxRingBufferElementId;
    fake_driver()->EnableActiveChannelsSupport(element_id_);
    fake_driver()->ReserveRingBufferSize(element_id_, 8192);

    CreateControlledDevice();

    auto [ring_buffer_client_end, ring_buffer_server_end] =
        CreateNaturalAsyncClientOrDie<fuchsia_audio_device::RingBuffer>();

    ring_buffer_client_ = fidl::Client<fuchsia_audio_device::RingBuffer>(
        std::move(ring_buffer_client_end), dispatcher(), ring_buffer_fidl_handler().get());
    bool received_callback = false;

    rb_format_ = SafeRingBufferFormatFromElementRingBufferFormatSets(
        element_id_, device()->ring_buffer_format_sets());
    requested_ring_buffer_bytes_ = 2000;
    control()
        ->client()
        ->CreateRingBuffer({{
            .element_id = element_id_,
            .options = fuchsia_audio_device::RingBufferOptions{{
                .format = rb_format_,
                .ring_buffer_min_bytes = requested_ring_buffer_bytes_,
            }},
            .ring_buffer_server = std::move(ring_buffer_server_end),
        }})
        .Then([&received_callback,
               this](fidl::Result<fuchsia_audio_device::Control::CreateRingBuffer>& result) {
          EXPECT_TRUE(result.is_ok()) << result.error_value();
          ring_buffer_ = std::move(result->ring_buffer());
          received_callback = true;
        });
    RunLoopUntilIdle();
    EXPECT_TRUE(received_callback);
    EXPECT_TRUE(ring_buffer_client_.is_valid());
  }

  std::shared_ptr<Device>& device() { return device_; }
  std::shared_ptr<FakeComposite>& fake_driver() { return fake_driver_; }
  void set_fake_driver(std::shared_ptr<FakeComposite> driver) { fake_driver_ = std::move(driver); }

  ElementId element_id() const { return element_id_; }
  std::unique_ptr<TestServerAndNaturalAsyncClient<ControlServer>>& control() { return control_; }

  fuchsia_audio::Format rb_format() const { return rb_format_; }
  uint32_t requested_ring_buffer_bytes() const { return requested_ring_buffer_bytes_; }

  fidl::Client<fuchsia_audio_device::RingBuffer>& ring_buffer_client() {
    return ring_buffer_client_;
  }
  std::optional<fuchsia_audio::RingBuffer>& ring_buffer() { return ring_buffer_; }

 private:
  std::shared_ptr<FakeComposite> fake_driver_;
  std::shared_ptr<Device> device_;

  ElementId element_id_;
  std::unique_ptr<TestServerAndNaturalAsyncClient<ControlServer>> control_;

  fuchsia_audio::Format rb_format_;
  uint32_t requested_ring_buffer_bytes_;

  fidl::Client<fuchsia_audio_device::RingBuffer> ring_buffer_client_;
  std::optional<fuchsia_audio::RingBuffer> ring_buffer_;

  std::shared_ptr<FidlThread> server_thread_;
};

}  // namespace media_audio

#endif  // SRC_MEDIA_AUDIO_SERVICES_DEVICE_REGISTRY_INSPECTOR_UNITTEST_H_
