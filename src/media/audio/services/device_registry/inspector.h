// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_SERVICES_DEVICE_REGISTRY_INSPECTOR_H_
#define SRC_MEDIA_AUDIO_SERVICES_DEVICE_REGISTRY_INSPECTOR_H_

#include <fidl/fuchsia.audio.device/cpp/common_types.h>
#include <lib/async/dispatcher.h>
#include <lib/inspect/component/cpp/component.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/cpp/vmo/types.h>

#include <string>

#include "src/media/audio/services/device_registry/basic_types.h"

namespace media_audio {

static constexpr std::string_view kDetectionConnectionErrors =
    "Device_detection_connection_failure_count";
static constexpr std::string_view kDetectionOtherErrors =
    "Device_detection_unclassified_error_count";
static constexpr std::string_view kDetectionUnsupportedDevices =
    "Device_detection_unsupported_device_count";

static constexpr std::string_view kDevices = "Devices";
static constexpr std::string_view kAddedAt = "added_at";
static constexpr std::string_view kAddedBy = "added_by";
static constexpr std::string_view kFailedAt = "failed_at";
static constexpr std::string_view kRemovedAt = "removed_at";
static constexpr std::string_view kDeviceType = "type";
static constexpr std::string_view kTokenId = "token_id";
static constexpr std::string_view kHealthy = "healthy";
static constexpr std::string_view kIsInput = "is_input";
static constexpr std::string_view kManufacturer = "manufacturer";
static constexpr std::string_view kProduct = "product";
static constexpr std::string_view kUniqueId = "unique_id";
static constexpr std::string_view kClockDomain = "clock_domain";
static constexpr std::string_view kDriverTimeout = "driver_timeouts";
static constexpr std::string_view kDriverLateResponse = "driver_late_responses";

static constexpr std::string_view kDaiElements = "DAI_elements";
static constexpr std::string_view kRingBufferElements = "RingBuffer_elements";
static constexpr std::string_view kDescription = "description";
static constexpr std::string_view kElementId = "element_id";

static constexpr std::string_view kFormatProps = "format";
static constexpr std::string_view kBitsPerFrame = "bits_per_slot";
static constexpr std::string_view kBitsPerSample = "bits_per_sample";
static constexpr std::string_view kChannelBitmask = "channel_bitmask";
static constexpr std::string_view kChannelCount = "channel_count";
static constexpr std::string_view kFramesPerSecond = "frames_per_second";
static constexpr std::string_view kFrameFormat = "frame_format";
static constexpr std::string_view kSampleFormat = "sample_format";

static constexpr std::string_view kBufferProps = "buffer";
static constexpr std::string_view kRequestedBytes = "requested_bytes";
static constexpr std::string_view kProducerFrames = "producer_frames";
static constexpr std::string_view kConsumerFrames = "consumer_frames";
static constexpr std::string_view kVmoBytes = "vmo_bytes";

static constexpr std::string_view kRunningIntervals = "Running_intervals";
static constexpr std::string_view kStartedAt = "started_at";
static constexpr std::string_view kStoppedAt = "stopped_at";

static constexpr std::string_view kSetActiveChannelsCalls = "SetActiveChannels_calls";
static constexpr std::string_view kCalledAt = "called_at";
static constexpr std::string_view kCompletedAt = "completed_at";

static constexpr std::string_view kFidlServers = "FIDL_servers";
static constexpr std::string_view kRegistryServerInstances = "RegistryServer_instances";
static constexpr std::string_view kObserverServerInstances = "ObserverServer_instances";
static constexpr std::string_view kControlCreatorServerInstances = "ControlCreatorServer_instances";
static constexpr std::string_view kControlServerInstances = "ControlServer_instances";
static constexpr std::string_view kRingBufferServerInstances = "RingBufferServer_instances";
static constexpr std::string_view kProviderServerInstances = "ProviderServer_instances";
static constexpr std::string_view kCreatedAt = "created_at";
static constexpr std::string_view kDestroyedAt = "destroyed_at";
static constexpr std::string_view kAddedDevices = "Added_devices";

//
// These classes encapsulate the creation and update of the fuchsia Inspect data store. The parent
// Inspector singleton entails the system-provided ComponentInspector and offers methods that enable
// ADR to chronicle objects without integrating directly with Inspect.
//
// Each class handles Inspect for an associated entity (which may not be an actual ADR object).

// This represents a single call to SetActiveChannels.
class SetActiveChannelsInspectInstance {
 public:
  SetActiveChannelsInspectInstance(inspect::Node set_active_channels_node, uint64_t channel_bitmask,
                                   const zx::time& called_at, const zx::time& completed_at);
  ~SetActiveChannelsInspectInstance();

