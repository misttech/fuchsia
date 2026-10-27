// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_TESTING_FAKES_DATA_PROVIDER_H_
#define SRC_DEVELOPER_FORENSICS_TESTING_FAKES_DATA_PROVIDER_H_

#include <fidl/fuchsia.feedback/cpp/fidl.h>

namespace forensics {
namespace fakes {

// Fake handler for fuchsia.feedback.DataProvider, returns valid payloads for GetSnapshot() and
// GetAnnotations(). Tests should not have hard expectations on these payloads as they're subject to
// change.
class DataProvider : public fidl::Server<fuchsia_feedback::DataProvider> {
 public:
  // |fuchsia_feedback::DataProvider|
  void GetAnnotations(GetAnnotationsRequest& request,
                      GetAnnotationsCompleter::Sync& completer) override;
  void GetSnapshot(GetSnapshotRequest& request, GetSnapshotCompleter::Sync& completer) override;
};

}  // namespace fakes
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_TESTING_FAKES_DATA_PROVIDER_H_
