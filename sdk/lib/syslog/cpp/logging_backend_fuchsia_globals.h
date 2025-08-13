// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYSLOG_CPP_LOGGING_BACKEND_FUCHSIA_GLOBALS_H_
#define LIB_SYSLOG_CPP_LOGGING_BACKEND_FUCHSIA_GLOBALS_H_

#include <zircon/availability.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

namespace fuchsia_logging::internal {

// These *must* be ABI stable.
inline constexpr uint8_t kInterestListenerDisabled = 0;
inline constexpr uint8_t kInterestListenerEnabledNonBlocking = 1;
inline constexpr uint8_t kInterestListenerEnabled = 2;

inline constexpr uint8_t kDefaultMinLogLevel = 0x30;  // Default to Info.

// This *must* be ABI stable.
struct LogSettings {
  uint8_t min_log_level = kDefaultMinLogLevel;
  uint8_t interest_listener_behavior = kInterestListenerEnabled;
  zx_handle_t log_sink = ZX_HANDLE_INVALID;
  const char** tags = nullptr;
  size_t tags_count = 0;
  void* dispatcher = nullptr;
  void (*severity_change_callback)(uint8_t severity) = nullptr;
  uint64_t reserved[7] = {};
};

// Prevent surprises...
static_assert(sizeof(LogSettings) == 96);

// These functions are an internal contract between the Fuchsia logging backend and the logging
// state shared library. API users should not call these directly, but they need to be exported to
// allow for global state management of logs within a single process.

extern "C" {

// Returns the current thread's koid.
zx_koid_t FuchsiaLogGetCurrentThreadKoid();

#if FUCHSIA_API_LEVEL_LESS_THAN(NEXT)

class LogState;

// Acquires the state lock.
void FuchsiaLogAcquireState();

// Updates the log state, requires that the state lock is held.
void FuchsiaLogSetStateLocked(LogState* new_state);

// Releases the state lock.
void FuchsiaLogReleaseState();

// Returns the current log state.
LogState* FuchsiaLogGetStateLocked();

#else

class Logger;

// Initializes the global logger. This is safe to call multiple times.
void FuchsiaLogInitGlobalLogger(const LogSettings*);

// Returns the global minimum severity.
uint8_t FuchsiaLogGetGlobalMinSeverity();

// Returns the global logger. This will create a logger with default settings if one does not
// already exist.
Logger* FuchsiaLogGetGlobalLogger();

// Writes a log record to a logger. The buffer should be in the appropriate diagnostics record
// format.
zx_status_t FuchsiaLogWrite(const Logger* logger, const void* buffer, size_t len);

// Calls `callback` for each tag for the logger.
void FuchsiaLogForEachTag(const Logger* logger, void* context,
                          void (*callback)(void* context, const char* tag));

#endif

}  // extern "C"
}  // namespace fuchsia_logging::internal

#endif  // LIB_SYSLOG_CPP_LOGGING_BACKEND_FUCHSIA_GLOBALS_H_
