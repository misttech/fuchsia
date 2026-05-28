// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/annotations/battery_info_provider.h"

#include <fidl/fuchsia.power.battery/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/syslog/cpp/macros.h>

#include <utility>

#include "src/developer/forensics/feedback/annotations/constants.h"

namespace forensics::feedback {
namespace {

Annotations BuildAnnotations(const Error error) {
  Annotations annotations;
  for (const std::string& key : BatteryInfoProvider::GetAnnotationKeys()) {
    annotations.insert_or_assign(key, ErrorOrString(error));
  }

  return annotations;
}

Annotations BatteryInfoToAnnotations(const fuchsia_power_battery::BatteryInfo& info) {
  Annotations annotations = BuildAnnotations(Error::kMissingValue);

  if (!info.status().has_value()) {
    FX_LOGS(WARNING) << "GetBatteryInfo returned no status";
    return annotations;
  }

  if (*info.status() == fuchsia_power_battery::BatteryStatus::kNotPresent) {
    // This should never happen. It means we're trying to collect battery annotations on a device
    // that doesn't have a battery, but serves the battery info protocol.
    FX_LOGS(ERROR) << "GetBatteryInfo returned BatteryStatus::kNotPresent";
    return BuildAnnotations(Error::kLogicError);
  }

  if (info.level_percent().has_value()) {
    annotations.insert_or_assign(
        kDeviceBatteryLevelKey,
        ErrorOrString(std::to_string(static_cast<int>(*info.level_percent()))));
  }

  if (info.charge_status().has_value()) {
    std::string state = "unrecognized";
    switch (*info.charge_status()) {
      case fuchsia_power_battery::ChargeStatus::kUnknown:
        state = "unknown";
        break;
      case fuchsia_power_battery::ChargeStatus::kNotCharging:
        state = "not charging";
        break;
      case fuchsia_power_battery::ChargeStatus::kCharging:
        state = "charging";
        break;
      case fuchsia_power_battery::ChargeStatus::kDischarging:
        state = "discharging";
        break;
      case fuchsia_power_battery::ChargeStatus::kFull:
        state = "full";
        break;
    }
    annotations.insert_or_assign(kDeviceBatteryStateKey, ErrorOrString(state));
  }

  if (info.charge_source().has_value()) {
    const bool is_on_charger =
        (*info.charge_source() != fuchsia_power_battery::ChargeSource::kUnknown &&
         *info.charge_source() != fuchsia_power_battery::ChargeSource::kNone);
    annotations.insert_or_assign(kDeviceBatteryOnChargerKey,
                                 ErrorOrString(is_on_charger ? "true" : "false"));
  }

  return annotations;
}

}  // namespace

BatteryInfoProvider::BatteryInfoProvider(async_dispatcher_t* dispatcher,
                                         std::shared_ptr<sys::ServiceDirectory> services,
                                         std::unique_ptr<backoff::Backoff> backoff)
    : dispatcher_(dispatcher), services_(std::move(services)), backoff_(std::move(backoff)) {
  Connect();
}

void BatteryInfoProvider::Connect() {
  zx::result endpoints = fidl::CreateEndpoints<fuchsia_power_battery::BatteryManager>();
  if (endpoints.is_error()) {
    FX_LOGS(ERROR) << "Failed to create endpoints: " << endpoints.status_string();
    return;
  }

  services_->Connect(fuchsia_power_battery::BatteryManager::kDiscoverableName,
                     endpoints->server.TakeChannel());

  client_ = fidl::Client(std::move(endpoints->client), dispatcher_, this);
}

void BatteryInfoProvider::on_fidl_error(const fidl::UnbindInfo error) {
  if (error.status() == ZX_ERR_NOT_FOUND) {
    // Invalidate the client so that future requests aren't made.
    client_ = fidl::Client<fuchsia_power_battery::BatteryManager>();
    FX_LOGS(ERROR) << "BatteryManager not found, will not attempt to reconnect";
    return;
  }

  FX_LOGS(WARNING) << "Lost connection to BatteryManager: " << error;

  reconnect_task_.PostDelayed(dispatcher_, backoff_->GetNext());
}

void BatteryInfoProvider::Get(::fit::callback<void(Annotations)> callback) {
  if (!client_.is_valid()) {
    callback(BuildAnnotations(Error::kNotAvailableInProduct));
    return;
  }

  client_->GetBatteryInfo().Then(
      [this, callback = std::move(callback)](
          fidl::Result<fuchsia_power_battery::BatteryManager::GetBatteryInfo>& result) mutable {
        if (result.is_error()) {
          FX_LOGS(ERROR) << "GetBatteryInfo failed: " << result.error_value();
          const Error error = FidlErrorToForensicsError(result.error_value());
          callback(BuildAnnotations(error));
          return;
        }

        backoff_->Reset();
        callback(BatteryInfoToAnnotations(result.value().info()));
      });
}

std::set<std::string> BatteryInfoProvider::GetAnnotationKeys() {
  return {
      kDeviceBatteryLevelKey,
      kDeviceBatteryStateKey,
      kDeviceBatteryOnChargerKey,
  };
}

std::set<std::string> BatteryInfoProvider::GetKeys() const {
  return BatteryInfoProvider::GetAnnotationKeys();
}

}  // namespace forensics::feedback
