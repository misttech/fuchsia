// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_CODEC_CODECS_SW_CODEC_ADAPTER_SW_IMPL_H_
#define SRC_MEDIA_CODEC_CODECS_SW_CODEC_ADAPTER_SW_IMPL_H_

#include <lib/fit/defer.h>
#include <lib/media/codec_impl/codec_adapter.h>
#include <lib/media/codec_impl/codec_port.h>
#include <lib/media/codec_impl/log.h>

#include <safemath/safe_math.h>

#include "chunk_input_stream.h"
#include "codec_adapter_sw.h"

template <typename CodecParams>
class CodecAdapterSWImpl : public CodecAdapterSW<fit::deferred_action<fit::closure>> {
 public:
  enum InputLoopStatus {
    kOk = 0,               // Indicates the loop should continue.
    kShouldTerminate = 1,  // Indicates the loop should break/terminate.
  };
  struct OutputItem {
    CodecPacket* packet;
    const CodecBuffer* buffer;

    // Number of valid data bytes currently in the `buffer`.
    uint32_t data_length;
    // Timestamp of the last processed input item.
    std::optional<uint64_t> timestamp;
  };

  CodecAdapterSWImpl(std::mutex& lock, CodecAdapterEvents* codec_adapter_events)
      : CodecAdapterSW(lock, codec_adapter_events) {}
  ~CodecAdapterSWImpl() = default;

  fuchsia_sysmem2::BufferCollectionConstraints CoreCodecGetBufferCollectionConstraints2(
      CodecPort port, const fuchsia::media::StreamBufferConstraints& stream_buffer_constraints,
      const fuchsia::media::StreamBufferPartialSettings& partial_settings) override {
    std::lock_guard<std::mutex> lock(lock_);
    fuchsia_sysmem2::BufferCollectionConstraints result;

    // The CodecImpl won't hand us the sysmem token, so we shouldn't expect to
    // have the token here.
    ZX_DEBUG_ASSERT(!partial_settings.has_sysmem_token());

    const auto constraints = BufferCollectionConstraints(port);

    result.min_buffer_count_for_camping() = constraints.min_buffer_count_for_camping;

    ZX_DEBUG_ASSERT(!result.min_buffer_count_for_dedicated_slack().has_value());
    ZX_DEBUG_ASSERT(!result.min_buffer_count_for_shared_slack().has_value());

    auto& bmc = result.buffer_memory_constraints().emplace();
    bmc.min_size_bytes() = constraints.buffer_memory_constraints.min_size_bytes;
    bmc.max_size_bytes() = constraints.buffer_memory_constraints.max_size_bytes;

    // These are all false because SW encode.
    bmc.physically_contiguous_required() = false;
    bmc.secure_required() = false;

    ZX_DEBUG_ASSERT(!result.image_format_constraints().has_value());

    // We don't have to fill out usage - CodecImpl takes care of that.
    ZX_DEBUG_ASSERT(!result.usage().has_value());

    return result;
  }

  void CoreCodecSetBufferCollectionInfo(
      CodecPort port,
      const fuchsia_sysmem2::BufferCollectionInfo& buffer_collection_info) override {}

 protected:
  void ProcessInputLoop() override {
    std::optional<CodecInputItem> maybe_input_item;
    while ((maybe_input_item = input_queue_.WaitForElement())) {
      CodecInputItem item = std::move(maybe_input_item.value());

      auto return_packet = fit::defer([this, &item] {
        if (item.is_packet()) {
          events_->onCoreCodecInputPacketDone(item.packet());
        }
      });

      bool is_active = [this, &item]() FXL_NO_THREAD_SAFETY_ANALYSIS {
        std::unique_lock<std::mutex> lock(lock_);
        while (output_reconfig_pending_ && !item.is_format_details()) {
          reconfig_cond_.wait(lock);
        }
        return stream_active_;
      }();
      if (!is_active) {
        return;
      }

      // Item is format details.
      if (item.is_format_details()) {
        if (input_packet_seen_) {
          events_->onCoreCodecFailCodec(
              "Midstream format change not supported after input packet.");
          return;
        }
        if (ProcessFormatDetails(item.format_details()) == kShouldTerminate) {
          // A failure was reported through `events_` or the stream was stopped.
          return;
        }
        {
          std::lock_guard<std::mutex> lock(lock_);
          output_reconfig_pending_ = true;
        }
        // CodecImpl guarantees that QueueInputFormatDetails() will happen before
        // any input packets (in CodecImpl::HandlePendingInputFormatDetails()),
        // regardless of whether the input format is ultimately provided during
        // StreamProcessor create or via a format item queued from the client.
        // So we can be sure that this call happens before any input packets.
        events_->onCoreCodecMidStreamOutputConstraintsChange(
            /*output_re_config_required=*/true);
      } else if (item.is_end_of_stream()) {
        ZX_DEBUG_ASSERT(codec_params_.has_value() == chunk_input_stream_.has_value());
        if (codec_params_ && chunk_input_stream_) {
          if (ProcessEndOfStream(&item) == kShouldTerminate) {
            // A failure was reported through `events_` or the stream was stopped.
            return;
          }
        }
        events_->onCoreCodecOutputEndOfStream(false);
      } else {
        // Input is packet.
        ZX_DEBUG_ASSERT(item.is_packet());
        input_packet_seen_ = true;
        auto status = ProcessInputPacket(item.packet());
        if (status == kShouldTerminate) {
          // A failure was reported through `events_` or the stream was stopped.
          return;
        }
      }
    }
  }

