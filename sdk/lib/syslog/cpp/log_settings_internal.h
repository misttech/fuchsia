// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYSLOG_CPP_LOG_SETTINGS_INTERNAL_H_
#define LIB_SYSLOG_CPP_LOG_SETTINGS_INTERNAL_H_

#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/logging_backend_fuchsia_globals.h>

namespace fuchsia_logging::internal {

template <typename T>
auto WithInternalSettings(const fuchsia_logging::LogSettings& settings, T callback) {
  std::vector<const char*> tags;
  tags.reserve(settings.tags.size());
  for (const std::string& tag : settings.tags) {
    tags.push_back(tag.c_str());
  }

  static_assert(InterestListenerBehavior::Disabled == kInterestListenerDisabled);
  static_assert(InterestListenerBehavior::EnabledNonBlocking ==
                kInterestListenerEnabledNonBlocking);
  static_assert(InterestListenerBehavior::Enabled == kInterestListenerEnabled);

  return callback(LogSettings{
      .min_log_level = settings.min_log_level,
      .interest_listener_behavior = settings.interest_listener_config_,
      .log_sink = settings.log_sink,
      .tags = tags.data(),
      .tags_count = settings.tags.size(),
      .dispatcher = settings.single_threaded_dispatcher,
      .severity_change_callback = settings.severity_change_callback,
  });
}

}  // namespace fuchsia_logging::internal

#endif  // LIB_SYSLOG_CPP_LOG_SETTINGS_INTERNAL_H_
