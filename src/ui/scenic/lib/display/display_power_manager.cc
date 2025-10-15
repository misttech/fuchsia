// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/display/display_power_manager.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <zircon/errors.h>
#include <zircon/status.h>

#include "src/ui/scenic/lib/display/display_manager.h"

namespace display {

namespace {

using PowerMode = fuchsia_ui_display_singleton::PowerMode;

constexpr char kDisplayPowerEvents[] = "display_power_events";
constexpr uint64_t kInspectHistorySize = 64;

std::string ToString(const PowerMode& power_mode) {
  switch (power_mode) {
    case PowerMode::kOff:
      return "off";
    case PowerMode::kOn:
      return "on";
    case PowerMode::kDoze:
      return "doze";
    case PowerMode::kDozeSuspend:
      return "doze_suspend";
    default:
      return "unknown";
  }
}

fuchsia_hardware_display_types::PowerMode ToDisplayPowerMode(const PowerMode& power_mode) {
  switch (power_mode) {
    case PowerMode::kOff:
      return fuchsia_hardware_display_types::PowerMode::kOff;
    case PowerMode::kOn:
      return fuchsia_hardware_display_types::PowerMode::kOn;
    case PowerMode::kDoze:
      return fuchsia_hardware_display_types::PowerMode::kDoze;
    case PowerMode::kDozeSuspend:
      return fuchsia_hardware_display_types::PowerMode::kDozeSuspend;
    default:
      FX_LOGS(ERROR) << "Unknown power mode: " << ToString(power_mode);
      return fuchsia_hardware_display_types::PowerMode::kOn;
  }
}

}  // namespace

DisplayPowerManager::DisplayPowerManager(DisplayManager& display_manager,
                                         inspect::Node& parent_node)
    : display_manager_(display_manager),
      inspect_display_power_events_(parent_node.CreateChild(kDisplayPowerEvents),
                                    kInspectHistorySize) {}

void DisplayPowerManager::SetPowerMode(SetPowerModeRequest& request,
                                       SetPowerModeCompleter::Sync& completer) {
  SetPowerMode(request.power_mode(),
               [completer = completer.ToAsync()](auto result) mutable { completer.Reply(result); });
}

void DisplayPowerManager::SetPowerMode(PowerMode power_mode,
                                       fit::function<void(fit::result<zx_status_t>)> completer) {
  // No display
  if (!display_manager_.default_display()) {
    completer(fit::error(ZX_ERR_NOT_FOUND));
    return;
  }

  // TODO(https://fxbug.dev/42177175): Since currently Scenic only supports one display,
  // the DisplayPowerManager will only control power of the default display.
  // Once Scenic and DisplayManager supports multiple displays, this needs to
  // be updated to control power of all available displays.
  auto& coordinator_proxy = display_manager_.coordinator_proxy();
  FX_DCHECK(coordinator_proxy);
  display::DisplayId id = display_manager_.default_display()->display_id();

  auto set_display_power_mode_result = coordinator_proxy->raw().sync()->SetDisplayPowerMode(
      id.ToFidl(), ToDisplayPowerMode(power_mode));
  if (!set_display_power_mode_result.ok()) {
    FX_LOGS(ERROR) << "Failed to call FIDL SetDisplayPowerMode(): "
                   << set_display_power_mode_result.status_string();
    completer(fit::error(ZX_ERR_INTERNAL));
    return;
  }

  if (set_display_power_mode_result->is_error()) {
    FX_LOGS(WARNING) << "DisplayCoordinator SetDisplayPowerMode() is not supported; error status: "
                     << zx_status_get_string(set_display_power_mode_result->error_value());
    completer(fit::error(ZX_ERR_NOT_SUPPORTED));
    return;
  }

  FX_LOGS(INFO) << "Successfully set display power mode: " << ToString(power_mode);
  inspect_display_power_events_.CreateEntry([power_mode](inspect::Node& n) {
    n.RecordInt(ToString(power_mode), zx_clock_get_monotonic());
  });
  completer(fit::ok());
}
}  // namespace display
