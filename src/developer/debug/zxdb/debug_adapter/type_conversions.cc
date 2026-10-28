// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/type_conversions.h"

#include "src/developer/debug/ipc/records.h"

namespace zxdb {

dap::ZxdbTracepointFrameSummary ToDapFrameSummary(const TracepointFrameSummary& frame) {
  dap::ZxdbTracepointFrameSummary summary;
  summary.name = frame.name;
  if (frame.file.has_value()) {
    summary.file = *frame.file;
  }
  if (frame.line.has_value()) {
    summary.line = *frame.line;
  }
  summary.ip = static_cast<int64_t>(frame.ip);
  summary.sp = static_cast<int64_t>(frame.sp);
  summary.trust = debug_ipc::StackFrame::TrustToString(frame.trust);
  return summary;
}

dap::array<dap::ZxdbTracepointFrameRecord> ToDapBacktrace(
    const std::vector<TracepointFrameRecord>& backtrace) {
  dap::array<dap::ZxdbTracepointFrameRecord> bt;
  bt.reserve(backtrace.size());
  for (const auto& rec : backtrace) {
    dap::ZxdbTracepointFrameRecord dap_rec;
    dap_rec.frame_index = static_cast<int64_t>(rec.frame_index);
    dap_rec.name = rec.name;
    if (rec.file.has_value()) {
      dap_rec.file = *rec.file;
    }
    if (rec.line.has_value()) {
      dap_rec.line = *rec.line;
    }
    dap_rec.ip = static_cast<int64_t>(rec.ip);
    dap_rec.sp = static_cast<int64_t>(rec.sp);
    dap_rec.trust = debug_ipc::StackFrame::TrustToString(rec.trust);
    dap_rec.elided = rec.elided;
    if (!rec.elide_description.empty()) {
      dap_rec.elide_description = rec.elide_description;
    }
    bt.push_back(dap_rec);
  }
  return bt;
}

dap::object ToDapLocals(const std::map<std::string, TracepointValue>& locals) {
  dap::object locals_obj;
  for (const auto& [name, val] : locals) {
    dap::object val_obj;
    val_obj["value"] = val.value;
    val_obj["type"] = val.type;
    locals_obj[name] = val_obj;
  }
  return locals_obj;
}

dap::object ToDapRegisters(const std::map<std::string, std::string>& registers) {
  dap::object regs_obj;
  for (const auto& [reg_name, reg_val] : registers) {
    regs_obj[reg_name] = reg_val;
  }
  return regs_obj;
}

dap::object ToDapEvaluations(const std::map<std::string, TracepointEvaluation>& evaluations) {
  dap::object evals_obj;
  for (const auto& [expr, eval] : evaluations) {
    dap::object eval_obj;
    if (eval.result.has_value()) {
      eval_obj["result"] = *eval.result;
    }
    if (eval.type.has_value()) {
      eval_obj["type"] = *eval.type;
    }
    if (eval.error.has_value()) {
      eval_obj["error"] = *eval.error;
    }
    evals_obj[expr] = eval_obj;
  }
  return evals_obj;
}

dap::ZxdbTracepointFrameCapture ToDapFrameCapture(const TracepointFrameCapture& capture) {
  dap::ZxdbTracepointFrameCapture dap_capture;
  dap_capture.matched = capture.matched;
  if (capture.frame.has_value()) {
    dap_capture.frame = ToDapFrameSummary(*capture.frame);
  }
  if (capture.backtrace.has_value()) {
    dap_capture.backtrace = ToDapBacktrace(*capture.backtrace);
  }
  if (capture.locals.has_value()) {
    dap_capture.locals = ToDapLocals(*capture.locals);
  }
  if (capture.registers.has_value()) {
    dap_capture.registers = ToDapRegisters(*capture.registers);
  }
  if (!capture.evaluations.empty()) {
    dap_capture.evaluations = ToDapEvaluations(capture.evaluations);
  }
  dap_capture.errors = capture.errors;
  return dap_capture;
}

}  // namespace zxdb