 private:
  static constexpr std::string_view kClassName = "SetActiveChannelsInspectInstance";

  inspect::Node set_active_channels_node_;
};

// This represents a single Start/Stop segment.
class RunningIntervalInspectInstance {
 public:
  RunningIntervalInspectInstance(inspect::Node running_interval_node, const zx::time& started_at);
  ~RunningIntervalInspectInstance();

  void RecordStopTime(const zx::time& stopped_at);

 private:
  static constexpr std::string_view kClassName = "RunningIntervalInspectInstance";

  inspect::Node running_interval_node_;
};

// This represents an active instance of the audio driver RingBuffer protocol.
class RingBufferInspectInstance {
 public:
  RingBufferInspectInstance(inspect::Node ring_buffer_instance_node, const zx::time& created_at);
  ~RingBufferInspectInstance();

  void RecordDestructionTime(const zx::time& destroyed_at);

  void RecordStartTime(const zx::time& started_at);
  void RecordStopTime(const zx::time& stopped_at);

  inspect::Node& inspect_node() { return ring_buffer_instance_node_; }
  std::shared_ptr<SetActiveChannelsInspectInstance> RecordSetActiveChannelsCall(
      uint64_t channel_bitmask, const zx::time& called_at, const zx::time& completed_at);

  void RecordBuffer(uint64_t requested_bytes, uint64_t producer_frames, uint64_t consumer_frames,
                    uint64_t vmo_size);
  void RecordFormat(uint32_t channel_count, uint32_t frames_per_second,
                    fuchsia_audio::SampleType sample_type);

 private:
  static constexpr std::string_view kClassName = "RingBufferInspectInstance";

  inspect::Node ring_buffer_instance_node_;

  inspect::Node set_active_channels_calls_root_node_;
  std::vector<std::shared_ptr<SetActiveChannelsInspectInstance>> set_active_channels_calls_;

  inspect::Node running_intervals_root_node_;
  std::vector<std::shared_ptr<RunningIntervalInspectInstance>> running_intervals_;

  inspect::Node buffer_node_;
  inspect::Node format_node_;
};

// This represents a ring buffer element expressed in the hardware topology. Over time, it may have
// RingBufferInspectInstance children, if a client connects to the RingBuffer API.
class RingBufferElement {
 public:
  RingBufferElement(inspect::Node ring_buffer_element_node, ElementId element_id,
                    const std::optional<std::string>& element_name);
  ~RingBufferElement();

  inspect::Node& inspect_node() { return ring_buffer_element_node_; }
  std::shared_ptr<RingBufferInspectInstance> RecordRingBufferInstance(const zx::time& created_at);

  ElementId element_id() const { return element_id_; }

 private:
  static constexpr std::string_view kClassName = "RingBufferElement";

  inspect::Node ring_buffer_element_node_;
  ElementId element_id_;

  std::vector<std::shared_ptr<RingBufferInspectInstance>> ring_buffer_instances_;
};

// This represents a DAI element expressed in the hardware topology.
class DaiElement {
 public:
  DaiElement(inspect::Node dai_element_node, ElementId element_id,
             const std::optional<std::string>& element_name);
  ~DaiElement();

