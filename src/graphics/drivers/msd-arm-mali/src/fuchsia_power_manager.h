// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DRIVERS_MSD_ARM_MALI_SRC_FUCHSIA_POWER_MANAGER_H_
#define SRC_GRAPHICS_DRIVERS_MSD_ARM_MALI_SRC_FUCHSIA_POWER_MANAGER_H_

#include <fidl/fuchsia.power.system/cpp/fidl.h>
#include <lib/inspect/cpp/inspect.h>

#include <optional>

#include "parent_device.h"
#include "power_manager.h"
#include "timeout_source.h"

class FuchsiaPowerManager final : public TimeoutSource,
                                  public fidl::WireServer<fuchsia_power_system::SuspendBlocker> {
 public:
  class Owner {
   public:
    using PowerStateCallback = fit::callback<void(bool)>;
    virtual void PostPowerStateChange(bool enabled, PowerStateCallback completer) = 0;
    virtual PowerManager* GetPowerManager() = 0;
  };

  explicit FuchsiaPowerManager(Owner* owner);

  bool Initialize(
      fdf::Namespace* incoming, inspect::Node& node,
      async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher());

  TimeoutSource::Clock::time_point GetCurrentTimeoutPoint() override;
  void TimeoutTriggered() override { DisablePower(); }

  void EnablePower();
  void DisablePower();

  // fuchsia.power.system/SuspendBlocker implementation.
  void AfterResume(AfterResumeCompleter::Sync& completer) override;
  void BeforeSuspend(BeforeSuspendCompleter::Sync& completer) override;
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_power_system::SuspendBlocker> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  static constexpr char kIsSystemSuspendingInspectNode[] = "is_system_suspending";
  static constexpr char kPoweredOnInspectNode[] = "powered_on";
  static constexpr char kPowerOnAfterSuspendInspectNode[] = "power_on_after_suspend";
  static constexpr char kSuspendBlockerName[] = "mali-gpu-suspend-blocker";

 private:
  void PowerUp(fit::closure callback);
  void PowerDown(fit::closure callback);

  Owner* owner_;
  // Our current power state.
  bool powered_on_ = false;

  bool powering_up_ = false;
  bool powering_down_ = false;

  // Whether the device is currently in suspend. While this is true, any power on requests
  // will be queued.
  bool in_suspend_ = false;

  // Whether we should power on or not after suspend. This is set based on the state we were
  // in when we went into suspend, and any requests we get during suspend.
  bool power_on_after_suspend_ = false;

  inspect::LazyNode lazy_;
  std::optional<fidl::ServerBindingRef<fuchsia_power_system::SuspendBlocker>>
      suspend_blocker_binding_;
};

#endif  // SRC_GRAPHICS_DRIVERS_MSD_ARM_MALI_SRC_FUCHSIA_POWER_MANAGER_H_
