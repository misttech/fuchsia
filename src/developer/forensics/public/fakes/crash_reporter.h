// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_PUBLIC_FAKES_CRASH_REPORTER_H_
#define SRC_DEVELOPER_FORENSICS_PUBLIC_FAKES_CRASH_REPORTER_H_

#include <fidl/fuchsia.feedback.testing/cpp/fidl.h>
#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <lib/async/dispatcher.h>

#include <memory>
#include <optional>

namespace forensics {
namespace fakes {

class CrashReporter;

class FakeCrashReporterQuerier
    : public fidl::Server<fuchsia_feedback_testing::FakeCrashReporterQuerier> {
 public:
  FakeCrashReporterQuerier(
      async_dispatcher_t* dispatcher, CrashReporter* crash_reporter,
      fidl::ServerEnd<fuchsia_feedback_testing::FakeCrashReporterQuerier> server_end,
      size_t num_crash_reports_filed);

  void UpdateAndNotify(size_t num_crash_reports_filed);

  // |fuchsia_feedback_testing::FakeCrashReporterQuerier|
  void WatchFile(WatchFileCompleter::Sync& completer) override;

 private:
  // If |completer_| has a value, execute it on the condition |watch_file_dirty_bit_| is set.
  void Notify();

  fidl::ServerBinding<fuchsia_feedback_testing::FakeCrashReporterQuerier> connection_;
  std::optional<WatchFileCompleter::Async> completer_;

  size_t num_crash_reports_filed_;
  bool watch_file_dirty_bit_;
};

// Fake handler for fuchsia.feedback.CrashReporter, returns an error if the filed crash report
// doesn't contain a program name. Otherwise, an ok response is returned.
class CrashReporter : public fidl::Server<fuchsia_feedback::CrashReporter> {
 public:
  explicit CrashReporter(async_dispatcher_t* dispatcher);

  void SetQuerier(fidl::ServerEnd<fuchsia_feedback_testing::FakeCrashReporterQuerier> server_end);
  void ResetQuerier();

  // |fuchsia_feedback::CrashReporter|
  void FileReport(FileReportRequest& request, FileReportCompleter::Sync& completer) override;

 private:
  async_dispatcher_t* dispatcher_;
  std::unique_ptr<FakeCrashReporterQuerier> querier_;
  size_t num_crash_reports_filed_;
};

}  // namespace fakes
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_PUBLIC_FAKES_CRASH_REPORTER_H_
