// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "sdk/lib/syslog/cpp/logging_backend_fuchsia_globals.h"

#include "sdk/lib/syslog/structured_backend/cpp/log_connection.h"
#include "sdk/lib/syslog/structured_backend/fuchsia_syslog.h"

#ifndef _ALL_SOURCE
#define _ALL_SOURCE  // To get MTX_INIT
#endif

#include <lib/async/default.h>
#include <lib/zx/result.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>
#include <zircon/process.h>
#include <zircon/syscalls.h>

#include <condition_variable>
#include <mutex>

#include "lib/component/incoming/cpp/protocol.h"

namespace fuchsia_logging::internal {

class LogState;

class Logger {
 public:
  static zx::result<std::unique_ptr<Logger>> Create(bool global, const LogSettings* settings) {
    auto logger = std::make_unique<Logger>();
    if (zx::result<> result = logger->Init(global, settings ? *settings : LogSettings());
        result.is_error()) {
      return result.take_error();
    }
    return zx::ok(std::move(logger));
  }

  static Logger& GlobalInstance(const LogSettings* settings = nullptr) {
    bool called = false;
    static Logger* global_logger = [&] {
      zx::result logger = Logger::Create(true, settings);
      called = true;
      // If there's an error, just return a placeholder logger that will just drop logs.
      return logger.is_ok() ? (*logger).release() : new Logger;
    }();
    if (settings && !called) {
      [[maybe_unused]] zx::result result = global_logger->Init(true, *settings);
    }
    return *global_logger;
  }

  Logger() = default;

  zx::result<> Init(bool global, const LogSettings& settings) __TA_NO_THREAD_SAFETY_ANALYSIS {
    std::unique_lock lock(mutex_);

    default_severity_ = settings.min_log_level;
    if (global) {
      set_min_severity(default_severity_);
    } else {
      min_severity_ = default_severity_;
    }

    fidl::ClientEnd<fuchsia_logger::LogSink> client_end;
    if (settings.log_sink == ZX_HANDLE_INVALID) {
      auto connect_result = component::Connect<fuchsia_logger::LogSink>();
      if (connect_result.is_error()) {
        return connect_result.take_error();
      }
      client_end = *std::move(connect_result);
    } else {
      client_end = fidl::ClientEnd<fuchsia_logger::LogSink>(zx::channel(settings.log_sink));
    }

    auto connection = LogConnection::Create(client_end);
    if (connection.is_error()) {
      return connection.take_error();
    }
    connection_ = *std::move(connection);

    tags_.clear();
    tags_.reserve(settings.tags_count);
    for (size_t i = 0; i < settings.tags_count; ++i) {
      tags_.push_back(std::string(settings.tags[i]));
    }

    async_dispatcher_t* dispatcher = settings.dispatcher
                                         ? static_cast<async_dispatcher_t*>(settings.dispatcher)
                                         : async_get_default_dispatcher();

    log_sink_ = {};
    ++sequence_;

    if (dispatcher && settings.interest_listener_behavior != kInterestListenerDisabled) {
      // Wait for any existing callback to finish before switching to a new one.
      condition_.wait(lock, [this]() __TA_REQUIRES(mutex_) { return !notification_pending_; });

      on_severity_changed_ =
          settings.severity_change_callback ? settings.severity_change_callback : +[](uint8_t) {};

      log_sink_.Bind(std::move(client_end), dispatcher);

      if (settings.interest_listener_behavior == kInterestListenerEnabled) {
        auto interest_result = log_sink_.sync()->WaitForInterestChange();
        uint64_t sequence = sequence_;
        lock.unlock();
        HandleInterestChange(sequence, interest_result);
      } else {
        PollInterest(sequence_);
      }
    }

    return zx::ok();
  }

  uint8_t min_severity() const { return min_severity_ref().load(std::memory_order_relaxed); }

  zx::result<> FlushSpan(cpp20::span<const uint8_t> span) const {
    std::scoped_lock lock(mutex_);
    return connection_.FlushSpan(span);
  }

  void ForEachTag(void* context, void (*callback)(void* context, const char* tag)) const {
    std::scoped_lock lock(mutex_);
    for (const std::string& tag : tags_) {
      callback(context, tag.c_str());
    }
  }

  static std::atomic<uint8_t>& GlobalMinSeverity() {
    static std::atomic<uint8_t> min_severity = FUCHSIA_LOG_INFO;
    return min_severity;
  }

 private:
  const std::atomic<uint8_t>& min_severity_ref() const {
    return min_severity_ ? *min_severity_ : GlobalMinSeverity();
  }
  std::atomic<uint8_t>& min_severity_ref() {
    return min_severity_ ? *min_severity_ : GlobalMinSeverity();
  }

  void set_min_severity(uint8_t severity) {
    min_severity_ref().store(severity, std::memory_order_relaxed);
  }

