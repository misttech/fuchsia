// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.logger/cpp/fidl.h>
#include <lib/async/default.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/syslog/cpp/log_level.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/logging_backend_fuchsia_globals.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/syslog/structured_backend/cpp/log_buffer.h>
#include <lib/syslog/structured_backend/cpp/raw_log_settings.h>
#include <lib/zx/channel.h>
#include <lib/zx/process.h>
#include <zircon/availability.h>
#include <zircon/process.h>
#include <zircon/processargs.h>

#include <optional>
#include <sstream>
#include <string>

#include "log_settings_internal.h"

namespace fuchsia_logging {
namespace {

zx_koid_t ProcessSelfKoid() {
  zx_info_handle_basic_t info;
  // We need to use _zx_object_get_info to avoid breaking the driver ABI.
  // fake_ddk can fake out this method, which results in us deadlocking
  // when used in certain drivers because the fake doesn't properly pass-through
  // to the real syscall in this case.
  zx_status_t status = _zx_object_get_info(zx_process_self(), ZX_INFO_HANDLE_BASIC, &info,
                                           sizeof(info), nullptr, nullptr);
  return status == ZX_OK ? info.koid : ZX_KOID_INVALID;
}

zx_koid_t globalPid = ProcessSelfKoid();
const char kTagFieldName[] = "tag";

void FixDefaultSettings(RawLogSettings& settings, bool interest_listener_enabled) {
  // If no handle is provided, try the incoming namespace.
#if FUCHSIA_API_LEVEL_AT_LEAST(32)
  if (settings.log_sink == ZX_HANDLE_INVALID) {
    // If there is no procargs handle, `log_sink` will be ZX_HANDLE_INVALID which will fallback to
    // the following `if` and connect to svc.
    settings.log_sink = zx_take_startup_handle(PA_LOG_SINK);
  }
#endif
  if (settings.log_sink == ZX_HANDLE_INVALID) {
    auto connect_result = component::Connect<fuchsia_logger::LogSink>();
    // If there is an error, we leave `log_sink` with ZX_HANDLE_INVALID which will result in a no-op
    // logger later.
    if (connect_result.is_ok()) {
      settings.log_sink = connect_result->TakeChannel().release();
    }
  }
  // If no dispatcher specified, try the default dispatcher.
  if (!settings.dispatcher && interest_listener_enabled) {
    settings.dispatcher = async_get_default_dispatcher();
  }
}

RawLogSettings GetDefaultSettings() {
  RawLogSettings settings;
  FixDefaultSettings(settings, true);
  return settings;
}

void BeginRecord(LogBuffer* buffer, fuchsia_logging::RawLogSeverity severity,
                 internal::NullSafeStringView file_name, unsigned int line,
                 internal::NullSafeStringView msg, internal::NullSafeStringView condition) {
  std::optional<std::string_view> msg_view = msg;

  // Optional so no allocation overhead occurs if condition isn't set.
  std::optional<std::string> modified_msg;
  if (std::optional<std::string_view> cond = condition; cond) {
    std::stringstream s;
    s << "Check failed: " << *cond << ". ";
    if (msg_view) {
      s << *msg_view;
    }
    modified_msg = s.str();
    msg_view = modified_msg->data();
  }
  buffer->BeginRecord(severity, file_name, line, msg_view, 0, globalPid,
                      internal::FuchsiaLogGetCurrentThreadKoid());
  internal::FuchsiaLogForEachTag(
      internal::FuchsiaLogGetGlobalLogger(GetDefaultSettings), buffer,
      +[](void* context, const char* tag) {
        auto buffer = reinterpret_cast<LogBuffer*>(context);
        buffer->WriteKeyValue(kTagFieldName, tag);
      });
}

void SetLogSettings(const fuchsia_logging::LogSettings& settings) {
  const bool interest_listener_enabled =
      settings.interest_listener_config_ != InterestListenerBehavior::Disabled;
  internal::WithRawSettings(settings, [interest_listener_enabled](RawLogSettings& raw_settings) {
    FixDefaultSettings(raw_settings, interest_listener_enabled);
    internal::FuchsiaLogInitGlobalLogger(&raw_settings);
  });
}

}  // namespace

zx::result<> FlushToGlobalLogger(LogBuffer& buffer) {
  cpp20::span<const uint8_t> data = buffer.EndRecord();
  return zx::make_result(internal::FuchsiaLogWrite(
      internal::FuchsiaLogGetGlobalLogger(GetDefaultSettings), data.data(), data.size()));
}

// Sets the default log severity. If not explicitly set,
// this defaults to INFO, or to the value specified by Archivist.
LogSettingsBuilder& LogSettingsBuilder::WithMinLogSeverity(
    fuchsia_logging::RawLogSeverity min_log_level) {
  settings_.min_log_level = min_log_level;
  return *this;
}

// Disables the interest listener.
LogSettingsBuilder& LogSettingsBuilder::DisableInterestListener() {
  WithInterestListenerConfiguration(fuchsia_logging::Disabled);
  return *this;
}

LogSettingsBuilder& LogSettingsBuilder::WithSeverityChangedListener(
    void* context, void (*callback)(void*, fuchsia_logging::RawLogSeverity severity)) {
  settings_.severity_change_callback = callback;
  settings_.severity_change_callback_context = context;
  return *this;
}

LogSettingsBuilder& LogSettingsBuilder::WithInterestListenerConfiguration(
    InterestListenerBehavior config) {
  settings_.interest_listener_config_ = config;
  return *this;
}

// Sets the log sink handle.
LogSettingsBuilder& LogSettingsBuilder::WithLogSink(zx_handle_t log_sink) {
  settings_.log_sink = log_sink;
  return *this;
}

// Sets the dispatcher to use.
LogSettingsBuilder& LogSettingsBuilder::WithDispatcher(async_dispatcher_t* dispatcher) {
  settings_.single_threaded_dispatcher = dispatcher;
  return *this;
}

LogSettingsBuilder& LogSettingsBuilder::WithTags(const std::initializer_list<std::string>& tags) {
  for (auto& tag : tags) {
    settings_.tags.push_back(tag);
  }
  return *this;
}

// Configures the log settings.
void LogSettingsBuilder::BuildAndInitialize() { SetLogSettings(settings_); }

LogBuffer LogBufferBuilder::Build() {
  LogBuffer buffer;
  fuchsia_logging::BeginRecord(
      &buffer, severity_,
      fuchsia_logging::internal::NullSafeStringView::CreateFromOptional(file_name_), line_,
      fuchsia_logging::internal::NullSafeStringView::CreateFromOptional(msg_),
      fuchsia_logging::internal::NullSafeStringView::CreateFromOptional(condition_));
  return buffer;
}

}  // namespace fuchsia_logging
