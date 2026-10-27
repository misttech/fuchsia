// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/testing/fakes/privacy_settings.h"

#include <lib/syslog/cpp/macros.h>

#include <optional>
#include <utility>

namespace forensics {
namespace fakes {

void PrivacySettings::Watch(WatchCompleter::Sync& completer) {
  FX_CHECK(!watcher_.has_value());
  watcher_ = completer.ToAsync();
  if (dirty_bit_) {
    NotifyWatcher();
  }
}

void PrivacySettings::SetUserDataSharingConsent(std::optional<bool> user_data_sharing_consent) {
  settings_.user_data_sharing_consent(std::move(user_data_sharing_consent));
  dirty_bit_ = true;

  if (watcher_.has_value()) {
    NotifyWatcher();
  }
}

void PrivacySettings::NotifyWatcher() {
  watcher_->Reply(settings_);
  watcher_.reset();
  dirty_bit_ = false;
}

}  // namespace fakes
}  // namespace forensics
