// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "codec_adapter_sbc_decoder.h"

#include <lib/media/codec_impl/codec_buffer.h>
#include <lib/trace/event.h>

#include <iomanip>
#include <limits>

#include "fuchsia/media/cpp/fidl.h"
#include "lib/media/codec_impl/codec_port.h"
#include "oi_codec_sbc.h"

namespace {

constexpr char kSbcMimeType[] = "audio/sbc";
constexpr char kMsbcMimeType[] = "audio/msbc";
constexpr char kPcmMimeType[] = "audio/pcm";
constexpr uint8_t kPcmBitsPerSample = 16;
constexpr size_t kMaxInputFrames = 64;

bool FormatDetailsAreEqualForOutputBuffers(const fuchsia::media::FormatDetails& lhs,
                                           const fuchsia::media::FormatDetails& rhs) {
  if (lhs.has_mime_type() != rhs.has_mime_type()) {
    return false;
  }
  if (lhs.has_mime_type() && lhs.mime_type() != rhs.mime_type()) {
    return false;
  }
  if (lhs.has_oob_bytes() != rhs.has_oob_bytes()) {
    return false;
  }
  if (lhs.has_oob_bytes() && lhs.oob_bytes() != rhs.oob_bytes()) {
    return false;
  }
  return true;
}

}  // namespace

CodecAdapterSbcDecoder::CodecAdapterSbcDecoder(std::mutex& lock,
                                               CodecAdapterEvents* codec_adapter_events)
    : CodecAdapterSW(lock, codec_adapter_events) {}

CodecAdapterSbcDecoder::~CodecAdapterSbcDecoder() = default;

void CodecAdapterSbcDecoder::ProcessInputLoop() {
  std::optional<CodecInputItem> maybe_input_item;
  while ((maybe_input_item = input_queue_.WaitForElement())) {
    CodecInputItem input_item = std::move(maybe_input_item.value());

    auto return_packet = fit::defer([this, &input_item] {
      if (input_item.is_packet()) {
        events_->onCoreCodecInputPacketDone(input_item.packet());
      }
    });

    bool is_active = [this, &input_item]() FXL_NO_THREAD_SAFETY_ANALYSIS {
      std::unique_lock<std::mutex> lock(lock_);
      while (output_reconfig_pending_ && !input_item.is_format_details()) {
        reconfig_cond_.wait(lock);
      }
      return stream_active_;
    }();
    if (!is_active) {
      return;
    }
    if (input_item.is_format_details()) {
      if (context_) {
        events_->onCoreCodecFailCodec("Midstream input format change is not supported.");
        return;
      }

      bool format_changed = true;
      if (last_format_details_) {
        format_changed = !FormatDetailsAreEqualForOutputBuffers(*last_format_details_,
                                                                input_item.format_details());
        // TODO(b/534928661): Ideally we wouldn't need to re-allocate output buffers if they're
        // already suitable, but currently sbc_decode_stream_switching_without_close requires
        // format_changed = true here.
        format_changed = true;
      }
      last_format_details_ = fidl::Clone(input_item.format_details());

      if (CreateContext(*last_format_details_) != kOk) {
        // Creation failed; a failure was reported through `events_`.
        return;
      }

      if (format_changed) {
        {
          std::lock_guard<std::mutex> lock(lock_);
          output_reconfig_pending_ = true;
        }
        events_->onCoreCodecMidStreamOutputConstraintsChange(
            /*output_re_config_required=*/true);
      }
    } else if (input_item.is_end_of_stream()) {
      events_->onCoreCodecOutputEndOfStream(/*error_detected_before=*/false);
    } else if (input_item.is_packet()) {
      ZX_DEBUG_ASSERT(context_);
      if (DecodeInput(input_item.packet()) == kShouldTerminate) {
        return;
      }
    }
  }
}

void CodecAdapterSbcDecoder::CleanUpAfterStream() {
  // If we have an output buffer pending but not sent, return it to the pool. CodecAdapterSW
  // expects all buffers returned after stream is stopped.
  if (output_buffer_) {
    auto base = output_buffer_->base();
    output_buffer_pool_.FreeBuffer(base);
    output_buffer_ = nullptr;
  }
  if (output_packet_) {
    free_output_packets_.Push(output_packet_);
    output_packet_ = nullptr;
  }
  output_offset_ = 0;
  {
    std::lock_guard<std::mutex> lock(lock_);
    context_ = std::nullopt;
  }
}

