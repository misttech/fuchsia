// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/syslog/cpp/logging_backend_fuchsia_globals.h"

#include <lib/zx/result.h>
#include <zircon/assert.h>
#include <zircon/availability.h>
#include <zircon/compiler.h>
#include <zircon/process.h>
#include <zircon/syscalls.h>

#include <atomic>
#include <mutex>

#include "sdk/lib/syslog/structured_backend/cpp/log_connection.h"
#include "sdk/lib/syslog/structured_backend/cpp/logger.h"
#include "sdk/lib/syslog/structured_backend/cpp/raw_log_settings.h"
#include "sdk/lib/syslog/structured_backend/fuchsia_syslog.h"

namespace fuchsia_logging::internal {

#if FUCHSIA_API_LEVEL_LESS_THAN(NEXT)
class LogState;
#endif

namespace {

// This thread's koid.
// Initialized on first use.
thread_local zx_koid_t tls_thread_koid{ZX_KOID_INVALID};

zx_koid_t GetKoid(zx_handle_t handle) {
  zx_info_handle_basic_t info;
  zx_status_t status =
      zx_object_get_info(handle, ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  return status == ZX_OK ? info.koid : ZX_KOID_INVALID;
}

}  // namespace

// This is internal::Logger which is distinct from the public one. This is currently only used for
// the global logger.
class Logger {
 public:
  static void InitGlobalInstance(const RawLogSettings& settings) {
    bool called = false;
    std::call_once(once_, [&called, settings]() {
      called = true;
      if (auto logger = fuchsia_logging::Logger::Create(settings, &min_severity_); logger.is_ok()) {
        global_instance_ = new Logger(*std::move(logger));
      } else {
        global_instance_ = new Logger;
      }
    });
    if (!called) {
      auto logger = fuchsia_logging::Logger::Create(settings, &min_severity_);
      std::scoped_lock lock(global_instance_->mutex_);
      if (logger.is_ok()) {
        global_instance_->logger_ = *std::move(logger);
      } else {
        global_instance_->logger_ = {};
      }
    }
  }

  static Logger& GlobalInstance(RawLogSettings (*get_default_settings)()) {
    std::call_once(once_, [get_default_settings] {
      if (auto logger = fuchsia_logging::Logger::Create(get_default_settings(), &min_severity_);
          logger.is_ok()) {
        global_instance_ = new Logger(*std::move(logger));
      } else {
        global_instance_ = new Logger;
      }
    });
    return *global_instance_;
  }

  zx::result<> FlushSpan(cpp20::span<const uint8_t> span) const { return logger().FlushSpan(span); }

  template <typename T>
  void ForEachTag(T callback) const {
    logger().ForEachTag(callback);
  }

  static FuchsiaLogSeverity min_severity() { return min_severity_.load(std::memory_order_relaxed); }

 private:
  Logger() = default;
  explicit Logger(fuchsia_logging::Logger logger) : logger_(std::move(logger)) {}

  fuchsia_logging::Logger logger() const {
    std::scoped_lock lock(mutex_);
    return logger_;
  }

  static std::once_flag once_;
  static Logger* global_instance_;
  static std::atomic<FuchsiaLogSeverity> min_severity_;

  mutable std::mutex mutex_;
  fuchsia_logging::Logger logger_ __TA_GUARDED(mutex_);
};

std::once_flag Logger::once_;
Logger* Logger::global_instance_;
std::atomic<FuchsiaLogSeverity> Logger::min_severity_;

extern "C" {

__EXPORT
zx_koid_t FuchsiaLogGetCurrentThreadKoid() {
  if (unlikely(tls_thread_koid == ZX_KOID_INVALID)) {
    tls_thread_koid = GetKoid(zx_thread_self());
  }
  ZX_DEBUG_ASSERT(tls_thread_koid != ZX_KOID_INVALID);
  return tls_thread_koid;
}

#if FUCHSIA_API_LEVEL_LESS_THAN(NEXT)

// Retained as no-op stubs so frozen .ifs goldens for API levels 29-33 continue to match.
// These were only declared at API levels < 29 and can be removed once API level 33 is retired.
__EXPORT void FuchsiaLogSetStateLocked(LogState* new_state) {}
__EXPORT void FuchsiaLogAcquireState() {}
__EXPORT void FuchsiaLogReleaseState() {}
__EXPORT LogState* FuchsiaLogGetStateLocked() { return nullptr; }

#endif  // FUCHSIA_API_LEVEL_LESS_THAN(NEXT)

__EXPORT
void FuchsiaLogInitGlobalLogger(const RawLogSettings* settings) {
  Logger::InitGlobalInstance(*settings);
}

__EXPORT uint8_t FuchsiaLogGetGlobalMinSeverity() { return Logger::min_severity(); }

__EXPORT
Logger* FuchsiaLogGetGlobalLogger(RawLogSettings (*get_default_settings)()) {
  return &Logger::GlobalInstance(get_default_settings);
}

__EXPORT
zx_status_t FuchsiaLogWrite(const Logger* logger, const void* buffer, size_t len) {
  return logger->FlushSpan(cpp20::span(static_cast<const uint8_t*>(buffer), len)).status_value();
}

__EXPORT
void FuchsiaLogForEachTag(const Logger* logger, void* context,
                          void (*callback)(void* context, const char* tag)) {
  logger->ForEachTag(
      [callback, context](const std::string& tag) { callback(context, tag.c_str()); });
}

}  // extern "C"

}  // namespace fuchsia_logging::internal