  inspect::Node& inspect_node() { return dai_element_node_; }
  void RecordSetDaiFormat(const zx::time& set_at,
                          const fuchsia_hardware_audio::DaiFormat& dai_format);

  ElementId element_id() const { return element_id_; }

 private:
  static constexpr std::string_view kClassName = "DaiElement";

  inspect::Node dai_element_node_;
  inspect::Node format_node_;
  ElementId element_id_;
};

// This represents an audio driver and its device. It is created when an audio device is detected in
// DevFs or added via Provider/AddDevice.
class DeviceInspectInstance {
 public:
  DeviceInspectInstance(inspect::Node device_node, std::string device_name,
                        fuchsia_audio_device::DeviceType device_type, const zx::time& added_at,
                        const std::string& added_by);
  ~DeviceInspectInstance();

  inspect::Node& inspect_node() { return device_node_; }

  void RecordTokenId(TokenId token_id);
  void RecordDeviceHealthOk();
  void RecordProperties(std::optional<bool> is_input, std::optional<std::string> manufacturer,
                        std::optional<std::string> product,
                        std::optional<std::string> unique_instance_id,
                        std::optional<ClockDomain> clock_domain);

  std::shared_ptr<DaiElement> RecordDaiElement(ElementId element_id,
                                               const std::optional<std::string>& element_name);

  std::shared_ptr<RingBufferElement> RecordRingBufferElement(
      ElementId element_id, const std::optional<std::string>& element_name);
  std::shared_ptr<RingBufferInspectInstance> RecordRingBufferInstance(ElementId element_id,
                                                                      const zx::time& created_at);

  void RecordCommandTimeout(const std::string& cmd_tag, const zx::duration& expected,
                            std::optional<zx::duration> actual);
  void RecordError(const zx::time& failed_at);
  void RecordRemoval(const zx::time& removed_at);

 private:
  static constexpr std::string_view kClassName = "DeviceInspectInstance";

  inspect::Node device_node_;
  std::string name_;

  inspect::Node dai_elements_root_node_;
  inspect::Node ring_buffer_elements_root_node_;

  inspect::BoolProperty healthy_;
  inspect::UintProperty count_timeout_;
  inspect::UintProperty count_late_response_;

  std::vector<std::shared_ptr<DaiElement>> dai_elements_;
  std::vector<std::shared_ptr<RingBufferElement>> ring_buffer_elements_;
};

// This represents a client connection to one of the six ADR FIDL protocols:
// Registry, Observer, ControlCreator, Control, RingBuffer or Provider.
class FidlServerInspectInstance {
 public:
  FidlServerInspectInstance(inspect::Node instance_node, const zx::time& created_at);
  ~FidlServerInspectInstance();

  void RecordDestructionTime(const zx::time& destroyed_at);

 protected:
  inspect::Node& instance_node() { return instance_node_; }

 private:
  static constexpr std::string_view kClassName = "FidlServerInspectInstance";

  inspect::Node instance_node_;
};

// We save additional information or each client Provider instance: the devices that it has added.
// We reuse DeviceInspectInstance but use only a subset (name, type, added_at).
class ProviderInspectInstance : public FidlServerInspectInstance {
 public:
  ProviderInspectInstance(inspect::Node provider_instance_node, const zx::time& created_at);
  ~ProviderInspectInstance();

  void RecordAddedDevice(const std::string& device_name,
                         const fuchsia_audio_device::DeviceType& device_type,
                         const zx::time& added_at);

 private:
  static constexpr std::string_view kClassName = "ProviderInspectInstance";

