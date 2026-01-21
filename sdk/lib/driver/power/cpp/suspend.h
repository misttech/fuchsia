// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_POWER_CPP_SUSPEND_H_
#define LIB_DRIVER_POWER_CPP_SUSPEND_H_

#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/incoming/cpp/namespace.h>
#include <lib/zx/result.h>

#include <optional>
#include <type_traits>

namespace fdf_power {

namespace internal {
zx::result<fidl::ServerEnd<fuchsia_power_system::SuspendBlocker>> RegisterSuspendHooks(
    fdf::Namespace& incoming, std::string_view name);
}

// This class is a wrapper for a callback type that must be called into exactly once
// before destruction. It is a move only type.
class Completer {
 public:
  explicit Completer(fit::callback<void()> callback) : callback_(std::move(callback)) {}

  Completer(Completer&& other) noexcept : callback_(std::move(other.callback_)) {
    other.callback_ = std::nullopt;
  }

  Completer(const Completer&) = delete;
  Completer& operator=(const Completer&) = delete;

  ~Completer();

  // Calls the wrapped callback function.
  // This method should not be invoked more than once.
  void operator()();

 private:
  std::optional<fit::callback<void()>> callback_;
};

// This is the completer for the Suspend operation in |Suspendable|.
class SuspendCompleter final : public Completer {
 public:
  using Completer::Completer;
  using Completer::operator();
};

// This is the completer for the Resume operation in |Suspendable|.
class ResumeCompleter final : public Completer {
 public:
  using Completer::Completer;
  using Completer::operator();
};

template <typename Driver>
class Suspendable {
 public:
  // Interface to be implemented.
  virtual void Suspend(SuspendCompleter completer) = 0;
  virtual void Resume(ResumeCompleter completer) = 0;
  virtual bool SuspendEnabled() = 0;

  explicit Suspendable() : server_(this) {
    static_cast<Driver*>(this)->RegisterInitMethods(
        fit::bind_member(this, &Suspendable::RegisterSuspendHooks));
  }

  // Returns true if:
  //   * suspend was enabled and we did one of the following
  //     a) got a value from `Driver::take_power_element_runner`
  //     b) successfully registered with SAG as a `fuchsia.power.system/SuspendBlocker`.
  //   * suspend was not enabled
  // Returns false if suspend was enabled and we failed to register with SAG.
  bool SuspendActive() { return binding_.index() != 0; }

  virtual ~Suspendable() = default;

 private:
  class Server : public fidl::Server<fuchsia_power_system::SuspendBlocker>,
                 public fidl::Server<fuchsia_power_broker::ElementRunner> {
   public:
    explicit Server(Suspendable<Driver>* parent) : parent_(parent) {}

   private:
    void BeforeSuspend(BeforeSuspendCompleter::Sync& completer) override {
      parent_->Suspend(
          SuspendCompleter([completer = completer.ToAsync()]() mutable { completer.Reply(); }));
    }

    void AfterResume(AfterResumeCompleter::Sync& completer) override {
      parent_->Resume(
          ResumeCompleter([completer = completer.ToAsync()]() mutable { completer.Reply(); }));
    }

    void SetLevel(SetLevelRequest& request, SetLevelCompleter::Sync& completer) override {
      if (request.level() > 0) {
        // Log if we receive a level we don't expect. Accept this level though because it provides
        // a transition mechanism for adding new levels without needing to change existing drivers.
        if (request.level() != 1) {
          fdf::warn("Level {} mapped to 1 since that is the maximum level.", request.level());
        }

        first_activation_occurred_ = true;
        parent_->Resume(
            ResumeCompleter([completer = completer.ToAsync()]() mutable { completer.Reply(); }));
      } else {
        if (first_activation_occurred_) {
          parent_->Suspend(
              SuspendCompleter([completer = completer.ToAsync()]() mutable { completer.Reply(); }));
        } else {
          completer.Reply();
        }
      }
    }
    void handle_unknown_method(
        fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementRunner> metadata,
        fidl::UnknownMethodCompleter::Sync& completer) override {}

    void handle_unknown_method(
        fidl::UnknownMethodMetadata<fuchsia_power_system::SuspendBlocker> metadata,
        fidl::UnknownMethodCompleter::Sync& completer) override {}

    Suspendable<Driver>* parent_;

    // Whether or not the power element has been set to a non-zero level for the first time. This
    // is necessary mostly because currently there is no way to create a power element at level
    // other than zero. We use this to avoid spurious transitions to level zero when the element
    // is created.
    bool first_activation_occurred_ = false;
  };

  // This call does one of three things
  //   1) If suspend is not enabled based on `SuspendEnabled`, it returns `zx::ok` immediately.
  //   2) If `Driver::take_power_element_runner` returns a value it uses calls to `SetLevel` to
  //      levels 0 and 1 to drive calls to `BeforeSuspend` and `AfterResume`, respectively.
  //   3) If neither (1) nor (2), it registers a `fuchsia.power.system/SuspendBlocker` with the
  //      `ActivityGovernor` protocol.
  zx::result<> RegisterSuspendHooks(async_dispatcher_t* dispatcher, fdf::Namespace& incoming,
                                    std::string_view name) {
    if (!SuspendEnabled()) {
      return zx::ok();
    }

    std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>> runner =
        static_cast<Driver*>(this)->take_power_element_runner();
    if (runner.has_value()) {
      binding_.emplace<fidl::ServerBinding<fuchsia_power_broker::ElementRunner>>(
          dispatcher, std::move(runner.value()), &server_, fidl::kIgnoreBindingClosure);
      return zx::ok();
    }

    zx::result server_end = internal::RegisterSuspendHooks(incoming, name);
    if (server_end.is_error()) {
      return server_end.take_error();
    }

    binding_.emplace<fidl::ServerBinding<fuchsia_power_system::SuspendBlocker>>(
        dispatcher, std::move(server_end.value()), &server_, fidl::kIgnoreBindingClosure);

    return zx::ok();
  }

  Server server_;
  std::variant<std::monostate, fidl::ServerBinding<fuchsia_power_system::SuspendBlocker>,
               fidl::ServerBinding<fuchsia_power_broker::ElementRunner>>
      binding_;
};

}  // namespace fdf_power

#endif
