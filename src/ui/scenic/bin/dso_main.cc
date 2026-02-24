// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/incoming/cpp/service_member_watcher.h>
#include <lib/inspect/component/cpp/component.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/scheduler/role.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace-provider/provider.h>
#include <lib/zx/channel.h>
#include <lib/zx/thread.h>

#include <memory>

#include "src/lib/dso/cpp/async.h"
#include "src/lib/fxl/command_line.h"
#include "src/lib/fxl/log_settings_command_line.h"
#include "src/ui/lib/escher/vk/vulkan_instance.h"
#include "src/ui/scenic/bin/app.h"

int dso_main_async(int argc, const char* argv[], const char* envp[], zx_handle_t svc_handle,
                   zx_handle_t pkg_handle, zx_handle_t directory_request_handle,
                   zx_handle_t lifecycle_handle, zx_handle_t config_handle,
                   fdf_dispatcher_t* fdf_dispatcher) {
  if (svc_handle == ZX_HANDLE_INVALID || pkg_handle == ZX_HANDLE_INVALID ||
      directory_request_handle == ZX_HANDLE_INVALID || config_handle == ZX_HANDLE_INVALID ||
      lifecycle_handle == ZX_HANDLE_INVALID) {
    return 1;
  }
  zx::channel lifecycle(lifecycle_handle);

  async_dispatcher_t* const dispatcher = fdf_dispatcher_get_async_dispatcher(fdf_dispatcher);
  if (dispatcher == nullptr) {
    return 2;
  }
  async_set_default_dispatcher(dispatcher);

  // This call creates ComponentContext, but does not start serving immediately. Outgoing directory
  // is served by App, after App::InitializeServices() is completed.
  auto svc_dir = std::make_shared<sys::ServiceDirectory>(zx::channel(svc_handle));
  zx::channel pkg_dir(pkg_handle);
  zx::channel out_dir(directory_request_handle);
  zx::vmo config(config_handle);
  auto app_context = std::make_unique<sys::ComponentContext>(svc_dir, dispatcher);

  auto command_line = fxl::CommandLineFromArgcArgv(argc, argv);
  fxl::LogSettings base_settings;
  if (!ParseLogSettings(command_line, &base_settings)) {
    return 3;
  }

  zx::channel log_client, log_server;
  zx::channel::create(0, &log_client, &log_server);
  zx_status_t s = svc_dir->Connect("fuchsia.logger.LogSink", std::move(log_server));
  if (s != ZX_OK) {
    return 4;
  }
  fuchsia_logging::LogSettingsBuilder log_settings;
  log_settings.WithMinLogSeverity(base_settings.min_log_level);
  log_settings.WithDispatcher(nullptr);
  log_settings.WithLogSink(log_client.release());
  log_settings.DisableInterestListener();
  log_settings.BuildAndInitialize();
  FX_LOGS(INFO) << "Started";

  zx::channel trace_client, trace_server;
  zx::channel::create(0, &trace_client, &trace_server);
  s = svc_dir->Connect("fuchsia.tracing.provider.Registry", std::move(trace_server));
  if (s != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to create trace provider: " << zx_status_get_string(s);
    return 5;
  }
  auto* const trace_provider = new trace::TraceProvider{std::move(trace_client), dispatcher};

  // Set up an inspect::Node to inject into the App.
  auto [inspect_client, inspect_server] = *fidl::CreateEndpoints<fuchsia_inspect::InspectSink>();
  s = svc_dir->Connect("fuchsia.inspect.InspectSink", inspect_server.TakeChannel());
  if (s != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to create inspector: " << zx_status_get_string(s);
    return 6;
  }
  inspect::PublishOptions opts;
  opts.client_end.emplace(std::move(inspect_client));
  auto* const inspector = new inspect::ComponentInspector{dispatcher, std::move(opts)};

  component::SyncServiceMemberWatcher<fuchsia_hardware_display::Service::Provider> watcher(
      fidl::UnownedClientEnd<fuchsia_io::Directory>(svc_dir->unowned_channel()));
  zx::result<fidl::ClientEnd<fuchsia_hardware_display::Provider>> provider_result =
      watcher.GetNextInstance(/*stop_at_idle=*/false);
  if (provider_result.is_error()) {
    FX_LOGS(ERROR) << "Failed to connect to display provider: " << provider_result.status_string();
    return 7;
  }
  fidl::ClientEnd<fuchsia_hardware_display::Provider> provider = std::move(provider_result).value();
  auto display_coordinator_promise = display::GetCoordinator(std::move(provider));

  // Instantiate Scenic app.
  // TODO(https://fxbug.dev/485919515): Free `app` when the program terminates
  auto* const app =
      new scenic_impl::App{std::move(app_context),
                           fidl::ClientEnd<fuchsia_io::Directory>(std::move(pkg_dir)),
                           fidl::ServerEnd<fuchsia_io::Directory>(std::move(out_dir)),
                           std::move(config),
                           inspector->root().CreateChild("scenic"),
                           std::move(display_coordinator_promise),
                           [lifecycle = std::move(lifecycle), trace_provider, inspector]() mutable {
                             delete trace_provider;
                             delete inspector;
                             // Dropping `lifecycle` causes the component to exit.
                           }};

  // TODO(https://fxbug.dev/403545512): Figure out if we should include here or in dso_runner
  // fuchsia_scheduler::SetRoleForRootVmar("fuchsia.ui.scenic");

  return 0;
}
