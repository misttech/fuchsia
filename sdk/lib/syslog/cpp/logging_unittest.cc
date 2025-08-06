// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>

#include <functional>
#include <mutex>
#include <optional>
#include <string>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "lib/syslog/cpp/log_level.h"
#ifndef __Fuchsia__
#include "host/encoder.h"
#endif
#include <zircon/types.h>

#include "src/lib/files/file.h"
#include "src/lib/files/scoped_temp_dir.h"
#include "src/lib/uuid/uuid.h"

#ifdef __Fuchsia__
#include <fuchsia/diagnostics/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async/dispatcher.h>
#include <lib/async/wait.h>
#include <lib/fpromise/promise.h>
#include <lib/syslog/structured_backend/cpp/log_connection.h>
#include <lib/zx/socket.h>

#include <cinttypes>

#include <rapidjson/document.h>
#include <src/diagnostics/lib/cpp-log-tester/log_tester.h>

#include "fuchsia/logger/cpp/fidl.h"
#include "src/lib/fxl/strings/join_strings.h"
#include "src/lib/fxl/strings/string_printf.h"
#endif

namespace fuchsia_logging {

namespace {
std::chrono::high_resolution_clock::time_point mock_time = std::chrono::steady_clock::now();
}

namespace {
class LoggingFixture : public ::testing::Test {
 public:
  LoggingFixture() : old_severity_(GetMinLogSeverity()), old_stderr_(dup(STDERR_FILENO)) {}
  ~LoggingFixture() {
    LogSettingsBuilder builder;
    builder.WithMinLogSeverity(old_severity_);
    builder.BuildAndInitialize();
  }

