// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/async_backtrace_subscription.h"

#include <lib/syslog/cpp/macros.h>

#include "src/developer/debug/zxdb/client/async_task.h"
#include "src/developer/debug/zxdb/client/async_task_tree.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/thread.h"

namespace dap {

DAP_IMPLEMENT_STRUCT_TYPEINFO(AsyncTaskNode, "", DAP_FIELD(name, "name"),
                              DAP_FIELD(children, "children"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(AsyncBacktraceUpdate, "vscode-fuchsia.updateAsyncBacktrace",
                              DAP_FIELD(id, "id"), DAP_FIELD(name, "name"),
                              DAP_FIELD(tasks, "tasks"))

}  // namespace dap

namespace zxdb {

AsyncBacktraceSubscription::AsyncBacktraceSubscription(fxl::WeakPtr<Session> session,
                                                       std::shared_ptr<dap::Session> dap)
    : session_(std::move(session)), dap_(std::move(dap)), weak_factory_(this) {
  session_->thread_observers().AddObserver(this);
}

AsyncBacktraceSubscription::~AsyncBacktraceSubscription() {
  if (session_) {
    session_->thread_observers().RemoveObserver(this);
  }
}

void AsyncBacktraceSubscription::DidCreateThread(Thread* thread) {
  CancelPendingBacktrace(thread);

  // Notify DAP client that a new thread has been created.
  dap_->send(dap::AsyncBacktraceUpdate{
      .id = static_cast<int64_t>(thread->GetKoid()),
      .name = thread->GetName(),
      .tasks = dap::array<dap::AsyncTaskNode>(),
  });
}

void AsyncBacktraceSubscription::OnThreadStopped(Thread* thread, const StopInfo& info) {
  CancelPendingBacktrace(thread);

  // Syncing the async task tree is expensive, so skip syncing when no frames are available.
  if (!thread->CurrentStopSupportsFrames()) {
    dap_->send(dap::AsyncBacktraceUpdate{
        .id = static_cast<int64_t>(thread->GetKoid()),
        .name = thread->GetName(),
        .tasks = dap::array<dap::AsyncTaskNode>(),
    });
    return;
  }

  CollectAndReportAsyncBacktrace(thread);
}

void AsyncBacktraceSubscription::DidUpdateStackFrames(Thread* thread) {
  // Notify DAP client to reset this thread's async backtrace subtree, since it has resumed.
  if (!thread->CurrentStopSupportsFrames()) {
    CancelPendingBacktrace(thread);

    dap_->send(dap::AsyncBacktraceUpdate{
        .id = static_cast<int64_t>(thread->GetKoid()),
        .name = thread->GetName(),
        .tasks = dap::array<dap::AsyncTaskNode>(),
    });
    return;
  }
}

void AsyncBacktraceSubscription::WillDestroyThread(Thread* thread) {
  CancelPendingBacktrace(thread);

  // Notify DAP client to remove this thread from the async backtrace view.
  dap_->send(dap::AsyncBacktraceUpdate{.id = static_cast<int64_t>(thread->GetKoid()),
                                       .name = thread->GetName()});
}

void AsyncBacktraceSubscription::CancelPendingBacktrace(const Thread* thread) {
  if (auto it = pending_backtraces_.find(thread->GetKoid()); it != pending_backtraces_.end()) {
    it->second.Cancel();
    pending_backtraces_.erase(it);
  }
}

void AsyncBacktraceSubscription::CollectAndReportAsyncBacktrace(Thread* thread) {
  // Enqueue into `pending_backtraces_` before collecting the async backtrace into an
  // `AsyncBacktraceUpdate` DAP event, allowing this backtrace to be cancelled if the thread state
  // happens to change in quick succession (e.g. thread resumes, or thread is destroyed).
  pending_backtraces_.emplace(thread->GetKoid(), [weak_this = weak_factory_.GetWeakPtr(),
                                                  weak_thread = thread->GetWeakPtr()](
                                                     const Err& err, const Frame* /*frame*/) {
    if (!weak_this || !weak_thread) {
      return;
    }

    // A common source of errors is when async tasks aren't found (e.g. thread is not running async
    // code). In these cases, we report an empty task tree to the DAP client for this thread.
    if (err.has_error()) {
      FX_LOGS(DEBUG) << "Failed to collect async task tree for thread \"" << weak_thread->GetKoid()
                     << "\": " << err.msg();
      weak_this->dap_->send(dap::AsyncBacktraceUpdate{
          .id = static_cast<int64_t>(weak_thread->GetKoid()),
          .name = weak_thread->GetName(),
          .tasks = dap::array<dap::AsyncTaskNode>(),
      });
      return;
    }

    weak_this->dap_->send(dap::AsyncBacktraceUpdate{
        .id = static_cast<int64_t>(weak_thread->GetKoid()),
        .name = weak_thread->GetName(),
        .tasks = weak_thread->GetAsyncTaskTree().Map<dap::AsyncTaskNode>(
            [](const zxdb::AsyncTask& task, dap::AsyncTaskNode* node) {
              node->name = task.GetIdentifier().GetFullName();
            },
            [](dap::AsyncTaskNode* node) { return &node->children; }),
    });
  });
  thread->GetAsyncTaskTree().Sync(thread, pending_backtraces_[thread->GetKoid()].callback());
}

}  // namespace zxdb
