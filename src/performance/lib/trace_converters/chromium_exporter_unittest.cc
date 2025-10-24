// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/performance/lib/trace_converters/chromium_exporter.h"

#include <fstream>
#include <sstream>
#include <vector>

#include <gtest/gtest.h>
#include <trace-reader/records.h>

namespace {

TEST(ChromiumExporterTest, ValidUtf8) {
  trace::EventData data(trace::EventData::Instant{trace::EventScope::kGlobal});
  std::vector<trace::Argument> arguments;
  arguments.push_back(trace::Argument("arg", trace::ArgumentValue::MakeString("foo\xb5\xb3")));
  trace::Record record(trace::Record::Event{1000, trace::ProcessThread(45, 46), "c\342\202at",
                                            "n\301a\205me", std::move(arguments), std::move(data)});

  // Enclosing the exporter in its own scope ensures that its
  // cleanup routines are called by the destructor before the
  // output stream is read. This way, we can obtain the full
  // output rather than a truncated version.
  {
    const std::filesystem::path& output = "/tmp/test-trace-valid-utf8.json";
    tracing::ChromiumExporter exporter(output);
    exporter.ExportRecord(record);
  }

  std::stringstream buffer;

  std::ifstream input("/tmp/test-trace-valid-utf8.json");

  buffer << input.rdbuf();

  EXPECT_EQ(buffer.str(),
            "{\"displayTimeUnit\":\"ns\",\"traceEvents\":[{\"cat\":\"c\uFFFDat\","
            "\"name\":\"n\uFFFDa\uFFFDme\",\"ts\":1.0,\"pid\":45,\"tid\":46,\"ph\":"
            "\"i\",\"s\":\"g\",\"args\":{\"arg\":\"foo\uFFFD\uFFFD\"}}"
            "],\"systemTraceEvents\":{\"type\":\"fuchsia\",\"events\":[]}}");
}

TEST(ChromiumExporterTest, UnknownLargeBlobEventDropped) {
  std::vector<trace::Argument> arguments;
  arguments.push_back(trace::Argument("arg", trace::ArgumentValue::MakeString("foo")));
  static const char blob[] = "some test blob data";
  trace::Record record(trace::LargeRecordData{trace::LargeRecordData::BlobEvent{
      "category",
      "no::UnknownName",
      1000,
      trace::ProcessThread(45, 46),
      std::move(arguments),
      blob,
      sizeof(blob),
  }});

  // Enclosing the exporter in its own scope ensures that its
  // cleanup routines are called by the destructor before the
  // output stream is read. This way, we can obtain the full
  // output rather than a truncated version.
  {
    std::string file_name = "/tmp/test-trace-unknow-blob.json";
    tracing::ChromiumExporter exporter(file_name);
    exporter.ExportRecord(record);
  }

  std::stringstream buffer;

  std::ifstream input("/tmp/test-trace-unknow-blob.json");

  buffer << input.rdbuf();

  EXPECT_EQ(buffer.str(),
            "{\"displayTimeUnit\":\"ns\",\"traceEvents\":["
            "],\"systemTraceEvents\":{\"type\":\"fuchsia\",\"events\":[]}}");
}

TEST(ChromiumExporterTest, UnknownLargeBlobAttachmentDropped) {
  static const char blob[] = "some test blob data";
  trace::Record record(trace::LargeRecordData{trace::LargeRecordData::BlobAttachment{
      "category",
      "no::UnknownName",
      blob,
      sizeof(blob),
  }});

  // Enclosing the exporter in its own scope ensures that its
  // cleanup routines are called by the destructor before the
  // output stream is read. This way, we can obtain the full
  // output rather than a truncated version.
  {
    const std::filesystem::path& output = "/tmp/test-trace-dropped-blob.json";
    tracing::ChromiumExporter exporter(output);
    exporter.ExportRecord(record);
  }
  std::stringstream buffer;

  std::ifstream input("/tmp/test-trace-dropped-blob.json");

  buffer << input.rdbuf();

  EXPECT_EQ(buffer.str(),
            "{\"displayTimeUnit\":\"ns\",\"traceEvents\":["
            "],\"systemTraceEvents\":{\"type\":\"fuchsia\",\"events\":[]}}");
}

TEST(ChromiumExporterTest, FidlBlobExported) {
  static const char blob[] = "some test blob data";
  trace::Record record(trace::LargeRecordData{trace::LargeRecordData::BlobEvent{
      "fidl:blob",
      "BlobName",
      1000,
      trace::ProcessThread(45, 46),
      std::vector<trace::Argument>(),
      blob,
      sizeof(blob),
  }});

  // Enclosing the exporter in its own scope ensures that its
  // cleanup routines are called by the destructor before the
  // output stream is read. This way, we can obtain the full
  // output rather than a truncated version.
  {
    const std::filesystem::path& file_name = "/tmp/test-trace-fidl-blob.json";
    tracing::ChromiumExporter exporter(file_name);
    exporter.ExportRecord(record);
  }

  std::stringstream buffer;

  std::ifstream input("/tmp/test-trace-fidl-blob.json");

  buffer << input.rdbuf();

  EXPECT_EQ(buffer.str(),
            "{\"displayTimeUnit\":\"ns\",\"traceEvents\":[{\"ph\":\"O\",\"id\":\"\",\"cat\":\"fidl:"
            "blob\",\"name\":\"BlobName\",\"ts\":1.0,\"pid\":45,\"tid\":46,\"blob\":"
            "\"c29tZSB0ZXN0IGJsb2IgZGF0YQA=\"}],\"systemTraceEvents\":{\"type\":\"fuchsia\","
            "\"events\":[]}}");
}

TEST(ChromiumExporterTest, EmptyTrace) {
  // Enclosing the exporter in its own scope ensures that its
  // cleanup routines are called by the destructor before the
  // output stream is read. This way, we can obtain the full
  // output rather than a truncated version.
  {
    const std::filesystem::path& file_name = "/tmp/test-trace-empty-trace.json";
    tracing::ChromiumExporter exporter(file_name);
  }

  std::stringstream buffer;

  std::ifstream input("/tmp/test-trace-empty-trace.json");
  buffer << input.rdbuf();

  EXPECT_EQ(buffer.str(),
            "{\"displayTimeUnit\":\"ns\",\"traceEvents\":["
            "],\"systemTraceEvents\":{\"type\":\"fuchsia\",\"events\":[]}}");
}

}  // namespace
