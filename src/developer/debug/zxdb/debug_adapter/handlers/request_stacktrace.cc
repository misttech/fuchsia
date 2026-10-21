// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_stacktrace.h"

#include <algorithm>

#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/source_file_provider_impl.h"
#include "src/developer/debug/zxdb/client/target.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/console/command_utils.h"
#include "src/developer/debug/zxdb/debug_adapter/context.h"

namespace dap {
DAP_IMPLEMENT_STRUCT_TYPEINFO_EXT(StackFrameZxdb, StackFrame, "", DAP_FIELD(trust, "trust"))
DAP_IMPLEMENT_STRUCT_TYPEINFO(StackTraceResponseZxdb, "", DAP_FIELD(stackFrames, "stackFrames"),
                              DAP_FIELD(totalFrames, "totalFrames"))
DAP_IMPLEMENT_STRUCT_TYPEINFO_EXT(StackTraceRequestZxdb, StackTraceRequest, "stackTrace",
                                  DAP_FIELD(remoteUnwind, "remoteUnwind"),
                                  DAP_FIELD(unwinder, "unwinder"))
}  // namespace dap

namespace zxdb {
namespace {

dap::StackTraceResponseZxdb PopulateStackTraceResponse(DebugAdapterContext* ctx, Thread* thread,
                                                       const dap::StackTraceRequest& req) {
  dap::StackTraceResponseZxdb response;
  auto& stack = thread->GetStack();
  int64_t total_frames = static_cast<int64_t>(stack.size());
  int64_t start_frame = req.startFrame.value(0);

  // If levels is 0, DAP specifies to return all frames.
  int64_t frames_to_return = total_frames;
  if (req.levels && req.levels.value() > 0) {
    frames_to_return = req.levels.value();
  }

  // Clamp the end frame to the actual end of the stack.
  int64_t end_frame = std::min(start_frame + frames_to_return, total_frames);

  auto elided_frames = ctx->GetElidedFrames(stack);
  auto file_provider = SourceFileProviderImpl(thread->GetProcess()->GetTarget()->settings());
  for (auto i = start_frame; i < end_frame; i++) {
    dap::StackFrameZxdb frame;
    auto location = stack[i]->GetLocation();
    frame.source = dap::Source{};

    // Try to get the source path.
    auto file_metadata =
        file_provider.GetFileMetadata(location.file_line().file(), location.file_line().comp_dir());
    if (!file_metadata.has_error()) {
      frame.source->path = file_metadata.take_value().full_path;
    }

    frame.line = location.file_line().line();
    frame.column = location.column();
    frame.name = location.symbol().Get()->GetFullName();
    frame.id = ctx->IdForFrame(thread->GetKoid(), i);
    frame.trust = debug_ipc::StackFrame::TrustToString(stack[i]->GetTrust());
    if (elided_frames[i]) {
      frame.presentationHint = "subtle";
      frame.source->origin = elided_frames[i].description;
    }
    response.stackFrames.push_back(frame);
  }
  response.totalFrames = total_frames;
  return response;
}

}  // namespace

void OnRequestStackTrace(
    DebugAdapterContext* ctx, const dap::StackTraceRequestZxdb& req,
    std::function<void(dap::ResponseOrError<dap::StackTraceResponseZxdb>)> callback) {
  Thread* thread = ctx->GetThread(static_cast<uint64_t>(req.threadId));
  if (thread) {
    Stack::SyncFrameOptions options;
    if (req.remoteUnwind.value(false)) {
      options.remote_unwind = true;
      options.force_update = true;
    }
    if (req.unwinder) {
      debug_ipc::StackFrame::Trust trust;
      if (Err err = StringToTrust(req.unwinder.value(), &trust); err.has_error()) {
        callback(dap::Error(err.msg()));
        return;
      }
      options.forced_unwinder = trust;
      options.force_update = true;
    }
    thread->GetStack().EnsureFrames(
        options, [ctx, weak_thread = thread->GetWeakPtr(),
                  request = dap::StackTraceRequestZxdb(req), callback](const Err& err) {
          if (!err.has_error() && weak_thread) {
            callback(PopulateStackTraceResponse(ctx, weak_thread.get(), request));
          } else {
            callback(dap::Error("Thread exited, no frames."));
          }
        });
  } else {
    callback(dap::Error("Thread not found."));
  }
}

}  // namespace zxdb
