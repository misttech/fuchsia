// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/performance/trace_manager/app.h"

#include <lib/syslog/cpp/macros.h>

#include <utility>

namespace tracing {

TraceManagerApp::TraceManagerApp(std::unique_ptr<sys::ComponentContext> context, Config config,
                                 async::Executor& executor)
    : context_(std::move(context)),
      dispatcher_(executor.dispatcher()),
      trace_manager_(this, std::move(config), executor) {
  context_->outgoing()->AddProtocol<fuchsia_tracing_provider::Registry>(
      provider_registry_bindings_.CreateHandler(&trace_manager_, executor.dispatcher(),
                                                fidl::kIgnoreBindingClosure));
  context_->outgoing()->AddProtocol<fuchsia_tracing_controller::Provisioner>(
      provisioner_bindings_.CreateHandler(&trace_manager_, executor.dispatcher(),
                                          fidl::kIgnoreBindingClosure));

  FX_LOGS(DEBUG) << "TraceManager services registered";
}

void TraceManagerApp::AddSessionBinding(
    std::shared_ptr<TraceController> trace_session,
    fidl::ServerEnd<fuchsia_tracing_controller::Session> session_controller) {
  session_bindings_.AddBinding(dispatcher_, std::move(session_controller), trace_session.get(),
                               [trace_session](fidl::UnbindInfo) {});
  session_bindings_.set_empty_set_handler([this]() { trace_manager_.OnEmptyControllerSet(); });

  FX_LOGS(DEBUG) << "TraceController registered";
}

TraceManagerApp::~TraceManagerApp() = default;

}  // namespace tracing
