// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/services/device_registry/inspector.h"

#include <lib/inspect/component/cpp/component.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/cpp/vmo/types.h>

#include <memory>
#include <string>

#include "src/media/audio/services/device_registry/logging.h"

namespace media_audio {

// static
// This singleton handles Inspect for the entire service.
std::unique_ptr<Inspector> Inspector::singleton_ = nullptr;

// static
void Inspector::Initialize(async_dispatcher_t* dispatcher) {
  ADR_LOG_STATIC(kTraceInspector);
  // Should only be called once.
  if (singleton_ == nullptr) {
    singleton_ = std::make_unique<Inspector>(dispatcher);
  } else {
    FX_LOGS(ERROR) << "Inspector::Initialize should only be called once";
  }
}

///////////////////////////////////////
// SetActiveChannelsInspectInstance methods
SetActiveChannelsInspectInstance::SetActiveChannelsInspectInstance(
    inspect::Node set_active_channels_node, uint64_t channel_bitmask, const zx::time& called_at,
    const zx::time& completed_at)
    : set_active_channels_node_(std::move(set_active_channels_node)) {
  ADR_LOG_METHOD(kTraceInspector);
  set_active_channels_node_.RecordInt(kCalledAt, called_at.get());
  set_active_channels_node_.RecordInt(kCompletedAt, completed_at.get());
  set_active_channels_node_.RecordUint(kChannelBitmask, channel_bitmask);
}

SetActiveChannelsInspectInstance::~SetActiveChannelsInspectInstance() {
  ADR_LOG_METHOD(kTraceInspector);
}

///////////////////////////////////////
// RunningIntervalInspectInstance methods
RunningIntervalInspectInstance::RunningIntervalInspectInstance(inspect::Node running_interval_node,
                                                               const zx::time& started_at)
    : running_interval_node_(std::move(running_interval_node)) {
  ADR_LOG_METHOD(kTraceInspector);
  running_interval_node_.RecordInt(kStartedAt, started_at.get());
}

RunningIntervalInspectInstance::~RunningIntervalInspectInstance() {
  ADR_LOG_METHOD(kTraceInspector);
}

void RunningIntervalInspectInstance::RecordStopTime(const zx::time& stopped_at) {
  ADR_LOG_METHOD(kTraceInspector) << kStoppedAt << stopped_at.get();
  running_interval_node_.RecordInt(kStoppedAt, stopped_at.get());
}

///////////////////////////////////////
// RingBufferInspectInstance methods
RingBufferInspectInstance::RingBufferInspectInstance(inspect::Node ring_buffer_instance_node,
                                                     const zx::time& created_at)
    : ring_buffer_instance_node_(std::move(ring_buffer_instance_node)) {
  ADR_LOG_METHOD(kTraceInspector);
  ring_buffer_instance_node_.RecordInt(kCreatedAt, created_at.get());
}

RingBufferInspectInstance::~RingBufferInspectInstance() { ADR_LOG_METHOD(kTraceInspector); }

void RingBufferInspectInstance::RecordDestructionTime(const zx::time& destroyed_at) {
  ADR_LOG_METHOD(kTraceInspector) << kDestroyedAt << destroyed_at.get();
  ring_buffer_instance_node_.RecordInt(kDestroyedAt, destroyed_at.get());
}

void RingBufferInspectInstance::RecordStartTime(const zx::time& started_at) {
  ADR_LOG_METHOD(kTraceInspector);
  if (running_intervals_.empty()) {
    running_intervals_root_node_ = ring_buffer_instance_node_.CreateChild(kRunningIntervals);
  }
  auto running_interval_node =
      running_intervals_root_node_.CreateChild(std::to_string(running_intervals_.size()));
  auto running_interval = std::make_shared<RunningIntervalInspectInstance>(
      std::move(running_interval_node), started_at);
  running_intervals_.push_back(running_interval);
}

void RingBufferInspectInstance::RecordStopTime(const zx::time& stopped_at) {
  ADR_LOG_METHOD(kTraceInspector) << kStoppedAt << stopped_at.get();
  if (!running_intervals_.empty()) {
    (*running_intervals_.rbegin())->RecordStopTime(stopped_at);
  }
}

std::shared_ptr<SetActiveChannelsInspectInstance>
RingBufferInspectInstance::RecordSetActiveChannelsCall(uint64_t channel_bitmask,
                                                       const zx::time& called_at,
                                                       const zx::time& completed_at) {
  ADR_LOG_METHOD(kTraceInspector);
  if (set_active_channels_calls_.empty()) {
    set_active_channels_calls_root_node_ =
        ring_buffer_instance_node_.CreateChild(kSetActiveChannelsCalls);
  }
  auto set_active_channels_instance_node = set_active_channels_calls_root_node_.CreateChild(
      std::to_string(set_active_channels_calls_.size()));
  auto set_active_channels_instance = std::make_shared<SetActiveChannelsInspectInstance>(
      std::move(set_active_channels_instance_node), channel_bitmask, called_at, completed_at);

  set_active_channels_calls_.push_back(set_active_channels_instance);
  return set_active_channels_instance;
}

void RingBufferInspectInstance::RecordBuffer(uint64_t requested_bytes, uint64_t producer_frames,
                                             uint64_t consumer_frames, uint64_t vmo_size) {
  buffer_node_ = ring_buffer_instance_node_.CreateChild(kBufferProps);
  buffer_node_.RecordUint(kRequestedBytes, requested_bytes);
  buffer_node_.RecordUint(kProducerFrames, producer_frames);
  buffer_node_.RecordUint(kConsumerFrames, consumer_frames);
  buffer_node_.RecordUint(kVmoBytes, vmo_size);
}

void RingBufferInspectInstance::RecordFormat(uint32_t channel_count, uint32_t frames_per_second,
                                             fuchsia_audio::SampleType sample_type) {
  format_node_ = ring_buffer_instance_node_.CreateChild(kFormatProps);
  format_node_.RecordUint(kChannelCount, channel_count);
  format_node_.RecordUint(kFramesPerSecond, frames_per_second);

  switch (sample_type) {
    case fuchsia_audio::SampleType::kUint8:
      format_node_.RecordString(kSampleFormat, "UINT_8");
      return;
    case fuchsia_audio::SampleType::kInt16:
      format_node_.RecordString(kSampleFormat, "INT_16");
      return;
    case fuchsia_audio::SampleType::kInt32:
      format_node_.RecordString(kSampleFormat, "INT_32");
      return;
    case fuchsia_audio::SampleType::kFloat32:
      format_node_.RecordString(kSampleFormat, "FLOAT_32");
      return;
    case fuchsia_audio::SampleType::kFloat64:
      format_node_.RecordString(kSampleFormat, "FLOAT_64");
      return;
    default:
      format_node_.RecordString(kSampleFormat, "UNKNOWN");
      return;
  }
}

///////////////////////////////////////
// RingBufferElement methods
RingBufferElement::RingBufferElement(inspect::Node ring_buffer_element_node, ElementId element_id,
                                     const std::optional<std::string>& element_name)
    : ring_buffer_element_node_(std::move(ring_buffer_element_node)), element_id_(element_id) {
  ADR_LOG_METHOD(kTraceInspector);
  ring_buffer_element_node_.RecordUint(kElementId, element_id);
  if (element_name.has_value()) {
    ring_buffer_element_node_.RecordString(kDescription, *element_name);
  }
  // Consider recording an 'is_input' bool, indicating dataflow direction (derived from Topology?).
}

RingBufferElement::~RingBufferElement() { ADR_LOG_METHOD(kTraceInspector); }

std::shared_ptr<RingBufferInspectInstance> RingBufferElement::RecordRingBufferInstance(
    const zx::time& created_at) {
  ADR_LOG_METHOD(kTraceInspector);

  auto ring_buffer_instance_node = ring_buffer_element_node_.CreateChild(
      std::string("instance_") + std::to_string(ring_buffer_instances_.size()));
  auto ring_buffer_instance =
      std::make_shared<RingBufferInspectInstance>(std::move(ring_buffer_instance_node), created_at);
  ADR_LOG_METHOD(kTraceInspector) << "returning " << ring_buffer_instance;

  ring_buffer_instances_.push_back(ring_buffer_instance);
  return ring_buffer_instance;
}

///////////////////////////////////////
// DaiElement methods
DaiElement::DaiElement(inspect::Node dai_element_node, ElementId element_id,
                       const std::optional<std::string>& element_name)
    : dai_element_node_(std::move(dai_element_node)), element_id_(element_id) {
  ADR_LOG_METHOD(kTraceInspector);
  dai_element_node_.RecordUint(kElementId, element_id);
  if (element_name.has_value()) {
    dai_element_node_.RecordString(kDescription, *element_name);
  }
}

DaiElement::~DaiElement() { ADR_LOG_METHOD(kTraceInspector); }

void DaiElement::RecordSupportedFormatSets(
    const std::vector<fuchsia_hardware_audio::DaiSupportedFormats>& format_sets) {
  ADR_LOG_METHOD(kTraceInspector);

  dai_format_sets_header_node_ = dai_element_node_.CreateChild(kSupportedFormats);
  dai_format_sets_.clear();
  for (auto i = 0u; i < format_sets.size(); ++i) {
    dai_format_sets_.emplace_back(DaiFormatSetRecord{});
    auto& dai_format_set = dai_format_sets_[i];
    dai_format_set.dai_format_set_node =
        dai_format_sets_header_node_.CreateChild("dai_format_set_" + std::to_string(i));

    const auto& channel_counts = format_sets[i].number_of_channels();
    dai_format_set.dai_format_set_channel_counts =
        dai_format_set.dai_format_set_node.CreateUintArray(kChannelCount, channel_counts.size());
    for (auto j = 0u; j < channel_counts.size(); ++j) {
      dai_format_set.dai_format_set_channel_counts.Set(j, channel_counts[j]);
    }

    const auto& sample_formats = format_sets[i].sample_formats();
    dai_format_set.dai_format_set_sample_formats =
        dai_format_set.dai_format_set_node.CreateStringArray(kSampleFormat, sample_formats.size());
    for (auto j = 0u; j < sample_formats.size(); ++j) {
      std::stringstream ss;
      ss << sample_formats[j];
      dai_format_set.dai_format_set_sample_formats.Set(j, ss.str());
    }

    const auto& frame_formats = format_sets[i].frame_formats();
    dai_format_set.dai_format_set_frame_formats =
        dai_format_set.dai_format_set_node.CreateStringArray(kFrameFormat, frame_formats.size());
    for (auto j = 0u; j < frame_formats.size(); ++j) {
      std::stringstream ss;
      ss << frame_formats[j];
      dai_format_set.dai_format_set_frame_formats.Set(j, ss.str());
    }

    const auto& frame_rates = format_sets[i].frame_rates();
    dai_format_set.dai_format_set_frame_rates =
        dai_format_set.dai_format_set_node.CreateUintArray(kFramesPerSecond, frame_rates.size());
    for (auto j = 0u; j < frame_rates.size(); ++j) {
      dai_format_set.dai_format_set_frame_rates.Set(j, frame_rates[j]);
    }

    const auto& frame_sizes = format_sets[i].bits_per_slot();
    dai_format_set.dai_format_set_frame_sizes =
        dai_format_set.dai_format_set_node.CreateUintArray(kBitsPerFrame, frame_sizes.size());
    for (auto j = 0u; j < frame_sizes.size(); ++j) {
      dai_format_set.dai_format_set_frame_sizes.Set(j, frame_sizes[j]);
    }

    const auto& sample_sizes = format_sets[i].bits_per_sample();
    dai_format_set.dai_format_set_sample_sizes =
        dai_format_set.dai_format_set_node.CreateUintArray(kBitsPerSample, sample_sizes.size());
    for (auto j = 0u; j < sample_sizes.size(); ++j) {
      dai_format_set.dai_format_set_sample_sizes.Set(j, sample_sizes[j]);
    }
  }
}

void DaiElement::RecordSetDaiFormat(const zx::time& set_at,
                                    const fuchsia_hardware_audio::DaiFormat& dai_format) {
  ADR_LOG_METHOD(kTraceInspector);
  format_node_ = dai_element_node_.CreateChild(kFormatProps);

  format_node_.RecordUint(kBitsPerFrame, dai_format.bits_per_slot());
  format_node_.RecordUint(kBitsPerSample, dai_format.bits_per_sample());
  format_node_.RecordUint(kChannelCount, dai_format.number_of_channels());
  format_node_.RecordUint(kChannelBitmask, dai_format.channels_to_use_bitmask());
  format_node_.RecordUint(kFramesPerSecond, dai_format.frame_rate());

  std::stringstream format_stream;
  format_stream << dai_format.frame_format();
  format_node_.RecordString(kFrameFormat, format_stream.str());

  format_stream.str("");
  format_stream.clear();
  format_stream << dai_format.sample_format();
  format_node_.RecordString(kSampleFormat, format_stream.str());
}

///////////////////////////////////////
// DeviceInspectInstance methods
DeviceInspectInstance::DeviceInspectInstance(inspect::Node device_node, std::string device_name,
                                             fuchsia_audio_device::DeviceType device_type,
                                             const zx::time& added_at, const std::string& added_by)
    : device_node_(std::move(device_node)), name_(std::move(device_name)) {
  ADR_LOG_METHOD(kTraceInspector);
  std::stringstream device_type_ss;
  device_node_.RecordInt(kAddedAt, added_at.get());
  device_node_.RecordString(kAddedBy, added_by);

  device_type_ss << device_type;
  device_node_.RecordString(kDeviceType, device_type_ss.str());

  count_timeout_ = device_node_.CreateUint(kDriverTimeout, 0);
  count_late_response_ = device_node_.CreateUint(kDriverLateResponse, 0);
}

DeviceInspectInstance::~DeviceInspectInstance() { ADR_LOG_METHOD(kTraceInspector); }

void DeviceInspectInstance::RecordTokenId(TokenId token_id) {
  ADR_LOG_METHOD(kTraceInspector);
  device_node_.RecordUint(kTokenId, token_id);
}

void DeviceInspectInstance::RecordDeviceHealthOk() {
  ADR_LOG_METHOD(kTraceInspector);
  healthy_ = device_node_.CreateBool(kHealthy, true);
}

void DeviceInspectInstance::RecordProperties(std::optional<bool> is_input,
                                             std::optional<std::string> manufacturer,
                                             std::optional<std::string> product,
                                             std::optional<std::string> unique_instance_id,
                                             std::optional<ClockDomain> clock_domain) {
  ADR_LOG_METHOD(kTraceInspector);
  if (is_input.has_value()) {
    device_node_.RecordBool(kIsInput, *is_input);
  }
  if (manufacturer.has_value()) {
    device_node_.RecordString(kManufacturer, *manufacturer);
  }
  if (product.has_value()) {
    device_node_.RecordString(kProduct, *product);
  }
  if (unique_instance_id.has_value()) {
    device_node_.RecordString(kUniqueId, *unique_instance_id);
  }
  if (clock_domain.has_value()) {
    std::string domain_str = std::to_string(*clock_domain);
    if (*clock_domain == fuchsia_hardware_audio::kClockDomainMonotonic) {
      domain_str += " (CLOCK_DOMAIN_MONOTONIC)";
    } else if (*clock_domain == fuchsia_hardware_audio::kClockDomainExternal) {
      domain_str += " (CLOCK_DOMAIN_EXTERNAL)";
    }
    device_node_.RecordString(kClockDomain, domain_str);
  }
}

std::shared_ptr<DaiElement> DeviceInspectInstance::RecordDaiElement(
    ElementId element_id, const std::optional<std::string>& element_name) {
  ADR_LOG_METHOD(kTraceInspector);
  if (dai_elements_.empty()) {
    dai_elements_root_node_ = device_node_.CreateChild(kDaiElements);
  }
  auto dai_element_node = dai_elements_root_node_.CreateChild(std::to_string(dai_elements_.size()));
  auto dai_element =
      std::make_shared<DaiElement>(std::move(dai_element_node), element_id, element_name);

  dai_elements_.push_back(dai_element);
  return dai_element;
}

std::shared_ptr<RingBufferElement> DeviceInspectInstance::RecordRingBufferElement(
    ElementId element_id, const std::optional<std::string>& element_name) {
  ADR_LOG_METHOD(kTraceInspector);
  if (ring_buffer_elements_.empty()) {
    ring_buffer_elements_root_node_ = device_node_.CreateChild(kRingBufferElements);
  }
  auto ring_buffer_element_node =
      ring_buffer_elements_root_node_.CreateChild(std::to_string(ring_buffer_elements_.size()));
  auto ring_buffer_element = std::make_shared<RingBufferElement>(
      std::move(ring_buffer_element_node), element_id, element_name);

  ring_buffer_elements_.push_back(ring_buffer_element);
  return ring_buffer_element;
}

std::shared_ptr<RingBufferInspectInstance> DeviceInspectInstance::RecordRingBufferInstance(
    ElementId element_id, const zx::time& created_at) {
  ADR_LOG_METHOD(kTraceInspector) << kElementId << element_id;
  auto found = std::find_if(ring_buffer_elements_.begin(), ring_buffer_elements_.end(),
                            [element_id](std::shared_ptr<RingBufferElement> rb_element) {
                              return (rb_element->element_id() == element_id);
                            });
  if (found == ring_buffer_elements_.end()) {
    ADR_WARN_OBJECT() << "Cannot create RB inspect instance: element_id " << element_id
                      << " not found";
    return nullptr;
  }
  return (*found)->RecordRingBufferInstance(created_at);
}

void DeviceInspectInstance::RecordCommandTimeout(const std::string& cmd_tag,
                                                 const zx::duration& expected,
                                                 std::optional<zx::duration> actual) {
  if (actual.has_value()) {
    count_late_response_.Add(1);
    ADR_LOG_METHOD(kTraceInspector)
        << "Driver command '" << cmd_tag << "' expected in " << expected.to_usecs()
        << " usec, received in " << actual->to_usecs() << " usec.";
  } else {
    count_timeout_.Add(1);
    ADR_LOG_METHOD(kTraceInspector) << "Driver command '" << cmd_tag << "' expected in "
                                    << expected.to_usecs() << " usec, not yet received.";
  }
}

void DeviceInspectInstance::RecordError(const zx::time& failed_at) {
  ADR_LOG_METHOD(kTraceInspector);
  healthy_ = device_node_.CreateBool(kHealthy, false);
  device_node_.RecordInt(kFailedAt, failed_at.get());
}

void DeviceInspectInstance::RecordRemoval(const zx::time& removed_at) {
  ADR_LOG_METHOD(kTraceInspector);
  device_node_.RecordInt(kRemovedAt, removed_at.get());
}

///////////////////////////////////////
// FidlServerInspectInstance methods
FidlServerInspectInstance::FidlServerInspectInstance(inspect::Node instance_node,
                                                     const zx::time& created_at)
    : instance_node_(std::move(instance_node)) {
  ADR_LOG_METHOD(kTraceInspector);
  instance_node_.RecordInt(kCreatedAt, created_at.get());
}

FidlServerInspectInstance::~FidlServerInspectInstance() { ADR_LOG_METHOD(kTraceInspector); }

void FidlServerInspectInstance::RecordDestructionTime(const zx::time& destroyed_at) {
  ADR_LOG_METHOD(kTraceInspector);
  instance_node_.RecordInt(kDestroyedAt, destroyed_at.get());
}

///////////////////////////////////////
// ProviderInspectInstance methods
ProviderInspectInstance::ProviderInspectInstance(inspect::Node provider_instance_node,
                                                 const zx::time& created_at)
    : FidlServerInspectInstance(std::move(provider_instance_node), created_at) {
  ADR_LOG_METHOD(kTraceInspector);
  provider_devices_root_node_ = instance_node().CreateChild(kAddedDevices);
}

ProviderInspectInstance::~ProviderInspectInstance() { ADR_LOG_METHOD(kTraceInspector); }

void ProviderInspectInstance::RecordAddedDevice(const std::string& device_name,
                                                const fuchsia_audio_device::DeviceType& device_type,
                                                const zx::time& added_at) {
  ADR_LOG_METHOD(kTraceInspector);
  auto instance_node = provider_devices_root_node_.CreateChild(device_name);
  auto instance = std::make_shared<DeviceInspectInstance>(std::move(instance_node), device_name,
                                                          device_type, added_at, "Provider");
  provider_devices_.push_back(instance);
}

///////////////////////////////////////
// Inspector methods
Inspector::Inspector(async_dispatcher_t* dispatcher)
    : component_inspector_(
          std::make_unique<inspect::ComponentInspector>(dispatcher, inspect::PublishOptions{})),
      inspect_root_(component_inspector_->root()) {
  ADR_LOG_METHOD(kTraceInspector);
  component_inspector_->Health().StartingUp();

  devices_root_ = inspect_root_.CreateChild(kDevices);
  fidl_servers_root_ = inspect_root_.CreateChild(kFidlServers);

  registry_servers_root_ = fidl_servers_root_.CreateChild(kRegistryServerInstances);
  observer_servers_root_ = fidl_servers_root_.CreateChild(kObserverServerInstances);
  control_creator_servers_root_ = fidl_servers_root_.CreateChild(kControlCreatorServerInstances);
  control_servers_root_ = fidl_servers_root_.CreateChild(kControlServerInstances);
  ring_buffer_servers_root_ = fidl_servers_root_.CreateChild(kRingBufferServerInstances);
  provider_servers_root_ = fidl_servers_root_.CreateChild(kProviderServerInstances);

  count_device_failed_to_connect_ = inspect_root_.CreateUint(kDetectionConnectionErrors, 0);
  count_device_watcher_failures_ = inspect_root_.CreateUint(kDetectionOtherErrors, 0);
  count_detected_unsupported_device_type_ =
      inspect_root_.CreateUint(kDetectionUnsupportedDevices, 0);
}

Inspector::~Inspector() { ADR_LOG_METHOD(kTraceInspector); }

void Inspector::RecordHealthOk() {
  ADR_LOG_METHOD(kTraceInspector);
  component_inspector_->Health().Ok();
}

void Inspector::RecordUnhealthy(const std::string& health_message) {
  ADR_LOG_METHOD(kTraceInspector);
  component_inspector_->Health().Unhealthy(health_message);
}

void Inspector::RecordDetectionFailureToConnect() {
  ADR_LOG_METHOD(kTraceInspector);
  count_device_failed_to_connect_.Add(1);
}

void Inspector::RecordDetectionFailureOther() {
  ADR_LOG_METHOD(kTraceInspector);
  count_device_watcher_failures_.Add(1);
}

void Inspector::RecordDetectionFailureUnsupported() {
  ADR_LOG_METHOD(kTraceInspector);
  count_detected_unsupported_device_type_.Add(1);
}

std::shared_ptr<DeviceInspectInstance> Inspector::RecordDeviceInitializing(
    const std::string& device_name, fuchsia_audio_device::DeviceType device_type,
    const zx::time& added_at, const std::string& added_by) {
  ADR_LOG_METHOD(kTraceInspector);
  auto instance_node = devices_root_.CreateChild(device_name);
  auto instance = std::make_shared<DeviceInspectInstance>(std::move(instance_node), device_name,
                                                          device_type, added_at, added_by);
  device_instances_.push_back(instance);
  return instance;
}

std::shared_ptr<FidlServerInspectInstance> Inspector::RecordRegistryInstance(
    const zx::time& created_at) {
  ADR_LOG_METHOD(kTraceInspector);
  auto instance_node =
      registry_servers_root_.CreateChild(std::to_string(registry_server_instances_.size()));
  auto fidl_instance =
      std::make_shared<FidlServerInspectInstance>(std::move(instance_node), created_at);
  registry_server_instances_.push_back(fidl_instance);
  return fidl_instance;
}

std::shared_ptr<FidlServerInspectInstance> Inspector::RecordObserverInstance(
    const zx::time& created_at) {
  ADR_LOG_METHOD(kTraceInspector);
  auto instance_node =
      observer_servers_root_.CreateChild(std::to_string(observer_server_instances_.size()));
  auto fidl_instance =
      std::make_shared<FidlServerInspectInstance>(std::move(instance_node), created_at);
  observer_server_instances_.push_back(fidl_instance);
  return fidl_instance;
}

std::shared_ptr<FidlServerInspectInstance> Inspector::RecordControlCreatorInstance(
    const zx::time& created_at) {
  ADR_LOG_METHOD(kTraceInspector);
  auto instance_node = control_creator_servers_root_.CreateChild(
      std::to_string(control_creator_server_instances_.size()));
  auto fidl_instance =
      std::make_shared<FidlServerInspectInstance>(std::move(instance_node), created_at);
  control_creator_server_instances_.push_back(fidl_instance);
  return fidl_instance;
}

std::shared_ptr<FidlServerInspectInstance> Inspector::RecordControlInstance(
    const zx::time& created_at) {
  ADR_LOG_METHOD(kTraceInspector);
  auto instance_node =
      control_servers_root_.CreateChild(std::to_string(control_server_instances_.size()));
  auto fidl_instance =
      std::make_shared<FidlServerInspectInstance>(std::move(instance_node), created_at);
  control_server_instances_.push_back(fidl_instance);
  return fidl_instance;
}

std::shared_ptr<FidlServerInspectInstance> Inspector::RecordRingBufferInstance(
    const zx::time& created_at) {
  ADR_LOG_METHOD(kTraceInspector);
  auto instance_node =
      ring_buffer_servers_root_.CreateChild(std::to_string(ring_buffer_server_instances_.size()));
  auto fidl_instance =
      std::make_shared<FidlServerInspectInstance>(std::move(instance_node), created_at);
  ring_buffer_server_instances_.push_back(fidl_instance);
  return fidl_instance;
}

std::shared_ptr<ProviderInspectInstance> Inspector::RecordProviderInspectInstance(
    const zx::time& created_at) {
  ADR_LOG_METHOD(kTraceInspector);
  auto instance_node =
      provider_servers_root_.CreateChild(std::to_string(provider_server_instances_.size()));
  auto fidl_instance =
      std::make_shared<ProviderInspectInstance>(std::move(instance_node), created_at);
  provider_server_instances_.push_back(fidl_instance);
  return fidl_instance;
}

}  // namespace media_audio