  void InitChunkInputStream(const fuchsia::media::FormatDetails& format_details)
      __TA_EXCLUDES(lock_) {
    {
      std::lock_guard<std::mutex> lock(lock_);
      min_output_buffer_size_ = safemath::checked_cast<uint32_t>(MinOutputBufferSize());
    }
    chunk_input_stream_.emplace(
        InputChunkSize(), CreateTimestampExtrapolator(format_details),
        [this](const ChunkInputStream::InputBlock input_block) {
          // Get the output buffer for encoding the current input block.
          if (!output_item_ || output_item_->packet == nullptr) {
            std::optional<CodecPacket*> maybe_output_packet = free_output_packets_.WaitForElement();
            if (!maybe_output_packet) {
              bool active;
              {
                std::lock_guard<std::mutex> lock(lock_);
                active = stream_active_;
              }
              if (!active) {
                return ChunkInputStream::kTerminate;
              }
              // We should close the stream since we couldn't fetch output buffer.
              events_->onCoreCodecFailCodec("Could not get output packet.");
              return ChunkInputStream::kTerminate;
            }
            CodecPacket* packet = *maybe_output_packet;
            ZX_DEBUG_ASSERT(packet);

            const CodecBuffer* buffer = output_buffer_pool_.AllocateBuffer();
            if (!buffer) {
              bool active;
              {
                std::lock_guard<std::mutex> lock(lock_);
                active = stream_active_;
              }
              if (!active) {
                return ChunkInputStream::kTerminate;
              }
              events_->onCoreCodecFailCodec("Could not get output buffer.");
              return ChunkInputStream::kTerminate;
            }
            auto checked_buffer_length = safemath::CheckedNumeric(buffer->size()).Cast<uint32_t>();
            ZX_DEBUG_ASSERT(checked_buffer_length.IsValid());
            ZX_DEBUG_ASSERT(checked_buffer_length.ValueOrDie() >= MinOutputBufferSize());

            // Set output item.
            output_item_.emplace(OutputItem{
                packet,
                buffer,
                0,
                std::nullopt,
            });
          }

          ZX_DEBUG_ASSERT(output_item_);
          ZX_DEBUG_ASSERT(output_item_->packet);
          ZX_DEBUG_ASSERT(output_item_->buffer);
          // When we get a new output buffer, we always ensure that its capacity is
          // at least `MinOutputBufferSize()`.
          // After processing every input block data, we output the output buffer if it
          // doesn't have at least `MinOutputBufferSize()` more free space.
          // Therefore, this should always be true.
          ZX_DEBUG_ASSERT(output_item_->buffer->size() >= MinOutputBufferSize());

          // Only process if the input block has some real data.
          if (input_block.non_padding_len != 0) {
            // Update timestamp if timestamp hasn't been updated for
            // the current output packet yet.
            if (output_item_->data_length == 0 && input_block.timestamp_ish) {
              output_item_->timestamp.emplace(*input_block.timestamp_ish);
            }

            // Process contents of input buffer to current output buffer.
            ZX_DEBUG_ASSERT(output_item_->buffer->size() - output_item_->data_length >=
                            MinOutputBufferSize());
            int produced =
                ProcessInputChunkData(const_cast<uint8_t*>(input_block.data), InputChunkSize(),
                                      output_item_->buffer->base() + output_item_->data_length,
                                      output_item_->buffer->size() - output_item_->data_length);
            if (produced < 0) {
              // We should close the stream since chunk processing was not successful.
              events_->onCoreCodecFailCodec("Could not process input chunk data.");
              return ChunkInputStream::kTerminate;
            }
            output_item_->data_length += produced;
          }

          // Output current buffer if it cannot encode another input block or if
          // current input block indicates end of stream and the output contains
          // some data.
          // TODO(https://fxbug.dev/42068031): consider outputting current packet if the input
          // queue is empty and the the chunk input stream does not have at least
          // one more chunk after the current chunk.
          if ((output_item_->buffer->size() - output_item_->data_length) < MinOutputBufferSize() ||
              (input_block.is_end_of_stream && output_item_->data_length > 0)) {
            SendAndResetOutputPacket();
          }

          return ChunkInputStream::kContinue;
        });
  }

