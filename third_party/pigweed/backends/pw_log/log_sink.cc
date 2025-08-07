// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "pw_log_sink/log_sink.h"

#include <fidl/fuchsia.logger/cpp/fidl.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/syslog/structured_backend/cpp/log_connection.h>
#include <lib/syslog/structured_backend/fuchsia_syslog.h>
#include <zircon/process.h>

#include "pw_log_fuchsia/log_fuchsia.h"

namespace {

FuchsiaLogSeverity FuchsiaLogSeverityFromFidl(fuchsia_diagnostics_types::Severity severity) {
  switch (severity) {
    case fuchsia_diagnostics_types::Severity::kTrace:
      return FUCHSIA_LOG_TRACE;
    case fuchsia_diagnostics_types::Severity::kDebug:
      return FUCHSIA_LOG_DEBUG;
    case fuchsia_diagnostics_types::Severity::kInfo:
      return FUCHSIA_LOG_INFO;
    case fuchsia_diagnostics_types::Severity::kWarn:
      return FUCHSIA_LOG_WARNING;
    case fuchsia_diagnostics_types::Severity::kError:
      return FUCHSIA_LOG_ERROR;
    case fuchsia_diagnostics_types::Severity::kFatal:
      return FUCHSIA_LOG_FATAL;
    default:
      return FUCHSIA_LOG_ERROR;
  }
}

FuchsiaLogSeverity PigweedLevelToFuchsiaSeverity(int pw_level) {
  switch (pw_level) {
    case PW_LOG_LEVEL_ERROR:
      return FUCHSIA_LOG_ERROR;
    case PW_LOG_LEVEL_WARN:
      return FUCHSIA_LOG_WARNING;
    case PW_LOG_LEVEL_INFO:
      return FUCHSIA_LOG_INFO;
    case PW_LOG_LEVEL_DEBUG:
      return FUCHSIA_LOG_DEBUG;
    default:
      return FUCHSIA_LOG_ERROR;
  }
}

class LogState {
 public:
  void Initialize(async_dispatcher_t* dispatcher) {
    dispatcher_ = dispatcher;

    auto client_end = ::component::Connect<fuchsia_logger::LogSink>();
    ZX_ASSERT(client_end.is_ok());

    auto log_connection = fuchsia_logging::LogConnection::Create(*client_end);
    ZX_ASSERT(log_connection.is_ok());

    log_sink_.Bind(std::move(*client_end), dispatcher_);

    // Get interest level synchronously to avoid dropping DEBUG logs during initialization (before
    // an async interest response would be received).
    ::fidl::WireResult<::fuchsia_logger::LogSink::WaitForInterestChange> interest_result =
        log_sink_.sync()->WaitForInterestChange();
    ZX_ASSERT(interest_result.ok());
    HandleInterest(interest_result->value()->data);

    log_connection_ = *std::move(log_connection);

    WaitForInterestChanged();
  }

  void HandleInterest(fuchsia_diagnostics_types::wire::Interest& interest) {
    if (!interest.has_min_severity()) {
      severity_ = FUCHSIA_LOG_INFO;
    } else {
      severity_ = FuchsiaLogSeverityFromFidl(interest.min_severity());
    }
  }

  void WaitForInterestChanged() {
    log_sink_->WaitForInterestChange().Then(
        [this](fidl::WireUnownedResult<fuchsia_logger::LogSink::WaitForInterestChange>&
                   interest_result) {
          if (!interest_result.ok()) {
            auto error = interest_result.error();
            ZX_ASSERT_MSG(error.is_dispatcher_shutdown(), "%s", error.FormatDescription().c_str());
            return;
          }
          HandleInterest(interest_result.value()->data);
          WaitForInterestChanged();
        });
  }

  fuchsia_logging::LogConnection& log_connection() { return log_connection_; }
  FuchsiaLogSeverity severity() const { return severity_; }

 private:
  fidl::WireClient<::fuchsia_logger::LogSink> log_sink_;
  async_dispatcher_t* dispatcher_;
  fuchsia_logging::LogConnection log_connection_;
  FuchsiaLogSeverity severity_ = FUCHSIA_LOG_INFO;
};

LogState log_state;

zx_koid_t GetKoid(zx_handle_t handle) {
  zx_info_handle_basic_t info;
  zx_status_t status =
      zx_object_get_info(handle, ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  return status == ZX_OK ? info.koid : ZX_KOID_INVALID;
}

thread_local const zx_koid_t thread_koid = GetKoid(zx_thread_self());
zx_koid_t const process_koid = GetKoid(zx_process_self());

}  // namespace

namespace pw_log_sink {

void InitializeLogging(async_dispatcher_t* dispatcher) { log_state.Initialize(dispatcher); }

}  // namespace pw_log_sink

extern "C" {

void pw_log_fuchsia_impl(int level, const char* module_name, const char* file_name, int line_number,
                         const char* message) {
  FuchsiaLogSeverity fuchsia_severity = PigweedLevelToFuchsiaSeverity(level);
  if (log_state.severity() > fuchsia_severity) {
    return;
  }

  fuchsia_logging::LogBuffer buffer;
  buffer.BeginRecord(fuchsia_severity, std::string_view(file_name), line_number,
                     std::string_view(message), /*dropped_count=*/0, process_koid, thread_koid);
  buffer.WriteKeyValue("tag", module_name);
  [[maybe_unused]] zx::result<> result = log_state.log_connection().FlushBuffer(buffer);
}

}  // extern C
