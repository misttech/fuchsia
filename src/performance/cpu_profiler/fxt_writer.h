// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_PERFORMANCE_CPU_PROFILER_FXT_WRITER_H_
#define SRC_PERFORMANCE_CPU_PROFILER_FXT_WRITER_H_

#include <lib/async/cpp/wait.h>
#include <lib/async/dispatcher.h>
#include <lib/zx/result.h>
#include <lib/zx/socket.h>

#include <cstdint>
#include <deque>
#include <memory>
#include <vector>

namespace profiler {

class FxtWriter;

class FxtRecordBuffer {
 public:
  explicit FxtRecordBuffer(FxtWriter* writer, uint64_t header)
      : buffer_({header}), writer_(writer) {}
  std::vector<uint64_t> buffer_;
  FxtWriter* writer_;
  void WriteWord(uint64_t word) { buffer_.push_back(word); }
  void WriteBytes(const void* buffer, size_t num_bytes);
  void Commit();
};

// Writes FXT records to a socket without ever blocking the calling thread.
//
// The profiler services thread and process start/exit exceptions for every profiled task on the
// same dispatcher that writes samples out. Those exceptions keep the starting or exiting thread
// paused until they are handled, so blocking on a full socket (e.g. because the host is draining
// it slowly) stalls thread creation and teardown in every profiled process. Instead, records that
// do not fit in the socket are buffered in memory and drained asynchronously on `dispatcher` as
// the socket becomes writable.
//
// All methods must be called on `dispatcher`.
class FxtWriter {
 public:
  // Upper bound on the amount of data buffered in memory waiting for the socket to drain. Records
  // which would exceed this are dropped (and counted) rather than blocking.
  static constexpr size_t kDefaultMaxBufferedBytes = size_t{256} * 1024 * 1024;

  FxtWriter(zx::socket socket, async_dispatcher_t* dispatcher,
            size_t max_buffered_bytes = kDefaultMaxBufferedBytes);
  ~FxtWriter();

  FxtWriter(const FxtWriter&) = delete;
  FxtWriter& operator=(const FxtWriter&) = delete;

  zx::result<FxtRecordBuffer> Reserve(uint64_t header);

  // Write a complete record. The record is either written in full (possibly asynchronously) or
  // dropped in full; it is never truncated.
  void Write(const void* data, size_t num_bytes);

  // Stop accepting records and close the socket once all buffered data has been written or the
  // peer has gone away. The writer keeps itself alive on its dispatcher until then.
  static void FlushAndClose(std::unique_ptr<FxtWriter> writer);

  size_t buffered_bytes() const { return buffered_bytes_; }
  uint64_t dropped_records() const { return dropped_records_; }

 private:
  void Enqueue(const uint8_t* data, size_t num_bytes);
  void Drain();
  void ArmWait();
  void OnSocketReady(async_dispatcher_t* dispatcher, async::WaitBase* wait, zx_status_t status,
                     const zx_packet_signal_t* signal);
  void HandleWriteError(zx_status_t status);
  void DiscardBuffered();
  // Close the socket and, if the writer owns itself, destroy it. Callers must not touch any members
  // after calling this.
  void FinishClose();

  zx::socket socket_;
  async_dispatcher_t* dispatcher_;
  const size_t max_buffered_bytes_;

  // Data waiting to be written, in order. `front_offset_` bytes of the first chunk have already
  // been written.
  std::deque<std::vector<uint8_t>> chunks_;
  size_t front_offset_ = 0;
  size_t buffered_bytes_ = 0;
  uint64_t dropped_records_ = 0;

  bool closing_ = false;
  // Set by FlushAndClose while buffered data is still being drained.
  std::unique_ptr<FxtWriter> self_;

  async::WaitMethod<FxtWriter, &FxtWriter::OnSocketReady> wait_{this};
};
}  // namespace profiler
#endif  // SRC_PERFORMANCE_CPU_PROFILER_FXT_WRITER_H_
