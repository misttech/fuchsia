// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_POWER_TESTING_FAKE_BATTERY_HARDWARE_BATTERY_SERVER_H_
#define SRC_POWER_TESTING_FAKE_BATTERY_HARDWARE_BATTERY_SERVER_H_

#include <fidl/fuchsia.hardware.power.battery/cpp/fidl.h>
#include <fidl/fuchsia.hardware.power.source/cpp/fidl.h>
#include <fidl/test.hardwarepowercontrol/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/zx/result.h>

#include <optional>

namespace fake_battery {

class HardwareBatteryServer : public fidl::Server<fuchsia_hardware_power_source::Source>,
                              public fidl::Server<fuchsia_hardware_power_battery::Battery>,
                              public fidl::Server<test_hardwarepowercontrol::Control> {
 public:
  explicit HardwareBatteryServer(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {
    source_spec_.type(fuchsia_hardware_power_source::SourceType::kBattery);

    battery_spec_.design_capacity_uah(test_hardwarepowercontrol::kDefaultFullCapacityUah);

    fuchsia_hardware_power_source::Status status;
    status.present(true);
    status.voltage_uv(test_hardwarepowercontrol::kDefaultPresentVoltageMv * 1000);
    status.current_ua(test_hardwarepowercontrol::kDefaultChargingCurrentUa);

    fuchsia_hardware_power_source::SinkRole sink_role;
    sink_role.type(fuchsia_hardware_power_source::SourceType::kAc);
    sink_role.name("Fake AC Charger");
    status.current_role(fuchsia_hardware_power_source::Role::WithSink(std::move(sink_role)));

    battery_status_.source_status(status);
    battery_status_.charge_status(fuchsia_hardware_power_battery::ChargeStatus::kCharging);
    battery_status_.level_percent(test_hardwarepowercontrol::kDefaultLevelPercent);
    battery_status_.health(fuchsia_hardware_power_battery::HealthStatus::kGood);
    battery_status_.time_remaining(zx::sec(59).to_nsecs());
    battery_status_.remaining_capacity_uah(test_hardwarepowercontrol::kDefaultRemainingChargeUah);
    battery_status_.full_charge_capacity_uah(test_hardwarepowercontrol::kDefaultFullCapacityUah);
    battery_status_.temperature_mc(test_hardwarepowercontrol::kDefaultTemperatureMc);
  }

  zx_status_t Init(const std::shared_ptr<fdf::OutgoingDirectory>& outgoing);
  void NotifyOnce(fuchsia_hardware_power_battery::Status status);

 private:
  using ssource = fidl::Server<fuchsia_hardware_power_source::Source>;
  using sbattery = fidl::Server<fuchsia_hardware_power_battery::Battery>;
  using scontrol = fidl::Server<test_hardwarepowercontrol::Control>;

  // fuchsia.hardware.power.source.Source implementation
  void GetSpec(ssource::GetSpecCompleter::Sync& completer) override;
  void GetStatus(ssource::GetStatusCompleter::Sync& completer) override;
  void SetRole(ssource::SetRoleRequest& request,
               ssource::SetRoleCompleter::Sync& completer) override;
  void Watch(ssource::WatchRequest& request, ssource::WatchCompleter::Sync& completer) override;

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_power_source::Source> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  // fuchsia.hardware.power.battery.Battery implementation
  void GetSpec(sbattery::GetSpecCompleter::Sync& completer) override;
  void GetStatus(sbattery::GetStatusCompleter::Sync& completer) override;
  void Watch(sbattery::WatchRequest& request, sbattery::WatchCompleter::Sync& completer) override;

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_power_battery::Battery> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  // test.hardwarepowercontrol.Control implementation
  void SetBatteryStatus(scontrol::SetBatteryStatusRequest& request,
                        scontrol::SetBatteryStatusCompleter::Sync& completer) override;
  void SetSourceStatus(scontrol::SetSourceStatusRequest& request,
                       scontrol::SetSourceStatusCompleter::Sync& completer) override;

  fuchsia_hardware_power_source::Spec source_spec_;
  fuchsia_hardware_power_battery::Spec battery_spec_;
  fuchsia_hardware_power_battery::Status battery_status_;

  async_dispatcher_t* dispatcher_ = nullptr;
  fidl::ServerBindingGroup<fuchsia_hardware_power_source::Source> source_bindings_;
  fidl::ServerBindingGroup<fuchsia_hardware_power_battery::Battery> battery_bindings_;
  fidl::ServerBindingGroup<test_hardwarepowercontrol::Control> control_bindings_;

  bool first_watch_ = true;
  bool state_changed_ = false;
  std::optional<sbattery::WatchCompleter::Async> watch_completer_;
};

}  // namespace fake_battery

#endif  // SRC_POWER_TESTING_FAKE_BATTERY_HARDWARE_BATTERY_SERVER_H_
