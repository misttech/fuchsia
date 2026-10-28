// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_tracepoint.h"

#include <limits>
#include <set>
#include <vector>

#include "src/developer/debug/shared/arch.h"
#include "src/developer/debug/zxdb/client/breakpoint_settings.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/tracepoint.h"
#include "src/developer/debug/zxdb/common/err_or.h"
#include "src/developer/debug/zxdb/debug_adapter/context.h"
#include "src/developer/debug/zxdb/debug_adapter/data_id.h"

namespace dap {

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointWatchSpec, "", DAP_FIELD(data_id, "dataId"),
                              DAP_FIELD(address, "address"), DAP_FIELD(expression, "expression"),
                              DAP_FIELD(byte_size, "byteSize"),
                              DAP_FIELD(access_type, "accessType"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointSpec, "", DAP_FIELD(id, "id"),
                              DAP_FIELD(function, "function"), DAP_FIELD(file, "file"),
                              DAP_FIELD(line, "line"), DAP_FIELD(address, "address"),
                              DAP_FIELD(watch, "watch"), DAP_FIELD(condition, "condition"),
                              DAP_FIELD(frame, "frame"), DAP_FIELD(backtrace, "backtrace"),
                              DAP_FIELD(raw_backtrace, "rawBacktrace"), DAP_FIELD(locals, "locals"),
                              DAP_FIELD(registers, "registers"),
                              DAP_FIELD(expressions, "expressions"),
                              DAP_FIELD(max_matching_frames, "maxMatchingFrames"),
                              DAP_FIELD(max_hits, "maxHits"), DAP_FIELD(enabled, "enabled"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointInfo, "", DAP_FIELD(id, "id"),
                              DAP_FIELD(verified, "verified"),
                              DAP_FIELD(max_matching_frames, "maxMatchingFrames"),
                              DAP_FIELD(hit_count, "hitCount"), DAP_FIELD(max_hits, "maxHits"),
                              DAP_FIELD(enabled, "enabled"),
                              DAP_FIELD(auto_disabled, "autoDisabled"),
                              DAP_FIELD(message, "message"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbSetTracepointsResponse, "", DAP_FIELD(tracepoints, "tracepoints"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbSetTracepointsRequest, "zxdb.SetTracepoints",
                              DAP_FIELD(tracepoints, "tracepoints"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointFrameSummary, "", DAP_FIELD(name, "name"),
                              DAP_FIELD(file, "file"), DAP_FIELD(line, "line"), DAP_FIELD(ip, "ip"),
                              DAP_FIELD(sp, "sp"), DAP_FIELD(trust, "trust"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointFrameRecord, "", DAP_FIELD(frame_index, "frameIndex"),
                              DAP_FIELD(name, "name"), DAP_FIELD(file, "file"),
                              DAP_FIELD(line, "line"), DAP_FIELD(ip, "ip"), DAP_FIELD(sp, "sp"),
                              DAP_FIELD(trust, "trust"), DAP_FIELD(elided, "elided"),
                              DAP_FIELD(elide_description, "elideDescription"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointFrameCapture, "", DAP_FIELD(matched, "matched"),
                              DAP_FIELD(frame, "frame"), DAP_FIELD(backtrace, "backtrace"),
                              DAP_FIELD(locals, "locals"), DAP_FIELD(registers, "registers"),
                              DAP_FIELD(evaluations, "evaluations"), DAP_FIELD(errors, "errors"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointEvent, "zxdb.Tracepoint",
                              DAP_FIELD(tracepoint_id, "tracepointId"),
                              DAP_FIELD(location, "location"), DAP_FIELD(hit_count, "hitCount"),
                              DAP_FIELD(max_hits, "maxHits"),
                              DAP_FIELD(auto_disabled, "autoDisabled"),
                              DAP_FIELD(truncated, "truncated"),
                              DAP_FIELD(thread_koid, "threadKoid"), DAP_FIELD(dwell_ms, "dwellMs"),
                              DAP_FIELD(capture, "capture"))

}  // namespace dap

namespace zxdb {

namespace {

struct AssignedClientIds {
  std::vector<int64_t> assigned_ids;
  std::set<int64_t> incoming_ids;
};

ErrOr<AssignedClientIds> AssignAndValidateClientIds(
    const dap::array<dap::ZxdbTracepointSpec>& specs,
    const std::map<int64_t, fxl::WeakPtr<Tracepoint>>& existing) {
  int64_t next_id = 1;
  for (const auto& [cid, _] : existing) {
    if (cid >= next_id) {
      next_id = cid + 1;
    }
  }

  AssignedClientIds result;
  result.assigned_ids.reserve(specs.size());

  for (const auto& dap_spec : specs) {
    if (dap_spec.id.has_value()) {
      if (*dap_spec.id <= 0) {
        return Err("Tracepoint id must be positive.");
      }
      if (!result.incoming_ids.insert(*dap_spec.id).second) {
        return Err("Duplicate tracepoint id in zxdb.SetTracepoints request.");
      }
      next_id = std::max(next_id, *dap_spec.id + 1);
    }

    if (dap_spec.line.has_value() && *dap_spec.line <= 0) {
      return Err("Line number must be positive.");
    }
    if (dap_spec.max_matching_frames.has_value() && *dap_spec.max_matching_frames <= 0) {
      return Err("Tracepoint maxMatchingFrames must be a positive integer.");
    }
    if (dap_spec.max_hits.has_value() && *dap_spec.max_hits <= 0) {
      return Err("maxHits must be positive.");
    }
    if (dap_spec.watch.has_value() && dap_spec.watch->byte_size.has_value() &&
        *dap_spec.watch->byte_size <= 0) {
      return Err("watch byteSize must be positive.");
    }
  }

  for (const auto& dap_spec : specs) {
    if (dap_spec.id.has_value()) {
      result.assigned_ids.push_back(*dap_spec.id);
    } else {
      while (existing.contains(next_id) || result.incoming_ids.contains(next_id)) {
        next_id++;
      }
      result.assigned_ids.push_back(next_id);
      result.incoming_ids.insert(next_id);
      next_id++;
    }
  }

  return result;
}

void PruneRemovedTracepoints(Session* session, const std::set<int64_t>& incoming_ids,
                             std::map<int64_t, fxl::WeakPtr<Tracepoint>>& existing) {
  std::vector<Tracepoint*> to_delete;
  for (const auto& [cid, tp] : existing) {
    if (!incoming_ids.contains(cid) && tp) {
      to_delete.push_back(tp.get());
    }
  }
  for (Tracepoint* tp : to_delete) {
    if (session) {
      session->system().DeleteTracepoint(tp);
    }
  }
  for (auto it = existing.begin(); it != existing.end();) {
    if (!incoming_ids.contains(it->first)) {
      it = existing.erase(it);
    } else {
      ++it;
    }
  }
}

ErrOr<TracepointWatchSpec> BuildWatchSpec(const dap::ZxdbTracepointWatchSpec& dap_watch,
                                          debug::Arch arch) {
  TracepointWatchSpec watch;
  if (dap_watch.expression.has_value()) {
    watch.expression = *dap_watch.expression;
  }
  if (dap_watch.data_id.has_value()) {
    auto parse_res = DataId::Parse(*dap_watch.data_id);
    if (parse_res.has_error()) {
      return parse_res.err();
    }
    watch.address = parse_res.value().address();
    watch.byte_size = parse_res.value().byte_size();
  }
  if (dap_watch.address.has_value()) {
    if (*dap_watch.address < 0) {
      return Err("Invalid negative watch address.");
    }
    watch.address = static_cast<uint64_t>(*dap_watch.address);
  }
  if (dap_watch.byte_size.has_value()) {
    watch.byte_size = static_cast<uint32_t>(*dap_watch.byte_size);
  }
  if (dap_watch.access_type.has_value()) {
    const std::string& access = *dap_watch.access_type;
    if (access == "read") {
      return Err("Read-only data breakpoints are not supported on this platform.");
    }
    if (access == "readWrite" || access == "read_write") {
      watch.access_type = debug_ipc::BreakpointType::kReadWrite;
    } else if (access == "write") {
      watch.access_type = debug_ipc::BreakpointType::kWrite;
    } else {
      return Err("Unknown accessType: %s", access.c_str());
    }
  }

  if (watch.address.has_value() &&
      *watch.address > std::numeric_limits<uint64_t>::max() - watch.byte_size) {
    return Err("Watch address range overflows 64-bit address space.");
  }
  if (Err err = BreakpointSettings::ValidateSize(arch, watch.access_type, watch.byte_size);
      err.has_error()) {
    return err;
  }
  return watch;
}

ErrOr<TracepointSpec> BuildTracepointSpec(const dap::ZxdbTracepointSpec& dap_spec,
                                          debug::Arch arch) {
  if (dap_spec.file.has_value() != dap_spec.line.has_value()) {
    return Err("Both file and line must be specified together for a line tracepoint.");
  }

  TracepointSpec spec;
  if (dap_spec.function.has_value()) {
    spec.function = *dap_spec.function;
  }
  if (dap_spec.address.has_value()) {
    if (*dap_spec.address < 0) {
      return Err("Invalid negative tracepoint address.");
    }
    spec.address = static_cast<uint64_t>(*dap_spec.address);
  }
  if (dap_spec.file.has_value() && dap_spec.line.has_value()) {
    spec.file_line = FileLine(*dap_spec.file, static_cast<int>(*dap_spec.line));
  }
  if (dap_spec.watch.has_value()) {
    auto watch_res = BuildWatchSpec(*dap_spec.watch, arch);
    if (watch_res.has_error()) {
      return watch_res.err();
    }
    spec.watch = watch_res.value();
  }
  if (dap_spec.condition.has_value()) {
    spec.condition = *dap_spec.condition;
  }
  if (dap_spec.frame.has_value()) {
    spec.frame = *dap_spec.frame;
  }
  if (dap_spec.backtrace.has_value()) {
    spec.backtrace = *dap_spec.backtrace;
  }
  if (dap_spec.raw_backtrace.has_value()) {
    spec.raw_backtrace = *dap_spec.raw_backtrace;
    if (spec.raw_backtrace) {
      spec.backtrace = true;
    }
  }
  if (dap_spec.locals.has_value()) {
    spec.locals = *dap_spec.locals;
  }
  if (dap_spec.registers.has_value()) {
    spec.registers = *dap_spec.registers;
  }
  if (dap_spec.expressions.has_value()) {
    spec.expressions = *dap_spec.expressions;
  }
  if (dap_spec.max_matching_frames.has_value()) {
    spec.max_matching_frames = static_cast<uint32_t>(*dap_spec.max_matching_frames);
  }
  return spec;
}

dap::ZxdbTracepointInfo CreateUnverifiedTracepointInfo(int64_t client_id,
                                                       const dap::ZxdbTracepointSpec& dap_spec,
                                                       const std::string& error_message) {
  dap::ZxdbTracepointInfo info;
  info.id = client_id;
  info.verified = false;
  info.message = error_message;
  if (dap_spec.max_matching_frames.has_value()) {
    info.max_matching_frames = *dap_spec.max_matching_frames;
  }
  if (dap_spec.enabled.has_value()) {
    info.enabled = *dap_spec.enabled;
  }
  if (dap_spec.max_hits.has_value()) {
    info.max_hits = *dap_spec.max_hits;
  }
  return info;
}

dap::ZxdbTracepointInfo UpsertTracepoint(DebugAdapterContext* ctx, int64_t client_id,
                                         const dap::ZxdbTracepointSpec& dap_spec,
                                         const TracepointSpec& spec) {
  auto& existing = ctx->dap_tracepoints();
  auto existing_it = existing.find(client_id);
  bool is_new = (existing_it == existing.end() || !existing_it->second);
  Tracepoint* tp = nullptr;
  if (!is_new) {
    tp = existing_it->second.get();
  } else {
    tp = ctx->session()->system().CreateNewTracepoint();
    existing[client_id] = tp->GetWeakPtr();
  }

  if (dap_spec.max_hits.has_value()) {
    tp->set_max_hits(static_cast<uint32_t>(*dap_spec.max_hits));
  }
  if (dap_spec.enabled.has_value() && *dap_spec.enabled != tp->enabled()) {
    tp->SetEnabled(*dap_spec.enabled);
  }
  if (is_new || !(tp->spec() == spec)) {
    tp->SetSpec(spec);
  }

  dap::ZxdbTracepointInfo info;
  info.id = client_id;
  info.verified = tp->verified();
  if (tp->spec().max_matching_frames.has_value()) {
    info.max_matching_frames = static_cast<int64_t>(*tp->spec().max_matching_frames);
  }
  info.hit_count = tp->hit_count();
  info.max_hits = tp->max_hits();
  info.enabled = tp->enabled();
  info.auto_disabled = tp->auto_disabled();
  return info;
}

}  // namespace

dap::ResponseOrError<dap::ZxdbSetTracepointsResponse> OnRequestZxdbSetTracepoints(
    DebugAdapterContext* ctx, const dap::ZxdbSetTracepointsRequest& req) {
  if (!ctx || !ctx->session()) {
    return dap::Error("No active debug session.");
  }

  auto& existing = ctx->dap_tracepoints();
  auto assign_res = AssignAndValidateClientIds(req.tracepoints, existing);
  if (assign_res.has_error()) {
    return dap::Error(assign_res.err().msg());
  }

  const auto& assigned_ids = assign_res.value().assigned_ids;
  const auto& incoming_ids = assign_res.value().incoming_ids;

  PruneRemovedTracepoints(ctx->session(), incoming_ids, existing);

  dap::ZxdbSetTracepointsResponse response;
  response.tracepoints.reserve(req.tracepoints.size());

  for (size_t i = 0; i < req.tracepoints.size(); ++i) {
    const auto& dap_spec = req.tracepoints[i];
    int64_t client_id = assigned_ids[i];

    auto cleanup_existing = [&]() {
      auto existing_it = existing.find(client_id);
      if (existing_it != existing.end()) {
        if (existing_it->second) {
          ctx->session()->system().DeleteTracepoint(existing_it->second.get());
        }
        existing.erase(client_id);
      }
    };

    auto spec_or_err = BuildTracepointSpec(dap_spec, ctx->session()->arch());
    if (spec_or_err.has_error()) {
      cleanup_existing();
      response.tracepoints.push_back(
          CreateUnverifiedTracepointInfo(client_id, dap_spec, spec_or_err.err().msg()));
      continue;
    }

    const TracepointSpec& spec = spec_or_err.value();
    Err valid_err = spec.Validate();
    if (valid_err.has_error()) {
      cleanup_existing();
      response.tracepoints.push_back(
          CreateUnverifiedTracepointInfo(client_id, dap_spec, valid_err.msg()));
      continue;
    }

    response.tracepoints.push_back(UpsertTracepoint(ctx, client_id, dap_spec, spec));
  }

  return response;
}

}  // namespace zxdb