  void PollInterest(uint64_t sequence) __TA_REQUIRES(mutex_) {
    log_sink_->WaitForInterestChange().Then(
        [this, sequence](const fidl::BaseWireResult<fuchsia_logger::LogSink::WaitForInterestChange>&
                             interest_result) { HandleInterestChange(sequence, interest_result); });
  }

  void HandleInterestChange(
      uint64_t sequence,
      const fidl::BaseWireResult<fuchsia_logger::LogSink::WaitForInterestChange>& interest_result)
      __TA_EXCLUDES(mutex_) {
    // FIDL can cancel the operation if the logger is being reconfigured
    // which results in an error.
    if (!interest_result.ok()) {
      return;
    }
    void (*on_severity_changed)(uint8_t severity);
    uint8_t new_severity;
    {
      std::scoped_lock lock(mutex_);
      if (sequence_ != sequence) {
        // The settings have been updated; ignore.
        return;
      }
      const auto& interest = interest_result->value()->data;
      new_severity = interest.has_min_severity() ? static_cast<uint8_t>(interest.min_severity())
                                                 : default_severity_;
      set_min_severity(new_severity);
      notification_pending_ = true;
      on_severity_changed = on_severity_changed_;
    }
    // Call the callback whilst not holding the lock in case it is reentrant.
    on_severity_changed(new_severity);
    {
      std::scoped_lock lock(mutex_);
      notification_pending_ = false;
      if (sequence == sequence_) {
        PollInterest(sequence);
      }
    }
    condition_.notify_all();
  }

  mutable std::mutex mutex_;
  std::condition_variable condition_;

  uint8_t default_severity_ __TA_GUARDED(mutex_) = FUCHSIA_LOG_INFO;
  LogConnection connection_ __TA_GUARDED(mutex_);
  std::vector<std::string> tags_ __TA_GUARDED(mutex_);

  // If this isn't set, this is for the global logger.
  std::optional<std::atomic<uint8_t>> min_severity_;

  fidl::WireSharedClient<fuchsia_logger::LogSink> log_sink_ __TA_GUARDED(mutex_);
  void (*on_severity_changed_)(uint8_t severity) __TA_GUARDED(mutex_) = nullptr;
  uint64_t sequence_ __TA_GUARDED(mutex_) = 0;

  // If true, indicates that the dispatcher is currently calling `on_severity_changed_`.
  bool notification_pending_ __TA_GUARDED(mutex_) = false;
};

namespace {

LogState* state = nullptr;
std::mutex state_lock;
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
// FuchsiaLogSetStateLocked, FuchsiaLogAcquireState, FuchsiaLogReleaseState and
// FuchsiaLogGetStateLocked can can be removed once the API level mentioned above and all preceding
// levels, have been retired.
#endif

__EXPORT
void FuchsiaLogSetStateLocked(LogState* new_state) { state = new_state; }

__EXPORT void FuchsiaLogAcquireState() __TA_NO_THREAD_SAFETY_ANALYSIS { return state_lock.lock(); }

__EXPORT void FuchsiaLogReleaseState() __TA_NO_THREAD_SAFETY_ANALYSIS {
  return state_lock.unlock();
}

__EXPORT
LogState* FuchsiaLogGetStateLocked() { return state; }

#if FUCHSIA_API_LEVEL_AT_LEAST(NEXT)

__EXPORT zx_status_t FuchsiaLogCreateLogger(const LogSettings* settings, Logger** logger_out) {
  if (auto logger = Logger::Create(false, settings); logger.is_error()) {
    return logger.status_value();
  } else {
    *logger_out = (*logger).release();
    return ZX_OK;
  }
}

__EXPORT void FuchsiaLogDestroyLogger(Logger* l) { std::unique_ptr<Logger> logger(l); }

__EXPORT
uint8_t FuchsiaLogGetMinSeverity(const Logger* logger) { return logger->min_severity(); }

__EXPORT
void FuchsiaLogInitGlobalLogger(const LogSettings* settings) { Logger::GlobalInstance(settings); }

__EXPORT uint8_t FuchsiaLogGetGlobalMinSeverity() {
  return Logger::GlobalMinSeverity().load(std::memory_order_relaxed);
}

__EXPORT
Logger* FuchsiaLogGetGlobalLogger() { return &Logger::GlobalInstance(); }

__EXPORT
zx_status_t FuchsiaLogWrite(const Logger* logger, const void* buffer, size_t len) {
  return logger->FlushSpan(cpp20::span(static_cast<const uint8_t*>(buffer), len)).status_value();
}

__EXPORT
void FuchsiaLogForEachTag(const Logger* logger, void* context,
                          void (*callback)(void* context, const char* tag)) {
  logger->ForEachTag(context, callback);
}

#endif  // FUCHSIA_API_LEVEL_AT_LEAST(NEXT)

}  // extern "C"

}  // namespace fuchsia_logging::internal