std::pair<fuchsia::media::FormatDetails, size_t> CodecAdapterSbcDecoder::OutputFormatDetails() {
  std::lock_guard<std::mutex> lock(lock_);
  FX_DCHECK(context_);

  fuchsia::media::AudioUncompressedFormat uncompressed;
  uncompressed.set_pcm(context_->output_format);

  fuchsia::media::AudioFormat audio_format;
  audio_format.set_uncompressed(std::move(uncompressed));

  fuchsia::media::FormatDetails format_details;
  format_details.set_mime_type(kPcmMimeType);
  format_details.mutable_domain()->set_audio(std::move(audio_format));

  return {std::move(format_details), context_->max_pcm_chunk_size()};
}

fuchsia_sysmem2::BufferCollectionConstraints
CodecAdapterSbcDecoder::CoreCodecGetBufferCollectionConstraints2(
    CodecPort port, const fuchsia::media::StreamBufferConstraints& stream_buffer_constraints,
    const fuchsia::media::StreamBufferPartialSettings& partial_settings) {
  std::lock_guard<std::mutex> lock(lock_);

  fuchsia_sysmem2::BufferCollectionConstraints result;

  // The CodecImpl won't hand us the sysmem token, so we shouldn't expect to
  // have the token here.
  ZX_DEBUG_ASSERT(!partial_settings.has_sysmem_token());

  // TODO(https://fxbug.dev/42084949): plumb/permit range of buffer count from further down,
  // instead of single number frame_count, and set this to the actual
  // stream-required # of reference frames + # that can concurrently decode.
  // Packets and buffers are not the same thing, and we should permit the # of
  // packets to be >= the # of buffers.  We shouldn't be
  // allocating buffers on behalf of the client here, but until we plumb the
  // range of frame_count and are more flexible on # of allocated buffers, we
  // have to make sure there are at least as many buffers as packets.  We
  // categorize the buffers as for camping and for slack.  This should change to
  // be just the buffers needed for camping and maybe 1 for shared slack.  If
  // the client wants more buffers the client can demand buffers in its own
  // fuchsia::sysmem::BufferCollection::SetConstraints().
  if (port == kOutputPort) {
    result.min_buffer_count_for_camping() = kMinOutputBufferCountForCamping;
  } else {
    result.min_buffer_count_for_camping() = kMinInputBufferCountForCamping;
  }
  ZX_DEBUG_ASSERT(!result.min_buffer_count_for_dedicated_slack().has_value());
  ZX_DEBUG_ASSERT(!result.min_buffer_count_for_shared_slack().has_value());

  uint32_t per_packet_buffer_bytes_min;
  uint32_t per_packet_buffer_bytes_max;
  if (port == kInputPort) {
    per_packet_buffer_bytes_min = kMaxInputFrames * SBC_MAX_FRAME_LEN;
    per_packet_buffer_bytes_max = kMaxInputFrames * SBC_MAX_FRAME_LEN;
  } else {
    ZX_ASSERT(min_output_buffer_size_.has_value());

    ZX_DEBUG_ASSERT(port == kOutputPort);
    per_packet_buffer_bytes_min = *min_output_buffer_size_;
    // At least for now, don't cap the per-packet buffer size for output.
    per_packet_buffer_bytes_max = 0xFFFFFFFF;
  }

  auto& bmc = result.buffer_memory_constraints().emplace();
  bmc.min_size_bytes() = per_packet_buffer_bytes_min;
  bmc.max_size_bytes() = per_packet_buffer_bytes_max;

  // These are all false because SW encode.
  bmc.physically_contiguous_required() = false;
  bmc.secure_required() = false;

  ZX_DEBUG_ASSERT(!result.image_format_constraints().has_value());

  // We don't have to fill out usage - CodecImpl takes care of that.
  ZX_DEBUG_ASSERT(!result.usage().has_value());

  return result;
}

