// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYSLOG_STRUCTURED_BACKEND_CPP_LOG_CONNECTION_H_
#define LIB_SYSLOG_STRUCTURED_BACKEND_CPP_LOG_CONNECTION_H_

#include <fidl/fuchsia.logger/cpp/fidl.h>
#include <lib/syslog/structured_backend/cpp/log_buffer.h>
#include <lib/zx/result.h>

namespace fuchsia_logging {

class Logger;

namespace internal {

// LogConnection represents a connection to a logger. This will not watch for interest updates.
class LogConnection {
 public:
  // Initializes a connection provided a client end.  This will not retain
  // `client_end`.
  static zx::result<LogConnection> Create(
      fidl::UnownedClientEnd<fuchsia_logger::LogSink> client_end);

  LogConnection() = default;
  LogConnection(zx::socket socket, FlushConfig config)
      : socket_(std::move(socket)), block_if_full_(config.block_if_full) {}

  LogConnection(LogConnection&&) = default;
  LogConnection& operator=(LogConnection&&) = default;

  zx::socket& socket() { return socket_; }
  bool IsValid() const { return socket_.is_valid(); }

  // Flushes the LogBuffer to the connection.
  zx::result<> FlushBuffer(LogBuffer& buffer) const { return FlushSpan(buffer.EndRecord()); }

 private:
  friend class fuchsia_logging::Logger;

  zx::result<> FlushSpan(cpp20::span<const uint8_t> data) const;

  zx::socket socket_;
  bool block_if_full_ = false;
};

}  // namespace internal

}  // namespace fuchsia_logging

#endif  // LIB_SYSLOG_STRUCTURED_BACKEND_CPP_LOG_CONNECTION_H_
