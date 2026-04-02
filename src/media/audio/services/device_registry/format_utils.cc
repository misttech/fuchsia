// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/services/device_registry/format_utils.h"

#include <lib/syslog/cpp/macros.h>

namespace media_audio {

namespace fha = fuchsia_hardware_audio;

namespace fad = fuchsia_audio_device;

std::optional<DriverPcmDetails> MapSampleTypeToDriverPcm(fuchsia_audio::SampleType sample_type) {
  switch (sample_type) {
    case fuchsia_audio::SampleType::kUint8:
      return DriverPcmDetails{fha::SampleFormat::kPcmUnsigned, 1, 8};
    case fuchsia_audio::SampleType::kInt16:
      return DriverPcmDetails{fha::SampleFormat::kPcmSigned, 2, 16};
    case fuchsia_audio::SampleType::kInt32:
      return DriverPcmDetails{fha::SampleFormat::kPcmSigned, 4, 32};
    case fuchsia_audio::SampleType::kFloat32:
      return DriverPcmDetails{fha::SampleFormat::kPcmFloat, 4, 32};
    case fuchsia_audio::SampleType::kFloat64:
      return DriverPcmDetails{fha::SampleFormat::kPcmFloat, 8, 64};
    default:
      return std::nullopt;
  }
}

std::optional<fuchsia_audio::SampleType> MapDriverPcmToSampleType(fha::SampleFormat sample_format,
                                                                  uint8_t bytes_per_sample) {
  if (sample_format == fha::SampleFormat::kPcmUnsigned && bytes_per_sample == 1) {
    return fuchsia_audio::SampleType::kUint8;
  }
  if (sample_format == fha::SampleFormat::kPcmSigned) {
    if (bytes_per_sample == 2) {
      return fuchsia_audio::SampleType::kInt16;
    }
    if (bytes_per_sample == 4) {
      return fuchsia_audio::SampleType::kInt32;
    }
  }
  if (sample_format == fha::SampleFormat::kPcmFloat) {
    if (bytes_per_sample == 4) {
      return fuchsia_audio::SampleType::kFloat32;
    }
    if (bytes_per_sample == 8) {
      return fuchsia_audio::SampleType::kFloat64;
    }
  }
  return std::nullopt;
}

size_t CountFormatMatches(const std::vector<fha::SampleFormat>& sample_formats,
                          fha::SampleFormat format_to_match) {
  return std::count(sample_formats.begin(), sample_formats.end(), format_to_match);
}

size_t CountUcharMatches(const std::vector<uint8_t>& uchars, size_t uchar_to_match) {
  return std::count(uchars.begin(), uchars.end(), static_cast<uint8_t>(uchar_to_match));
}

// Map from fuchsia_hardware_audio::PcmSupportedFormats to fuchsia_audio_device::PcmFormatSet.
std::optional<fad::PcmFormatSet> MapPcmSupportedFormats(
    const fha::PcmSupportedFormats& pcm_formats) {
  if (!pcm_formats.frame_rates() || pcm_formats.frame_rates()->empty()) {
    FX_LOGS(WARNING) << "Could not translate a format set - frame_rates was empty";
    return std::nullopt;
  }
  const uint32_t max_format_rate =
      *std::max_element(pcm_formats.frame_rates()->begin(), pcm_formats.frame_rates()->end());

  // Construct channel_sets
  std::vector<fad::ChannelSet> channel_sets;
  if (!pcm_formats.channel_sets()) {
    FX_LOGS(WARNING) << "Could not translate a format set - channel_sets was absent";
    return std::nullopt;
  }
  for (const auto& chan_set : *pcm_formats.channel_sets()) {
    std::vector<fad::ChannelAttributes> attributes;
    for (const auto& attribs : *chan_set.attributes()) {
      std::optional<uint32_t> max_channel_frequency;
      if (attribs.max_frequency().has_value()) {
        max_channel_frequency = std::min(*attribs.max_frequency(), max_format_rate / 2);
      }
      attributes.push_back({{
          .min_frequency = attribs.min_frequency(),
          .max_frequency = max_channel_frequency,
      }});
    }
    channel_sets.push_back({{.attributes = attributes}});
  }
  if (channel_sets.empty()) {
    FX_LOGS(WARNING) << "Could not translate a format set - channel_sets was empty";
    return std::nullopt;
  }

  // Construct our sample_types by intersecting vectors received from the device.
  std::vector<fuchsia_audio::SampleType> sample_types;
  if (!pcm_formats.sample_formats() || !pcm_formats.bytes_per_sample()) {
    FX_LOGS(WARNING)
        << "Could not translate a format set - missing sample_formats or bytes_per_sample";
    return std::nullopt;
  }
  const std::vector<fuchsia_audio::SampleType> kAllSampleTypes = {
      fuchsia_audio::SampleType::kUint8,   fuchsia_audio::SampleType::kInt16,
      fuchsia_audio::SampleType::kInt32,   fuchsia_audio::SampleType::kFloat32,
      fuchsia_audio::SampleType::kFloat64,
  };

  for (auto type : kAllSampleTypes) {
    auto driver_pcm = MapSampleTypeToDriverPcm(type);
    if (driver_pcm &&
        CountFormatMatches(*pcm_formats.sample_formats(), driver_pcm->sample_format) > 0 &&
        CountUcharMatches(*pcm_formats.bytes_per_sample(), driver_pcm->bytes_per_sample) > 0) {
      sample_types.push_back(type);
    }
  }

  if (sample_types.empty()) {
    FX_LOGS(WARNING) << "Could not translate a format set - sample_types was empty";
    return std::nullopt;
  }

  if (pcm_formats.frame_rates()->empty()) {
    FX_LOGS(WARNING) << "Could not translate a format set - frame_rates was empty";
    return std::nullopt;
  }

  // Make a copy of the frame_rates result, so we can sort it.
  std::vector<uint32_t> frame_rates = *pcm_formats.frame_rates();
  std::ranges::sort(frame_rates);

  return fad::PcmFormatSet{{
      .channel_sets = channel_sets,
      .sample_types = sample_types,
      .frame_rates = frame_rates,
  }};
}

// Translate from fuchsia_hardware_audio::SupportedFormats to fuchsia_audio_device::PcmFormatSet.
std::vector<fad::PcmFormatSet> TranslateRingBufferFormatSets(
    const std::vector<fha::SupportedFormats2>& ring_buffer_format_sets) {
  std::vector<fad::PcmFormatSet> translated_ring_buffer_format_sets;
  for (const auto& ring_buffer_format_set : ring_buffer_format_sets) {
    if (ring_buffer_format_set.Which() != fha::SupportedFormats2::Tag::kPcmSupportedFormats) {
      FX_LOGS(WARNING) << "TranslateRingBufferFormatSets: ignored unsupported format set type "
                       << static_cast<uint32_t>(ring_buffer_format_set.Which());
      continue;
    }
    auto pcm_format_set =
        MapPcmSupportedFormats(ring_buffer_format_set.pcm_supported_formats().value());
    if (pcm_format_set) {
      translated_ring_buffer_format_sets.push_back(std::move(*pcm_format_set));
    } else {
      FX_LOGS(WARNING) << "TranslateRingBufferFormatSets: could not translate format set";
    }
  }
  return translated_ring_buffer_format_sets;
}

std::vector<fuchsia_audio_device::PacketStreamSupportedFormats> TranslatePacketStreamFormatSets(
    const std::vector<fha::SupportedFormats2>& packet_stream_format_sets) {
  std::vector<fuchsia_audio_device::PacketStreamSupportedFormats> translated_formats;

  for (const auto& packet_stream_format_set : packet_stream_format_sets) {
    if (packet_stream_format_set.Which() == fha::SupportedFormats2::Tag::kPcmSupportedFormats) {
      auto pcm_format_set =
          MapPcmSupportedFormats(packet_stream_format_set.pcm_supported_formats().value());
      if (pcm_format_set) {
        translated_formats.emplace_back(
            fad::PacketStreamSupportedFormats::WithPcmFormat(std::move(*pcm_format_set)));
      } else {
        FX_LOGS(WARNING) << "TranslatePacketStreamFormatSets: could not translate pcm format set";
      }
    } else if (packet_stream_format_set.Which() ==
               fha::SupportedFormats2::Tag::kSupportedEncodings) {
      auto& encodings = packet_stream_format_set.supported_encodings().value();
      if (encodings.encoding_types()->empty() || encodings.decoded_channel_sets()->empty()) {
        FX_LOGS(WARNING) << "Could not translate encoded format set - missing"
                         << (encodings.encoding_types()->empty() ? " encoding_types" : "")
                         << (encodings.decoded_channel_sets()->empty() ? " decoded_channel_sets"
                                                                       : "");
        continue;
      }
      translated_formats.emplace_back(
          fad::PacketStreamSupportedFormats::WithSupportedEncodings(encodings));
    } else {
      FX_LOGS(WARNING) << "TranslatePacketStreamFormatSets: ignored unsupported format set type "
                       << static_cast<uint32_t>(packet_stream_format_set.Which());
    }
  }
  return translated_formats;
}

}  // namespace media_audio
