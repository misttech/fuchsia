// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "output_sink.h"

#include <limits>

OutputSink::OutputSink(Sender sender, thrd_t writer_thread)
    : sender_(std::move(sender)), writer_thread_(writer_thread) {
  ZX_DEBUG_ASSERT(sender_);
}

void OutputSink::AddOutputPacket(CodecPacket* output_packet) {
  ZX_DEBUG_ASSERT(output_packet);

  if (output_packet->buffer()) {
    // Safe to push the raw CodecBuffer* and release the packet's KeepAlive via SetBuffer(nullptr):
    // for !IsSupportsDynamicBuffers(), CoreCodecRecycleOutputPacket (which calls AddOutputPacket)
    // is only ever called by CodecImpl for the current buffer_lifetime_ordinal prior to
    // EnsureBuffersNotConfigured(kOutputPort), while CodecImpl still holds
    // CodecBuffer::until_remove_started_child_vmo_ open. See comment on free_output_buffers_.
    free_output_buffers_.Push(std::move(output_packet->buffer()));
    output_packet->SetBuffer(nullptr);
  }
  ZX_DEBUG_ASSERT(output_packet->buffer() == nullptr);

  free_output_packets_.Push(std::move(output_packet));
}

void OutputSink::AddOutputBuffer(const CodecBuffer* output_buffer) {
  ZX_DEBUG_ASSERT(output_buffer);

  free_output_buffers_.Push(std::move(output_buffer));
  buffer_count_++;
}

OutputSink::Status OutputSink::NextOutputBlock(size_t write_size,
                                               std::optional<uint64_t> timestamp_ish,
                                               Writer output_block_writer) {
  ZX_DEBUG_ASSERT(thrd_current() == writer_thread_);
  ZX_DEBUG_ASSERT(write_size > 0);

  if (!CurrentPacketHasRoomFor(write_size)) {
    if (SendCurrentPacket() != kOk) {
      return kUserError;
    }

    Status status = SetNewPacketForWrite(write_size);
    if (status != kOk) {
      return status;
    }
  }
  ZX_DEBUG_ASSERT(current_packet_);

  if (current_packet_->valid_length_bytes() == 0 && timestamp_ish) {
    current_packet_->SetTimstampIsh(*timestamp_ish);
  }

  auto output_block = OutputBlock{
      .data = current_packet_->buffer()->base() + current_packet_->valid_length_bytes(),
      .len = write_size,
      .buffer = current_packet_->buffer(),
  };

  auto [bytes_written, write_status, key_frame] = output_block_writer(output_block);
  if (write_status != kSuccess) {
    return kUserError;
  }

  if (bytes_written > std::numeric_limits<uint32_t>::max()) {
    return kUserError;
  }

  current_packet_->SetValidLengthBytes(current_packet_->valid_length_bytes() +
                                       static_cast<uint32_t>(bytes_written));
  current_packet_->SetKeyFrame(key_frame);
  return kOk;
}

OutputSink::Status OutputSink::Flush() {
  ZX_DEBUG_ASSERT(thrd_current() == writer_thread_);

  if (current_packet_ == nullptr || current_packet_->valid_length_bytes() == 0) {
    return kOk;
  }

  return SendCurrentPacket();
}

bool OutputSink::CurrentPacketHasRoomFor(size_t write_size) {
  return (current_packet_ != nullptr) &&
         current_packet_->buffer()->size() - current_packet_->valid_length_bytes() >= write_size;
}

OutputSink::Status OutputSink::SendCurrentPacket() {
  if (!current_packet_) {
    return kOk;
  }
  ZX_DEBUG_ASSERT_MSG(current_packet_->valid_length_bytes() > 0,
                      "Attempting to send empty packet.");

  if (sender_(current_packet_) != kSuccess) {
    return kUserError;
  }

  current_packet_ = nullptr;
  return kOk;
}

bool OutputSink::HasPendingPacket() { return current_packet_ != nullptr; }

uint32_t OutputSink::OutputBufferCount() { return buffer_count_; }

OutputSink::Status OutputSink::SetNewPacketForWrite(size_t write_size) {
  // It is safe to pop and read raw buffer/packet pointers here without KeepAlive/zx::vmo handles
  // in the free queues because EnsureBuffersNotConfigured(kOutputPort) (which calls Reset(false)
  // before releasing CodecBuffer::until_remove_started_child_vmo_) can only run when no stream is
  // active (after CoreCodecStopStream() has stopped the writer thread) or during
  // onCoreCodecMidStreamOutputConstraintsChange (which strictly requires the CodecAdapter to pause
  // the writer thread before triggering the reconfig and keep it paused until
  // CoreCodecMidStreamOutputBufferReConfigFinish()). Thus, the writer thread will never block or
  // run here during EnsureBuffersNotConfigured(). See comment on free_output_buffers_.
  auto maybe_buffer = free_output_buffers_.WaitForElement();
  if (!maybe_buffer) {
    return kUserTerminatedWait;
  }
  ZX_DEBUG_ASSERT_MSG(*maybe_buffer, "A null buffer made it into the queue.");
  const CodecBuffer* buffer = *maybe_buffer;

  if (buffer->size() < write_size) {
    return kBuffersTooSmall;
  }

  auto maybe_packet = free_output_packets_.WaitForElement();
  if (!maybe_packet) {
    return kUserTerminatedWait;
  }
  ZX_DEBUG_ASSERT_MSG(*maybe_packet, "A null packet made it into the queue.");
  current_packet_ = *maybe_packet;
  current_packet_->SetBuffer(buffer);
  current_packet_->SetStartOffset(0);
  current_packet_->SetValidLengthBytes(0);

  return kOk;
}

void OutputSink::StopAllWaits() {
  free_output_buffers_.StopAllWaits();
  free_output_packets_.StopAllWaits();
}

void OutputSink::Reset(bool keep_data) {
  free_output_buffers_.Reset(keep_data);
  free_output_packets_.Reset(keep_data);
  if (!keep_data) {
    buffer_count_ = 0;
    current_packet_ = nullptr;
  }
}
