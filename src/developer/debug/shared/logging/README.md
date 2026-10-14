<!--* freshness: { reviewed: '2026-10-12' } *-->

# Shared Debugger Logging (`logging`)

## Overview

`//src/developer/debug/shared/logging` provides the logging infrastructure
shared by `zxdb` and `debug_agent`. It supports severity-based general logging
with pluggable backends, category-filtered hierarchical debug logging with
duration tracking, and source-location capture.

## Interface

Public headers expose these macros, enums, and classes:

- `LOGS(severity)`, `DEBUG_LOG(category)`, `DEBUG_LOG_WITH_LOCATION(category,
  location)`: Macros that emit severity logs and category-filtered scoped debug
  logs (`logging.h`, `debug.h`).
- `FROM_HERE`, `FROM_HERE_NO_FUNC`, `STRINGIFY(x, y)`, `STRINGIFY_INTERNAL(x,
  y)`: Source location and preprocessor stringification macros
  (`file_line_function.h`).
- `debug::LogSeverity`: Severity enum (`kInfo`, `kWarn`, `kError`)
  (`logging.h`).
- `debug::LogStatement`: Scoped log accumulator (`LogStatement`,
  `~LogStatement`, `stream`) (`logging.h`).
- `debug::LogBackend`: Pluggable sink interface for general log statements
  (`WriteLog`, `Set`, `Unset`, `~LogBackend`) (`logging.h`).
- `debug::LogCategory`: Category enum (`debug.h`) for `kAgent`, `kArchArm64`,
  `kArchx64`, `kBreakpoint`, `kJob`, `kMessageLoop`, `kProcess`, `kRemoteAPI`,
  `kSession`, `kSetting`, `kTest`, `kTiming`, `kThread`, and `kWatchpoint`. Also
  includes `kWorkerPool`, `kDebugAdapter`, `kBuildIDIndex`, `kProcessSymbols`,
  `kModuleSymbols`, `kSystemSymbols`, `kDownloadManager`, `kAll`, and `kNone`.
- `debug::SetDebugLogging(bool activate)`, `debug::IsDebugLoggingActive()`,
  `debug::SetLogCategories(std::initializer_list<LogCategory> categories)`:
  Functions that control debug logging activation and active category filters
  (`debug.h`).
- `debug::DebugLogStatement`: Scoped hierarchical debug log entry with duration
  tracking (`DebugLogStatement`, `~DebugLogStatement`, `stream`, `origin`,
  `category`, `start_time`) (`debug.h`).
- `debug::FileLineFunction`: Value type holding source file, line number, and
  function name (`FileLineFunction`, `is_valid`, `file`, `line`, `function`,
  `ToString`, `operator<`, `operator==`, `operator!=`) (`file_line_function.h`).

## Invariants

Callers and implementations must preserve these invariants:

- `FileLineFunction` stores raw `const char*` pointers for file and function
  names without taking ownership. Callers must ensure string literals outlive
  the `FileLineFunction` instance.
- `FileLineFunction::is_valid()` requires a non-null `file_` pointer and `line_
  > 0`.
- `FileLineFunction` comparison operators (`<`, `==`, `!=`) compare the line
  number and `file_` pointer address (not string content) and ignore
  `function_`.
- `DEBUG_LOG_WITH_LOCATION` declares a block-scoped `DebugLogStatement` tied to
  `__LINE__`. Nested debug logs in the same scope form a parent-child tree that
  flushes when the root `DebugLogStatement` destructs.
- A `LogCategory` is active when `fuchsia_logging` `Trace` severity is active,
  or when `Debug` severity is active and the active set contains `kAll` or that
  category.
- `PushLogEntry` and `PopLogEntry` lock `gLogMutex` before mutating the global
  debug log tree. `LogBackend::Set`, `LogBackend::Unset`, and `SetLogCategories`
  update global state without a mutex.

## Side Effects

These logging primitives produce the following global and I/O side effects:

- `LogStatement::~LogStatement()` sends the formatted string to `log_backend`
  via `WriteLog()` and writes to `fuchsia_logging::LogMessage` when
  `log_enable_syslog` is `true`.
- `LogBackend::Set()` and `LogBackend::Unset()` update the module-global
  `log_backend` pointer and `log_enable_syslog` flag.
- `SetDebugLogging()` re-initializes process-wide Fuchsia syslog settings via
  `fuchsia_logging::LogSettingsBuilder::BuildAndInitialize()`.
- `SetLogCategories()` updates the static active `LogCategory` set in
  `debug.cc`.
- `DebugLogStatement` construction and destruction update the global log tree
  (`gRootSlot`, `gCurrentSlot`) under `gLogMutex`. When the root statement
  destructs, it writes the tree via `FX_LOGS(DEBUG)` on Fuchsia or `fprintf` to
  `stderr` on host platforms.

## Verification

Run this command to build `shared/logging`:

- Build: `fx build //src/developer/debug/shared/logging`
