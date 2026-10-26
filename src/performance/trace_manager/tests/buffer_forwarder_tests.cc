// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//
#include <lib/zx/socket.h>
#include <lib/zx/vmo.h>

#include <array>
#include <vector>

#include <gtest/gtest.h>

#include "src/performance/trace_manager/buffer_forwarder.h"
#include "src/performance/trace_manager/deferred_buffer_forwarder.h"

TEST(BufferForwarderTest, DeferredForwarder) {
  zx::socket ep0, ep1;
  zx::socket::create(0, &ep0, &ep1);
  tracing::DeferredBufferForwarder forwarder(std::move(ep1));
  forwarder.WriteMagicNumberRecord();

  zx_signals_t pending;
  // We shouldn't see any data on the socket yet
  zx_status_t res = ep0.wait_one(ZX_SOCKET_READABLE, zx::time::infinite_past(), &pending);
  ASSERT_EQ(res, ZX_ERR_TIMED_OUT);

  forwarder.Flush();

  // Now we should see data
  res = ep0.wait_one(ZX_SOCKET_READABLE, zx::time::infinite_past(), &pending);
  ASSERT_EQ(res, ZX_OK);
  ASSERT_TRUE(pending & ZX_SOCKET_READABLE);

  uint64_t buffer[8];
  size_t actual;
  ep0.read(0, buffer, 64, &actual);

  // We should only read the 8 bytes of the record we wrote.
  ASSERT_EQ(actual, size_t{8});

  // And the bytes should be the FXT magic bytes.
  ASSERT_EQ(buffer[0], uint64_t{0x0016547846040010});

  // The socket should now be empty.
  res = ep0.wait_one(ZX_SOCKET_READABLE, zx::time::infinite_past(), &pending);
  ASSERT_EQ(res, ZX_ERR_TIMED_OUT);
}

TEST(BufferForwarderTest, SocketBackpressureThreshold) {
  zx::socket ep0, ep1;
  ASSERT_EQ(zx::socket::create(0, &ep0, &ep1), ZX_OK);

  zx::socket ep1_dup;
  ASSERT_EQ(ep1.duplicate(ZX_RIGHT_SAME_RIGHTS, &ep1_dup), ZX_OK);

  tracing::BufferForwarder forwarder(std::move(ep1));
  ASSERT_FALSE(forwarder.IsSocketBackpressureExceeded());

  zx_info_socket_t info;
  ASSERT_EQ(ep0.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  const size_t tx_buf_max = info.tx_buf_max;
  ASSERT_GT(tx_buf_max, 0u);

  const size_t threshold = (tx_buf_max * forwarder.backpressure_percentage()) / 100;
  ASSERT_GT(threshold, 0u);

  size_t bytes_to_write = threshold + 100;
  if (bytes_to_write > tx_buf_max) {
    bytes_to_write = tx_buf_max;
  }
  std::vector<uint8_t> mock_data(bytes_to_write, 0x55);
  size_t actual_written = 0;
  ASSERT_EQ(ep1_dup.write(0, mock_data.data(), mock_data.size(), &actual_written), ZX_OK);
  ASSERT_EQ(actual_written, bytes_to_write);

  ASSERT_TRUE(forwarder.IsSocketBackpressureExceeded());

  std::vector<uint8_t> read_buffer(bytes_to_write);
  size_t actual_read = 0;
  ASSERT_EQ(ep0.read(0, read_buffer.data(), read_buffer.size(), &actual_read), ZX_OK);
  ASSERT_EQ(actual_read, bytes_to_write);

  ASSERT_FALSE(forwarder.IsSocketBackpressureExceeded());
}

TEST(BufferForwarderTest, WriteChunkOffsetAbove4GiB) {
  zx::socket ep0, ep1;
  ASSERT_EQ(zx::socket::create(0, &ep0, &ep1), ZX_OK);
  tracing::BufferForwarder forwarder(std::move(ep1));

  // VMO pages are committed lazily, so this only allocates the pages we write.
  const uint64_t page_size = zx_system_get_page_size();
  constexpr uint64_t k4GiB = uint64_t{1} << 32;
  zx::vmo vmo;
  ASSERT_EQ(zx::vmo::create(k4GiB + 2 * page_size, 0u, &vmo), ZX_OK);

  // Put distinct data at the same in-page offset below and above 4 GiB so that truncating the
  // upper 32 bits of the offset would forward the wrong bytes.
  constexpr uint64_t kInPageOffset = 128;
  constexpr std::array<uint64_t, 2> kLowWords = {0x1111111111111111ULL, 0x2222222222222222ULL};
  constexpr std::array<uint64_t, 2> kHighWords = {0x3333333333333333ULL, 0x4444444444444444ULL};
  ASSERT_EQ(vmo.write(kLowWords.data(), kInPageOffset, sizeof(kLowWords)), ZX_OK);
  const uint64_t high_offset = k4GiB + kInPageOffset;
  ASSERT_EQ(vmo.write(kHighWords.data(), high_offset, sizeof(kHighWords)), ZX_OK);

  ASSERT_EQ(forwarder.WriteChunkBy(tracing::BufferForwarder::ForwardStrategy::Size, vmo,
                                   high_offset, sizeof(kHighWords)),
            tracing::TransferStatus::kComplete);

  std::array<uint64_t, 2> actual_words{};
  size_t actual_read = 0;
  ASSERT_EQ(ep0.read(0, actual_words.data(), sizeof(actual_words), &actual_read), ZX_OK);
  EXPECT_EQ(actual_read, sizeof(kHighWords));
  EXPECT_EQ(actual_words, kHighWords);
}