 private:
  RawLogSeverity old_severity_;
  int old_stderr_;
};

using LoggingFixtureDeathTest = LoggingFixture;
#ifdef __Fuchsia__
std::string SeverityToString(const int32_t severity) {
  if (severity == LogSeverity::Trace) {
    return "TRACE";
  } else if (severity == LogSeverity::Debug) {
    return "DEBUG";
  } else if (severity > LogSeverity::Debug && severity < LogSeverity::Info) {
    return fxl::StringPrintf("VLOG(%d)", LogSeverity::Info - severity);
  } else if (severity == LogSeverity::Info) {
    return "INFO";
  } else if (severity == LogSeverity::Warn) {
    return "WARN";
  } else if (severity == LogSeverity::Error) {
    return "ERROR";
  } else if (severity == LogSeverity::Fatal) {
    return "FATAL";
  }
  return "INVALID";
}

std::string Format(const fuchsia::logger::LogMessage& message) {
  return fxl::StringPrintf("[%05d.%03d][%05" PRIu64 "][%05" PRIu64 "][%s] %s: %s\n",
                           static_cast<int>(message.time.get() / 1000000000ULL),
                           static_cast<int>((message.time.get() / 1000000ULL) % 1000ULL),
                           message.pid, message.tid, fxl::JoinStrings(message.tags, ", ").c_str(),
                           SeverityToString(message.severity).c_str(), message.msg.c_str());
}

static std::string RetrieveLogs(zx::channel channel) {
  auto logs = log_tester::RetrieveLogsAsLogMessage(std::move(channel));
  std::stringstream stream;
  for (const auto& log : logs) {
    stream << Format(log);
  }
  return stream.str();
}
#endif

#ifdef __Fuchsia__
using LogState = zx::channel;
static LogState SetupLogs(bool wait_for_initial_interest = true) {
  return log_tester::SetupFakeLog(wait_for_initial_interest);
}

static std::string ReadLogs(zx::channel& remote) { return RetrieveLogs(std::move(remote)); }
#else

struct TestLogState {
  files::ScopedTempDir temp_dir;
  std::string log_file;
};

using LogState = std::unique_ptr<TestLogState>;

static LogState SetupLogs(bool wait_for_initial_interest = true) {
  auto state = std::make_unique<TestLogState>();
  {
    std::string log_file_out;
    state->temp_dir.NewTempFile(&log_file_out);
    state->log_file = std::move(log_file_out);
  }
  LogSettingsBuilder builder;
  builder.WithLogFile(state->log_file);
  builder.BuildAndInitialize();
  return state;
}

static std::string ReadLogs(LogState& state) {
  std::string log;
  files::ReadFileToString(state->log_file, &log);
  return log;
}
#endif

TEST_F(LoggingFixture, Log) {
  LogState state = SetupLogs();

  int error_line = __LINE__ + 1;
  FX_LOGS(ERROR) << "something at error";

  int info_line = __LINE__ + 1;
  FX_LOGS(INFO) << "and some other at info level";

  std::string log = ReadLogs(state);

  EXPECT_THAT(log, testing::HasSubstr("ERROR: [sdk/lib/syslog/cpp/logging_unittest.cc(" +
                                      std::to_string(error_line) + ")] something at error"));

  EXPECT_THAT(log, testing::HasSubstr("INFO: [logging_unittest.cc(" + std::to_string(info_line) +
                                      ")] and some other at info level"));
}

TEST_F(LoggingFixture, LogFirstN) {
  constexpr int kLimit = 5;
  constexpr int kCycles = 20;
  constexpr const char* kLogMessage = "Hello";
  static_assert(kCycles > kLimit);

  LogState state = SetupLogs();

  for (int i = 0; i < kCycles; ++i) {
    FX_LOGS_FIRST_N(ERROR, kLimit) << kLogMessage;
  }

  std::string log = ReadLogs(state);

  int count = 0;
  size_t pos = 0;
  while ((pos = log.find(kLogMessage, pos)) != std::string::npos) {
    ++count;
    ++pos;
  }
  EXPECT_EQ(kLimit, count);
}

TEST_F(LoggingFixture, LogT) {
  LogState state = SetupLogs();

  int error_line = __LINE__ + 1;
  FX_LOGST(ERROR, "first") << "something at error";

  int info_line = __LINE__ + 1;
  FX_LOGST(INFO, "second") << "and some other at info level";

  std::string log = ReadLogs(state);

  EXPECT_THAT(log, testing::HasSubstr("first] ERROR: [sdk/lib/syslog/cpp/logging_unittest.cc(" +
                                      std::to_string(error_line) + ")] something at error"));

  EXPECT_THAT(log,
              testing::HasSubstr("second] INFO: [logging_unittest.cc(" + std::to_string(info_line) +
                                 ")] and some other at info level"));
}

TEST_F(LoggingFixtureDeathTest, CheckFailed) { ASSERT_DEATH(FX_CHECK(false), ""); }

#if defined(__Fuchsia__)
TEST_F(LoggingFixture, Plog) {
  auto remote = log_tester::SetupFakeLog();

  FX_PLOGS(ERROR, ZX_OK) << "should be ok";
  FX_PLOGS(ERROR, ZX_ERR_ACCESS_DENIED) << "got access denied";

  std::string log = RetrieveLogs(std::move(remote));

  EXPECT_THAT(log, testing::HasSubstr("should be ok: 0 (ZX_OK)"));
  EXPECT_THAT(log, testing::HasSubstr("got access denied: -30 (ZX_ERR_ACCESS_DENIED)"));
}

TEST_F(LoggingFixture, PlogT) {
  auto remote = log_tester::SetupFakeLog(false);

  int line1 = __LINE__ + 1;
  FX_PLOGST(ERROR, "abcd", ZX_OK) << "should be ok";

  int line2 = __LINE__ + 1;
  FX_PLOGST(ERROR, "qwerty", ZX_ERR_ACCESS_DENIED) << "got access denied";

  std::string log = RetrieveLogs(std::move(remote));

  EXPECT_THAT(log, testing::HasSubstr("abcd] ERROR: [sdk/lib/syslog/cpp/logging_unittest.cc(" +
                                      std::to_string(line1) + ")] should be ok: 0 (ZX_OK)"));
  EXPECT_THAT(log, testing::HasSubstr("qwerty] ERROR: [sdk/lib/syslog/cpp/logging_unittest.cc(" +
                                      std::to_string(line2) +
                                      ")] got access denied: -30 (ZX_ERR_ACCESS_DENIED)"));
}
#endif  // defined(__Fuchsia__)

TEST_F(LoggingFixture, SLog) {
  LogState state = SetupLogs(false);
  std::string log_id = uuid::Generate();

  int line1 = __LINE__ + 1;
  FX_LOG_KV(ERROR, nullptr, FX_KV("some_msg", "String log"));

  int line2 = __LINE__ + 1;
  FX_LOG_KV(ERROR, nullptr, FX_KV("some_msg", 42));

  int line4 = __LINE__ + 1;
  FX_LOG_KV(ERROR, "msg", FX_KV("first", 42), FX_KV("second", "string"));

  int line5 = __LINE__ + 1;
  FX_LOG_KV(ERROR, "String log");

  int line6 = __LINE__ + 1;
  FX_LOG_KV(ERROR, nullptr, FX_KV("float", 0.25f));

  int line7 = __LINE__ + 1;
  FX_LOG_KV(ERROR, "String with quotes", FX_KV("value", "char is '\"'"));

  std::string log = ReadLogs(state);
  EXPECT_THAT(
      log, testing::HasSubstr("ERROR: [" + std::string("sdk/lib/syslog/cpp/logging_unittest.cc") +
                              "(" + std::to_string(line1) + ")] some_msg=\"String log\""));
  EXPECT_THAT(
      log, testing::HasSubstr("ERROR: [" + std::string("sdk/lib/syslog/cpp/logging_unittest.cc") +
                              "(" + std::to_string(line2) + ")] some_msg=42"));
  EXPECT_THAT(
      log, testing::HasSubstr("ERROR: [" + std::string("sdk/lib/syslog/cpp/logging_unittest.cc") +
                              "(" + std::to_string(line4) + ")] msg first=42 second=\"string\""));
  EXPECT_THAT(
      log, testing::HasSubstr("ERROR: [" + std::string("sdk/lib/syslog/cpp/logging_unittest.cc") +
                              "(" + std::to_string(line5) + ")] String log"));
  EXPECT_THAT(
      log, testing::HasSubstr("ERROR: [" + std::string("sdk/lib/syslog/cpp/logging_unittest.cc") +
                              "(" + std::to_string(line6) + ")] float=0.25"));

  EXPECT_THAT(log, testing::HasSubstr(
                       "ERROR: [" + std::string("sdk/lib/syslog/cpp/logging_unittest.cc") + "(" +
                       std::to_string(line7) + ")] String with quotes value=\"char is '\\\"'\""));
}

TEST_F(LoggingFixture, BackendDirect) {
  LogState state = SetupLogs(false);

  {
    LogBufferBuilder builder(LogSeverity::Error);
    auto buffer =
        builder.WithFile("foo.cc", 42).WithMsg("Log message").WithCondition("condition").Build();
    buffer.WriteKeyValue("tag", "fake tag");
    buffer.Flush();
  }
  LogBufferBuilder builder(LogSeverity::Error);
  auto buffer =
      builder.WithMsg("fake message").WithCondition("condition").WithFile("foo.cc", 42).Build();
  buffer.WriteKeyValue("tag", "fake tag");
  buffer.WriteKeyValue("foo", static_cast<int64_t>(42));
  buffer.Flush();

  std::string log = ReadLogs(state);
  EXPECT_THAT(log,
              testing::HasSubstr("ERROR: [foo.cc(42)] Check failed: condition. Log message\n"));
  EXPECT_THAT(log, testing::HasSubstr(
                       "ERROR: [foo.cc(42)] Check failed: condition. fake message foo=42\n"));
}

TEST_F(LoggingFixture, MacroCompilationTest) {
  uint8_t zero = 0;
  FX_LOG_KV(DEBUG, "test log", FX_KV("key", static_cast<uint16_t>(zero)));
  FX_LOG_KV(DEBUG, "test log", FX_KV("key", static_cast<uint32_t>(zero)));
  FX_LOG_KV(DEBUG, "test log", FX_KV("key", static_cast<uint64_t>(zero)));
  FX_LOG_KV(DEBUG, "test log", FX_KV("key", static_cast<size_t>(zero)));

  FX_LOG_KV(DEBUG, "test log", FX_KV("key", static_cast<int16_t>(zero)));
  FX_LOG_KV(DEBUG, "test log", FX_KV("key", static_cast<int32_t>(zero)));
  FX_LOG_KV(DEBUG, "test log", FX_KV("key", static_cast<int64_t>(zero)));
}

TEST(StructuredLogging, LOGS) {
  std::string str;
  // 5mb log shouldn't crash
  str.resize(1000 * 5000);
  memset(str.data(), 's', str.size() - 1);
  FX_LOGS(INFO) << str;
}

#ifndef __Fuchsia__
TEST(StructuredLogging, Remaining) {
  LogSettingsBuilder builder;
  std::string log_file;
  files::ScopedTempDir temp_dir;
  ASSERT_TRUE(temp_dir.NewTempFile(&log_file));
  builder.WithLogFile(log_file);
  builder.BuildAndInitialize();
  LogBufferBuilder builder2(LogSeverity::Info);
  auto buffer = builder2.WithFile("test", 5).WithMsg("test_msg").Build();
  auto header = internal::MsgHeader::CreatePtr(&buffer);
  auto initial = header->RemainingSpace();
  header->WriteChar('t');
  ASSERT_EQ(header->RemainingSpace(), initial - 1);
  header->WriteString("est");
  ASSERT_EQ(header->RemainingSpace(), initial - 4);
}

TEST(StructuredLogging, FlushAndReset) {
  LogBufferBuilder builder(LogSeverity::Info);
  auto buffer = builder.WithFile("test", 5).WithMsg("test_msg").Build();
  auto header = internal::MsgHeader::CreatePtr(&buffer);
  auto initial = header->RemainingSpace();
  header->WriteString("test");
  ASSERT_EQ(header->RemainingSpace(), initial - 4);
  header->FlushAndReset();
  ASSERT_EQ(header->RemainingSpace(),
            LogBuffer::data_size() - 2);  // last byte reserved for NULL terminator
}
#endif

#ifdef __Fuchsia__

class TestLogSink : public fidl::Server<fuchsia_logger::LogSink> {
 public:
  zx::socket& socket() {
    std::unique_lock lock(mutex_);
    condition_.wait(lock, [this] { return socket_.is_valid(); });
    return socket_;
  }

