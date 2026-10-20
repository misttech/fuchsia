// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/media/cpp/fidl.h>

#include <cstdint>
#include <cstring>
#include <mutex>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "src/media/codec/codecs/sw/sbc/codec_adapter_sbc_decoder.h"
#include "src/media/codec/codecs/sw/sbc/codec_adapter_sbc_encoder.h"
#include "src/media/codec/codecs/test/test_codec_packets.h"
#include "src/media/codec/codecs/test/test_fake_codec_adapter_events.h"

namespace {

constexpr size_t kUnpaddedMsbcFrameBytes = 57;
constexpr size_t kScoPaddedMsbcFrameBytes = 58;
constexpr size_t kMsbcPcmBatchBytes = 240;  // 15 blocks * 8 subbands * 2 bytes/sample
constexpr uint8_t kMsbcSyncword = 0xAD;

fuchsia::media::FormatDetails MakeValidSbcDecoderFormatDetails() {
  SbcCodecInfo codec_info = {
      .sampling_frequency = kSbcSamplingFrequency44100Hz,
      .channel_mode = kSbcChannelModeJointStereo,
      .block_length = 0b0001,     // 16 blocks
      .subbands = 0b01,           // 8 subbands
      .allocation_method = 0b01,  // Loudness
      .min_bitpool_value = 2,
      .max_bitpool_value = 53,
  };
  std::vector<uint8_t> oob_bytes(sizeof(SbcCodecInfo));
  std::memcpy(oob_bytes.data(), &codec_info, sizeof(SbcCodecInfo));

  fuchsia::media::FormatDetails format_details;
  format_details.set_format_details_version_ordinal(0);
  format_details.set_mime_type("audio/sbc");
  format_details.set_oob_bytes(std::move(oob_bytes));
  return format_details;
}

fuchsia::media::FormatDetails MakeValidMsbcDecoderFormatDetails() {
  // Matches bt-hfp CodecId::MSBC::oob_bytes(): 16 kHz, Mono, 16 blocks, 8 subbands, Loudness,
  // bitpool 26.
  SbcCodecInfo codec_info = {
      .sampling_frequency = kSbcSamplingFrequency16000Hz,
      .channel_mode = kSbcChannelModeMono,
      .block_length = 0b0001,     // 16 blocks
      .subbands = 0b01,           // 8 subbands
      .allocation_method = 0b01,  // Loudness
      .min_bitpool_value = 26,
      .max_bitpool_value = 26,
  };
  std::vector<uint8_t> oob_bytes(sizeof(SbcCodecInfo));
  std::memcpy(oob_bytes.data(), &codec_info, sizeof(SbcCodecInfo));

  fuchsia::media::FormatDetails format_details;
  format_details.set_format_details_version_ordinal(0);
  format_details.set_mime_type("audio/msbc");
  format_details.set_oob_bytes(std::move(oob_bytes));
  return format_details;
}

fuchsia::media::FormatDetails MakeValidMsbcEncoderFormatDetails() {
  fuchsia::media::PcmFormat pcm;
  pcm.pcm_mode = fuchsia::media::AudioPcmMode::LINEAR;
  pcm.bits_per_sample = 16;
  pcm.frames_per_second = 16000;
  pcm.channel_map = {fuchsia::media::AudioChannelId::LF};

  fuchsia::media::AudioUncompressedFormat uncompressed;
  uncompressed.set_pcm(std::move(pcm));

  fuchsia::media::AudioFormat audio;
  audio.set_uncompressed(std::move(uncompressed));

  fuchsia::media::DomainFormat domain;
  domain.set_audio(std::move(audio));

  fuchsia::media::EncoderSettings encoder_settings;
  encoder_settings.set_msbc(fuchsia::media::MSbcEncoderSettings{});

  fuchsia::media::FormatDetails format_details;
  format_details.set_format_details_version_ordinal(0);
  format_details.set_domain(std::move(domain));
  format_details.set_encoder_settings(std::move(encoder_settings));
  return format_details;
}

// Encodes one 15-block * 8-subband = 120-sample (240-byte) 16 kHz mono PCM frame into a 57-byte
// mSBC frame (starting with OI_SBC_MSBC_SYNCWORD = 0xAD).
std::vector<uint8_t> EncodeSingleMsbcFrame() {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  CodecAdapterSbcEncoder encoder(lock, &events);

  auto format_details = MakeValidMsbcEncoderFormatDetails();
  encoder.CoreCodecInit(format_details);
  ZX_ASSERT(events.fail_codec_count() == 0u);

  auto output_buffers = Buffers({512});
  auto output_packets = Packets(1);
  encoder.CoreCodecAddBuffer(kOutputPort, output_buffers.ptr(0));
  output_packets.ptr(0)->SetIsNew(false);
  encoder.CoreCodecRecycleOutputPacket(output_packets.ptr(0));

  encoder.CoreCodecStartStream();
  encoder.CoreCodecQueueInputFormatDetails(format_details);
  events.WaitForOutputConstraintsChangeCount(1);
  encoder.CoreCodecMidStreamOutputBufferReConfigFinish();

  auto input_buffers = Buffers({kMsbcPcmBatchBytes});
  std::memset(input_buffers.ptr(0)->base(), 0, kMsbcPcmBatchBytes);
  auto input_packets = Packets(1);
  CodecPacket* input_packet = input_packets.ptr(0);
  input_packet->SetBuffer(input_buffers.ptr(0));
  input_packet->SetStartOffset(0);
  input_packet->SetValidLengthBytes(kMsbcPcmBatchBytes);

  encoder.CoreCodecQueueInputPacket(input_packet);
  encoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(1);
  ZX_ASSERT(events.fail_codec_count() == 0u);
  ZX_ASSERT(events.output_packet_count() == 1u);

  CodecPacket* out_packet = output_packets.ptr(0);
  ZX_ASSERT(out_packet->valid_length_bytes() == kUnpaddedMsbcFrameBytes);
  const uint8_t* begin = out_packet->buffer()->base() + out_packet->start_offset();
  std::vector<uint8_t> msbc_frame(begin, begin + out_packet->valid_length_bytes());
  encoder.CoreCodecStopStream();
  return msbc_frame;
}

fuchsia::media::FormatDetails MakeValidSbcEncoderFormatDetails() {
  fuchsia::media::PcmFormat pcm;
  pcm.pcm_mode = fuchsia::media::AudioPcmMode::LINEAR;
  pcm.bits_per_sample = 16;
  pcm.frames_per_second = 44100;
  pcm.channel_map = {fuchsia::media::AudioChannelId::LF, fuchsia::media::AudioChannelId::RF};

  fuchsia::media::AudioUncompressedFormat uncompressed;
  uncompressed.set_pcm(std::move(pcm));

  fuchsia::media::AudioFormat audio;
  audio.set_uncompressed(std::move(uncompressed));

  fuchsia::media::DomainFormat domain;
  domain.set_audio(std::move(audio));

  fuchsia::media::SbcEncoderSettings sbc_settings;
  sbc_settings.sub_bands = static_cast<fuchsia::media::SbcSubBands>(SUB_BANDS_8);
  sbc_settings.block_count = fuchsia::media::SbcBlockCount::BLOCK_COUNT_16;
  sbc_settings.allocation = fuchsia::media::SbcAllocation::ALLOC_LOUDNESS;
  sbc_settings.channel_mode = fuchsia::media::SbcChannelMode::JOINT_STEREO;
  sbc_settings.bit_pool = 53;

  fuchsia::media::EncoderSettings encoder_settings;
  encoder_settings.set_sbc(sbc_settings);

  fuchsia::media::FormatDetails format_details;
  format_details.set_format_details_version_ordinal(0);
  format_details.set_domain(std::move(domain));
  format_details.set_encoder_settings(std::move(encoder_settings));
  return format_details;
}

TEST(CodecAdapterSbcDecoderTest, EmptyStreamEndOfStreamAndPostStopOutputConstraints) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  CodecAdapterSbcDecoder decoder(lock, &events);

