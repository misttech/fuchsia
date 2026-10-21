// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_attach.h"

#include <zircon/types.h>

#include <algorithm>
#include <cctype>
#include <cstdint>
#include <string>

#include "src/developer/debug/ipc/filter_utils.h"
#include "src/developer/debug/ipc/records.h"
#include "src/developer/debug/shared/logging/logging.h"
#include "src/developer/debug/zxdb/client/filter.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/target.h"
#include "src/developer/debug/zxdb/debug_adapter/handlers/request_launch.h"
#include "src/developer/debug/zxdb/expr/expr_number_utils.h"

namespace dap {

// NOLINTNEXTLINE(modernize-type-traits)
DAP_IMPLEMENT_STRUCT_TYPEINFO_EXT(AttachRequestZxdb, AttachRequest, "attach",
                                  DAP_FIELD(process, "process"), DAP_FIELD(command, "command"),
                                  DAP_FIELD(recursive, "recursive"), DAP_FIELD(weak, "weak"),
                                  DAP_FIELD(exact, "exact"), DAP_FIELD(job_only, "job_only"),
                                  DAP_FIELD(cwd, "cwd"))

}  // namespace dap

namespace zxdb {
namespace {

Target* FindOrCreateIdleTarget(System& system) {
  for (Target* target : system.GetTargets()) {
    if (target && target->GetState() == Target::State::kNone) {
      return target;
    }
  }
  return system.CreateNewTarget(/*clone=*/nullptr);
}

// Reuses an existing filter from this DAP session if one matches (pattern, type), or creates
// and tracks a new connection-scoped filter so disconnect teardown leaves persistent user
// filters (e.g. from ~/.fuchsia/debug/zxdbrc) untouched.
Filter* FindOrCreateFilter(DebugAdapterContext* context, const std::string& pattern,
                           debug_ipc::Filter::Type type) {
  for (Filter* existing_filter : context->filters()) {
    if (existing_filter && existing_filter->pattern() == pattern &&
        existing_filter->type() == type) {
      return existing_filter;
    }
  }

  Filter* filter = context->session()->system().CreateNewFilter();
  filter->SetPattern(pattern);
  filter->SetType(type);
  context->StoreFilter(filter);
  return filter;
}

}  // namespace

dap::ResponseOrError<dap::AttachResponse> OnRequestAttach(DebugAdapterContext* context,
                                                          const dap::AttachRequestZxdb& req) {
  dap::AttachResponse response;

  if (req.process.empty()) {
    dap::Error err;
    err.message = "Process attach pattern must not be empty!";
    return err;
  }

  // Mutable copy: ResolveFilterType trims process-name patterns to Zircon's 31-char limit.
  std::string pattern = req.process;
  const bool is_recursive = req.recursive.value(false);
  const bool is_weak = req.weak.value(false);
  const bool is_exact = req.exact.value(false);
  const bool is_job_only = req.job_only.value(false);

  // Purely decimal patterns attach directly by KOID unless exact process-name or job-only
  // filter matching was explicitly requested. Cast char to unsigned char to avoid UB in ::isdigit.
  if (!is_exact && !is_job_only &&
      std::ranges::all_of(pattern, [](unsigned char c) { return ::isdigit(c); })) {
    uint64_t koid = ZX_KOID_INVALID;
    if (StringToUint64(pattern, &koid).has_error()) {
      dap::Error err;
      err.message = "Failed to parse process ID \"" + pattern + "\"";
      return err;
    }
    if (koid == ZX_KOID_INVALID) {
      dap::Error err;
      err.message = "Invalid process ID \"" + pattern + "\"";
      return err;
    }

    Target* target = FindOrCreateIdleTarget(context->session()->system());
    const debug_ipc::AttachConfig::Priority priority =
        is_weak ? debug_ipc::AttachConfig::Priority::kWeak
                : debug_ipc::AttachConfig::Priority::kStrong;
    target->Attach(
        koid, {.priority = priority, .target = debug_ipc::TaskType::kProcess},
        [](const fxl::WeakPtr<Target>& /*target*/, const Err& err, uint64_t /*timestamp*/) {
          if (err.has_error()) {
            LOGS(Error) << "Async process attach failed: " << err.msg();
          }
        });

    return response;
  }

  const debug_ipc::Filter::Type type = debug_ipc::ResolveFilterType(pattern, {.exact = is_exact});
  Filter* filter = FindOrCreateFilter(context, pattern, type);

  // Always synchronize option flags so a reused filter does not retain stale settings.
  filter->SetRecursive(is_recursive);
  filter->SetWeak(is_weak);
  filter->SetJobOnly(is_job_only);

  if (req.command) {
    dap::RunInTerminalRequest run_request;
    run_request.title = "zxdb launch";
    run_request.kind = "integrated";
    SplitDapCommand(req.command.value(), run_request.args);
    if (req.cwd) {
      run_request.cwd = req.cwd.value();
    }
    context->dap().send(run_request);
  }

  return response;
}

}  // namespace zxdb
