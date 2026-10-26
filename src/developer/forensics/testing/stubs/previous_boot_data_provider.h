// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_TESTING_STUBS_PREVIOUS_BOOT_DATA_PROVIDER_H_
#define SRC_DEVELOPER_FORENSICS_TESTING_STUBS_PREVIOUS_BOOT_DATA_PROVIDER_H_

#include <fidl/fuchsia.diagnostics.persistence/cpp/fidl.h>
#include <fidl/fuchsia.diagnostics.persistence/cpp/test_base.h>
#include <zircon/errors.h>

#include <utility>
#include <vector>

#include "src/developer/forensics/testing/stubs/fidl_server.h"

namespace forensics::stubs {

class PreviousBootDataProviderBase
    : public SingleBindingFidlServer<fuchsia_diagnostics_persistence::PreviousBootDataProvider> {};

class PreviousBootDataProviderReturnsData : public PreviousBootDataProviderBase {
 public:
  explicit PreviousBootDataProviderReturnsData(
      fuchsia_diagnostics_persistence::PreviousBootData data)
      : data_(std::move(data)) {}

  void WatchPreviousBootData(WatchPreviousBootDataRequest& request,
                             WatchPreviousBootDataCompleter::Sync& completer) override {
    completer.Reply(std::move(data_));
  }

 private:
  fuchsia_diagnostics_persistence::PreviousBootData data_;
};

class PreviousBootDataProviderClosesConnection : public PreviousBootDataProviderBase {
 public:
  void WatchPreviousBootData(WatchPreviousBootDataRequest& request,
                             WatchPreviousBootDataCompleter::Sync& completer) override {
    CloseConnection(ZX_ERR_PEER_CLOSED);
  }
};

class PreviousBootDataProviderNeverReturns : public PreviousBootDataProviderBase {
 public:
  void WatchPreviousBootData(WatchPreviousBootDataRequest& request,
                             WatchPreviousBootDataCompleter::Sync& completer) override {
    completers_.push_back(completer.ToAsync());
  }

 private:
  std::vector<WatchPreviousBootDataCompleter::Async> completers_;
};

}  // namespace forensics::stubs

#endif  // SRC_DEVELOPER_FORENSICS_TESTING_STUBS_PREVIOUS_BOOT_DATA_PROVIDER_H_