  auto format_details = MakeValidSbcDecoderFormatDetails();
  decoder.CoreCodecInit(format_details);
  ASSERT_EQ(events.fail_codec_count(), 0u);

  // Stream 1: Queue EndOfStream immediately without any FormatDetails or Packet.
  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(1);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 1u);
  decoder.CoreCodecStopStream();

  // Stream 2: Queue FormatDetails so CreateContext caches min_output_buffer_size_, then stop
  // stream (which resets context_ in CleanUpAfterStream()).
  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputFormatDetails(format_details);
  events.WaitForOutputConstraintsChangeCount(1);
  decoder.CoreCodecMidStreamOutputBufferReConfigFinish();
  EXPECT_EQ(events.fail_codec_count(), 0u);
  decoder.CoreCodecStopStream();

  // Between streams: CoreCodecGetBufferCollectionConstraints2(kOutputPort) must succeed even
  // though context_ was reset by CleanUpAfterStream().
  fuchsia::media::StreamBufferConstraints stream_constraints;
  fuchsia::media::StreamBufferPartialSettings partial_settings;
  auto constraints = decoder.CoreCodecGetBufferCollectionConstraints2(
      kOutputPort, stream_constraints, partial_settings);
  ASSERT_TRUE(constraints.buffer_memory_constraints().has_value());
  // (16 bits / 8) * SBC_MAX_SAMPLES_PER_FRAME (128) * SBC_MAX_CHANNELS (2) = 512 bytes.
  EXPECT_EQ(constraints.buffer_memory_constraints()->min_size_bytes(), 512u);

  // Stream 3: Queue EndOfStream without FormatDetails or Packet after a configured stream.
  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(2);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 2u);
  EXPECT_EQ(events.output_packet_count(), 0u);
  decoder.CoreCodecStopStream();
}

