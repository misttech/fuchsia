// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_PERFORMANCE_TRACE_MANAGER_TRACE_MANAGER_H_
#define SRC_PERFORMANCE_TRACE_MANAGER_TRACE_MANAGER_H_

#include <fidl/fuchsia.tracing.controller/cpp/fidl.h>
#include <fidl/fuchsia.tracing.provider/cpp/fidl.h>
#include <fidl/fuchsia.tracing/cpp/fidl.h>
#include <lib/async/cpp/executor.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fidl/cpp/wire/channel.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/zx/socket.h>

#include <list>
#include <queue>
#include <variant>

#include "src/performance/trace_manager/config.h"
#include "src/performance/trace_manager/provider_connection.h"
#include "src/performance/trace_manager/trace_provider_bundle.h"
#include "src/performance/trace_manager/trace_session.h"

namespace tracing {

// forward decl, here to break mutual header dependency
class TraceManagerApp;
class TraceManager;

class TraceController : public fidl::Server<fuchsia_tracing_controller::Session> {
  friend TraceManager;

 public:
  TraceController(TraceManagerApp* app, std::unique_ptr<TraceSession> session);
  ~TraceController() override;

  void TerminateTracing(fit::closure cb);

  void OnAlert(const std::string& alert_name);

  // For testing.
  TraceSession* session() const { return session_.get(); }

 private:
  // |fuchsia_tracing_controller::Session| implementation.
  void StartTracing(StartTracingRequest& request, StartTracingCompleter::Sync& completer) override;
  void StopTracing(StopTracingRequest& request, StopTracingCompleter::Sync& completer) override;
  void WatchAlert(WatchAlertCompleter::Sync& completer) override;
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_tracing_controller::Session> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  void SendSessionStateEvent(fuchsia_tracing_controller::SessionState state);
  static fuchsia_tracing_controller::SessionState TranslateSessionState(TraceSession::State state);

 private:
  TraceManagerApp* const app_;

  // We only set this to false when aborting.
  bool write_results_on_terminate_ = true;

  std::unique_ptr<TraceSession> session_;
  std::queue<std::string> alerts_;
  std::queue<WatchAlertCompleter::Async> watch_alert_completers_;
};

class TraceManager : public fidl::Server<fuchsia_tracing_controller::Provisioner>,
                     public fidl::Server<fuchsia_tracing_provider::Registry> {
  friend TraceController;

 public:
  TraceManager(TraceManagerApp* app, Config config, async::Executor& executor);
  ~TraceManager() override;

  // For testing.
  TraceSession* session() const;
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_tracing_provider::Registry> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  void OnEmptyControllerSet();

 private:
  // |fuchsia_tracing_controller::Provisioner| implementation.
  void InitializeTracing(InitializeTracingRequest& request,
                         InitializeTracingCompleter::Sync& completer) override;
  void GetProviders(GetProvidersCompleter::Sync& completer) override;
  void GetKnownCategories(GetKnownCategoriesCompleter::Sync& completer) override;
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_tracing_controller::Provisioner> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  // Deprecated, but we need to support the old apis until all supported api levels drop support for
  // the api.
  void RegisterProvider(RegisterProviderRequest& request,
                        RegisterProviderCompleter::Sync& completer) override;
  // Deprecated, but we need to support the old apis until all supported api levels drop support for
  // the api.
  void RegisterProviderSynchronously(
      RegisterProviderSynchronouslyRequest& request,
      RegisterProviderSynchronouslyCompleter::Sync& completer) override;

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  void RegisterV2(RegisterV2Request& request, RegisterV2Completer::Sync& completer) override;
  void RegisterV2Synchronously(RegisterV2SynchronouslyRequest& request,
                               RegisterV2SynchronouslyCompleter::Sync& completer) override;
#endif

  void RegisterProviderWorker(fidl::ClientEnd<fuchsia_tracing_provider::Provider> provider,
                              uint64_t pid, const std::string& name);
  void RegisterProviderV2Worker(fidl::ClientEnd<fuchsia_tracing_provider::ProviderV2> provider,
                                uint64_t pid, const std::string& name);

  void CloseSession();

  TraceManagerApp* const app_;

  const Config config_;

  std::shared_ptr<TraceController> trace_controller_;
  uint32_t next_provider_id_ = 1u;
  std::list<std::variant<TraceProviderBundle, ProviderConnection>> providers_;
  async::Executor& executor_;

  TraceManager(const TraceManager&) = delete;
  TraceManager(TraceManager&&) = delete;
  TraceManager& operator=(const TraceManager&) = delete;
  TraceManager& operator=(TraceManager&&) = delete;
};

}  // namespace tracing

#endif  // SRC_PERFORMANCE_TRACE_MANAGER_TRACE_MANAGER_H_
