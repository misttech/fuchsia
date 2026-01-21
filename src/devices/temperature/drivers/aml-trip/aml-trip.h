// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_TEMPERATURE_DRIVERS_AML_TRIP_AML_TRIP_H_
#define SRC_DEVICES_TEMPERATURE_DRIVERS_AML_TRIP_AML_TRIP_H_

#include <fidl/fuchsia.hardware.temperature/cpp/wire.h>
#include <fidl/fuchsia.hardware.trippoint/cpp/driver/wire.h>
#include <lib/driver/component/cpp/driver_base.h>

#include "aml-trip-device.h"

namespace temperature {

class AmlTrip final : public fdf::DriverBase {
 public:
  static constexpr std::string_view kDriverName = "aml-trip";
  static constexpr std::string_view kChildNodeName = "aml-trip-device";
  static constexpr size_t kSensorMmioIndex = 0;
  static constexpr size_t kTrimMmioIndex = 1;

  AmlTrip(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher dispatcher)
      : fdf::DriverBase(kDriverName, std::move(start_args), std::move(dispatcher)) {}

  // Lifecycle Management.
  zx::result<> Start() override;
  void PrepareStop(fdf::PrepareStopCompleter completer) override;
  void Stop() override;

 private:
  std::unique_ptr<AmlTripDevice> device_;
  fidl::ServerBindingGroup<fuchsia_hardware_trippoint::TripPoint> trippoint_bindings_;
  fidl::ServerBindingGroup<fuchsia_hardware_temperature::Device> temperature_bindings_;
  fdf::OwnedChildNode child_;
};

}  // namespace temperature

#endif  // SRC_DEVICES_TEMPERATURE_DRIVERS_AML_TRIP_AML_TRIP_H_