  void CleanUpAfterStream() __TA_EXCLUDES(lock_) override {
    if (output_item_) {
      if (output_item_->buffer) {
        auto base = output_item_->buffer->base();
        output_buffer_pool_.FreeBuffer(base);
        output_item_->buffer = nullptr;
      }
      if (output_item_->packet) {
        free_output_packets_.Push(output_item_->packet);
        output_item_->packet = nullptr;
      }
      output_item_ = std::nullopt;
    }
    ResetCodecParams();
    chunk_input_stream_ = std::nullopt;
    input_packet_seen_ = false;
  }

  // Processes format details and initializes appropriate internal configurations based
  // on it.
  // `codec_params_` should be initialized in this method, followed by calling
  // `InitChunkInputStream()` so that `chunk_input_stream_` and `min_output_buffer_size_` are
  // initialized.
  virtual InputLoopStatus ProcessFormatDetails(const fuchsia::media::FormatDetails& format_details)
      __TA_EXCLUDES(lock_) = 0;

  // Processes the data from input data. If processing was successful, returns the number
  // of output data bytes produced. If an error occurred, it returns -1.
  // For encoder, this method encodes the input data. For decoder, it decodes the input data.
  // The method manipulates the output buffer.
  virtual int ProcessInputChunkData(const uint8_t* input_data, size_t input_data_size,
                                    uint8_t* output_buffer, size_t output_buffer_size) = 0;

  // The minimum frame size (number of input data bytes that can be processed) is 1 byte.
  // For convenience and intuition, we enforce the same for `ChunkInputStream::InputBlock`.
  virtual size_t InputChunkSize() = 0;

  // Returns the minimum number of bytes required to hold output data that is produced from
  // processing a single input chunk.
  //
  // `codec_params_` is guaranteed to be populated when `CodecAdapterSWImpl` calls this method.
  // `OutputFormatDetails()` runs on `input_processing_thread_` when reached through
  // `CoreCodecGetOutputFormat()`, and holds `lock_` when calling this method so that direct
  // callers such as tests, and any future off-thread caller, are safe. Because this method is
  // called both with `lock_` held and without `lock_` held (on `input_processing_thread_`),
  // implementations must not acquire `lock_`.
  // Implementations of `BufferCollectionConstraints(kOutputPort)` should read
  // `min_output_buffer_size_` rather than calling this method if the size depends on
  // `codec_params_`.
  virtual size_t MinOutputBufferSize() = 0;

  // Returns the constraints to use for `CodecAdapter::CoreCodecGetBufferCollectionConstraints2`
  // depending on the port. The fields used are:
  // - fuchsia::sysmem::BufferCollectionConstraints.min_buffer_count_for_camping
  // - fuchsia::sysmem::BufferCollectionConstraints.buffer_memory_constraints.min_size_bytes
  // - fuchsia::sysmem::BufferCollectionConstraints.buffer_memory_constraints.max_size_bytes
  //
  // For `kOutputPort`, once `ProcessFormatDetails()` has succeeded at least once, this method may
  // be called even after `CleanUpAfterStream()` (where `ResetCodecParams()` has reset
  // `codec_params_`) before a subsequent stream runs `ProcessFormatDetails()`. Implementations
  // must not assume `codec_params_.has_value()` here and should use `min_output_buffer_size_` if
  // the minimum output buffer size depends on stream format details.
  virtual fuchsia::sysmem::BufferCollectionConstraints BufferCollectionConstraints(CodecPort port)
      __TA_REQUIRES(lock_) = 0;

  virtual TimestampExtrapolator CreateTimestampExtrapolator(
      const fuchsia::media::FormatDetails& format_details) = 0;

