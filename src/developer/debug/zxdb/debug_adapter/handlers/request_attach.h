// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_ATTACH_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_ATTACH_H_

#include <dap/typeof.h>

#include "src/developer/debug/zxdb/debug_adapter/context.h"

namespace dap {

class AttachRequestZxdb : public AttachRequest {
 public:
  // Process KOID, process name, component name (.cm), or component moniker suffix to attach to.
  dap::string process;
  // Optional shell command to run in an integrated terminal after installing the attach filter.
  optional<dap::string> command;
  // Match child realms recursively when attaching to a component moniker.
  optional<dap::boolean> recursive;
  // Attach weakly so symbol loading is deferred until an exception occurs.
  optional<dap::boolean> weak;
  // Match the full process name rather than a substring.
  optional<dap::boolean> exact;
  // Attach only to the job rather than processes within it.
  optional<dap::boolean> job_only;
  // Current working directory for running the shell command.
  optional<string> cwd;
};

DAP_DECLARE_STRUCT_TYPEINFO(AttachRequestZxdb);

}  // namespace dap

namespace zxdb {

dap::ResponseOrError<dap::AttachResponse> OnRequestAttach(DebugAdapterContext* context,
                                                          const dap::AttachRequestZxdb& req);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_ATTACH_H_
