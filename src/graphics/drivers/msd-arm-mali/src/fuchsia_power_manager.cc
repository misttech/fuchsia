// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "fuchsia_power_manager.h"

#include <lib/driver/power/cpp/element-description-builder.h>
#include <lib/driver/power/cpp/power-support.h>
#include <lib/driver/power/cpp/types.h>
#include <lib/fit/defer.h>

FuchsiaPowerManager::FuchsiaPowerManager(Owner* owner) : owner_(owner) {}

bool FuchsiaPowerManager::Initialize(fdf::Namespace* incoming, inspect::Node& node,
                                     async_dispatcher_t* dispatcher) {
  if (!incoming) {
    return false;
  }

  lazy_ = node.CreateLazyValues("lazy_fuchsia_power_manager", [this] {
    inspect::Inspector inspector;
    inspector.GetRoot().CreateBool(kIsSystemSuspendingInspectNode, in_suspend_, &inspector);
    inspector.GetRoot().CreateBool(kPoweredOnInspectNode, powered_on_, &inspector);
    inspector.GetRoot().CreateBool(kPowerOnAfterSuspendInspectNode, power_on_after_suspend_,
                                   &inspector);
    return fpromise::make_ok_promise(std::move(inspector));
  });

  auto activity_governor = incoming->Connect<fuchsia_power_system::ActivityGovernor>();
  if (activity_governor.is_error() || !activity_governor->is_valid()) {
    MAGMA_LOG(ERROR, "Failed to connect to system activity governor: %s",
              activity_governor.status_string());
    return false;
  }

  auto [sag_client_end, sag_server_end] =
      fidl::Endpoints<fuchsia_power_system::SuspendBlocker>::Create();
  fidl::Arena arena;
  std::string suspend_blocker_name = kSuspendBlockerName;

  fidl::WireResult result =
      fidl::WireCall(activity_governor.value())
          ->RegisterSuspendBlocker(
              fuchsia_power_system::wire::ActivityGovernorRegisterSuspendBlockerRequest::Builder(
                  arena)
                  .name(fidl::StringView::FromExternal(suspend_blocker_name))
                  .suspend_blocker(std::move(sag_client_end))
                  .Build());

  if (!result.ok() || result->is_error()) {
    MAGMA_LOG(
        ERROR,
        "Failed to register suspend blocker: %s. Perhaps the product does not have Power Framework?",
        result.status_string());
    return false;
  }

  suspend_blocker_binding_ = fidl::BindServer(dispatcher, std::move(sag_server_end), this);
  return true;
}

TimeoutSource::Clock::time_point FuchsiaPowerManager::GetCurrentTimeoutPoint() {
  // If we are off or going off there's no timeout.
  if (!powered_on_ || powering_down_) {
    return Clock::time_point::max();
  }
  return owner_->GetPowerManager()->GetGpuPowerdownTimeout();
}

void FuchsiaPowerManager::EnablePower() {
  // Do nothing if we are on or in the process of being on.
  if (powered_on_ || powering_up_) {
    return;
  }
  // Do nothing if we are in suspend, we will power back on during resume.
  if (in_suspend_) {
    power_on_after_suspend_ = true;
    return;
  }
  PowerUp([]() {});
}

void FuchsiaPowerManager::DisablePower() {
  // Do nothing if we are off or in the process of being off.
  if (!powered_on_ || powering_down_) {
    return;
  }
  PowerDown([]() {});
}

void FuchsiaPowerManager::PowerUp(fit::closure callback) {
  powering_up_ = true;
  owner_->PostPowerStateChange(true,
                               [this, callback = std::move(callback)](bool powered_on) mutable {
                                 powering_up_ = false;
                                 powered_on_ = powered_on;
                                 callback();
                               });
}

void FuchsiaPowerManager::PowerDown(fit::closure callback) {
  powering_down_ = true;
  owner_->PostPowerStateChange(false,
                               [this, callback = std::move(callback)](bool powered_on) mutable {
                                 powering_down_ = false;
                                 powered_on_ = powered_on;
                                 callback();
                               });
}

void FuchsiaPowerManager::AfterResume(AfterResumeCompleter::Sync& completer) {
  in_suspend_ = false;
  if (power_on_after_suspend_) {
    PowerUp([completer = completer.ToAsync()]() mutable { completer.Reply(); });
  }
  power_on_after_suspend_ = false;
}

void FuchsiaPowerManager::BeforeSuspend(BeforeSuspendCompleter::Sync& completer) {
  in_suspend_ = true;
  power_on_after_suspend_ = powered_on_;
  PowerDown([completer = completer.ToAsync()]() mutable { completer.Reply(); });
}

void FuchsiaPowerManager::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_power_system::SuspendBlocker> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  MAGMA_LOG(WARNING, "Encountered unexpected method: %lu", metadata.method_ordinal);
}
