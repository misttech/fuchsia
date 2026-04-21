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

int RustStrcmp(CPPArray<uint8_t> rust_string, const char* c_str) {
  return strncmp(reinterpret_cast<const char*>(rust_string.ptr), c_str, rust_string.len);
}

TEST(LogDecoder, DecodesArchivistArguments) {
  constexpr char kTestMoniker[] = "some_moniker";
  constexpr char kTestUrl[] = "fuchsia-pkg://fuchsia.com/test#test.cm";

  // 1. Build Manifest Record
  fuchsia_logging::LogBufferBuilder manifest_builder(fuchsia_logging::LogSeverity::Info);
  auto manifest_buffer = manifest_builder.Build();
  manifest_buffer.WriteKeyValue("moniker", kTestMoniker);
  manifest_buffer.WriteKeyValue("url", kTestUrl);
  std::span<const uint8_t> manifest_span = manifest_buffer.EndRecord();

  // 2. Build Log Record
  fuchsia_logging::LogBufferBuilder log_builder(fuchsia_logging::LogSeverity::Info);
  auto log_buffer = log_builder.WithMsg("test message").WithFile("test_file", 42).Build();
  std::span<const uint8_t> log_span = log_buffer.EndRecord();

  // 3. Concatenate and Flip Bit
  std::vector<uint8_t> new_buffer(manifest_span.size() + log_span.size());
  memcpy(new_buffer.data(), manifest_span.data(), manifest_span.size());
  memcpy(new_buffer.data() + manifest_span.size(), log_span.data(), log_span.size());

  // Flip LOG_CONTROL_BIT (bit 47) in header of first record
  uint64_t* header_ptr = reinterpret_cast<uint64_t*>(new_buffer.data());
  *header_ptr |= (1ULL << 47);

  auto messages = fuchsia_decode_log_messages_to_struct(new_buffer.data(), new_buffer.size(), true);
  ASSERT_EQ(messages.messages.len, static_cast<size_t>(1));
  ASSERT_EQ(messages.messages.ptr[0]->tags.len, static_cast<size_t>(1));
  EXPECT_EQ(RustStrcmp(messages.messages.ptr[0]->tags.ptr[0], kTestMoniker), 0);
  EXPECT_EQ(RustStrcmp(messages.messages.ptr[0]->message, "[test_file(42)] test message"), 0);
  fuchsia_free_log_messages(messages);
}

TEST(LogDecoder, HandlesInvalidInput) {
  // A simple invalid byte sequence. A valid log message would have a different structure.
  const uint8_t invalid_data[] = {0xDE, 0xAD, 0xBE, 0xEF};
  auto messages = fuchsia_decode_log_messages_to_struct(invalid_data, sizeof(invalid_data), false);
  ASSERT_EQ(messages.state, nullptr);
  ASSERT_EQ(std::string(messages.error_str), std::string("found invalid header"));
  fuchsia_free_decoded_log_message(messages.error_str);
  // fuchsia_free_log_messages should not be called here, as the state is null.
}

}  // namespace
}  // namespace log_decoder
