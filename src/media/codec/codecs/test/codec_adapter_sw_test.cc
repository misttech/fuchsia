// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "../sw/codec_adapter_sw.h"

#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>

#include <gtest/gtest.h>

#include "test_codec_packets.h"

class CodecAdapterSWDummy : public CodecAdapterSW<fit::deferred_action<fit::closure>> {
 public:
  CodecAdapterSWDummy(std::mutex& lock)
      : CodecAdapterSW(
            lock,
            /* bad ptr to pass non-null assert */ reinterpret_cast<CodecAdapterEvents*>(0xaa)) {}

  // Much like the real fit::defer(s) in in_use_by_client_, the fit::defer we're
  // putting in in_use_by_client_ touches output_buffer_pool_ in a way that'll
  // crash if output_buffer_pool_ is already destructed.
  void EntangleClientMapAndBufferPoolDestructors() {
    std::lock_guard<std::mutex> lock(lock_);
    fit::closure deferred = [this]() {
      // This call will crash if output_buffer_pool_ has already been
      // destructed, much like the real-world fit::defer(s) that would be in
      // in_use_by_client_ if we delete CodecAdapterSW with stuff in flight.
      output_buffer_pool_.has_buffers_in_use();
    };
    in_use_by_client_[nullptr] = fit::defer(std::move(deferred));
  }

  fuchsia_sysmem2::BufferCollectionConstraints CoreCodecGetBufferCollectionConstraints2(
      CodecPort port, const fuchsia::media::StreamBufferConstraints& stream_buffer_constraints,
      const fuchsia::media::StreamBufferPartialSettings& partial_settings) override {
    return fuchsia_sysmem2::BufferCollectionConstraints();
  }

  void CoreCodecSetBufferCollectionInfo(
      CodecPort port,
      const fuchsia_sysmem2::BufferCollectionInfo& buffer_collection_info) override {}

 protected:
  virtual void ProcessInputLoop() override {}

  void CleanUpAfterStream() __TA_EXCLUDES(lock_) override {}

  std::pair<fuchsia::media::FormatDetails, size_t> OutputFormatDetails()
      __TA_EXCLUDES(lock_) override {
    return {fuchsia::media::FormatDetails(), 0};
  }
};

TEST(CodecAdapterSW, DoesNotCrashOnDestruction) {
  // To pass, this test must not crash.
  std::mutex lock;
  auto under_test = CodecAdapterSWDummy(lock);
}

TEST(BufferPool, ResetAndFreeBuffer) {
  BufferPool pool;
  auto buffers = Buffers({1024, 2048});

  // Add buffers to pool
  pool.AddBuffer(buffers.ptr(0));
  pool.AddBuffer(buffers.ptr(1));

  // Allocate buffers
  const CodecBuffer* buf0 = pool.AllocateBuffer(1024);
  const CodecBuffer* buf1 = pool.AllocateBuffer(2048);
  EXPECT_EQ(buf0, buffers.ptr(0));
  EXPECT_EQ(buf1, buffers.ptr(1));
  EXPECT_TRUE(pool.has_buffers_in_use());

  // Reset the pool, forgetting all active allocations. This should not crash
  // even if buffers are in use.
  pool.Reset(false);
  EXPECT_FALSE(pool.has_buffers_in_use());

  // A late/delayed free of the old buffer should be safely ignored and not
  // push it back to the free list or crash.
  pool.FreeBuffer(buf0->base());

  // Stop waits so that AllocateBuffer returns immediately instead of blocking
  // if the queue is empty.
  pool.StopAllWaits();

  // If we try to allocate now, it should return nullptr because the pool was reset,
  // and the late free should have been ignored.
  const CodecBuffer* allocated = pool.AllocateBuffer();
  EXPECT_EQ(allocated, nullptr);
}
