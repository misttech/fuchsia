// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "hardware_battery_server.h"

#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/logging/cpp/logger.h>

namespace fake_battery {

zx_status_t HardwareBatteryServer::Init(const std::shared_ptr<fdf::OutgoingDirectory>& outgoing) {
  auto result = outgoing->AddService<fuchsia_hardware_power_battery::Service>(
      fuchsia_hardware_power_battery::Service::InstanceHandler({
          .battery =
              [this](fidl::ServerEnd<fuchsia_hardware_power_battery::Battery> server) {
                battery_bindings_.AddBinding(dispatcher_, std::move(server), this,
                                             fidl::kIgnoreBindingClosure);
              },
          .power_source =
              [this](fidl::ServerEnd<fuchsia_hardware_power_source::Source> server) {
                source_bindings_.AddBinding(dispatcher_, std::move(server), this,
                                            fidl::kIgnoreBindingClosure);
              },
      }));
  if (result.is_error()) {
    return result.status_value();
  }

  auto control_result = outgoing->AddService<test_hardwarepowercontrol::Service>(
      test_hardwarepowercontrol::Service::InstanceHandler({
          .control =
              [this](fidl::ServerEnd<test_hardwarepowercontrol::Control> server) {
                fdf::info("Control connection request received");
                control_bindings_.AddBinding(dispatcher_, std::move(server), this,
                                             fidl::kIgnoreBindingClosure);
              },
      }));
  return control_result.status_value();
}

void HardwareBatteryServer::GetSpec(ssource::GetSpecCompleter::Sync& completer) {
  completer.Reply(zx::ok(source_spec_));
}

void HardwareBatteryServer::GetStatus(ssource::GetStatusCompleter::Sync& completer) {
  if (battery_status_.source_status()) {
    completer.Reply(zx::ok(*battery_status_.source_status()));
  } else {
    completer.Reply(zx::error(fuchsia_hardware_power_source::Error::kInternal));
  }
}

void HardwareBatteryServer::SetRole(ssource::SetRoleRequest& request,
                                    ssource::SetRoleCompleter::Sync& completer) {
  completer.Reply(zx::error(fuchsia_hardware_power_source::Error::kNotSupported));
}

void HardwareBatteryServer::Watch(ssource::WatchRequest& request,
                                  ssource::WatchCompleter::Sync& completer) {
  completer.Close(ZX_ERR_NOT_SUPPORTED);
}

void HardwareBatteryServer::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_hardware_power_source::Source> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  fdf::error("Unknown method called: ordinal={}", metadata.method_ordinal);
}

void HardwareBatteryServer::GetSpec(sbattery::GetSpecCompleter::Sync& completer) {
  completer.Reply(zx::ok(battery_spec_));
}

void HardwareBatteryServer::GetStatus(sbattery::GetStatusCompleter::Sync& completer) {
  completer.Reply(zx::ok(battery_status_));
}

void HardwareBatteryServer::Watch(sbattery::WatchRequest& request,
                                  sbattery::WatchCompleter::Sync& completer) {
  if (watch_completer_) {
    completer.Close(ZX_ERR_ALREADY_BOUND);
    return;
  }
  watch_completer_ = completer.ToAsync();

  if (first_watch_ || state_changed_) {
    NotifyOnce(battery_status_);
    first_watch_ = false;
    state_changed_ = false;
  }
}

void HardwareBatteryServer::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_hardware_power_battery::Battery> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  fdf::error("Unknown method called: ordinal={}", metadata.method_ordinal);
}

void HardwareBatteryServer::SetBatteryStatus(scontrol::SetBatteryStatusRequest& request,
                                             scontrol::SetBatteryStatusCompleter::Sync& completer) {
  fdf::info("SetBatteryStatus called");
  auto existing_source = battery_status_.source_status();
  battery_status_ = request.status();
  if (!battery_status_.source_status().has_value()) {
    battery_status_.source_status(existing_source);
  }
  state_changed_ = true;
  if (watch_completer_) {
    NotifyOnce(battery_status_);
    state_changed_ = false;
  }
  completer.Reply();
}

void HardwareBatteryServer::SetSourceStatus(scontrol::SetSourceStatusRequest& request,
                                            scontrol::SetSourceStatusCompleter::Sync& completer) {
  battery_status_.source_status(request.status());
  state_changed_ = true;
  if (watch_completer_) {
    NotifyOnce(battery_status_);
    state_changed_ = false;
  }
  completer.Reply();
}

void HardwareBatteryServer::NotifyOnce(fuchsia_hardware_power_battery::Status status) {
  if (!watch_completer_) {
    fdf::error("No active watcher to notify.");
    return;
  }

  watch_completer_->Reply({status, {}});
  watch_completer_.reset();
}

}  // namespace fake_battery
