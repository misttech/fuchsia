// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_REMOTE_UNWINDER_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_REMOTE_UNWINDER_H_

#include <lib/fit/function.h>

#include <vector>

#include "src/developer/debug/zxdb/client/stack.h"

namespace unwinder {
struct Frame;
class Registers;
}  // namespace unwinder

namespace zxdb {

class ThreadImpl;

// Wraps the unwinder library for host-side unwinding in zxdb client.
class RemoteUnwinder {
 public:
  explicit RemoteUnwinder(ThreadImpl* thread);

  // Unwinds the stack starting from the given registers.
  void UnwindWithRegisters(const unwinder::Registers& regs, const Stack::SyncFrameOptions& options,
                           fit::callback<void(std::vector<unwinder::Frame>)> on_done);

 private:
  ThreadImpl* thread_;
};

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_REMOTE_UNWINDER_H_