TEST(CodecAdapterSbcDecoderTest, MsbcEmptyStreamEndOfStreamDecodeAndPostStopOutputConstraints) {
  std::vector<uint8_t> msbc_frame = EncodeSingleMsbcFrame();
  ASSERT_EQ(msbc_frame.size(), kUnpaddedMsbcFrameBytes);
  ASSERT_EQ(msbc_frame[0], kMsbcSyncword);

  std::mutex lock;
  FakeCodecAdapterEvents events;
  CodecAdapterSbcDecoder decoder(lock, &events);

  auto format_details = MakeValidMsbcDecoderFormatDetails();
  decoder.CoreCodecInit(format_details);
  ASSERT_EQ(events.fail_codec_count(), 0u);

  // Stream 1: Queue EndOfStream immediately without any FormatDetails or Packet.
  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(1);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 1u);
  decoder.CoreCodecStopStream();

  // Stream 2: Queue "audio/msbc" FormatDetails, decode one 58-byte SCO-padded mSBC frame (57-byte
  // mSBC frame + 1 zero padding byte), and verify OI_CODEC_SBC_DecoderConfigureMSbc() ran (plain
  // SBC mode would reject syncword 0xAD with OI_CODEC_SBC_NO_SYNCWORD).
  auto output_buffers = Buffers({512});
  auto output_packets = Packets(1);
  decoder.CoreCodecAddBuffer(kOutputPort, output_buffers.ptr(0));
  output_packets.ptr(0)->SetIsNew(false);
  decoder.CoreCodecRecycleOutputPacket(output_packets.ptr(0));

  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputFormatDetails(format_details);
  events.WaitForOutputConstraintsChangeCount(1);
  decoder.CoreCodecMidStreamOutputBufferReConfigFinish();
  EXPECT_EQ(events.fail_codec_count(), 0u);

  auto input_buffers = Buffers({kScoPaddedMsbcFrameBytes});
  std::memcpy(input_buffers.ptr(0)->base(), msbc_frame.data(), msbc_frame.size());
  input_buffers.ptr(0)->base()[msbc_frame.size()] = 0;  // 1-byte SCO padding

  auto input_packets = Packets(1);
  CodecPacket* input_packet = input_packets.ptr(0);
  input_packet->SetBuffer(input_buffers.ptr(0));
  input_packet->SetStartOffset(0);
  input_packet->SetValidLengthBytes(kScoPaddedMsbcFrameBytes);

  decoder.CoreCodecQueueInputPacket(input_packet);
  decoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(2);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_packet_count(), 1u);
  // 15 blocks * 8 subbands * 1 channel * 2 bytes/sample = 240 bytes of PCM output.
  EXPECT_EQ(output_packets.ptr(0)->valid_length_bytes(), kMsbcPcmBatchBytes);
  decoder.CoreCodecRecycleOutputPacket(output_packets.ptr(0));
  decoder.CoreCodecStopStream();

  // Between streams: CoreCodecGetBufferCollectionConstraints2(kOutputPort) must succeed even
  // though context_ was reset by CleanUpAfterStream().
  fuchsia::media::StreamBufferConstraints stream_constraints;
  fuchsia::media::StreamBufferPartialSettings partial_settings;
  auto constraints = decoder.CoreCodecGetBufferCollectionConstraints2(
      kOutputPort, stream_constraints, partial_settings);
  ASSERT_TRUE(constraints.buffer_memory_constraints().has_value());
  // (16 bits / 8) * SBC_MAX_SAMPLES_PER_FRAME (128) * SBC_MAX_CHANNELS (2) = 512 bytes.
  EXPECT_EQ(constraints.buffer_memory_constraints()->min_size_bytes(), 512u);

  // Stream 3: Queue EndOfStream without FormatDetails or Packet after a configured stream.
  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(3);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 3u);
  EXPECT_EQ(events.output_packet_count(), 1u);
  decoder.CoreCodecStopStream();
}

