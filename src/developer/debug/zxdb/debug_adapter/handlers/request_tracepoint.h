// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_TRACEPOINT_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_TRACEPOINT_H_

#include <dap/protocol.h>
#include <dap/session.h>
#include <dap/typeof.h>
#include <dap/types.h>

// LINT.IfChange
namespace dap {

struct ZxdbTracepointWatchSpec {
  optional<string> data_id;
  optional<integer> address;
  optional<string> expression;
  optional<integer> byte_size;
  optional<string> access_type;
};
DAP_DECLARE_STRUCT_TYPEINFO(ZxdbTracepointWatchSpec);

struct ZxdbTracepointSpec {
  optional<integer> id;
  optional<string> function;
  optional<string> file;
  optional<integer> line;
  optional<integer> address;
  optional<ZxdbTracepointWatchSpec> watch;
  optional<string> condition;
  optional<boolean> frame;
  optional<boolean> backtrace;
  optional<boolean> raw_backtrace;
  optional<boolean> locals;
  optional<boolean> registers;
  optional<array<string>> expressions;
  optional<integer> max_matching_frames;
  optional<integer> max_hits;
  optional<boolean> enabled;
};
DAP_DECLARE_STRUCT_TYPEINFO(ZxdbTracepointSpec);

struct ZxdbTracepointInfo {
  integer id = 0;
  boolean verified = false;
  optional<integer> max_matching_frames;
  integer hit_count = 0;
  integer max_hits = 0;
  boolean enabled = true;
  boolean auto_disabled = false;
  optional<string> message;
};
DAP_DECLARE_STRUCT_TYPEINFO(ZxdbTracepointInfo);

struct ZxdbSetTracepointsResponse : public Response {
  array<ZxdbTracepointInfo> tracepoints;
};
DAP_DECLARE_STRUCT_TYPEINFO(ZxdbSetTracepointsResponse);

struct ZxdbSetTracepointsRequest : public Request {
  using Response = ZxdbSetTracepointsResponse;
  array<ZxdbTracepointSpec> tracepoints;
};
DAP_DECLARE_STRUCT_TYPEINFO(ZxdbSetTracepointsRequest);

struct ZxdbTracepointFrameSummary {
  string name;
  optional<string> file;
  optional<integer> line;
  integer ip = 0;
  integer sp = 0;
  optional<string> trust;
};
DAP_DECLARE_STRUCT_TYPEINFO(ZxdbTracepointFrameSummary);

struct ZxdbTracepointFrameRecord {
  integer frame_index = 0;
  string name;
  optional<string> file;
  optional<integer> line;
  integer ip = 0;
  integer sp = 0;
  optional<string> trust;
  boolean elided = false;
  optional<string> elide_description;
};
DAP_DECLARE_STRUCT_TYPEINFO(ZxdbTracepointFrameRecord);

struct ZxdbTracepointFrameCapture {
  boolean matched = true;
  optional<ZxdbTracepointFrameSummary> frame;
  optional<array<ZxdbTracepointFrameRecord>> backtrace;
  optional<object> locals;
  optional<object> registers;
  optional<object> evaluations;
  array<string> errors;
};
DAP_DECLARE_STRUCT_TYPEINFO(ZxdbTracepointFrameCapture);

struct ZxdbTracepointEvent : public Event {
  integer tracepoint_id = 0;
  string location;
  integer hit_count = 0;
  integer max_hits = 0;
  boolean auto_disabled = false;
  boolean truncated = false;
  integer thread_koid = 0;
  number dwell_ms = 0.0;
  ZxdbTracepointFrameCapture capture;
};
DAP_DECLARE_STRUCT_TYPEINFO(ZxdbTracepointEvent);

}  // namespace dap
// LINT.ThenChange(//src/lib/debug/dap/python/zxdb/models.py)

namespace zxdb {

class DebugAdapterContext;

dap::ResponseOrError<dap::ZxdbSetTracepointsResponse> OnRequestZxdbSetTracepoints(
    DebugAdapterContext* ctx, const dap::ZxdbSetTracepointsRequest& req);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_TRACEPOINT_H_
