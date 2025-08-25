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
  fuchsia_logging::LogBufferBuilder builder(fuchsia_logging::LogSeverity::Info);
  auto buffer = builder.WithMsg("test message").WithFile("test_file", 42).Build();
  buffer.WriteKeyValue("$__url", "ignored_value");
  buffer.WriteKeyValue("$__rolled_out", static_cast<uint64_t>(1));
  buffer.WriteKeyValue("$__moniker", kTestMoniker);
  std::span<const uint8_t> span = buffer.EndRecord();
  auto messages = fuchsia_decode_log_messages_to_struct(span.data(), span.size(), true);
  ASSERT_EQ(messages.messages.len, static_cast<size_t>(1));
  ASSERT_EQ(messages.messages.ptr[0]->tags.len, static_cast<size_t>(1));
  EXPECT_EQ(RustStrcmp(messages.messages.ptr[0]->tags.ptr[0], kTestMoniker), 0);
  EXPECT_EQ(RustStrcmp(messages.messages.ptr[0]->message, "[test_file(42)] test message"), 0);
  fuchsia_free_log_messages(messages);
}

}  // namespace
}  // namespace log_decoder
