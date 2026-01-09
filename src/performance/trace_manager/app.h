// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_PERFORMANCE_TRACE_MANAGER_APP_H_
#define SRC_PERFORMANCE_TRACE_MANAGER_APP_H_

#include <fidl/fuchsia.tracing.controller/cpp/fidl.h>
#include <fidl/fuchsia.tracing.provider/cpp/fidl.h>
#include <lib/fidl/cpp/wire/channel.h>
#include <lib/sys/cpp/component_context.h>

#include <memory>

#include "src/performance/trace_manager/trace_manager.h"

namespace tracing {

class TraceManagerApp {
 public:
  TraceManagerApp(std::unique_ptr<sys::ComponentContext> context, Config config,
                  async::Executor& executor);
  ~TraceManagerApp();

  void AddSessionBinding(std::shared_ptr<TraceController> trace_session,
                         fidl::ServerEnd<fuchsia_tracing_controller::Session> session_controller);

  void CloseSessionBindings() { session_bindings_.CloseAll(ZX_ERR_PEER_CLOSED); }

  // For testing.
  sys::ComponentContext* context() const { return context_.get(); }
  const TraceManager* trace_manager() const { return &trace_manager_; }

  fidl::ServerBindingGroup<fuchsia_tracing_controller::Session>& session_bindings() {
    return session_bindings_;
  }

 private:
  std::unique_ptr<sys::ComponentContext> context_;

  async_dispatcher_t* dispatcher_;
  TraceManager trace_manager_;

  fidl::ServerBindingGroup<fuchsia_tracing_provider::Registry> provider_registry_bindings_;
  fidl::ServerBindingGroup<fuchsia_tracing_controller::Provisioner> provisioner_bindings_;
  fidl::ServerBindingGroup<fuchsia_tracing_controller::Session> session_bindings_;

  TraceManagerApp(const TraceManagerApp&) = delete;
  TraceManagerApp(TraceManagerApp&&) = delete;
  TraceManagerApp& operator=(const TraceManagerApp&) = delete;
  TraceManagerApp& operator=(TraceManagerApp&&) = delete;
};

}  // namespace tracing

#endif  // SRC_PERFORMANCE_TRACE_MANAGER_APP_H_
