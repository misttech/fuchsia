// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/syslog/cpp/logger.h"

#if FUCHSIA_API_LEVEL_AT_LEAST(NEXT)

#include <lib/syslog/cpp/log_settings_internal.h>

namespace fuchsia_logging {

zx::result<Logger> Logger::Create(const LogSettings& settings) {
  internal::Logger* logger;
  if (zx_status_t status =
          internal::WithInternalSettings(settings,
                                         [&](const internal::LogSettings& settings) {
                                           return FuchsiaLogCreateLogger(&settings, &logger);
                                         });
      status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(Logger(logger));
}

Logger::Logger(Logger&& other) : logger_(other.logger_) { other.logger_ = nullptr; }

Logger& Logger::operator=(Logger&& other) {
  if (this == &other) {
    return *this;
  }
  if (logger_) {
    internal::FuchsiaLogDestroyLogger(logger_);
  }
  logger_ = other.logger_;
  other.logger_ = nullptr;
  return *this;
}

Logger::~Logger() {
  if (logger_) {
    internal::FuchsiaLogDestroyLogger(logger_);
  }
}

LogSeverity Logger::GetMinimumSeverity() const {
  return static_cast<LogSeverity>(internal::FuchsiaLogGetMinSeverity(logger_));
}

zx::result<> Logger::FlushBuffer(LogBuffer& buffer) const {
  auto span = buffer.EndRecord();
  if (span.empty()) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::make_result(internal::FuchsiaLogWrite(logger_, span.data(), span.size()));
}

}  // namespace fuchsia_logging

#endif