fuchsia::media::PcmFormat CodecAdapterSbcDecoder::DecodeCodecInfo(
    const std::vector<uint8_t>& oob_bytes) {
  fuchsia::media::PcmFormat out;
  SbcCodecInfo codec_info;

  FX_DCHECK(oob_bytes.size() == sizeof(SbcCodecInfo));

  // SBC codec info assumed to be msbf order
  memcpy(&codec_info, oob_bytes.data(), sizeof(SbcCodecInfo));

  out.bits_per_sample = kPcmBitsPerSample;

  switch (codec_info.channel_mode) {
    case kSbcChannelModeMono:
      out.channel_map = {fuchsia::media::AudioChannelId::LF};
      break;
    default:
      out.channel_map = {fuchsia::media::AudioChannelId::LF, fuchsia::media::AudioChannelId::RF};
      break;
  }

  switch (codec_info.sampling_frequency) {
    case kSbcSamplingFrequency16000Hz:
      out.frames_per_second = 16000;
      break;
    case kSbcSamplingFrequency32000Hz:
      out.frames_per_second = 32000;
      break;
    case kSbcSamplingFrequency44100Hz:
      out.frames_per_second = 44100;
      break;
    case kSbcSamplingFrequency48000Hz:
      out.frames_per_second = 48000;
      break;
    default:
      FX_LOGS(WARNING) << "invalid frequency";
      break;
  }

  return out;
}

CodecAdapterSbcDecoder::InputLoopStatus CodecAdapterSbcDecoder::CreateContext(
    const fuchsia::media::FormatDetails& format_details) {
  if (!format_details.has_mime_type() ||
      (format_details.mime_type() != kSbcMimeType && format_details.mime_type() != kMsbcMimeType)) {
    events_->onCoreCodecFailCodec("SBC Decoder received mime type that was not sbc or msbc audio.");
    return kShouldTerminate;
  }
  if (!format_details.has_oob_bytes() ||
      format_details.oob_bytes().size() != sizeof(SbcCodecInfo)) {
    events_->onCoreCodecFailCodec(
        "SBC Decoder received oob info input that was not for sbc audio.");
    return kShouldTerminate;
  }

  auto output_pcm_format = DecodeCodecInfo(format_details.oob_bytes());

  uint8_t channels = static_cast<uint8_t>(output_pcm_format.channel_map.size());
  const char* fail_message = nullptr;
  {
    std::lock_guard<std::mutex> lock(lock_);
    // We create Context in place without moving or copying because it has internal pointers to
    // other parts of itself.
    context_.emplace(std::move(output_pcm_format));
    OI_STATUS status = OI_CODEC_SBC_DecoderReset(&context_->context, context_->context_data,
                                                 sizeof(context_->context_data), channels,
                                                 /*pcmStride=*/channels, /*enhanced=*/false);
    if (!OI_SUCCESS(status)) {
      context_ = std::nullopt;
      fail_message = "Failed to reset SBC decoder";
    } else if (format_details.mime_type() == kMsbcMimeType) {
      status = OI_CODEC_SBC_DecoderConfigureMSbc(&context_->context);
      if (!OI_SUCCESS(status)) {
        context_ = std::nullopt;
        fail_message = "Failed to set mSBC on decoder";
      }
    }
    if (!fail_message) {
      min_output_buffer_size_ = context_->max_pcm_chunk_size();
    }
  }
  if (fail_message) {
    events_->onCoreCodecFailCodec(fail_message);
    return kShouldTerminate;
  }
  return kOk;
}