  inspect::Node provider_devices_root_node_;
  std::vector<std::shared_ptr<DeviceInspectInstance>> provider_devices_;
};

// This singleton manages (and owns, from a lifetime mgmt standpoint) Inspect for the entire ADR.
// Inspect for core/audio_device_registry contains two sections: Devices and FIDL Servers.
// The Devices section contains info on devices that have been detected and presented to clients.
// The FIDL Servers section contains info on client instances of the six ADR FIDL protocols.
class Inspector {
 public:
  static void Initialize(async_dispatcher_t* dispatcher);
  static std::unique_ptr<Inspector>& Singleton() { return singleton_; }
  std::unique_ptr<inspect::ComponentInspector>& component_inspector() {
    return component_inspector_;
  }

  explicit Inspector(async_dispatcher_t* dispatcher);
  ~Inspector();

  void RecordHealthOk();
  void RecordUnhealthy(const std::string& health_message);

  void RecordDetectionFailureToConnect();
  void RecordDetectionFailureOther();
  void RecordDetectionFailureUnsupported();

  std::shared_ptr<DeviceInspectInstance> RecordDeviceInitializing(
      const std::string& device_name, fuchsia_audio_device::DeviceType device_type,
      const zx::time& added_at, const std::string& added_by);

  // Create an Inspect node for the instance (e.g. a child of control_servers_root_ if this is a
  // Control instance), wrap it in a FidlServerInspectInstance object, and return a shared_ptr to
  // it. This class (via control_server_instances_ etc.) owns the instance node; the child
  // FidlServerInspectInstance creates nodes or properties attached to the instance node.
  std::shared_ptr<FidlServerInspectInstance> RecordRegistryInstance(const zx::time& created_at);
  std::shared_ptr<FidlServerInspectInstance> RecordObserverInstance(const zx::time& created_at);
  std::shared_ptr<FidlServerInspectInstance> RecordControlCreatorInstance(
      const zx::time& created_at);
  std::shared_ptr<FidlServerInspectInstance> RecordControlInstance(const zx::time& created_at);
  std::shared_ptr<FidlServerInspectInstance> RecordRingBufferInstance(const zx::time& created_at);
  std::shared_ptr<ProviderInspectInstance> RecordProviderInspectInstance(
      const zx::time& created_at);

 private:
  static constexpr std::string_view kClassName = "Inspector";

  static std::unique_ptr<Inspector> singleton_;

  std::unique_ptr<inspect::ComponentInspector> component_inspector_;
  inspect::Node& inspect_root_;

  inspect::UintProperty count_device_failed_to_connect_;
  inspect::UintProperty count_device_watcher_failures_;
  inspect::UintProperty count_detected_unsupported_device_type_;

  // The top-level "Devices" parent node
  inspect::Node devices_root_;

  // Each entry in this vector represents an audio driver and device.
  // Note that entries are not removed when a device encounters an error or is removed.
  std::vector<std::shared_ptr<DeviceInspectInstance>> device_instances_;

  // The top-level "FIDL Servers" parent node
  inspect::Node fidl_servers_root_;

  inspect::Node registry_servers_root_;
  inspect::Node observer_servers_root_;
  inspect::Node control_creator_servers_root_;
  inspect::Node control_servers_root_;
  inspect::Node ring_buffer_servers_root_;
  inspect::Node provider_servers_root_;

  // Each entry in these vectors represents a client instance of one of the ADR protocols and
  // includes an Inspect node for that instance as well as timestamps for creation and destruction.
  // Note that entries are not removed when a client disconnects.
  std::vector<std::shared_ptr<FidlServerInspectInstance>> registry_server_instances_;
  std::vector<std::shared_ptr<FidlServerInspectInstance>> observer_server_instances_;
  std::vector<std::shared_ptr<FidlServerInspectInstance>> control_creator_server_instances_;
  std::vector<std::shared_ptr<FidlServerInspectInstance>> control_server_instances_;
  std::vector<std::shared_ptr<FidlServerInspectInstance>> ring_buffer_server_instances_;
  std::vector<std::shared_ptr<ProviderInspectInstance>> provider_server_instances_;
};

}  // namespace media_audio

#endif  // SRC_MEDIA_AUDIO_SERVICES_DEVICE_REGISTRY_INSPECTOR_H_
