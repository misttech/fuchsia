// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/syslog/structured_backend/cpp/log_buffer.h>
#include <lib/zx/channel.h>
#include <lib/zx/socket.h>

#include <atomic>
#include <cstring>
#include <thread>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/files/file.h"
#include "src/lib/files/scoped_temp_dir.h"

namespace fuchsia_logging {

TEST(StructuredLogging, Log) {
  FX_LOG_KV(WARNING, "test_log", FX_KV("foo", "bar"));
  constexpr std::string_view kStringView = "string_view";
  FX_LOG_KV(WARNING, "test_log", FX_KV("foo", kStringView));
  const std::string kString = "string";
  FX_LOG_KV(WARNING, "test_log", FX_KV("foo", kString));
  // TODO(https://fxbug.dev/42135333): Figure out how to verify this appropriately.
}

class SideEffectTracker {
 public:
  explicit SideEffectTracker(bool* output) { *output = true; }
  operator int64_t() { return 0; }
};

TEST(StructuredLogging, NoSideEffectsIfLoggingIsDisabled) {
  bool called = false;
  FX_LOG_KV(DEBUG, "test", FX_KV("a", static_cast<int64_t>(SideEffectTracker(&called))));
  ASSERT_FALSE(called);
  FX_LOG_KV(INFO, "test", FX_KV("a", static_cast<int64_t>(SideEffectTracker(&called))));
  ASSERT_TRUE(called);
}

template <typename T>
static std::optional<std::string_view> ToStringView(T input) {
  return input;
}

TEST(StructuredLogging, NullSafeStringView) {
  // Construct from nullptr directly.
  ASSERT_EQ(ToStringView(internal::NullSafeStringView(nullptr)), std::nullopt);
  // Construct from nullptr via const char*.
  ASSERT_EQ(ToStringView(internal::NullSafeStringView(static_cast<const char*>(nullptr))),
            std::nullopt);
  // Construct from std::string
  ASSERT_EQ(ToStringView(internal::NullSafeStringView(std::string("test"))),
            std::string_view("test"));
  // Construct from non-null const char*
  ASSERT_EQ(ToStringView(internal::NullSafeStringView("test")), std::string_view("test"));
}

// Test to validate that SetLogSettings and log initialization is thread-safe.
TEST(StructuredLogging, ThreadInitialization) {
  // TODO(bbosak): Convert to actual stress test.
  auto start = zx_clock_get_monotonic();
  std::atomic_bool running = true;
  std::thread thread_a([&]() {
    while (running) {
      zx::channel temp[2];
      zx::channel::create(0, &temp[0], &temp[1]);
      LogSettingsBuilder builder;
      builder.DisableWaitForInitialInterest().WithLogSink(temp[0].release()).BuildAndInitialize();
    }
  });
  std::thread thread_b([&]() {
    while (running) {
      FX_LOG_KV(WARNING, "test_log", FX_KV("foo", "bar"));
    }
  });
  while (true) {
    auto duration = (zx_clock_get_monotonic() - start);
    if (duration > ZX_SEC(4)) {
      running = false;
      break;
    }

    zx::channel temp[2];
    zx::channel::create(0, &temp[0], &temp[1]);
    LogSettingsBuilder builder;
    builder.DisableWaitForInitialInterest().WithLogSink(temp[0].release()).BuildAndInitialize();
    FX_LOG_KV(WARNING, "test_log", FX_KV("foo", "bar"));
  }
  thread_a.join();
  thread_b.join();
  LogSettingsBuilder builder;
  builder.DisableWaitForInitialInterest().BuildAndInitialize();
}

TEST(StructuredLogging, BackendDirect) {
  {
    LogBufferBuilder builder(LogSeverity::Warn);
    auto buffer =
        builder.WithFile("foo.cc", 42).WithCondition("condition").WithMsg("fake tag").Build();
    buffer.Flush();
  }
  LogBufferBuilder builder(LogSeverity::Warn);
  auto buffer =
      builder.WithFile("foo.cc", 42).WithCondition("condition").WithMsg("fake tag").Build();
  buffer.WriteKeyValue("foo", static_cast<int64_t>(42));
  buffer.WriteKeyValue("bar", true);
  ASSERT_TRUE(buffer.Flush());
  // TODO(https://fxbug.dev/42135333): Figure out how to verify this appropriately.
}

TEST(StructuredLogging, Overflow) {
  std::vector<char> very_large_string;
  very_large_string.resize(1000 * 1000);
  memset(very_large_string.data(), 5, very_large_string.size());
  very_large_string[very_large_string.size() - 1] = 0;
  {
    LogBufferBuilder builder(LogSeverity::Warn);
    auto buffer =
        builder.WithFile("foo.cc", 42).WithCondition("condition").WithMsg("fake tag").Build();
    buffer.Flush();
  }
  LogBufferBuilder builder(LogSeverity::Warn);
  auto buffer =
      builder.WithFile("foo.cc", 42).WithCondition("condition").WithMsg("fake tag").Build();
  buffer.WriteKeyValue("foo", static_cast<int64_t>(42));
  buffer.WriteKeyValue("bar", very_large_string.data());

  ASSERT_FALSE(buffer.Flush());
}

TEST(StructuredLogging, LOGS) {
  std::string str;
  // 5mb log shouldn't crash
  str.resize(1000 * 5000);
  memset(str.data(), 's', str.size() - 1);
  FX_LOGS(INFO) << str;
}

TEST(StructuredLogging, SocketLimit) {
  zx::socket local, remote;
  ASSERT_EQ(zx::socket::create(ZX_SOCKET_DATAGRAM, &local, &remote), ZX_OK);

  std::atomic<bool> writer_finished = false;
  const size_t kNumMessages = 5000;
  constexpr std::string_view kTestMessage = "test message";

  std::thread writer_thread([&]() {
    for (size_t i = 0; i < kNumMessages; ++i) {
      LogBuffer buffer;
      buffer.BeginRecord(LogSeverity::Info, {}, 0, kTestMessage, zx::unowned_socket(remote), 0, 0,
                         0);
      if (!buffer.FlushRecord({.block_if_full = true})) {
        break;
      }
    }
    writer_finished = true;
  });

  // Give the writer a chance to fill the buffer and block.
  zx::nanosleep(zx::deadline_after(zx::msec(250)));
  ASSERT_FALSE(writer_finished.load());

  size_t total_bytes_read = 0;
  std::vector<char> read_buffer(65536);

  // Read from the socket until the writer is done.
  while (!writer_finished.load()) {
    size_t bytes_read;
    zx_status_t status = local.read(0, read_buffer.data(), read_buffer.size(), &bytes_read);
    if (status == ZX_ERR_SHOULD_WAIT) {
      local.wait_one(ZX_SOCKET_READABLE | ZX_SOCKET_PEER_CLOSED, zx::deadline_after(zx::msec(200)),
                     nullptr);
      continue;
    }
    ASSERT_EQ(status, ZX_OK);
    total_bytes_read += bytes_read;
  }

  writer_thread.join();

  // Drain any remaining messages.
  while (true) {
    size_t bytes_read;
    zx_status_t status = local.read(0, read_buffer.data(), read_buffer.size(), &bytes_read);
    if (status == ZX_ERR_SHOULD_WAIT) {
      break;
    }
    if (status != ZX_OK) {
      ASSERT_EQ(status, ZX_ERR_PEER_CLOSED);
      break;
    }
    total_bytes_read += bytes_read;
  }
  // 100KB should far exceed the capacity in a datagram socket.
  EXPECT_GE(total_bytes_read, kNumMessages * kTestMessage.size());
}

}  // namespace fuchsia_logging