TEST(CodecAdapterSbcDecoderTest, MsbcDecodeKeepsBytesLeftInSyncAcrossPaddedAndUnpaddedFrames) {
  std::vector<uint8_t> msbc_frame = EncodeSingleMsbcFrame();
  ASSERT_EQ(msbc_frame.size(), kUnpaddedMsbcFrameBytes);
  ASSERT_EQ(msbc_frame[0], kMsbcSyncword);

  std::mutex lock;
  FakeCodecAdapterEvents events;
  CodecAdapterSbcDecoder decoder(lock, &events);

  auto format_details = MakeValidMsbcDecoderFormatDetails();
  decoder.CoreCodecInit(format_details);
  ASSERT_EQ(events.fail_codec_count(), 0u);

  auto output_buffers = Buffers({512, 512, 512});
  auto output_packets = Packets(3);
  for (size_t i = 0; i < 3; ++i) {
    decoder.CoreCodecAddBuffer(kOutputPort, output_buffers.ptr(i));
    output_packets.ptr(i)->SetIsNew(false);
    decoder.CoreCodecRecycleOutputPacket(output_packets.ptr(i));
  }

  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputFormatDetails(format_details);
  events.WaitForOutputConstraintsChangeCount(1);
  decoder.CoreCodecMidStreamOutputBufferReConfigFinish();
  EXPECT_EQ(events.fail_codec_count(), 0u);

  auto input_buffers = Buffers({
      2 * kUnpaddedMsbcFrameBytes,
      kUnpaddedMsbcFrameBytes,
      2 * kScoPaddedMsbcFrameBytes,
  });

  // Input packet 0: two back-to-back unpadded 57-byte mSBC frames (114 bytes). Without
  // recomputing `bytes_left`, `OI_CODEC_SBC_DecodeFrame()` subtracts 58 after the first frame,
  // leaving `bytes_left == 56 < 57` and dropping the second frame cleanly without an OOB read.
  for (size_t i = 0; i < 2; ++i) {
    std::memcpy(input_buffers.ptr(0)->base() + (i * kUnpaddedMsbcFrameBytes), msbc_frame.data(),
                msbc_frame.size());
  }

  // Input packet 1: single unpadded 57-byte mSBC frame. Without recomputing `bytes_left`,
  // `bytes_left` underflows from 0 to `UINT32_MAX`.
  std::memcpy(input_buffers.ptr(1)->base(), msbc_frame.data(), msbc_frame.size());

  // Input packet 2: two 58-byte SCO-padded mSBC frames in one packet (116 bytes).
  for (size_t i = 0; i < 2; ++i) {
    uint8_t* dst = input_buffers.ptr(2)->base() + (i * kScoPaddedMsbcFrameBytes);
    std::memcpy(dst, msbc_frame.data(), msbc_frame.size());
    // 1-byte SCO padding.
    dst[msbc_frame.size()] = 0;
  }

  auto input_packets = Packets(3);
  CodecPacket* input_packet_0 = input_packets.ptr(0);
  input_packet_0->SetBuffer(input_buffers.ptr(0));
  input_packet_0->SetStartOffset(0);
  input_packet_0->SetValidLengthBytes(2 * kUnpaddedMsbcFrameBytes);

  CodecPacket* input_packet_1 = input_packets.ptr(1);
  input_packet_1->SetBuffer(input_buffers.ptr(1));
  input_packet_1->SetStartOffset(0);
  input_packet_1->SetValidLengthBytes(kUnpaddedMsbcFrameBytes);

  CodecPacket* input_packet_2 = input_packets.ptr(2);
  input_packet_2->SetBuffer(input_buffers.ptr(2));
  input_packet_2->SetStartOffset(0);
  input_packet_2->SetValidLengthBytes(2 * kScoPaddedMsbcFrameBytes);

  decoder.CoreCodecQueueInputPacket(input_packet_0);
  decoder.CoreCodecQueueInputPacket(input_packet_1);
  decoder.CoreCodecQueueInputPacket(input_packet_2);
  decoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(1);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  ASSERT_EQ(events.output_packet_count(), 3u);
  // Input packet 0 (2 unpadded frames): 2 * 240 = 480 bytes.
  EXPECT_EQ(output_packets.ptr(0)->valid_length_bytes(), 2 * kMsbcPcmBatchBytes);
  // Input packet 1 (1 unpadded frame): 15 blocks * 8 subbands * 1 channel * 2 bytes/sample = 240.
  EXPECT_EQ(output_packets.ptr(1)->valid_length_bytes(), kMsbcPcmBatchBytes);
  // Input packet 2 (2 SCO-padded frames): 2 * 240 = 480 bytes.
  EXPECT_EQ(output_packets.ptr(2)->valid_length_bytes(), 2 * kMsbcPcmBatchBytes);

  for (size_t i = 0; i < 3; ++i) {
    decoder.CoreCodecRecycleOutputPacket(output_packets.ptr(i));
  }
  decoder.CoreCodecStopStream();
}

