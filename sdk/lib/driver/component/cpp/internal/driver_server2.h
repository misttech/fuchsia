// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_COMPONENT_CPP_INTERNAL_DRIVER_SERVER2_H_
#define LIB_DRIVER_COMPONENT_CPP_INTERNAL_DRIVER_SERVER2_H_

#include <fidl/fuchsia.driver.framework/cpp/driver/wire.h>
#include <fidl/fuchsia.driver.framework/cpp/type_conversions.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/start_completer.h>
#include <lib/driver/component/cpp/stop_completer.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/symbols/symbols.h>
#include <zircon/availability.h>

namespace fdf_internal {

// This will shim a |DriverBase| based driver with the new FIDL based registration.
template <typename DriverBaseImpl>
class DriverServer2 final : public fdf::WireServer<fuchsia_driver_framework::Driver> {
  static_assert(std::is_base_of_v<fdf::DriverBase2, DriverBaseImpl>,
                "The driver type must implement the fdf::DriverBase2 class.");

  static_assert(!std::is_abstract_v<DriverBaseImpl>,
                "The driver class must not be abstract. Try making it a final class to "
                "see the unimplemented pure virtual methods. Eg: "
                "class Driver final : public fdf::DriverBase2");

  static_assert(std::is_default_constructible_v<DriverBaseImpl>,
                "The driver must be default constructible from. ");

 public:
  // Initialize the fuchsia_driver_framework::Driver server.
  static void* initialize(fdf_handle_t server_handle) {
    fdf_dispatcher_t* dispatcher = fdf_dispatcher_get_current_dispatcher();
    DriverServer2* driver_server = new DriverServer2(dispatcher, server_handle);
    return driver_server;
  }

  // Destroy the fuchsia_driver_framework::Driver server.
  static void destroy(void* token) {
    DriverServer2* driver_server = static_cast<DriverServer2*>(token);
    delete driver_server;
  }

  DriverServer2(fdf_dispatcher_t* dispatcher, fdf_handle_t server_handle)
      : dispatcher_(dispatcher), driver_(std::make_unique<DriverBaseImpl>()) {
#if FUCHSIA_API_LEVEL_AT_LEAST(32)
    fdf_dispatcher_t* always_on_dispatcher = fdf_dispatcher_get_always_on_dispatcher(dispatcher_);
#else
    fdf_dispatcher_t* always_on_dispatcher = dispatcher_;
#endif
    binding_.emplace(always_on_dispatcher,
                     fdf::ServerEnd<fuchsia_driver_framework::Driver>(fdf::Channel(server_handle)),
                     this, fidl::kIgnoreBindingClosure);
  }

  ~DriverServer2() final = default;

  void Start(StartRequestView request, fdf::Arena& arena,
             StartCompleter::Sync& completer) override {
    fdf::DriverContext context(fidl::ToNatural(request->start_args));
    driver_->DriverBaseInternalInit(context, fdf::UnownedSynchronizedDispatcher(dispatcher_));

    fdf::StartCompleter start_completer(
        [reply_arena = std::move(arena), reply_completer = completer.ToAsync()](
            zx::result<> result) mutable { reply_completer.buffer(reply_arena).Reply(result); });

    // Post a task to do this so that the WireServerDispatcher, the caller of this method,
    // can clean up correctly. Otherwise the destruction of the arena from the callback could
    // run too early, causing use-after-frees during the cleanup of the request.
    async::PostTask(fdf_dispatcher_get_async_dispatcher(dispatcher_),
                    [this, context = std::move(context),
                     inner_completer = std::move(start_completer)]() mutable {
                      driver_->Start(std::move(context), std::move(inner_completer));
                    });
  }

  void Stop(fdf::Arena& arena, StopCompleter::Sync& completer) override {
    ZX_ASSERT(driver_);
    driver_->Stop(fdf::StopCompleter([this](zx::result<> result) { StopBinding(); }));
  }

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  void Suspend(fdf::Arena& arena, SuspendCompleter::Sync& completer) override {
    ZX_ASSERT(driver_);

    fdf::SuspendCompleter suspend_completer(
        [reply_arena = std::move(arena), reply_completer = completer.ToAsync()](
            zx::result<> result) mutable { reply_completer.buffer(reply_arena).Reply(result); });

    driver_->SystemSuspend(std::move(suspend_completer));
  }

