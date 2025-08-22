// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef EXAMPLES_COMPONENTS_PW_RPC_RUNNER_LOG_PROXY_H_
#define EXAMPLES_COMPONENTS_PW_RPC_RUNNER_LOG_PROXY_H_

#include <lib/syslog/structured_backend/cpp/logger.h>

#include <string>

#include "pw_stream/socket_stream.h"

class LogProxy {
 public:
  LogProxy() = default;
  LogProxy(LogProxy&&) = default;
  LogProxy& operator=(LogProxy&&) = default;
  ~LogProxy() = default;

  // Instantiates a LogProxy that will proxy logs from the pigweed program reachable over the
  // network at `host:port`.
  //
  // Call Detach() to run the LogProxy.
  LogProxy(pw::stream::SocketStream stream, fuchsia_logging::Logger logger)
      : stream_(std::move(stream)), logger_(std::move(logger)) {}

  // Launches the log proxy to run in a separate thread. Consumes *this.
  void Detach();

 private:
  void Run();

  pw::stream::SocketStream stream_;
  fuchsia_logging::Logger logger_;
};

#endif  // EXAMPLES_COMPONENTS_PW_RPC_RUNNER_LOG_PROXY_H_
