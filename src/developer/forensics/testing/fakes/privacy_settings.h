// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_TESTING_FAKES_PRIVACY_SETTINGS_H_
#define SRC_DEVELOPER_FORENSICS_TESTING_FAKES_PRIVACY_SETTINGS_H_

#include <fidl/fuchsia.settings/cpp/fidl.h>
#include <fidl/fuchsia.settings/cpp/test_base.h>

#include <optional>

#include "src/developer/forensics/testing/stubs/fidl_server.h"

namespace forensics {
namespace fakes {

// Fake server for fuchsia.settings.Privacy.
//
// The hanging get pattern behind Watch() requires us to maintain a separate handler per connection
// to be able to track each connection. Here, we only make a single connection in the unit tests
// anyway so it's fine if the fake service can have at most one connection.
class PrivacySettings : public stubs::SingleBindingFidlServer<fuchsia_settings::Privacy> {
 public:
  void SetUserDataSharingConsent(std::optional<bool> user_data_sharing_consent);

  // |fuchsia_settings::Privacy|
  void Watch(WatchCompleter::Sync& completer) override;

 private:
  void NotifyWatcher();

  fuchsia_settings::PrivacySettings settings_;
  bool dirty_bit_ = true;
  std::optional<WatchCompleter::Async> watcher_;
};

}  // namespace fakes
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_TESTING_FAKES_PRIVACY_SETTINGS_H_