TEST(CodecAdapterSbcEncoderTest, EmptyStreamEndOfStreamAndPostStopOutputConstraints) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  CodecAdapterSbcEncoder encoder(lock, &events);

  auto format_details = MakeValidSbcEncoderFormatDetails();
  encoder.CoreCodecInit(format_details);
  ASSERT_EQ(events.fail_codec_count(), 0u);

  // Stream 1: Queue EndOfStream immediately without any FormatDetails or Packet.
  encoder.CoreCodecStartStream();
  encoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(1);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 1u);
  encoder.CoreCodecStopStream();

  // Stream 2: Queue FormatDetails and a partial input packet (16 bytes < 512-byte PCM batch size)
  // so chunk_input_stream_ buffers partial bytes in scratch_block_, then stop stream without EOS.
  encoder.CoreCodecStartStream();
  encoder.CoreCodecQueueInputFormatDetails(format_details);
  events.WaitForOutputConstraintsChangeCount(1);
  encoder.CoreCodecMidStreamOutputBufferReConfigFinish();

  auto input_buffers = Buffers({4096});
  auto input_packets = Packets(1);
  CodecPacket* input_packet = input_packets.ptr(0);
  input_packet->SetBuffer(input_buffers.ptr(0));
  input_packet->SetStartOffset(0);
  input_packet->SetValidLengthBytes(16);  // Partial PCM batch (< 512 bytes)
  encoder.CoreCodecQueueInputPacket(input_packet);
  events.WaitForInputPacketDoneCount(1);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  encoder.CoreCodecStopStream();

  // Between streams: CoreCodecGetBufferCollectionConstraints2(kOutputPort) must succeed using
  // cached min_output_buffer_size_.
  fuchsia::media::StreamBufferConstraints stream_constraints;
  fuchsia::media::StreamBufferPartialSettings partial_settings;
  auto constraints = encoder.CoreCodecGetBufferCollectionConstraints2(
      kOutputPort, stream_constraints, partial_settings);
  ASSERT_TRUE(constraints.buffer_memory_constraints().has_value());
  // For 8 subbands, 16 blocks, JOINT_STEREO, bitpool 53: 4 + (8 * 2 / 2) + ceil((8 + 16 * 53) / 8)
  // = 12 + 107 = 119 bytes.
  EXPECT_EQ(constraints.buffer_memory_constraints()->min_size_bytes(), 119u);

  // Stream 3: Queue EndOfStream without FormatDetails or Packet; must not flush stale partial
  // bytes from Stream 2 or dereference null context_.
  encoder.CoreCodecStartStream();
  encoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(2);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 2u);
  EXPECT_EQ(events.output_packet_count(), 0u);
  encoder.CoreCodecStopStream();
}

}  // namespace
