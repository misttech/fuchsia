// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_DIAGNOSTICS_FAKE_LOG_SINK_CPP_FAKE_LOG_SINK_H_
#define SRC_LIB_DIAGNOSTICS_FAKE_LOG_SINK_CPP_FAKE_LOG_SINK_H_

#include <fidl/fuchsia.logger/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/diagnostics/reader/cpp/logs.h>

#include <memory>
#include <optional>
#include <vector>

#include <sdk/lib/syslog/cpp/log_level.h>

namespace fuchsia_logging {

// NOTE: This will not safely handle clients which call `ConnectStructured` multiple times. Clients
// that do this can cause deadlocks.
class FakeLogSink {
 public:
  // If `server_end` is not provided, this will connect the global logger to this log sink.
  explicit FakeLogSink(RawLogSeverity severity = Info,
                       fidl::ServerEnd<fuchsia_logger::LogSink> server_end = {});

  FakeLogSink(FakeLogSink&&) = default;
  FakeLogSink& operator=(FakeLogSink&&) = default;

  // Changes the severity and notifies listening clients.
  void SetSeverity(RawLogSeverity severity);

  // Returns a record. This will block until one is available. If an error is encountered the
  // returned buffer will be empty.
  // NOTE: This will only read records from the most recently provided socket.
  std::vector<uint8_t> ReadRecord();

  // Similar to the last, but parses the record as LogsData.
  std::optional<diagnostics::reader::LogsData> ReadLogsData();

  // Returns true if a record is available for reading.
  bool WaitForRecord(zx::time deadline) const;

 private:
  class Impl;

  std::unique_ptr<async::Loop> loop_;
  std::shared_ptr<Impl> impl_;
};

}  // namespace fuchsia_logging

#endif  // SRC_LIB_DIAGNOSTICS_FAKE_LOG_SINK_CPP_FAKE_LOG_SINK_H_