  // Reset `codec_params_` to `std::nullopt` under `lock_`.
  void ResetCodecParams() __TA_EXCLUDES(lock_) {
    std::lock_guard<std::mutex> lock(lock_);
    codec_params_ = std::nullopt;
  }

  // Parameters required for actual codec work.
  //
  // See the Threading / Locking Rules comment on `CodecAdapterSW::input_processing_thread_`:
  // - `std::optional` engagement and stream-invariant format fields are populated under `lock_` in
  //   `ProcessFormatDetails()` and reset under `lock_` in `ResetCodecParams()` (called from
  //   `CleanUpAfterStream()`).
  // - Per-chunk working state inside `CodecParams` (e.g. CVSD `accumulator`, `step_size`, and
  //   `historic_bits`; LC3 `aligned_input_scratch` and encoder/decoder state) is mutated without
  //   `lock_` on `input_processing_thread_` during `ProcessInputChunkData()`.
  std::optional<CodecParams> codec_params_;

  // Minimum output buffer size from the most recent `InitChunkInputStream()` call (during
  // `ProcessFormatDetails()`), retained across `CleanUpAfterStream()` so
  // `CoreCodecGetBufferCollectionConstraints2(kOutputPort)` can succeed after a stream stops
  // before the next stream processes its format details.
  std::optional<uint32_t> min_output_buffer_size_ __TA_GUARDED(lock_);

 private:
  void PostSerial(async_dispatcher_t* dispatcher, fit::closure to_run) {
    zx_status_t post_result = async::PostTask(dispatcher, std::move(to_run));
    ZX_ASSERT_MSG(post_result == ZX_OK, "async::PostTask() failed - result: %d", post_result);
  }

  InputLoopStatus ProcessEndOfStream(CodecInputItem* item) {
    ZX_DEBUG_ASSERT(item->is_end_of_stream());
    return ProcessCodecPacket(nullptr);
  }

  InputLoopStatus ProcessInputPacket(const CodecPacket* packet) {
    return ProcessCodecPacket(packet);
  }

  InputLoopStatus ProcessCodecPacket(const CodecPacket* packet) {
    ZX_DEBUG_ASSERT(codec_params_);
    ZX_DEBUG_ASSERT(chunk_input_stream_);
    ChunkInputStream::Status status;
    if (packet == nullptr) {
      status = chunk_input_stream_->Flush();
    } else {
      status = chunk_input_stream_->ProcessInputPacket(packet);
    }

    switch (status) {
      case ChunkInputStream::kExtrapolationFailedWithoutTimebase:
        events_->onCoreCodecFailCodec("Timebase was not set for extrapolation.");
        return kShouldTerminate;
      case ChunkInputStream::kUserTerminated:
        return kShouldTerminate;
      default:
        return kOk;
    };
  }

  // Outputs and resets current output buffer.
  void SendAndResetOutputPacket() {
    ZX_DEBUG_ASSERT(output_item_);
    ZX_DEBUG_ASSERT(output_item_->packet);
    ZX_DEBUG_ASSERT(output_item_->data_length > 0);

    auto packet = output_item_->packet;
    packet->SetBuffer(output_item_->buffer);
    packet->SetStartOffset(0);
    packet->SetValidLengthBytes(output_item_->data_length);
    if (output_item_->timestamp) {
      packet->SetTimstampIsh(*output_item_->timestamp);
    } else {
      packet->ClearTimestampIsh();
    }

    {
      TRACE_INSTANT("codec_runner", "Media:PacketSent", TRACE_SCOPE_THREAD);

      fit::closure free_buffer = [this, base = packet->buffer()->base()] {
        output_buffer_pool_.FreeBuffer(base);
      };
      std::lock_guard<std::mutex> lock(lock_);
      in_use_by_client_[packet] = fit::defer(std::move(free_buffer));
    }

    events_->onCoreCodecOutputPacket(packet,
                                     /*error_detected_before=*/false,
                                     /*error_detected_during=*/false);

    // Reset output item.
    output_item_ = std::nullopt;
  }

  // Current input buffer where we buffer data from the input packet
  // before sending it over to be encoded as output. CVSD encoder processes
  // 2 bytes of data to produce 1 bit of output data. Hence, the buffer should
  // be initialized to 16 bytes size.
  std::optional<ChunkInputStream> chunk_input_stream_;

  // Current output item that we are currently encoding into.
  std::optional<OutputItem> output_item_;

  bool input_packet_seen_ = false;
};

#endif  // SRC_MEDIA_CODEC_CODECS_SW_CODEC_ADAPTER_SW_IMPL_H_
