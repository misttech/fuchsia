// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/public/fakes/crash_reporter.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <lib/fit/result.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <memory>

namespace forensics {
namespace fakes {

using namespace fuchsia_feedback;

CrashReporter::CrashReporter(async_dispatcher_t* dispatcher)
    : dispatcher_(dispatcher), num_crash_reports_filed_(0) {}

void CrashReporter::FileReport(FileReportRequest& request, FileReportCompleter::Sync& completer) {
  if (!request.report().program_name().has_value()) {
    completer.Reply(fit::error(FilingError::kInvalidArgsError));
  } else {
    completer.Reply(fit::ok(FileReportResults{{
        .result = FilingSuccess::kReportUploaded,
        .report_id = "01234567890abcdef",
    }}));
  }

  ++num_crash_reports_filed_;
  if (querier_) {
    querier_->UpdateAndNotify(num_crash_reports_filed_);
  }
}

void CrashReporter::SetQuerier(
    fidl::ServerEnd<fuchsia_feedback_testing::FakeCrashReporterQuerier> server_end) {
  FX_LOGS(INFO) << "Registering FakeCrashReporterQuerier";
  querier_ = std::make_unique<FakeCrashReporterQuerier>(dispatcher_, this, std::move(server_end),
                                                        num_crash_reports_filed_);
}

void CrashReporter::ResetQuerier() {
  FX_LOGS(INFO) << "Deregistering FakeCrashReporterQuerier";
  querier_.reset();
  // We reset so that the next Querier gets values starting at 0 as they would expect.
  num_crash_reports_filed_ = 0;
}

FakeCrashReporterQuerier::FakeCrashReporterQuerier(
    async_dispatcher_t* dispatcher, CrashReporter* crash_reporter,
    fidl::ServerEnd<fuchsia_feedback_testing::FakeCrashReporterQuerier> server_end,
    size_t num_crash_reports_filed)
    : connection_(dispatcher, std::move(server_end), this,
                  [crash_reporter](fidl::UnbindInfo) { crash_reporter->ResetQuerier(); }),
      num_crash_reports_filed_(num_crash_reports_filed),
      watch_file_dirty_bit_(true) {}

void FakeCrashReporterQuerier::UpdateAndNotify(size_t num_crash_reports_filed) {
  num_crash_reports_filed_ = num_crash_reports_filed;
  watch_file_dirty_bit_ = true;
  Notify();
}

void FakeCrashReporterQuerier::Notify() {
  if (!completer_.has_value() || !watch_file_dirty_bit_) {
    return;
  }

  completer_->Reply(num_crash_reports_filed_);
  completer_ = std::nullopt;
  watch_file_dirty_bit_ = false;
}

void FakeCrashReporterQuerier::WatchFile(WatchFileCompleter::Sync& completer) {
  completer_ = completer.ToAsync();
  Notify();
}

}  // namespace fakes
}  // namespace forensics
