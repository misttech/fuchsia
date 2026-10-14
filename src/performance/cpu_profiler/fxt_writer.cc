// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "fxt_writer.h"

#include <lib/syslog/cpp/macros.h>

#include <algorithm>
#include <cstring>
#include <utility>

namespace profiler {
namespace {
// Buffered data is stored in chunks of this size so that a large backlog doesn't require large
// contiguous allocations or repeated reallocation.
constexpr size_t kChunkSize = 1024 * 1024;
}  // namespace

void profiler::FxtRecordBuffer::WriteBytes(const void* buffer, size_t num_bytes) {
  // Calculate how many 64-bit words are needed for the given number of bytes.
  size_t num_words = (num_bytes + sizeof(uint64_t) - 1) / sizeof(uint64_t);

  size_t old_size_in_words = buffer_.size();
  buffer_.resize(old_size_in_words + num_words);
  uint64_t* dest = buffer_.data() + old_size_in_words;
  dest[num_words - 1] = 0;
  memcpy(dest, buffer, num_bytes);
}

void profiler::FxtRecordBuffer::Commit() {
  writer_->Write(buffer_.data(), buffer_.size() * sizeof(uint64_t));
}

profiler::FxtWriter::FxtWriter(zx::socket socket, async_dispatcher_t* dispatcher,
                               size_t max_buffered_bytes)
    : socket_(std::move(socket)), dispatcher_(dispatcher), max_buffered_bytes_(max_buffered_bytes) {
  wait_.set_object(socket_.get());
  wait_.set_trigger(ZX_SOCKET_WRITABLE | ZX_SOCKET_PEER_CLOSED);
}

profiler::FxtWriter::~FxtWriter() {
  if (dropped_records_ > 0) {
    FX_LOGS(WARNING) << "Dropped " << dropped_records_
                     << " records because the output socket could not keep up";
  }
  if (buffered_bytes_ > 0) {
    FX_LOGS(WARNING) << "Discarding " << buffered_bytes_ << " bytes of unwritten profiling data";
  }
}

zx::result<profiler::FxtRecordBuffer> profiler::FxtWriter::Reserve(uint64_t header) {
  return zx::ok(profiler::FxtRecordBuffer(this, header));
}

void profiler::FxtWriter::Write(const void* data, size_t num_bytes) {
  if (!socket_ || closing_ || num_bytes == 0) {
    return;
  }
  const uint8_t* bytes = static_cast<const uint8_t*>(data);

  // Fast path: nothing is queued ahead of this record, so try writing it straight to the socket.
  if (buffered_bytes_ == 0) {
    size_t written = 0;
    zx_status_t status = socket_.write(0, bytes, num_bytes, &written);
    if (status == ZX_OK) {
      if (written == num_bytes) {
        return;
      }
      // The socket took part of the record. The rest must follow, regardless of the buffering
      // limit, or the stream would be corrupted.
      Enqueue(bytes + written, num_bytes - written);
      ArmWait();
      return;
    }
    if (status != ZX_ERR_SHOULD_WAIT) {
      HandleWriteError(status);
      return;
    }
  }

  if (buffered_bytes_ + num_bytes > max_buffered_bytes_) {
    if (dropped_records_++ == 0) {
      FX_LOGS(WARNING) << "Output socket is not keeping up; " << buffered_bytes_
                       << " bytes are buffered. Dropping records until it catches up.";
    }
    return;
  }
  Enqueue(bytes, num_bytes);
  ArmWait();
}

void profiler::FxtWriter::Enqueue(const uint8_t* data, size_t num_bytes) {
  buffered_bytes_ += num_bytes;
  while (num_bytes > 0) {
    if (chunks_.empty() || chunks_.back().size() >= kChunkSize) {
      chunks_.emplace_back();
      chunks_.back().reserve(kChunkSize);
    }
    std::vector<uint8_t>& chunk = chunks_.back();
    size_t n = std::min(num_bytes, kChunkSize - chunk.size());
    chunk.insert(chunk.end(), data, data + n);
    data += n;
    num_bytes -= n;
  }
}

void profiler::FxtWriter::ArmWait() {
  if (wait_.is_pending()) {
    return;
  }
  if (zx_status_t status = wait_.Begin(dispatcher_); status != ZX_OK) {
    FX_PLOGS(ERROR, status) << "Failed to wait on output socket";
    HandleWriteError(status);
  }
}

void profiler::FxtWriter::Drain() {
  while (!chunks_.empty()) {
    std::vector<uint8_t>& front = chunks_.front();
    size_t written = 0;
    zx_status_t status =
        socket_.write(0, front.data() + front_offset_, front.size() - front_offset_, &written);
    if (status == ZX_ERR_SHOULD_WAIT) {
      ArmWait();
      return;
    }
    if (status != ZX_OK) {
      HandleWriteError(status);
      return;
    }
    front_offset_ += written;
    buffered_bytes_ -= written;
    if (front_offset_ == front.size()) {
      chunks_.pop_front();
      front_offset_ = 0;
    }
  }
  if (closing_) {
    FinishClose();
  }
}

void profiler::FxtWriter::OnSocketReady(async_dispatcher_t* dispatcher, async::WaitBase* wait,
                                        zx_status_t status, const zx_packet_signal_t* signal) {
  if (status != ZX_OK) {
    // The dispatcher is shutting down.
    DiscardBuffered();
    if (closing_) {
      FinishClose();
    }
    return;
  }
  if (signal->observed & ZX_SOCKET_WRITABLE) {
    Drain();
    return;
  }
  HandleWriteError(ZX_ERR_PEER_CLOSED);
}

void profiler::FxtWriter::HandleWriteError(zx_status_t status) {
  if (status != ZX_ERR_PEER_CLOSED) {
    FX_PLOGS(ERROR, status) << "Failed to write samples to socket";
  }
  DiscardBuffered();
  wait_.Cancel();
  socket_.reset();
  if (closing_) {
    FinishClose();
  }
}

void profiler::FxtWriter::DiscardBuffered() {
  chunks_.clear();
  front_offset_ = 0;
  buffered_bytes_ = 0;
}

void profiler::FxtWriter::FinishClose() {
  socket_.reset();
  // If we own ourselves, this destroys `this`. It must be the last thing we do.
  std::unique_ptr<FxtWriter> self = std::move(self_);
}

// static
void profiler::FxtWriter::FlushAndClose(std::unique_ptr<FxtWriter> writer) {
  if (!writer) {
    return;
  }
  writer->closing_ = true;
  if (!writer->socket_ || writer->buffered_bytes_ == 0) {
    // Nothing left to write; destroying the writer closes the socket.
    return;
  }
  FxtWriter* raw = writer.get();
  raw->self_ = std::move(writer);
  raw->ArmWait();
}

}  // namespace profiler
