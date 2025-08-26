// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <fcntl.h>
#include <lib/syslog/internal/global.h>
#include <lib/syslog/internal/logger.h>
#include <lib/syslog/internal/wire_format.h>
#include <poll.h>
#include <string.h>
#include <unistd.h>

#include <utility>

#include <fbl/string.h>
#include <fbl/string_printf.h>
#include <fbl/unique_fd.h>
#include <zxtest/zxtest.h>

#include "src/lib/diagnostics/fake-log-sink/cpp/fake_log_sink.h"
#include "zircon/system/ulib/syslog/helpers.h"

namespace {

const char* kFileName = syslog::internal::StripPath(__FILE__);
const char* kFilePath = syslog::internal::StripDots(__FILE__);

zx::result<fuchsia_logging::FakeLogSink> init_helper(std::span<const char*> tags = {},
                                                     fx_log_severity_t severity = FX_LOG_INFO) {
  if (auto endpoints = fidl::CreateEndpoints<fuchsia_logger::LogSink>(); endpoints.is_error()) {
    return endpoints.take_error();
  } else {
    fuchsia_logging::FakeLogSink sink(severity, std::move(endpoints->server));
    fx_logger_config_t config = {
        .min_severity = severity,
        .log_sink_channel = endpoints->client.TakeChannel().release(),
        .tags = tags.data(),
        .num_tags = tags.size(),
    };
    if (zx_status_t status = fx_log_reconfigure(&config); status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(std::move(sink));
  }
}

bool ends_with(const char* str, const fbl::String& suffix) {
  size_t str_len = strlen(str);
  size_t suffix_len = suffix.size();
  if (str_len < suffix_len) {
    return false;
  }
  str += str_len - suffix_len;
  return strcmp(str, suffix.c_str()) == 0;
}

void output_compare_helper(fuchsia_logging::FakeLogSink& sink, fx_log_severity_t severity,
                           const char* msg, int line, std::span<const char*> tags = {}) {
  auto message = sink.ReadLogsData();
  ASSERT_TRUE(message);
  ASSERT_EQ(message->metadata().tags.size(), tags.size());
  const char* file = severity > FX_LOG_INFO ? kFilePath : kFileName;
  for (size_t i = 0; i < tags.size(); ++i) {
    ASSERT_EQ(message->metadata().tags[i], tags[i]);
  }
  EXPECT_EQ(message->message(), std::string(msg));
  if (message->metadata().file.has_value()) {
    EXPECT_EQ(message->metadata().file, std::string(file));
  }
  EXPECT_EQ(message->metadata().line, line);
}

TEST(SyslogSocketTests, TestLogSimpleWrite) {
  auto sink = init_helper();
  ASSERT_OK(sink);
  const char* msg = "test message";
  int line = __LINE__ + 1;
  FX_LOG(INFO, nullptr, msg);
  output_compare_helper(*sink, FX_LOG_INFO, msg, line);
}

TEST(SyslogSocketTests, TestLogWrite) {
  auto sink = init_helper();
  ASSERT_OK(sink);
  int line = __LINE__ + 1;
  FX_LOGF(INFO, nullptr, "%d, %s", 10, "just some number");
  output_compare_helper(*sink, FX_LOG_INFO, "10, just some number", line);
}

TEST(SyslogSocketTests, TestLogPreprocessedMessage) {
  auto sink = init_helper();
  ASSERT_OK(sink);
  int line = __LINE__ + 1;
  FX_LOG(INFO, nullptr, "%d, %s");
  output_compare_helper(*sink, FX_LOG_INFO, "%d, %s", line);
}

TEST(SyslogSocketTests, TestLogSeverity) {
  auto sink = init_helper();
  ASSERT_OK(sink);

  FX_LOG_SET_SEVERITY(WARNING);
  FX_LOGF(INFO, nullptr, "%d, %s", 10, "just some number");
  EXPECT_FALSE(sink->WaitForRecord(zx::time::infinite_past()));

  int line = __LINE__ + 1;
  FX_LOGF(WARNING, nullptr, "%d, %s", 10, "just some number");
  output_compare_helper(*sink, FX_LOG_WARNING, "10, just some number", line);
}

TEST(SyslogSocketTests, TestLogWriteWithTag) {
  auto sink = init_helper();
  ASSERT_OK(sink);
  int line = __LINE__ + 1;
  FX_LOGF(INFO, "tag", "%d, %s", 10, "just some string");
  const char* tags[] = {"tag"};
  output_compare_helper(*sink, FX_LOG_INFO, "10, just some string", line, tags);
}

TEST(SyslogSocketTests, TestLogWriteWithGlobalTag) {
  const char* gtags[] = {"gtag"};
  auto sink = init_helper(gtags);
  ASSERT_OK(sink);
  int line = __LINE__ + 1;
  FX_LOGF(INFO, "tag", "%d, %s", 10, "just some string");
  const char* tags[] = {"gtag", "tag"};
  output_compare_helper(*sink, FX_LOG_INFO, "10, just some string", line, tags);
}

TEST(SyslogSocketTests, TestLogWriteWithMultiGlobalTag) {
  const char* gtags[] = {"gtag", "gtag2"};
  auto sink = init_helper(gtags);
  ASSERT_OK(sink);
  int line = __LINE__ + 1;
  FX_LOGF(INFO, "tag", "%d, %s", 10, "just some string");
  const char* tags[] = {"gtag", "gtag2", "tag"};
  output_compare_helper(*sink, FX_LOG_INFO, "10, just some string", line, tags);
}

TEST(SyslogSocketTests, TestGetTags) {
  const char* tags[] = {"gtag", "gTag"};
  auto sink = init_helper(tags);
  ASSERT_OK(sink);
  std::vector<std::string_view> logger_tags;
  fx_logger_get_tags(
      fx_log_get_logger(),
      [](void* context, const char* tag) {
        static_cast<decltype(&logger_tags)>(context)->push_back(tag);
      },
      &logger_tags);
  EXPECT_EQ(logger_tags.size(), std::size(tags));
  EXPECT_EQ(logger_tags[0], tags[0]);
  EXPECT_EQ(logger_tags[1], tags[1]);
}

TEST(SyslogSocketTests, TestLogFallback) {
  const char* gtags[] = {"gtag", "gtag2"};
  auto sink = init_helper(gtags);
  ASSERT_OK(sink);

  int pipefd[2];
  EXPECT_EQ(pipe2(pipefd, O_NONBLOCK), 0);
  fbl::unique_fd fd_to_close1(pipefd[0]);
  fbl::unique_fd fd_to_close2(pipefd[1]);
  fx_logger_activate_fallback(fx_log_get_logger(), pipefd[0]);

  int line = __LINE__ + 1;
  FX_LOGF(INFO, "tag", "%d, %s", 10, "just some string");

  char buf[256];
  size_t n = read(pipefd[1], buf, sizeof(buf));
  EXPECT_GT(n, 0u);
  buf[n] = 0;
  EXPECT_TRUE(
      ends_with(buf, fbl::StringPrintf("[gtag, gtag2, tag] INFO: [%s(%d)] 10, just some string\n",
                                       kFileName, line)),
      "%s", buf);
}

TEST(SyslogSocketTests, TestVlogSimpleWrite) {
  auto sink = init_helper({}, 1);  // 1 is INFO-1
  ASSERT_OK(sink);
  const char* msg = "test message";
  int line = __LINE__ + 1;
  FX_VLOG(1, nullptr, msg);
  output_compare_helper(*sink, (FX_LOG_INFO - 1), msg, line);
}

TEST(SyslogSocketTests, TestWriteWithNullptrFile) {
  // Ensure that we support nullptr filenames. See b/350577005 for justification.
  auto sink = init_helper();
  ASSERT_OK(sink);
  const char* msg = "test message";
  fx_logger_t* logger = fx_log_get_logger();
  fx_logger_log(logger, FX_LOG_INFO, nullptr, msg);
  output_compare_helper(*sink, FX_LOG_INFO, msg, 0);
}

TEST(SyslogSocketTests, TestVlogWrite) {
  auto sink = init_helper({}, 1);  // 1 is INFO-1
  ASSERT_OK(sink);
  int line = __LINE__ + 1;
  FX_VLOGF(1, nullptr, "%d, %s", 10, "just some number");
  output_compare_helper(*sink, (FX_LOG_INFO - 1), "10, just some number", line);
}

TEST(SyslogSocketTests, TestVlogWriteWithTag) {
  auto sink = init_helper({}, 5);  // INFO-5
  ASSERT_OK(sink);
  int line = __LINE__ + 1;
  FX_VLOGF(5, "tag", "%d, %s", 10, "just some string");
  const char* tags[] = {"tag"};
  output_compare_helper(*sink, (FX_LOG_INFO - 5), "10, just some string", line, tags);
}

TEST(SyslogSocketTests, TestLogVerbosity) {
  auto sink = init_helper();
  ASSERT_OK(sink);

  FX_VLOGF(10, nullptr, "%d, %s", 10, "just some number");
  EXPECT_FALSE(sink->WaitForRecord(zx::time::infinite_past()));

  FX_VLOGF(1, nullptr, "%d, %s", 10, "just some number");
  EXPECT_FALSE(sink->WaitForRecord(zx::time::infinite_past()));

  FX_LOG_SET_VERBOSITY(1);  // INFO - 1
  int line = __LINE__ + 1;
  FX_VLOGF(1, nullptr, "%d, %s", 10, "just some number");
  output_compare_helper(*sink, (FX_LOG_INFO - 1), "10, just some number", line);
}

TEST(SyslogSocketTests, TestLogReconfiguration) {
  // Initialize with no tags.
  auto sink = init_helper();
  ASSERT_OK(sink);
  int line = __LINE__ + 1;
  FX_LOG(INFO, NULL, "Hi");
  output_compare_helper(*sink, FX_LOG_INFO, "Hi", line);

  // Now reconfigure the logger and add tags.
  const char* tags[] = {"tag1", "tag2"};

  auto new_sink = init_helper(tags);
  ASSERT_OK(new_sink);

  line = __LINE__ + 1;
  FX_LOG(INFO, NULL, "Hi");
  output_compare_helper(*new_sink, FX_LOG_INFO, "Hi", line, tags);
}

}  // namespace
