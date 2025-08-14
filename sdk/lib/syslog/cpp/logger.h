// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYSLOG_CPP_LOGGER_H_
#define LIB_SYSLOG_CPP_LOGGER_H_

#include <zircon/availability.h>

#if FUCHSIA_API_LEVEL_AT_LEAST(NEXT)

#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/logging_backend_fuchsia_globals.h>
#include <lib/syslog/structured_backend/cpp/log_buffer.h>
#include <lib/zx/result.h>

namespace fuchsia_logging {

class Logger {
 public:
  static zx::result<Logger> Create(const LogSettings& settings);

  Logger() = default;
  Logger(Logger&&);
  Logger& operator=(Logger&&);
  ~Logger();

  LogSeverity GetMinimumSeverity() const;
  zx::result<> FlushBuffer(LogBuffer& buffer) const;

  template <typename T>
  void ForEachTag(T callback) const {
    internal::FuchsiaLogForEachTag(
        logger_, &callback, +[](void* c, const char* tag) {
          T* callback = reinterpret_cast<T*>(c);
          (*callback)(tag);
        });
  }

 private:
  explicit Logger(internal::Logger* logger) : logger_(logger) {}

  internal::Logger* logger_ = nullptr;
};

}  // namespace fuchsia_logging

#endif

#endif  // LIB_SYSLOG_CPP_LOGGER_H_
