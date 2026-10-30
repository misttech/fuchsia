// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_TYPE_CONVERSIONS_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_TYPE_CONVERSIONS_H_

#include "src/developer/debug/zxdb/client/tracepoint_capture.h"
#include "src/developer/debug/zxdb/debug_adapter/handlers/request_tracepoint.h"

namespace zxdb {

// Converts a captured client tracepoint frame record into its DAP wire representation.
dap::ZxdbTracepointFrameCapture ToDapFrameCapture(const TracepointFrameCapture& capture);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_TYPE_CONVERSIONS_H_