 private:
  void ConnectStructured(ConnectStructuredRequest& request,
                         ConnectStructuredCompleter::Sync& completer) override {
    std::unique_lock lock(mutex_);
    socket_ = std::move(request.socket());
    condition_.notify_all();
  }

  void WaitForInterestChange(WaitForInterestChangeCompleter::Sync& completer) override { FAIL(); }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_logger::LogSink> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    FAIL();
  }

  std::mutex mutex_;
  std::condition_variable condition_;
  zx::socket socket_;
};

TEST(LogConnection, Basic) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  loop.StartThread();
  zx::channel client, server;
  ASSERT_EQ(zx::channel::create(0, &client, &server), ZX_OK);

  TestLogSink log_sink;
  auto binding = fidl::BindServer(
      loop.dispatcher(), fidl::ServerEnd<fuchsia_logger::LogSink>(std::move(server)), &log_sink);

  auto connection =
      LogConnection::Create(fidl::ClientEnd<fuchsia_logger::LogSink>(std::move(client)));
  ASSERT_EQ(connection.status_value(), ZX_OK);

  ASSERT_TRUE(connection->is_valid());

  LogBuffer buffer;
  buffer.BeginRecord(FUCHSIA_LOG_INFO, {}, {}, "foo", 1, 2, 3);
  ASSERT_EQ(connection->FlushBuffer(buffer).status_value(), ZX_OK);

