// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/syslog/cpp/macros.h>
#include <zircon/types.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <rapidjson/document.h>
#include <rapidjson/error/en.h>
#include <rapidjson/pointer.h>

#include "log_decoder.h"

namespace log_decoder {
namespace {

TEST(LogDecoder, DecodesCorrectly) {
  fuchsia_logging::LogBufferBuilder builder(fuchsia_logging::LogSeverity::Info);
  auto buffer = builder.WithMsg("test message").WithFile(__FILE__, __LINE__).Build();
  buffer.WriteKeyValue("tag", "some tag");
  buffer.WriteKeyValue("tag", "some other tag");
  buffer.WriteKeyValue("user property", 5.2);
  std::span<const uint8_t> span = buffer.EndRecord();
  const char* json = fuchsia_decode_log_message_to_json(span.data(), span.size());

  rapidjson::Document d;
  d.Parse(json);
  auto& entry = d[rapidjson::SizeType(0)];
  auto& tags = entry["metadata"]["tags"];
  auto& payload = entry["payload"]["root"];
  auto& keys = payload["keys"];
  ASSERT_EQ(tags[0].GetString(), std::string("some tag"));
  ASSERT_EQ(tags[1].GetString(), std::string("some other tag"));
  ASSERT_EQ(keys["user property"].GetDouble(), 5.2);
  ASSERT_EQ(payload["message"]["value"].GetString(), std::string("test message"));
  fuchsia_free_decoded_log_message(const_cast<char*>(json));
}

int RustStrcmp(CppString rust_string, const char* c_str) {
  size_t c_len = strlen(c_str);
  if (rust_string.inner.len != c_len) {
    return rust_string.inner.len < c_len ? -1 : 1;
  }
  return strncmp(reinterpret_cast<const char*>(rust_string.inner.ptr), c_str,
                 rust_string.inner.len);
}

TEST(LogDecoder, DecodesArchivistArguments) {
  constexpr char kTestMoniker[] = "some_moniker";
  constexpr char kTestUrl[] = "fuchsia-pkg://fuchsia.com/test#test.cm";
  fuchsia_logging::LogBufferBuilder builder(fuchsia_logging::LogSeverity::Info);
  auto buffer = builder.WithMsg("test message").WithFile("test_file", 42).Build();
  std::span<const uint8_t> span = buffer.EndRecord();
  std::vector<uint8_t> new_buffer(span.size() + 4 + 4 + 8 + ((sizeof(kTestMoniker) - 1 + 7) & ~7) +
                                  ((sizeof(kTestUrl) - 1 + 7) & ~7));
  memcpy(new_buffer.data(), span.data(), span.size());
  size_t current_offset = span.size();
  uint32_t moniker_len = sizeof(kTestMoniker) - 1;
  uint32_t url_len = sizeof(kTestUrl) - 1;
  uint64_t rolled_out_logs = 1;
  memcpy(new_buffer.data() + current_offset, &moniker_len, 4);
  current_offset += 4;
  memcpy(new_buffer.data() + current_offset, &url_len, 4);
  current_offset += 4;
  memcpy(new_buffer.data() + current_offset, &rolled_out_logs, 8);
  current_offset += 8;
  memcpy(new_buffer.data() + current_offset, kTestMoniker, moniker_len);
  current_offset += (moniker_len + 7) & ~7;
  memcpy(new_buffer.data() + current_offset, kTestUrl, url_len);
  current_offset += (url_len + 7) & ~7;

  auto messages =
      fuchsia_decode_log_messages_to_struct(new_buffer.data(), current_offset, true, nullptr);
  ASSERT_EQ(messages.messages.len, static_cast<size_t>(1));
  ASSERT_EQ(messages.messages.ptr[0]->tags.len, static_cast<size_t>(1));
  EXPECT_EQ(RustStrcmp(messages.messages.ptr[0]->tags.ptr[0], kTestMoniker), 0);
  EXPECT_EQ(RustStrcmp(messages.messages.ptr[0]->message, "[test_file(42)] test message"), 0);
  fuchsia_free_log_messages(messages);
}

TEST(LogDecoder, HandlesInvalidInput) {
  // A simple invalid byte sequence. A valid log message would have a different structure.
  const uint8_t invalid_data[] = {0xDE, 0xAD, 0xBE, 0xEF};
  auto messages =
      fuchsia_decode_log_messages_to_struct(invalid_data, sizeof(invalid_data), true, nullptr);
  ASSERT_EQ(std::string(messages.error_str), std::string("couldn't parse message: InvalidHeader"));
  fuchsia_free_log_messages(messages);
}

}  // namespace
}  // namespace log_decoder