  void Resume(ResumeRequestView request, fdf::Arena& arena,
              ResumeCompleter::Sync& completer) override {
    ZX_ASSERT(driver_);

    fdf::ResumeCompleter resume_completer(
        [reply_arena = std::move(arena), reply_completer = completer.ToAsync()](
            zx::result<> result) mutable { reply_completer.buffer(reply_arena).Reply(result); });

    std::optional<fuchsia_power_broker::LeaseToken> lease;
    if (request->power_element_lease.is_valid()) {
      lease.emplace(std::move(request->power_element_lease));
    }

    driver_->SystemResume(std::move(lease), std::move(resume_completer));
  }
#endif

  void HealthCheck(HealthCheckRequestView request, fdf::Arena& arena,
                   HealthCheckCompleter::Sync& completer) override {
    ZX_ASSERT(driver_);

    fuchsia_driver_framework::HealthCheckRequest level = request->request;

    fdf::DriverBase2::HealthCheckCompleter hc_completer(
        [guard =
             HealthCheckReplyGuard(driver_.get(), level, std::move(arena), completer.ToAsync())](
            fit::result<fuchsia_driver_framework::HealthCheckError,
                        fuchsia_driver_framework::HealthCheckResponse>
                result) mutable {
          // A reply is only needed once; a second invocation of the completer (or one after the
          // binding went away) must not touch Inspect or the binding.
          if (!guard.completer.is_reply_needed()) {
            return;
          }
          guard.driver->RecordHealthCheckToInspect(guard.level, result);
          if (result.is_ok()) {
            guard.completer.buffer(guard.arena)
                .ReplySuccess(fidl::ToWire(guard.arena, result.value()));
          } else {
            guard.completer.buffer(guard.arena).ReplyError(result.error_value());
          }
        });

    driver_->HealthCheck(level, std::move(hc_completer));
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_driver_framework::Driver> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    if (driver_) {
      FDF_LOGL(INFO, driver_->logger(), "fdf::Driver server received unknown method.");
    }
  }

  void* GetDriverBaseImpl() {
    if (driver_) {
      return driver_.get();
    }

    return nullptr;
  }

 private:
  // Ensures that if a custom `HealthCheck` implementation drops its `HealthCheckCompleter`
  // without invoking it (e.g., due to an early return or cancelled async task), the FIDL server
  // replies with `HealthCheckError::kInternalError` instead of closing the driver channel. The
  // error is also mirrored to Inspect so that a stale record from an earlier check does not
  // survive.
  struct HealthCheckReplyGuard {
    fdf::DriverBase2* driver;
    fuchsia_driver_framework::HealthCheckRequest level;
    fdf::Arena arena;
    HealthCheckCompleter::Async completer;

    HealthCheckReplyGuard(fdf::DriverBase2* d, fuchsia_driver_framework::HealthCheckRequest l,
                          fdf::Arena a, HealthCheckCompleter::Async c)
        : driver(d), level(l), arena(std::move(a)), completer(std::move(c)) {}
    HealthCheckReplyGuard(HealthCheckReplyGuard&&) noexcept = default;
    HealthCheckReplyGuard& operator=(HealthCheckReplyGuard&&) = delete;
    ~HealthCheckReplyGuard() {
      if (completer.is_reply_needed()) {
        driver->RecordHealthCheckToInspect(
            level, fit::error(fuchsia_driver_framework::HealthCheckError::kInternalError));
        completer.buffer(arena).ReplyError(
            fuchsia_driver_framework::wire::HealthCheckError::kInternalError);
      }
    }
  };

  void StopBinding() {
    if (fdf_dispatcher_get_current_dispatcher() == dispatcher_) {
      binding_.reset();
      return;
    }

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
    fdf_dispatcher_t* always_on_dispatcher = fdf_dispatcher_get_always_on_dispatcher(dispatcher_);
#else
    fdf_dispatcher_t* always_on_dispatcher = dispatcher_;
#endif
    async::PostTask(fdf_dispatcher_get_async_dispatcher(always_on_dispatcher),
                    [this]() { binding_.reset(); });
  }

  fdf_dispatcher_t* dispatcher_;

  // `driver_` is declared before `binding_` so that it is destroyed after it: tearing down the
  // binding first guarantees that no in-flight request (or a HealthCheckReplyGuard held by the
  // driver) can reach a partially destroyed driver.
  std::unique_ptr<fdf::DriverBase2> driver_;
  std::optional<fdf::ServerBinding<fuchsia_driver_framework::Driver>> binding_;
};

}  // namespace fdf_internal

#endif  // LIB_DRIVER_COMPONENT_CPP_INTERNAL_DRIVER_SERVER2_H_
