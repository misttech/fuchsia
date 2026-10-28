// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_data_breakpoint.h"

#include <inttypes.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/syslog/cpp/macros.h>

#include <limits>
#include <string>
#include <string_view>
#include <utility>

#include "src/developer/debug/shared/arch.h"
#include "src/developer/debug/zxdb/client/breakpoint.h"
#include "src/developer/debug/zxdb/client/breakpoint_settings.h"
#include "src/developer/debug/zxdb/client/execution_scope.h"
#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/target.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/common/err_or.h"
#include "src/developer/debug/zxdb/debug_adapter/data_id.h"
#include "src/developer/debug/zxdb/expr/expr.h"
#include "src/developer/debug/zxdb/expr/expr_value.h"
#include "src/developer/debug/zxdb/expr/format.h"
#include "src/developer/debug/zxdb/expr/format_node.h"
#include "src/developer/debug/zxdb/expr/resolve_ptr_ref.h"
#include "src/developer/debug/zxdb/symbols/function.h"
#include "src/developer/debug/zxdb/symbols/variable.h"
#include "src/lib/fxl/strings/string_number_conversions.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace zxdb {

namespace {

using DataBreakpointInfoCallback =
    fit::function<void(dap::ResponseOrError<dap::DataBreakpointInfoResponse>)>;

void ProcessEvaluatedValue(const fxl::WeakPtr<DebugAdapterContext>& weak_ctx, uint64_t process_koid,
                           const std::string& name, const ErrOrValue& result,
                           DataBreakpointInfoCallback callback) {
  // Every path out of this function must invoke `callback`. The DAP session only writes a response
  // when the callback is called, so dropping it leaves the client waiting forever.
  if (!weak_ctx) {
    callback(dap::Error("Debug adapter context was destroyed."));
    return;
  }

  if (result.has_error()) {
    callback(dap::Error(result.err().msg()));
    return;
  }

  ErrOr<DataId> data_id =
      DataId::FromExprValue(weak_ctx->session()->arch(), process_koid, name, result.value());
  if (data_id.has_error()) {
    dap::DataBreakpointInfoResponse response;
    response.dataId = dap::null();
    response.description = data_id.err().msg();
    callback(response);
    return;
  }

  dap::DataBreakpointInfoResponse response;
  response.dataId = data_id.value().ToString();
  response.description = fxl::StringPrintf("%s (0x%" PRIx64 ", %u bytes)", name.c_str(),
                                           data_id.value().address(), data_id.value().byte_size());
  response.accessTypes = dap::array<dap::DataBreakpointAccessType>{"write", "readWrite"};
  response.canPersist = false;
  callback(response);
}

void ResolveChildVariable(DebugAdapterContext* ctx, VariablesRecord* record, Frame* frame,
                          uint64_t process_koid, const std::string& name,
                          DataBreakpointInfoCallback callback) {
  FormatNode* parent_node = record->parent ? record->parent.get() : record->child.get();
  if (!parent_node) {
    callback(dap::Error("No node pointer for variable."));
    return;
  }
  FormatNode* target_child = nullptr;
  for (const auto& child : parent_node->children()) {
    if (child->name() == name) {
      target_child = child.get();
      break;
    }
  }
  if (!target_child) {
    callback(dap::Error(fxl::StringPrintf("Child variable '%s' not found.", name.c_str())));
    return;
  }

  if (target_child->state() == FormatNode::kUnevaluated) {
    auto weak_child = target_child->GetWeakPtr();
    auto weak_ctx = ctx->GetWeakPtr();
    auto eval_ctx = frame->GetEvalContext();
    FillFormatNodeValue(
        target_child, eval_ctx,
        fit::defer_callback([weak_ctx, weak_child, eval_ctx, process_koid, name,
                             cb = std::move(callback)]() mutable {
          if (!weak_ctx || !weak_child) {
            cb(dap::Error("Variable went out of scope while it was being evaluated."));
            return;
          }
          if (weak_child->err().has_error()) {
            ProcessEvaluatedValue(weak_ctx, process_koid, name, weak_child->err(), std::move(cb));
            return;
          }
          EnsureResolveReference(
              eval_ctx, weak_child->value(),
              [weak_ctx, process_koid, name, cb = std::move(cb)](const ErrOrValue& val) mutable {
                ProcessEvaluatedValue(weak_ctx, process_koid, name, val, std::move(cb));
              });
        }));
    return;
  }

  if (target_child->err().has_error()) {
    ProcessEvaluatedValue(ctx->GetWeakPtr(), process_koid, name, target_child->err(),
                          std::move(callback));
  } else {
    EnsureResolveReference(frame->GetEvalContext(), target_child->value(),
                           [weak_ctx = ctx->GetWeakPtr(), process_koid, name,
                            cb = std::move(callback)](const ErrOrValue& val) mutable {
                             ProcessEvaluatedValue(weak_ctx, process_koid, name, val,
                                                   std::move(cb));
                           });
  }
}

void ResolveFunctionParameter(DebugAdapterContext* ctx, Frame* frame, uint64_t process_koid,
                              const std::string& name, DataBreakpointInfoCallback callback) {
  const Location& location = frame->GetLocation();
  if (!location.symbol()) {
    callback(dap::Error("There is no symbol information for the frame."));
    return;
  }
  const Function* function = location.symbol().Get()->As<Function>();
  if (!function) {
    callback(dap::Error("Symbols are corrupt."));
    return;
  }
  fxl::RefPtr<Variable> target_param;
  if (!name.empty()) {
    for (const auto& param : function->parameters()) {
      const Variable* var = param.Get()->As<Variable>();
      if (var && var->GetAssignedName() == name) {
        target_param = RefPtrTo(var);
        break;
      }
    }
  }
  if (!target_param) {
    callback(dap::Error(fxl::StringPrintf("Function parameter '%s' not found.", name.c_str())));
    return;
  }
  auto eval_ctx = frame->GetEvalContext();
  eval_ctx->GetVariableValue(
      std::move(target_param), [weak_ctx = ctx->GetWeakPtr(), eval_ctx, process_koid, name,
                                cb = std::move(callback)](const ErrOrValue& result) mutable {
        if (result.has_error()) {
          ProcessEvaluatedValue(weak_ctx, process_koid, name, result, std::move(cb));
          return;
        }
        EnsureResolveReference(
            eval_ctx, result.value(),
            [weak_ctx, process_koid, name, cb = std::move(cb)](const ErrOrValue& resolved) mutable {
              ProcessEvaluatedValue(weak_ctx, process_koid, name, resolved, std::move(cb));
            });
      });
}

// Returns the KOID of the process owning `frame`'s thread, or an `Err` if the thread is not
// stopped.
ErrOr<uint64_t> ProcessKoidForStoppedFrame(DebugAdapterContext* ctx, Frame* frame) {
  if (Err err = ctx->CheckStoppedThread(frame->GetThread()); err.has_error()) {
    return err;
  }
  Process* process = frame->GetThread()->GetProcess();
  FX_DCHECK(process);
  uint64_t koid = process->GetKoid();
  FX_DCHECK(koid != 0);
  return koid;
}

void HandleVariablesReferenceRequest(DebugAdapterContext* ctx,
                                     const dap::DataBreakpointInfoRequest& req,
                                     DataBreakpointInfoCallback callback) {
  auto* record = ctx->VariablesRecordForID(req.variablesReference.value());
  if (!record) {
    callback(dap::Error("Invalid variables reference."));
    return;
  }
  auto* frame = ctx->FrameforId(record->frame_id);
  if (!frame) {
    callback(dap::Error("Invalid frame for variables reference."));
    return;
  }
  ErrOr<uint64_t> koid_or = ProcessKoidForStoppedFrame(ctx, frame);
  if (koid_or.has_error()) {
    callback(dap::Error(koid_or.err().msg()));
    return;
  }
  uint64_t process_koid = koid_or.value();

  switch (record->type) {
    case VariablesType::kRegister: {
      dap::DataBreakpointInfoResponse response;
      response.dataId = dap::null();
      response.description = "Registers cannot be watched.";
      callback(response);
      return;
    }
    case VariablesType::kChildVariable:
      ResolveChildVariable(ctx, record, frame, process_koid, req.name, std::move(callback));
      return;
    case VariablesType::kArguments:
      ResolveFunctionParameter(ctx, frame, process_koid, req.name, std::move(callback));
      return;
    case VariablesType::kLocal:
      EvalExpression(req.name, frame->GetEvalContext(), /*follow_references=*/true,
                     [weak_ctx = ctx->GetWeakPtr(), process_koid, name = req.name,
                      cb = std::move(callback)](const ErrOrValue& result) mutable {
                       ProcessEvaluatedValue(weak_ctx, process_koid, name, result, std::move(cb));
                     });
      return;
    case VariablesType::kVariablesTypeCount:
      break;
  }
  callback(dap::Error("Invalid variables type."));
}

void HandleFrameExpressionRequest(DebugAdapterContext* ctx,
                                  const dap::DataBreakpointInfoRequest& req,
                                  DataBreakpointInfoCallback callback) {
  auto* frame = ctx->FrameforId(req.frameId.value());
  if (!frame) {
    callback(dap::Error("Invalid frame ID."));
    return;
  }
  ErrOr<uint64_t> koid_or = ProcessKoidForStoppedFrame(ctx, frame);
  if (koid_or.has_error()) {
    callback(dap::Error(koid_or.err().msg()));
    return;
  }
  uint64_t process_koid = koid_or.value();
  EvalExpression(req.name, frame->GetEvalContext(), /*follow_references=*/true,
                 [weak_ctx = ctx->GetWeakPtr(), process_koid, name = req.name,
                  cb = std::move(callback)](const ErrOrValue& result) mutable {
                   ProcessEvaluatedValue(weak_ctx, process_koid, name, result, std::move(cb));
                 });
}

}  // namespace

