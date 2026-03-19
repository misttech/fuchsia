// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.audio.signalprocessing/cpp/common_types.h>
#include <fidl/fuchsia.hardware.audio.signalprocessing/cpp/natural_types.h>
#include <fidl/fuchsia.hardware.audio/cpp/natural_types.h>
#include <lib/zx/time.h>

#include <unordered_map>

#include "src/media/audio/services/device_registry/testing/fakes/fake_composite.h"

namespace media_audio {

namespace fha = fuchsia_hardware_audio;
namespace fhasp = fuchsia_hardware_audio_signalprocessing;

// static definitions: signalprocessing elements/topologies and format sets

// DaiFormats and format sets
//
const fha::DaiFrameFormat FakeComposite::kDefaultDaiFrameFormat =
    fha::DaiFrameFormat::WithFrameFormatStandard(fha::DaiFrameFormatStandard::kI2S);
const fha::DaiFrameFormat FakeComposite::kDefaultDaiFrameFormat2 =
    fha::DaiFrameFormat::WithFrameFormatStandard(fha::DaiFrameFormatStandard::kNone);

const std::vector<uint32_t> FakeComposite::kDefaultDaiNumberOfChannelsSet{
    kDefaultDaiNumberOfChannels, kDefaultDaiNumberOfChannels2};
const std::vector<uint32_t> FakeComposite::kDefaultDaiNumberOfChannelsSet2{
    kDefaultDaiNumberOfChannels2};
const std::vector<fha::DaiSampleFormat> FakeComposite::kDefaultDaiSampleFormatSet{
    kDefaultDaiSampleFormat};
const std::vector<fha::DaiSampleFormat> FakeComposite::kDefaultDaiSampleFormatSet2{
    kDefaultDaiSampleFormat, kDefaultDaiSampleFormat2};
const std::vector<fha::DaiFrameFormat> FakeComposite::kDefaultDaiFrameFormatSet{
    kDefaultDaiFrameFormat, kDefaultDaiFrameFormat2};
const std::vector<fha::DaiFrameFormat> FakeComposite::kDefaultDaiFrameFormatSet2{
    kDefaultDaiFrameFormat2};
const std::vector<uint32_t> FakeComposite::kDefaultDaiFrameRateSet{kDefaultDaiFrameRate};
const std::vector<uint32_t> FakeComposite::kDefaultDaiFrameRateSet2{kDefaultDaiFrameRate,
                                                                    kDefaultDaiFrameRate2};
const std::vector<uint8_t> FakeComposite::kDefaultDaiBitsPerSlotSet{kDefaultDaiBitsPerSlot,
                                                                    kDefaultDaiBitsPerSlot2};
const std::vector<uint8_t> FakeComposite::kDefaultDaiBitsPerSlotSet2{kDefaultDaiBitsPerSlot2};
const std::vector<uint8_t> FakeComposite::kDefaultDaiBitsPerSampleSet{kDefaultDaiBitsPerSample};
const std::vector<uint8_t> FakeComposite::kDefaultDaiBitsPerSampleSet2{kDefaultDaiBitsPerSample,
                                                                       kDefaultDaiBitsPerSample2};

const fha::DaiSupportedFormats FakeComposite::kDefaultDaiFormatSet{{
    .number_of_channels = kDefaultDaiNumberOfChannelsSet,
    .sample_formats = kDefaultDaiSampleFormatSet,
    .frame_formats = kDefaultDaiFrameFormatSet,
    .frame_rates = kDefaultDaiFrameRateSet,
    .bits_per_slot = kDefaultDaiBitsPerSlotSet,
    .bits_per_sample = kDefaultDaiBitsPerSampleSet,
}};
const fha::DaiSupportedFormats FakeComposite::kDefaultDaiFormatSet2{{
    .number_of_channels = kDefaultDaiNumberOfChannelsSet2,
    .sample_formats = kDefaultDaiSampleFormatSet2,
    .frame_formats = kDefaultDaiFrameFormatSet2,
    .frame_rates = kDefaultDaiFrameRateSet2,
    .bits_per_slot = kDefaultDaiBitsPerSlotSet2,
    .bits_per_sample = kDefaultDaiBitsPerSampleSet2,
}};

// DaiFormatSets that are returned by the driver.
const std::vector<fha::DaiSupportedFormats> FakeComposite::kDefaultDaiFormatSets{
    kDefaultDaiFormatSet};
const std::vector<fha::DaiSupportedFormats> FakeComposite::kDefaultDaiFormatSets2{
    kDefaultDaiFormatSet2};

// Map of Dai format sets, by element. Used within the driver.
const std::unordered_map<ElementId, std::vector<fha::DaiSupportedFormats>>
    FakeComposite::kDefaultDaiFormatsMap = {{
        {kSourceDaiElementId, kDefaultDaiFormatSets},
        {kDestDaiElementId, kDefaultDaiFormatSets2},
    }};

// Specific DAI formats
const fha::DaiFormat FakeComposite::kDefaultDaiFormat{{
    .number_of_channels = kDefaultDaiNumberOfChannels,
    .channels_to_use_bitmask = (1u << kDefaultDaiNumberOfChannels) - 1u,
    .sample_format = kDefaultDaiSampleFormat,
    .frame_format = kDefaultDaiFrameFormat,
    .frame_rate = kDefaultDaiFrameRate,
    .bits_per_slot = kDefaultDaiBitsPerSlot,
    .bits_per_sample = kDefaultDaiBitsPerSample,
}};
const fha::DaiFormat FakeComposite::kDefaultDaiFormat2{{
    .number_of_channels = kDefaultDaiNumberOfChannels2,
    .channels_to_use_bitmask = (1u << kDefaultDaiNumberOfChannels2) - 1u,
    .sample_format = kDefaultDaiSampleFormat2,
    .frame_format = kDefaultDaiFrameFormat2,
    .frame_rate = kDefaultDaiFrameRate2,
    .bits_per_slot = kDefaultDaiBitsPerSlot2,
    .bits_per_sample = kDefaultDaiBitsPerSample2,
}};

// RingBufferFormats and format sets
//
const fha::ChannelAttributes FakeComposite::kDefaultChannelAttributes1{{
    .min_frequency = kDefaultChannelAttributes1MinFrequency,
    .max_frequency = kDefaultChannelAttributes1MaxFrequency,
}};
const fha::ChannelAttributes FakeComposite::kDefaultChannelAttributes2{{
    .min_frequency = kDefaultChannelAttributes2MinFrequency,
    // no .max_frequency is specified
}};
const fha::ChannelAttributes FakeComposite::kDefaultChannelAttributes3{{
    // no .min_frequency is specified
    .max_frequency = kDefaultChannelAttributes3MaxFrequency,
}};
const std::vector<fha::ChannelAttributes> FakeComposite::kDefaultChannelAttributesSet1{
    kDefaultChannelAttributes1,
};
const std::vector<fha::ChannelAttributes> FakeComposite::kDefaultChannelAttributesSet2{
    kDefaultChannelAttributes2,
};
const fha::ChannelSet FakeComposite::kDefaultChannelSet1{{
    .attributes = kDefaultChannelAttributesSet1,
}};
const fha::ChannelSet FakeComposite::kDefaultChannelSet2{{
    .attributes = kDefaultChannelAttributesSet2,
}};
const std::vector<fha::ChannelSet> FakeComposite::kDefaultChannelSets1{
    kDefaultChannelSet1,
};
const std::vector<fha::ChannelSet> FakeComposite::kDefaultChannelSets2{kDefaultChannelSet2};

const std::vector<fha::SampleFormat> FakeComposite::kDefaultRbSampleFormats1{
    kDefaultRbSampleFormat1};
const std::vector<fha::SampleFormat> FakeComposite::kDefaultRbSampleFormats2{
    kDefaultRbSampleFormat2};
const std::vector<uint8_t> FakeComposite::kDefaultRbBytesPerSampleSet1{kDefaultRbBytesPerSample1};
const std::vector<uint8_t> FakeComposite::kDefaultRbBytesPerSampleSet2{kDefaultRbBytesPerSample2};
const std::vector<uint8_t> FakeComposite::kDefaultRbValidBitsPerSampleSet1{
    kDefaultRbValidBitsPerSample1};
const std::vector<uint8_t> FakeComposite::kDefaultRbValidBitsPerSampleSet2{
    kDefaultRbValidBitsPerSample2};
const std::vector<uint32_t> FakeComposite::kDefaultRbFrameRates1{kDefaultRbFrameRate1};
const std::vector<uint32_t> FakeComposite::kDefaultRbFrameRates2{kDefaultRbFrameRate2};
const std::vector<uint32_t> FakeComposite::kDefaultPsFrameRates1{kDefaultPsFrameRate1};
const std::vector<uint32_t> FakeComposite::kDefaultPsFrameRates2{kDefaultPsFrameRate2};

const std::vector<fuchsia_hardware_audio::EncodingType> FakeComposite::kDefaultPsEncodingTypes1{
    kDefaultPsEncodingType1};
const std::vector<fuchsia_hardware_audio::EncodingType> FakeComposite::kDefaultPsEncodingTypes2{
    kDefaultPsEncodingType2};

const fha::PcmSupportedFormats FakeComposite::kDefaultPcmRingBufferFormatSet1{{
    .channel_sets = kDefaultChannelSets1,
    .sample_formats = kDefaultRbSampleFormats1,
    .bytes_per_sample = kDefaultRbBytesPerSampleSet1,
    .valid_bits_per_sample = kDefaultRbValidBitsPerSampleSet1,
    .frame_rates = kDefaultRbFrameRates1,
}};
const fha::PcmSupportedFormats FakeComposite::kDefaultPcmRingBufferFormatSet2{{
    .channel_sets = kDefaultChannelSets2,
    .sample_formats = kDefaultRbSampleFormats2,
    .bytes_per_sample = kDefaultRbBytesPerSampleSet2,
    .valid_bits_per_sample = kDefaultRbValidBitsPerSampleSet2,
    .frame_rates = kDefaultRbFrameRates2,
}};
const fha::SupportedEncodings FakeComposite::kDefaultEncodingSet1{{
    .decoded_channel_sets = kDefaultChannelSets1,
    .decoded_frame_rates = kDefaultPsFrameRates1,
    .encoding_types = kDefaultPsEncodingTypes1,
}};
const fha::SupportedEncodings FakeComposite::kDefaultEncodingSet2{{
    .decoded_channel_sets = kDefaultChannelSets2,
    .decoded_frame_rates = kDefaultPsFrameRates2,
    .encoding_types = kDefaultPsEncodingTypes2,
}};

const fha::SupportedFormats2 FakeComposite::kDefaultRbFormatSet1 =
    fha::SupportedFormats2::WithPcmSupportedFormats(kDefaultPcmRingBufferFormatSet1);
const fha::SupportedFormats2 FakeComposite::kDefaultRbFormatSet2 =
    fha::SupportedFormats2::WithPcmSupportedFormats(kDefaultPcmRingBufferFormatSet2);
const fha::SupportedFormats2 FakeComposite::kDefaultPsFormatSet1 =
    fha::SupportedFormats2::WithPcmSupportedFormats(kDefaultPcmRingBufferFormatSet1);
const fha::SupportedFormats2 FakeComposite::kDefaultPsFormatSet2 =
    fha::SupportedFormats2::WithSupportedEncodings(kDefaultEncodingSet2);

// RingBuffer and PacketStream format sets that are returned by the driver.
const std::vector<fha::SupportedFormats2> FakeComposite::kDefaultRbFormatSets1{
    kDefaultRbFormatSet1};
const std::vector<fha::SupportedFormats2> FakeComposite::kDefaultRbFormatSets2{
    kDefaultRbFormatSet2};
const std::vector<fha::SupportedFormats2> FakeComposite::kDefaultPsFormatSets1{
    kDefaultPsFormatSet1};
const std::vector<fha::SupportedFormats2> FakeComposite::kDefaultPsFormatSets2{
    kDefaultPsFormatSet2};

// Map of RingBuffer format sets, by element. Used internally by the driver.
const std::unordered_map<ElementId, std::vector<fha::SupportedFormats2>>
    FakeComposite::kDefaultRbFormatsMap = {{
        {kDestRbElementId, kDefaultRbFormatSets1},
        {kSourceRbElementId, kDefaultRbFormatSets2},
    }};
const std::unordered_map<ElementId, std::vector<fha::SupportedFormats2>>
    FakeComposite::kDefaultPsFormatsMap = {{
        {kDestPsElementId, kDefaultPsFormatSets1},
        {kSourcePsElementId, kDefaultPsFormatSets2},
    }};

// signalprocessing elements and topologies
//
// Individual elements
const std::string FakeComposite::kSourceDaiElementDescription =
    "DaiInterconnect source element description";
const std::string FakeComposite::kDestDaiElementDescription =
    "DaiInterconnect destination element description";
const std::string FakeComposite::kSourceRbElementDescription =
    "RingBuffer source element description";
const std::string FakeComposite::kSourcePsElementDescription =
    "PacketStream source element description";
const std::string FakeComposite::kDestRbElementDescription =
    "RingBuffer destination element description";
const std::string FakeComposite::kDestPsElementDescription =
    "PacketStream destination element description";
const std::string FakeComposite::kMuteElementDescription = "Mute element description";

const fhasp::Element FakeComposite::kSourceDaiElement{{
    .id = kSourceDaiElementId,
    .type = fhasp::ElementType::kDaiInterconnect,
    .type_specific = fhasp::TypeSpecificElement::WithDaiInterconnect({{
        .plug_detect_capabilities = fhasp::PlugDetectCapabilities::kCanAsyncNotify,
    }}),
    .description = kSourceDaiElementDescription,
    .can_stop = true,
    .can_bypass = false,
}};
const fhasp::Element FakeComposite::kDestDaiElement{{
    .id = kDestDaiElementId,
    .type = fhasp::ElementType::kDaiInterconnect,
    .type_specific = fhasp::TypeSpecificElement::WithDaiInterconnect({{
        .plug_detect_capabilities = fhasp::PlugDetectCapabilities::kCanAsyncNotify,
    }}),
    .description = kDestDaiElementDescription,
    .can_stop = true,
    .can_bypass = false,
}};
const fhasp::Element FakeComposite::kSourceRbElement{{
    .id = kSourceRbElementId,
    .type = fhasp::ElementType::kRingBuffer,
    .description = kSourceRbElementDescription,
    .can_stop = false,
    .can_bypass = false,
}};
const fhasp::Element FakeComposite::kSourcePsElement{{
    .id = kSourcePsElementId,
    .type = fhasp::ElementType::kPacketStream,
    .description = kSourcePsElementDescription,
    .can_stop = false,
    .can_bypass = false,
}};
const fhasp::Element FakeComposite::kDestRbElement{{
    .id = kDestRbElementId,
    .type = fhasp::ElementType::kRingBuffer,
    .description = kDestRbElementDescription,
    .can_stop = false,
    .can_bypass = false,
}};
const fhasp::Element FakeComposite::kDestPsElement{{
    .id = kDestPsElementId,
    .type = fhasp::ElementType::kPacketStream,
    .description = kDestPsElementDescription,
    .can_stop = false,
    .can_bypass = false,
}};
const fhasp::Element FakeComposite::kMuteElement{{
    .id = kMuteElementId,
    .type = fhasp::ElementType::kMute,
    .description = kMuteElementDescription,
    .can_stop = false,
    .can_bypass = true,
}};

// ElementStates - note that the two Dai elements have vendor_specific_data that can be queried.
const zx::duration FakeComposite::kSourceDaiElementProcessingDelay = zx::nsec(0);
const zx::duration FakeComposite::kDestDaiElementProcessingDelay = zx::nsec(123);
const zx::duration FakeComposite::kSourceRbElementProcessingDelay = zx::nsec(42);
const zx::duration FakeComposite::kSourcePsElementProcessingDelay = zx::nsec(68);
const fhasp::ElementState FakeComposite::kSourceDaiElementInitState{{
    .type_specific = fhasp::TypeSpecificElementState::WithDaiInterconnect({{
        .plug_state = fhasp::PlugState{{
            .plugged = true,
            .plug_state_time = 0,
        }},
        .external_delay = 0,
    }}),
    .vendor_specific_data = std::vector<uint8_t>{1, 2, 3, 4, 5, 6, 7, 8},
    .started = false,
    .bypassed = false,
    .processing_delay = kSourceDaiElementProcessingDelay.get(),
}};
const fhasp::ElementState FakeComposite::kDestDaiElementInitState{{
    .type_specific = fhasp::TypeSpecificElementState::WithDaiInterconnect({{
        .plug_state = fhasp::PlugState{{
            .plugged = true,
            .plug_state_time = 0,
        }},
        .external_delay = 123,
    }}),
    .vendor_specific_data = std::vector<uint8_t>{8, 7, 6, 5, 4, 3, 2, 1, 0},
    .started = false,
    .bypassed = false,
    .processing_delay = kDestDaiElementProcessingDelay.get(),
}};
const fhasp::ElementState FakeComposite::kSourceRbElementInitState{{
    .started = true,
    .bypassed = false,
    .processing_delay = kSourceRbElementProcessingDelay.get(),
}};
const fhasp::ElementState FakeComposite::kSourcePsElementInitState{{
    .started = true,
    .bypassed = false,
    .processing_delay = kSourcePsElementProcessingDelay.get(),
}};
const fhasp::ElementState FakeComposite::kDestRbElementInitState{{
    .started = true,
    .bypassed = false,
}};
const fhasp::ElementState FakeComposite::kDestPsElementInitState{{
    .started = true,
    .bypassed = false,
}};
const fhasp::ElementState FakeComposite::kMuteElementInitState{{
    .started = true,
    .bypassed = true,
}};

// Element set
const std::vector<fhasp::Element> FakeComposite::kElements{{
    kSourceDaiElement,
    kDestDaiElement,
    kSourceRbElement,
    kSourcePsElement,
    kDestRbElement,
    kDestPsElement,
    kMuteElement,
}};

// Topologies and element paths
//
// element paths
const fhasp::EdgePair FakeComposite::kTopologyInputEdgePair{{
    .processing_element_id_from = kSourceDaiElementId,
    .processing_element_id_to = kDestRbElementId,
}};
const fhasp::EdgePair FakeComposite::kTopologyPsCaptureEdgePair{{
    .processing_element_id_from = kSourceDaiElementId,
    .processing_element_id_to = kDestPsElementId,
}};
const fhasp::EdgePair FakeComposite::kTopologyOutputEdgePair{{
    .processing_element_id_from = kSourceRbElementId,
    .processing_element_id_to = kDestDaiElementId,
}};
const fhasp::EdgePair FakeComposite::kTopologyPsOutputEdgePair{{
    .processing_element_id_from = kSourcePsElementId,
    .processing_element_id_to = kDestDaiElementId,
}};
const fhasp::EdgePair FakeComposite::kTopologyRbToMuteEdgePair{{
    .processing_element_id_from = kSourceRbElementId,
    .processing_element_id_to = kMuteElementId,
}};
const fhasp::EdgePair FakeComposite::kTopologyMuteToDaiEdgePair{{
    .processing_element_id_from = kMuteElementId,
    .processing_element_id_to = kDestDaiElementId,
}};

// Individual topologies
const fhasp::Topology FakeComposite::kInputOnlyTopology{{
    .id = kInputOnlyTopologyId,
    .processing_elements_edge_pairs = {{
        kTopologyInputEdgePair,
    }},
}};
const fhasp::Topology FakeComposite::kPacketStreamCaptureTopology{{
    .id = kPacketStreamCaptureTopologyId,
    .processing_elements_edge_pairs = {{
        kTopologyPsCaptureEdgePair,
    }},
}};
const fhasp::Topology FakeComposite::kFullDuplexTopology{{
    .id = kFullDuplexTopologyId,
    .processing_elements_edge_pairs = {{
        kTopologyInputEdgePair,
        kTopologyOutputEdgePair,
    }},
}};
const fhasp::Topology FakeComposite::kOutputOnlyTopology{{
    .id = kOutputOnlyTopologyId,
    .processing_elements_edge_pairs = {{
        kTopologyOutputEdgePair,
    }},
}};
const fhasp::Topology FakeComposite::kPacketStreamOutputTopology{{
    .id = kPacketStreamOutputTopologyId,
    .processing_elements_edge_pairs = {{
        kTopologyPsOutputEdgePair,
    }},
}};
const fhasp::Topology FakeComposite::kOutputWithMuteTopology{{
    .id = kOutputWithMuteTopologyId,
    .processing_elements_edge_pairs = {{
        kTopologyRbToMuteEdgePair,
        kTopologyMuteToDaiEdgePair,
    }},
}};

// Topology set
const std::vector<fhasp::Topology> FakeComposite::kTopologies{{
    kInputOnlyTopology,
    kPacketStreamCaptureTopology,
    kFullDuplexTopology,
    kOutputOnlyTopology,
    kPacketStreamOutputTopology,
    kOutputWithMuteTopology,
}};

}  // namespace media_audio
