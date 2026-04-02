// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/services/device_registry/common.h"

#include <fidl/fuchsia.audio.device/cpp/natural_types.h>
#include <fidl/fuchsia.audio/cpp/common_types.h>
#include <fidl/fuchsia.hardware.audio/cpp/common_types.h>
#include <fidl/fuchsia.hardware.audio/cpp/natural_types.h>

#include <vector>

#include "src/media/audio/services/device_registry/format_utils.h"
#include "src/media/audio/services/device_registry/validate.h"

namespace media_audio {

namespace fad = fuchsia_audio_device;
namespace fha = fuchsia_hardware_audio;

bool DaiFormatIsSupported(ElementId element_id,
                          const std::vector<fad::ElementDaiFormatSet>& element_dai_format_sets,
                          const fha::DaiFormat& format) {
  std::optional<std::vector<fha::DaiSupportedFormats>> dai_format_sets;
  for (const auto& element_sets_entry : element_dai_format_sets) {
    if (element_sets_entry.element_id().has_value() &&
        *element_sets_entry.element_id() == element_id) {
      if (!element_sets_entry.format_sets().has_value()) {
        return false;
      }
      dai_format_sets = *element_sets_entry.format_sets();
      break;
    }
  }

  if (!dai_format_sets.has_value()) {
    return false;
  }

  for (const auto& dai_format_set : *dai_format_sets) {
    bool match = false;
    for (auto channel_count : dai_format_set.number_of_channels()) {
      if (channel_count == format.number_of_channels()) {
        match = true;
        break;
      }
    }
    if (!match) {
      continue;
    }

    match = false;
    for (const auto& sample_format : dai_format_set.sample_formats()) {
      if (sample_format == format.sample_format()) {
        match = true;
        break;
      }
    }
    if (!match) {
      continue;
    }

    match = false;
    for (const auto& frame_format : dai_format_set.frame_formats()) {
      if (frame_format == format.frame_format()) {
        match = true;
        break;
      }
    }
    if (!match) {
      continue;
    }

    match = false;
    for (const auto& rate : dai_format_set.frame_rates()) {
      if (rate == format.frame_rate()) {
        match = true;
        break;
      }
    }
    if (!match) {
      continue;
    }

    match = false;
    for (const auto& bits : dai_format_set.bits_per_slot()) {
      if (bits == format.bits_per_slot()) {
        match = true;
        break;
      }
    }
    if (!match) {
      continue;
    }

    match = false;
    for (const auto& bits : dai_format_set.bits_per_sample()) {
      if (bits == format.bits_per_sample()) {
        match = true;
        break;
      }
    }
    if (!match) {
      continue;
    }
    // This DaiFormatSet survived with a match on all aspects.
    return true;
  }
  // None of the DaiFormatSets survived through all of the aspects.
  return false;
}

namespace {

bool FormatSetSupportsPcmFormat(const fad::PcmFormatSet& format_set,
                                const fha::PcmFormat& pcm_format) {
  if (!format_set.sample_types()) {
    return false;
  }
  bool match = false;
  for (auto sample_type : *format_set.sample_types()) {
    auto driver_pcm = MapSampleTypeToDriverPcm(sample_type);
    if (driver_pcm && driver_pcm->sample_format == pcm_format.sample_format() &&
        driver_pcm->bytes_per_sample == pcm_format.bytes_per_sample()) {
      match = true;
      break;
    }
  }
  if (!match) {
    return false;
  }

  match = false;
  if (!format_set.channel_sets().has_value()) {
    return false;
  }
  for (const auto& channel_set : *format_set.channel_sets()) {
    if (channel_set.attributes().has_value() &&
        channel_set.attributes()->size() == pcm_format.number_of_channels()) {
      match = true;
      break;
    }
  }
  if (!match) {
    return false;
  }

  match = false;
  if (!format_set.frame_rates().has_value()) {
    return false;
  }
  for (auto frame_rate : *format_set.frame_rates()) {
    if (frame_rate == pcm_format.frame_rate()) {
      match = true;
      break;
    }
  }
  return match;
}

bool FormatSetSupportsEncoding(const fha::SupportedEncodings& format_set,
                               const fha::Encoding& encoding) {
  if (!format_set.encoding_types()) {
    return false;
  }
  bool match = false;
  for (auto encoding_type : *format_set.encoding_types()) {
    if (encoding_type == *encoding.encoding_type()) {
      match = true;
      break;
    }
  }
  if (!match) {
    return false;
  }

  match = false;
  if (!format_set.decoded_channel_sets().has_value()) {
    return false;
  }
  for (const auto& channel_set : *format_set.decoded_channel_sets()) {
    if (channel_set.attributes() &&
        channel_set.attributes()->size() == *encoding.decoded_channel_count()) {
      match = true;
      break;
    }
  }
  if (!match) {
    return false;
  }

  match = false;
  if (!format_set.decoded_frame_rates().has_value()) {
    return false;
  }
  for (auto frame_rate : *format_set.decoded_frame_rates()) {
    if (frame_rate == *encoding.decoded_frame_rate()) {
      match = true;
      break;
    }
  }
  return match;
}

}  // namespace

bool RingBufferFormatIsSupported(
    ElementId element_id,
    const std::vector<fad::ElementRingBufferFormatSet>& element_ring_buffer_format_sets,
    const fha::Format2& format) {
  if (format.Which() != fha::Format2::Tag::kPcmFormat ||
      !ValidatePcmFormat(format.pcm_format().value())) {
    return false;
  }
  std::optional<std::vector<fad::PcmFormatSet>> ring_buffer_format_sets;
  for (const auto& element_sets_entry : element_ring_buffer_format_sets) {
    if (element_sets_entry.element_id().has_value() &&
        *element_sets_entry.element_id() == element_id) {
      if (!element_sets_entry.format_sets().has_value()) {
        return false;
      }
      ring_buffer_format_sets = *element_sets_entry.format_sets();
      break;
    }
  }
  if (!ring_buffer_format_sets.has_value()) {
    return false;
  }

  for (const auto& ring_buffer_format_set : *ring_buffer_format_sets) {
    if (FormatSetSupportsPcmFormat(ring_buffer_format_set, format.pcm_format().value())) {
      return true;
    }
  }
  return false;
}

bool PacketStreamFormatIsSupported(
    ElementId element_id,
    const std::vector<fad::ElementPacketStreamFormatSet>& element_packet_stream_format_sets,
    const fha::Format2& format) {
  if (!ValidatePacketStreamFormat(format)) {
    return false;
  }

  const fad::ElementPacketStreamFormatSet* element_format_set = nullptr;
  for (const auto& entry : element_packet_stream_format_sets) {
    if (entry.element_id().has_value() && *entry.element_id() == element_id) {
      element_format_set = &entry;
      break;
    }
  }

  if (!element_format_set || !element_format_set->format_sets().has_value()) {
    return false;
  }

  if (format.pcm_format().has_value()) {
    for (const auto& packet_stream_format : *element_format_set->format_sets()) {
      if (packet_stream_format.pcm_format().has_value() &&
          FormatSetSupportsPcmFormat(packet_stream_format.pcm_format().value(),
                                     format.pcm_format().value())) {
        return true;
      }
    }
    return false;
  }

  if (format.encoding().has_value()) {
    for (const auto& packet_stream_format : *element_format_set->format_sets()) {
      if (packet_stream_format.supported_encodings().has_value() &&
          FormatSetSupportsEncoding(packet_stream_format.supported_encodings().value(),
                                    format.encoding().value())) {
        return true;
      }
    }
    return false;
  }

  return false;  // `format` contains an unknown union variant.
}

}  // namespace media_audio
