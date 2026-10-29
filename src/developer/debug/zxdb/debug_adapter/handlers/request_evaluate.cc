// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_evaluate.h"

#include <string>
#include <utility>

#include <dap/session.h>

#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/console/command_utils.h"
#include "src/developer/debug/zxdb/debug_adapter/context.h"
#include "src/developer/debug/zxdb/expr/expr.h"
#include "src/developer/debug/zxdb/format/format.h"
#include "src/developer/debug/zxdb/symbols/type.h"
#include "src/lib/fxl/memory/ref_ptr.h"

namespace zxdb {

void OnRequestEvaluate(DebugAdapterContext* ctx, const dap::EvaluateRequest& req,
                       fit::function<void(dap::ResponseOrError<dap::EvaluateResponse>)> callback) {
  if (req.context.has_value() && *req.context == "hover") {
    callback(dap::Error("Hover evaluation is not supported."));
    return;
  }

  if (!req.frameId.has_value()) {
    callback(dap::Error("Frame ID is required"));
    return;
  }

  Frame* frame = ctx->FrameforId(*req.frameId);
  if (!frame) {
    callback(dap::Error("Invalid frame ID"));
    return;
  }

  if (Err err = ctx->CheckStoppedThread(frame->GetThread()); err.has_error()) {
    callback(dap::Error(err.msg()));
    return;
  }

  fxl::RefPtr<EvalContext> eval_context = frame->GetEvalContext();

  FormatBufferOptions options;
  options.wrapping = FormatBufferOptions::Wrapping::kSmart;
  options.verbosity = FormatBufferOptions::Verbosity::kMinimal;
  options.pointer_expand_depth = 1;
  options.max_depth = 16;
  if (Thread* thread = frame->GetThread(); thread && thread->GetProcess()) {
    options.num_format = GetNumberFormatForTarget(thread->GetProcess()->GetTarget());
  }

  // TODO(https://fxbug.dev/529329366): Populate variablesReference in EvaluateResponse so compound
  // values can be expanded via VariablesRequest.
  EvalExpression(req.expression, eval_context, /*follow_references=*/true,
                 [eval_context, options, cb = std::move(callback)](ErrOrValue value) mutable {
                   if (value.has_error()) {
                     cb(dap::Error(value.err().msg()));
                     return;
                   }

                   std::string type_str;
                   if (value.value().type()) {
                     type_str = value.value().type()->GetFullName();
                   }

                   fxl::RefPtr<AsyncOutputBuffer> out =
                       FormatValue(value.take_value(), options, eval_context);
                   if (out->is_complete()) {
                     dap::EvaluateResponse resp;
                     resp.result = out->DestructiveFlatten().AsString();
                     resp.type = std::move(type_str);
                     cb(resp);
                   } else {
                     out->SetCompletionCallback(
                         [out, cb = std::move(cb), type_str = std::move(type_str)]() mutable {
                           dap::EvaluateResponse resp;
                           resp.result = out->DestructiveFlatten().AsString();
                           resp.type = std::move(type_str);
                           cb(resp);
                         });
                   }
                 });
}

}  // namespace zxdb
