// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_TYPE_CONVERSIONS_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_TYPE_CONVERSIONS_H_

#include <map>
#include <string>
#include <vector>

#include <dap/types.h>

#include "src/developer/debug/zxdb/client/tracepoint_capture.h"
#include "src/developer/debug/zxdb/debug_adapter/handlers/request_tracepoint.h"

namespace zxdb {

dap::ZxdbTracepointFrameSummary ToDapFrameSummary(const TracepointFrameSummary& frame);

dap::array<dap::ZxdbTracepointFrameRecord> ToDapBacktrace(
    const std::vector<TracepointFrameRecord>& backtrace);

dap::object ToDapLocals(const std::map<std::string, TracepointValue>& locals);

dap::object ToDapRegisters(const std::map<std::string, std::string>& registers);

dap::object ToDapEvaluations(const std::map<std::string, TracepointEvaluation>& evaluations);

dap::ZxdbTracepointFrameCapture ToDapFrameCapture(const TracepointFrameCapture& capture);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_TYPE_CONVERSIONS_H_