void OnRequestDataBreakpointInfo(
    DebugAdapterContext* ctx, const dap::DataBreakpointInfoRequest& req,
    fit::function<void(dap::ResponseOrError<dap::DataBreakpointInfoResponse>)> callback) {
  if (req.variablesReference.has_value() && req.variablesReference.value() != 0) {
    HandleVariablesReferenceRequest(ctx, req, std::move(callback));
  } else if (req.frameId.has_value()) {
    HandleFrameExpressionRequest(ctx, req, std::move(callback));
  } else {
    callback(dap::Error("Either variablesReference or frameId must be specified."));
  }
}

dap::ResponseOrError<dap::SetDataBreakpointsResponse> OnRequestSetDataBreakpoints(
    DebugAdapterContext* ctx, const dap::SetDataBreakpointsRequest& req) {
  // TODO(https://fxbug.dev/532512768): Diff requested data breakpoints against existing ones so
  // unchanged watchpoints in running processes are not removed and reinstalled.
  ctx->DeleteAllDataBreakpoints();

  dap::SetDataBreakpointsResponse response;
  for (const auto& request_bp : req.breakpoints) {
    ErrOr<DataId> data_id = DataId::Parse(request_bp.dataId);
    if (data_id.has_error()) {
      response.breakpoints.push_back(dap::Breakpoint{
          .message = data_id.err().msg(),
          .verified = false,
      });
      continue;
    }

    BreakpointSettings::Type type = BreakpointSettings::Type::kWrite;
    if (request_bp.accessType.has_value()) {
      const std::string& access = request_bp.accessType.value();
      if (access == "write") {
        type = BreakpointSettings::Type::kWrite;
      } else if (access == "readWrite") {
        type = BreakpointSettings::Type::kReadWrite;
      } else if (access == "read") {
        response.breakpoints.push_back(dap::Breakpoint{
            .message = "Read-only data breakpoints are not supported on this platform.",
            .verified = false,
        });
        continue;
      } else {
        response.breakpoints.push_back(dap::Breakpoint{
            .message = "Unknown accessType: " + access,
            .verified = false,
        });
        continue;
      }
    }

    if (request_bp.hitCondition.has_value()) {
      response.breakpoints.push_back(dap::Breakpoint{
          .message = "Hit conditions are not supported for data breakpoints.",
          .verified = false,
      });
      continue;
    }

    if (Err err = data_id.value().ValidateSize(ctx->session()->arch(), type); err.has_error()) {
      response.breakpoints.push_back(dap::Breakpoint{
          .message = err.msg(),
          .verified = false,
      });
      continue;
    }

    // The dataId encodes a raw address, which is only meaningful within the address space it was
    // evaluated in. Scope the watchpoint strictly to the process identified by `process_koid()`.
    Process* process = ctx->session()->system().ProcessFromKoid(data_id.value().process_koid());
    if (!process || !process->GetTarget()) {
      response.breakpoints.push_back(dap::Breakpoint{
          .message = "No active process to set a data breakpoint on.",
          .verified = false,
      });
      continue;
    }
    Target* target = process->GetTarget();

    Breakpoint* breakpoint = ctx->session()->system().CreateNewBreakpoint();
    BreakpointSettings settings;
    settings.type = type;
    settings.byte_size = data_id.value().byte_size();
    settings.scope = ExecutionScope(target);
    settings.locations = {InputLocation(data_id.value().address())};
    if (request_bp.condition.has_value()) {
      settings.condition = request_bp.condition.value();
    }
    breakpoint->SetSettings(settings);
    ctx->StoreDataBreakpoint(breakpoint);

    response.breakpoints.push_back(dap::Breakpoint{
        .id = ctx->IdForBreakpoint(breakpoint),
        .verified = (!breakpoint->GetLocations().empty()),
    });
  }

  return response;
}

}  // namespace zxdb
