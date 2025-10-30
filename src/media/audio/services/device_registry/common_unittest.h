// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_SERVICES_DEVICE_REGISTRY_COMMON_UNITTEST_H_
#define SRC_MEDIA_AUDIO_SERVICES_DEVICE_REGISTRY_COMMON_UNITTEST_H_

#include <fidl/fuchsia.audio.device/cpp/natural_types.h>

#include "src/media/audio/services/device_registry/basic_types.h"

namespace media_audio {

// Convenience function to avoid taking a dependency upon audio/lib/[format | format2].
inline uint32_t frame_size(const fuchsia_audio::Format& format) {
  if (!format.sample_type().has_value()) {
    return 0;
  }
  if (!format.channel_count().has_value()) {
    return 0;
  }

  uint32_t sample_size = 0;
  switch (*format.sample_type()) {
    case fuchsia_audio::SampleType::kUint8:
      sample_size = 1;
      break;
    case fuchsia_audio::SampleType::kInt16:
      sample_size = 2;
      break;
    case fuchsia_audio::SampleType::kInt32:
    case fuchsia_audio::SampleType::kFloat32:
      sample_size = 4;
      break;
    case fuchsia_audio::SampleType::kFloat64:
      sample_size = 8;
      break;
    default:
      break;
  }
  return sample_size * *format.channel_count();
}

fuchsia_hardware_audio::DaiFormat SafeDaiFormatFromElementDaiFormatSets(
    ElementId element_id,
    const std::vector<fuchsia_audio_device::ElementDaiFormatSet>& element_dai_format_sets);
fuchsia_hardware_audio::DaiFormat SafeDaiFormatFromDaiFormatSets(
    const std::vector<fuchsia_hardware_audio::DaiSupportedFormats>& dai_format_sets);

fuchsia_hardware_audio::DaiFormat SecondDaiFormatFromElementDaiFormatSets(
    ElementId element_id,
    const std::vector<fuchsia_audio_device::ElementDaiFormatSet>& element_dai_format_sets);
fuchsia_hardware_audio::DaiFormat SecondDaiFormatFromDaiFormatSets(
    const std::vector<fuchsia_hardware_audio::DaiSupportedFormats>& dai_format_sets);

fuchsia_hardware_audio::DaiFormat UnsupportedDaiFormatFromElementDaiFormatSets(
    ElementId element_id,
    const std::vector<fuchsia_audio_device::ElementDaiFormatSet>& element_dai_format_sets);
fuchsia_hardware_audio::DaiFormat UnsupportedDaiFormatFromDaiFormatSets(
    const std::vector<fuchsia_hardware_audio::DaiSupportedFormats>& dai_format_sets);

fuchsia_audio::Format SafeRingBufferFormatFromElementRingBufferFormatSets(
    ElementId element_id, const std::vector<fuchsia_audio_device::ElementRingBufferFormatSet>&
                              element_ring_buffer_format_sets);
fuchsia_audio::Format SafeRingBufferFormatFromRingBufferFormatSets(
    const std::vector<fuchsia_audio_device::PcmFormatSet>& ring_buffer_format_sets);

fuchsia_hardware_audio::Format SafeDriverRingBufferFormatFromDriverRingBufferFormatSets(
    const std::vector<fuchsia_hardware_audio::SupportedFormats>& driver_ring_buffer_format_sets);
fuchsia_hardware_audio::Format SafeDriverRingBufferFormatFromElementDriverRingBufferFormatSets(
    ElementId element_id,
    const std::vector<std::pair<ElementId, std::vector<fuchsia_hardware_audio::SupportedFormats>>>&
        element_driver_ring_buffer_format_sets);

}  // namespace media_audio

#endif  // SRC_MEDIA_AUDIO_SERVICES_DEVICE_REGISTRY_COMMON_UNITTEST_H_