CodecAdapterSbcDecoder::InputLoopStatus CodecAdapterSbcDecoder::DecodeInput(
    const CodecPacket* input_packet) {
  FX_DCHECK(context_);
  FX_DCHECK(input_packet);

  uint32_t bytes_left = input_packet->valid_length_bytes();
  uint8_t* input_data = input_packet->buffer()->base() + input_packet->start_offset();

  while (bytes_left > 0) {
    uint8_t* output = CurrentOutputBlock();

    if (output == nullptr) {
      // The stream is ending.
      return kShouldTerminate;
    }

    FX_DCHECK(output_buffer_);
    FX_DCHECK(output_packet_);

    if (output_buffer_->size() - output_offset_ > std::numeric_limits<uint32_t>::max()) {
      FX_LOGS(WARNING) << "Could not represent output_bytes as uint32_t";
      break;
    }

    uint32_t output_bytes = static_cast<uint32_t>(output_buffer_->size() - output_offset_);

    OI_STATUS status =
        OI_CODEC_SBC_DecodeFrame(&context_->context, const_cast<const OI_BYTE**>(&input_data),
                                 &bytes_left, reinterpret_cast<int16_t*>(output), &output_bytes);
    if (!OI_SUCCESS(status)) {
      FX_LOGS(WARNING) << "decode failure " << status << " bytes left " << bytes_left;
      break;
    }

    // Must be called for each frame to send when output_buffer is low
    // output_bytes is the same on every call for a given stream (set from DecodeFrame)
    QueueAndSend(output_bytes);
  }

  SendQueuedOutput();

  return kOk;
}

uint8_t* CodecAdapterSbcDecoder::CurrentOutputBlock() {
  if (output_packet_ == nullptr) {
    std::optional<CodecPacket*> maybe_output_packet = free_output_packets_.WaitForElement();
    if (!maybe_output_packet) {
      // The stream is ending.
      return nullptr;
    }
    FX_DCHECK(*maybe_output_packet != nullptr);
    output_packet_ = *maybe_output_packet;
  }

  if (!output_buffer_) {
    output_buffer_ = output_buffer_pool_.AllocateBuffer();
    output_offset_ = 0;
  }

  if (!output_buffer_) {
    return nullptr;
  }

  FX_DCHECK(output_offset_ < output_buffer_->size());

  // Caller must set `output_buffer` to `nullptr` when space is insufficient.
  uint8_t* output = output_buffer_->base() + output_offset_;
  return output;
}

void CodecAdapterSbcDecoder::SendQueuedOutput() {
  if (!output_buffer_ || !output_packet_ || !output_offset_) {
    return;
  }

  TRACE_INSTANT("codec_runner", "Media:PacketSent", TRACE_SCOPE_THREAD);

  if (output_offset_ > std::numeric_limits<uint32_t>::max()) {
    events_->onCoreCodecFailCodec("Could not represent output_offset_ as uint32_t");
    return;
  }

  output_packet_->SetBuffer(output_buffer_);
  output_packet_->SetValidLengthBytes(static_cast<uint32_t>(output_offset_));
  output_packet_->SetStartOffset(0);

  {
    fit::closure free_buffer = [this, base = output_packet_->buffer()->base()] {
      output_buffer_pool_.FreeBuffer(base);
    };
    std::lock_guard<std::mutex> lock(lock_);
    in_use_by_client_[output_packet_] = fit::defer(std::move(free_buffer));
  }

  events_->onCoreCodecOutputPacket(output_packet_,
                                   /*error_detected_before=*/false,
                                   /*error_detected_during=*/false);
  output_packet_ = nullptr;
  output_buffer_ = nullptr;
  output_offset_ = 0;
}

void CodecAdapterSbcDecoder::QueueAndSend(size_t bytes_written) {
  FX_DCHECK(output_offset_ + bytes_written <= output_buffer_->size());

  output_offset_ += bytes_written;

  // Presume that the output buffer will remain the same size for the next frame,
  // and that each frame decodes to the same number of output bytes, both of which are true for SBC.
  // Send output packet if the next decoded frame will not fit in this buffer
  if ((output_buffer_->size() - output_offset_) < bytes_written) {
    SendQueuedOutput();
  }
}

void CodecAdapterSbcDecoder::CoreCodecSetBufferCollectionInfo(
    CodecPort port, const fuchsia_sysmem2::BufferCollectionInfo& buffer_collection_info) {
  if (port == kInputPort) {
    ZX_DEBUG_ASSERT(buffer_collection_info.buffers()->size() >= kMinInputBufferCountForCamping);
  } else {
    ZX_DEBUG_ASSERT(buffer_collection_info.buffers()->size() >= kMinOutputBufferCountForCamping);
  }
}
