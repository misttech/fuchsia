// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_console_command.h"

#include <string>
#include <utility>
#include <vector>

#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/console/command_context.h"
#include "src/developer/debug/zxdb/debug_adapter/context.h"
#include "src/lib/fxl/memory/ref_ptr.h"

namespace dap {

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbConsoleCommandResponse, "", DAP_FIELD(output, "output"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbConsoleCommandRequest, "zxdb.ConsoleCommand",
                              DAP_FIELD(command, "command"), DAP_FIELD(frameId, "frameId"))

}  // namespace dap

namespace zxdb {

void OnRequestZxdbConsoleCommand(
    DebugAdapterContext* ctx, const dap::ZxdbConsoleCommandRequest& req,
    fit::function<void(dap::ResponseOrError<dap::ZxdbConsoleCommandResponse>)> callback) {
  if (req.command.empty()) {
    callback(dap::Error("Command cannot be empty"));
    return;
  }

  if (req.frameId.has_value()) {
    Frame* frame = ctx->FrameforId(req.frameId.value());
    if (!frame) {
      callback(dap::Error("Invalid frame ID"));
      return;
    }
    ctx->console()->context().SetActiveTarget(frame->GetThread()->GetProcess()->GetTarget());
    ctx->console()->context().SetActiveThreadForTarget(frame->GetThread());
    ctx->console()->context().SetActiveFrameForThread(frame);
  }

  ctx->console()->ProcessInputLine(
      req.command,
      fxl::MakeRefCounted<OfflineCommandContext>(
          ctx->console(),
          [cb = std::move(callback)](OutputBuffer output, std::vector<Err> errors) mutable {
            if (!errors.empty()) {
              std::string err_msg = output.AsString();
              if (err_msg.empty()) {
                err_msg = errors.front().msg();
              }
              cb(dap::Error(err_msg));
              return;
            }

            dap::ZxdbConsoleCommandResponse resp;
            resp.output = output.AsString();
            cb(resp);
          }),
      /*add_to_history=*/false);
}

}  // namespace zxdb
