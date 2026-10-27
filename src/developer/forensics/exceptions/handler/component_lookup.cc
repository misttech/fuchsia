// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/exceptions/handler/component_lookup.h"

#include <fidl/fuchsia.driver.crash/cpp/fidl.h>
#include <fidl/fuchsia.sys2/cpp/fidl.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fpromise/result.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include "src/developer/forensics/utils/errors.h"
#include "src/developer/forensics/utils/promise_timeout.h"
#include "src/lib/fidl/contrib/fpromise/client.h"

namespace forensics {
namespace exceptions {
namespace handler {
namespace {

// LINT.IfChange
constexpr std::string_view kDriverHostPattern = "bootstrap/driver-hosts:";
// LINT.ThenChange(//src/devices/bin/driver_framework/meta/driver_framework.bootstrap_shard.cml)

::fpromise::promise<ComponentInfo> GetInfo(async_dispatcher_t* dispatcher,
                                           const std::shared_ptr<sys::ServiceDirectory>& services,
                                           zx::duration timeout, zx_koid_t thread_koid) {
  using FindComponentByThreadKoidErrors =
      fidl::ErrorsIn<fuchsia_sys2::CrashIntrospect::FindComponentByThreadKoid>;

  auto [client_end, server_end] = fidl::Endpoints<fuchsia_sys2::CrashIntrospect>::Create();
  services->Connect(fidl::DiscoverableProtocolName<fuchsia_sys2::CrashIntrospect>,
                    server_end.TakeChannel());
  fidl::Client<fuchsia_sys2::CrashIntrospect> client(std::move(client_end), dispatcher);

  ::fpromise::promise<ComponentInfo, Error> find_component_promise =
      fidl_fpromise::as_promise(client->FindComponentByThreadKoid({thread_koid}))
          .or_else([](FindComponentByThreadKoidErrors& error) {
            if (error.is_framework_error()) {
              FX_LOGS(WARNING) << "Failed FindComponentByThreadKoid, error: " << error;
              return ::fpromise::error(FidlErrorToForensicsError(error.framework_error()));
            }

            // RESOURCE_NOT_FOUND most likely means a thread from a process outside a component,
            // which is not an error.
            if (error.domain_error() == fuchsia_component::Error::kResourceNotFound) {
              return ::fpromise::error(Error::kMissingValue);
            }

            FX_LOGS(WARNING) << "Failed FindComponentByThreadKoid, error: " << error;
            return ::fpromise::error(Error::kBadValue);
          })
          .and_then(
              [](const fuchsia_sys2::CrashIntrospectFindComponentByThreadKoidResponse& response) {
                const fuchsia_sys2::ComponentCrashInfo& info = response.info();
                std::string moniker = info.moniker().value_or("");
                if (!moniker.empty() && moniker[0] == '/') {
                  moniker = moniker.substr(1);
                }
                return ::fpromise::ok(ComponentInfo{
                    .url = info.url().value_or(""),
                    .realm_path = "",
                    .moniker = moniker,
                });
              });

  // Keep |client| alive until the flow completes.
  return MakePromiseTimeout<ComponentInfo>(std::move(find_component_promise), dispatcher, timeout)
      .then([client = std::move(client)](::fpromise::result<ComponentInfo, Error>& result)
                -> ::fpromise::result<ComponentInfo> {
        if (result.is_error()) {
          return ::fpromise::error();
        }

        return ::fpromise::ok(result.take_value());
      });
}

::fpromise::promise<ComponentInfo> GetDriverInfo(
    async_dispatcher_t* dispatcher,
    fidl::Client<fuchsia_driver_crash::CrashIntrospect>& driver_crash_introspect,
    zx::duration timeout, zx_koid_t process_koid, zx_koid_t thread_koid, ComponentInfo& fallback) {
  using FindDriverCrashErrors =
      fidl::ErrorsIn<fuchsia_driver_crash::CrashIntrospect::FindDriverCrash>;

  ::fpromise::promise<ComponentInfo, Error> find_driver_crash_promise =
      fidl_fpromise::as_promise(
          driver_crash_introspect->FindDriverCrash({process_koid, thread_koid}))
          .or_else([](FindDriverCrashErrors& error) {
            FX_LOGS(INFO) << "Failed FindDriverCrash for driver: " << error;
            return fpromise::error(Error::kConnectionError);
          })
          .and_then([](const fuchsia_driver_crash::CrashIntrospectFindDriverCrashResponse& result) {
            return fpromise::ok(ComponentInfo{
                .url = result.info().url().value_or(""),
                .realm_path = "",
                .moniker = result.info().node_moniker().value_or(""),
            });
          });

  return MakePromiseTimeout<ComponentInfo>(std::move(find_driver_crash_promise), dispatcher,
                                           timeout)
      .or_else([fallback](Error& result) {
        FX_LOGS(INFO) << "Falling back to component attribution. Error: " << ToString(result);
        return fpromise::ok(fallback);
      });
}

}  // namespace

::fpromise::promise<ComponentInfo> GetComponentInfo(
    async_dispatcher_t* dispatcher, std::shared_ptr<sys::ServiceDirectory> services,
    fidl::Client<fuchsia_driver_crash::CrashIntrospect>& driver_crash_introspect,
    const zx::duration timeout, zx_koid_t process_koid, zx_koid_t thread_koid) {
  return GetInfo(dispatcher, services, timeout, thread_koid)
      .or_else([]() {
        FX_LOGS(INFO) << "Failed FindComponentByThreadKoid, crash will lack component attribution";
        return ::fpromise::error();
      })
      .and_then(
          [dispatcher, services, timeout, process_koid, thread_koid,
           &driver_crash_introspect](ComponentInfo& info) -> ::fpromise::promise<ComponentInfo> {
            // Drivers will crash under the driver host, we can attribute to the specific driver
            // that crashed using the driver specific CrashIntrospect.
            if (info.moniker.starts_with(kDriverHostPattern) && driver_crash_introspect) {
              return GetDriverInfo(dispatcher, driver_crash_introspect, timeout, process_koid,
                                   thread_koid, info);
            }

            return ::fpromise::make_ok_promise(info);
          });
}

}  // namespace handler
}  // namespace exceptions
}  // namespace forensics
