// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_CONSOLE_COMMAND_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_CONSOLE_COMMAND_H_

#include <lib/fit/function.h>

#include <dap/protocol.h>
#include <dap/session.h>
#include <dap/typeof.h>
#include <dap/types.h>

namespace dap {

struct ZxdbConsoleCommandResponse : public Response {
  string output;
};

DAP_DECLARE_STRUCT_TYPEINFO(ZxdbConsoleCommandResponse);

struct ZxdbConsoleCommandRequest : public Request {
  using Response = ZxdbConsoleCommandResponse;
  string command;
  optional<integer> frameId;
};

DAP_DECLARE_STRUCT_TYPEINFO(ZxdbConsoleCommandRequest);

}  // namespace dap

namespace zxdb {

class DebugAdapterContext;

void OnRequestZxdbConsoleCommand(
    DebugAdapterContext* ctx, const dap::ZxdbConsoleCommandRequest& req,
    fit::function<void(dap::ResponseOrError<dap::ZxdbConsoleCommandResponse>)> callback);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_CONSOLE_COMMAND_H_
