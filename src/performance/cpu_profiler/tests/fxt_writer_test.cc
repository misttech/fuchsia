// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/performance/cpu_profiler/fxt_writer.h"

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fxt/serializer.h>
#include <lib/zx/socket.h>
#include <lib/zx/time.h>

#include <algorithm>
#include <cstring>
#include <memory>
#include <vector>

#include <gtest/gtest.h>

#include "zircon/types.h"

namespace profiler {
namespace {

// Fill `socket` until it can't take any more data, and return how many bytes were written.
size_t FillSocket(const zx::socket& socket) {
  std::vector<uint8_t> junk(4096, 0xAB);
  size_t total = 0;
  for (;;) {
    size_t written = 0;
    zx_status_t status = socket.write(0, junk.data(), junk.size(), &written);
    if (status == ZX_ERR_SHOULD_WAIT) {
      return total;
    }
    EXPECT_EQ(status, ZX_OK);
    if (status != ZX_OK) {
      return total;
    }
    total += written;
  }
}

// Read and discard exactly `num_bytes` bytes from `socket`.
void Discard(const zx::socket& socket, size_t num_bytes) {
  std::vector<uint8_t> buf(4096);
  while (num_bytes > 0) {
    size_t actual = 0;
    ASSERT_EQ(socket.read(0, buf.data(), std::min(buf.size(), num_bytes), &actual), ZX_OK);
    num_bytes -= actual;
  }
}

// Read everything currently in `socket`, running `loop` so that the writer can refill it.
std::vector<uint64_t> ReadAllWords(const zx::socket& socket, async::Loop& loop) {
  std::vector<uint8_t> bytes;
  std::vector<uint8_t> buf(64 * 1024);
  for (;;) {
    size_t actual = 0;
    zx_status_t status = socket.read(0, buf.data(), buf.size(), &actual);
    if (status == ZX_OK) {
      bytes.insert(bytes.end(), buf.begin(), buf.begin() + static_cast<ptrdiff_t>(actual));
      loop.RunUntilIdle();
      continue;
    }
    if (status == ZX_ERR_SHOULD_WAIT) {
      loop.RunUntilIdle();
      status = socket.read(0, buf.data(), buf.size(), &actual);
      if (status == ZX_OK) {
        bytes.insert(bytes.end(), buf.begin(), buf.begin() + static_cast<ptrdiff_t>(actual));
        loop.RunUntilIdle();
        continue;
      }
    }
    break;
  }
  EXPECT_EQ(bytes.size() % sizeof(uint64_t), 0u);
  std::vector<uint64_t> words(bytes.size() / sizeof(uint64_t));
  memcpy(words.data(), bytes.data(), words.size() * sizeof(uint64_t));
  return words;
}

void WriteTwoWordRecord(FxtWriter& writer, uint64_t header, uint64_t payload) {
  zx::result<FxtRecordBuffer> buffer = writer.Reserve(header);
  ASSERT_TRUE(buffer.is_ok());
  buffer->WriteWord(payload);
  buffer->Commit();
}

}  // namespace

TEST(FxtWriterTest, WriteSingleRecord) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  zx::socket socket1, socket2;
  ASSERT_EQ(zx::socket::create(0, &socket1, &socket2), ZX_OK);

  FxtWriter writer(std::move(socket1), loop.dispatcher());
  const uint64_t header = 1234;
  zx::result<FxtRecordBuffer> buffer = writer.Reserve(header);
  ASSERT_TRUE(buffer.is_ok());

  buffer->WriteWord(1234);

  ASSERT_EQ(buffer->buffer_.size(), 2u);
  EXPECT_EQ(buffer->buffer_[1], 1234u);
}

TEST(FxtWriterTest, WriteBytes) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  zx::socket socket1, socket2;
  ASSERT_EQ(zx::socket::create(0, &socket1, &socket2), ZX_OK);

  FxtWriter writer(std::move(socket1), loop.dispatcher());
  zx::result<FxtRecordBuffer> buffer = writer.Reserve(0);
  ASSERT_TRUE(buffer.is_ok());

  const char* test_string = "Hello, world!";
  buffer->WriteBytes(test_string, strlen(test_string));

  ASSERT_EQ(buffer->buffer_.size(), 3u);
  EXPECT_EQ(buffer->buffer_[0], 0u);
  EXPECT_STREQ(reinterpret_cast<const char*>(&buffer->buffer_[1]), test_string);
}

TEST(FxtWriterTest, Commit) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  zx::socket socket1, socket2;
  ASSERT_EQ(zx::socket::create(0, &socket1, &socket2), ZX_OK);

  FxtWriter writer(std::move(socket1), loop.dispatcher());
  zx::result<FxtRecordBuffer> buffer = writer.Reserve(42);
  ASSERT_TRUE(buffer.is_ok());

  buffer->WriteWord(1234);
  buffer->Commit();

  std::vector<uint64_t> read_buffer(2);
  size_t actual;
  ASSERT_EQ(socket2.read(0, read_buffer.data(), read_buffer.size() * sizeof(uint64_t), &actual),
            ZX_OK);
  ASSERT_EQ(actual, 2 * sizeof(uint64_t));
  EXPECT_EQ(read_buffer[0], 42u);
  EXPECT_EQ(read_buffer[1], 1234u);
}