  uint8_t buf[256];
  size_t actual;
  ASSERT_EQ(log_sink.socket().read(0, buf, std::size(buf), &actual), ZX_OK);

  std::span<const uint8_t> span = buffer.EndRecord();
  ASSERT_EQ(actual, span.size());
  EXPECT_EQ(memcmp(buf, span.data(), actual), 0);
}

TEST(LogConnection, BlockIfFull) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  loop.StartThread();

  zx::channel client, server;
  ASSERT_EQ(zx::channel::create(0, &client, &server), ZX_OK);

  TestLogSink log_sink;
  auto binding = fidl::BindServer(
      loop.dispatcher(), fidl::ServerEnd<fuchsia_logger::LogSink>(std::move(server)), &log_sink);

  auto connection =
      LogConnection::Create(fidl::ClientEnd<fuchsia_logger::LogSink>(std::move(client)));
  ASSERT_EQ(connection.status_value(), ZX_OK);

  ASSERT_TRUE(connection->is_valid());

  // Keep logging and we should eventually get ZX_ERR_SHOULD_WAIT.
  LogBuffer buffer;
  buffer.BeginRecord(FUCHSIA_LOG_INFO, {}, {}, "foo", 1, 2, 3);

  int count = 0;
  for (;;) {
    auto result = connection->FlushBuffer(buffer);
    if (result.is_error()) {
      ASSERT_EQ(result.status_value(), ZX_ERR_SHOULD_WAIT);
      break;
    }
    ++count;
  }

  zx::socket socket;
  ASSERT_EQ(connection->socket().duplicate(ZX_RIGHT_SAME_RIGHTS, &socket), ZX_OK);
  LogConnection connection2(std::move(socket), {.block_if_full = true});

  std::thread thread([&] {
    // Delay reading the socket to make it more likely that we block when flushing the buffer.
    usleep(10000);

    uint8_t buf[256];
    size_t actual;
    for (int i = 0; i < count; ++i) {
      ASSERT_EQ(log_sink.socket().read(0, buf, std::size(buf), &actual), ZX_OK);
    }
  });

  for (int i = 0; i < count; ++i) {
    ASSERT_EQ(connection2.FlushBuffer(buffer).status_value(), ZX_OK);
  }

  thread.join();
}

TEST(LogConnection, EncodingError) {
  zx::socket client, server;
  ASSERT_EQ(zx::socket::create(0, &client, &server), ZX_OK);

  LogConnection connection(std::move(client), {});
  LogBuffer buffer;
  std::string message;
  message.resize(sizeof internal::LogBufferData::data, 'a');

  // This should result in an invalid message because it's too big.
  buffer.BeginRecord(FUCHSIA_LOG_INFO, {}, 0, message, 0, 1, 2);

  EXPECT_EQ(connection.FlushBuffer(buffer).status_value(), ZX_ERR_INVALID_ARGS);
}

#endif

}  // namespace
}  // namespace fuchsia_logging