TEST(FxtWriterTest, WriteToSocket) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  zx::socket socket1, socket2;
  ASSERT_EQ(zx::socket::create(0, &socket1, &socket2), ZX_OK);
  FxtWriter writer(std::move(socket1), loop.dispatcher());
  zx_status_t status = fxt::WriteThreadRecord(&writer, 0, fxt::Koid(123), fxt::Koid(456));
  ASSERT_EQ(status, ZX_OK);

  std::vector<uint64_t> read_buffer(3);
  size_t actual;
  ASSERT_EQ(socket2.read(0, read_buffer.data(), read_buffer.size() * sizeof(uint64_t), &actual),
            ZX_OK);
  ASSERT_EQ(actual, 3 * sizeof(uint64_t));
  EXPECT_EQ(read_buffer[1], 123u);
  EXPECT_EQ(read_buffer[2], 456u);
}

// Committing records to a full socket must not block, and the buffered records must be delivered
// in order once the reader catches up.
TEST(FxtWriterTest, FullSocketDoesNotBlock) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  zx::socket socket1, socket2;
  ASSERT_EQ(zx::socket::create(0, &socket1, &socket2), ZX_OK);
  const size_t prefill = FillSocket(socket1);
  ASSERT_GT(prefill, 0u);

  FxtWriter writer(std::move(socket1), loop.dispatcher());
  constexpr uint64_t kNumRecords = 100'000;
  for (uint64_t i = 0; i < kNumRecords; i++) {
    WriteTwoWordRecord(writer, /*header=*/i, /*payload=*/~i);
  }
  EXPECT_EQ(writer.buffered_bytes(), kNumRecords * 2 * sizeof(uint64_t));
  EXPECT_EQ(writer.dropped_records(), 0u);

  Discard(socket2, prefill);
  std::vector<uint64_t> words = ReadAllWords(socket2, loop);
  ASSERT_EQ(words.size(), kNumRecords * 2);
  for (uint64_t i = 0; i < kNumRecords; i++) {
    ASSERT_EQ(words[2 * i], i);
    ASSERT_EQ(words[2 * i + 1], ~i);
  }
  EXPECT_EQ(writer.buffered_bytes(), 0u);
}

// Once the buffering limit is reached, whole records are dropped rather than blocking or
// truncating them.
TEST(FxtWriterTest, DropsWholeRecordsOverLimit) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  zx::socket socket1, socket2;
  ASSERT_EQ(zx::socket::create(0, &socket1, &socket2), ZX_OK);
  const size_t prefill = FillSocket(socket1);

  constexpr size_t kRecordSize = 2 * sizeof(uint64_t);
  constexpr size_t kMaxRecords = 10;
  FxtWriter writer(std::move(socket1), loop.dispatcher(), kMaxRecords * kRecordSize);
  for (uint64_t i = 0; i < kMaxRecords + 5; i++) {
    WriteTwoWordRecord(writer, i, ~i);
  }
  EXPECT_EQ(writer.buffered_bytes(), kMaxRecords * kRecordSize);
  EXPECT_EQ(writer.dropped_records(), 5u);

  Discard(socket2, prefill);
  std::vector<uint64_t> words = ReadAllWords(socket2, loop);
  ASSERT_EQ(words.size(), kMaxRecords * 2);
  for (uint64_t i = 0; i < kMaxRecords; i++) {
    EXPECT_EQ(words[2 * i], i);
    EXPECT_EQ(words[2 * i + 1], ~i);
  }
}

// FlushAndClose delivers everything that was buffered before closing the socket.
TEST(FxtWriterTest, FlushAndCloseDrainsBufferedData) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  zx::socket socket1, socket2;
  ASSERT_EQ(zx::socket::create(0, &socket1, &socket2), ZX_OK);
  const size_t prefill = FillSocket(socket1);

  auto writer = std::make_unique<FxtWriter>(std::move(socket1), loop.dispatcher());
  constexpr uint64_t kNumRecords = 1000;
  for (uint64_t i = 0; i < kNumRecords; i++) {
    WriteTwoWordRecord(*writer, i, ~i);
  }
  FxtWriter::FlushAndClose(std::move(writer));

  Discard(socket2, prefill);
  std::vector<uint64_t> words = ReadAllWords(socket2, loop);
  ASSERT_EQ(words.size(), kNumRecords * 2);
  EXPECT_EQ(words.back(), ~(kNumRecords - 1));

  // The writer closed its end once everything was written.
  zx_signals_t observed = 0;
  EXPECT_EQ(socket2.wait_one(ZX_SOCKET_PEER_CLOSED, zx::time::infinite_past(), &observed), ZX_OK);
}

// If the reader goes away, buffered data is discarded and the writer cleans up after itself.
TEST(FxtWriterTest, FlushAndCloseWithPeerClosed) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  zx::socket socket1, socket2;
  ASSERT_EQ(zx::socket::create(0, &socket1, &socket2), ZX_OK);
  FillSocket(socket1);

  auto writer = std::make_unique<FxtWriter>(std::move(socket1), loop.dispatcher());
  WriteTwoWordRecord(*writer, 1, 2);
  ASSERT_GT(writer->buffered_bytes(), 0u);
  FxtWriter::FlushAndClose(std::move(writer));

  socket2.reset();
  // Running the loop delivers PEER_CLOSED to the writer, which then destroys itself. Leak checking
  // (e.g. ASan) catches it if it doesn't.
  EXPECT_EQ(loop.RunUntilIdle(), ZX_OK);
}

}  // namespace profiler
