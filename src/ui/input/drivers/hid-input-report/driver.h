// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_INPUT_DRIVERS_HID_INPUT_REPORT_DRIVER_H_
#define SRC_UI_INPUT_DRIVERS_HID_INPUT_REPORT_DRIVER_H_

#include <fidl/fuchsia.driver.framework/cpp/wire.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>

#include "src/ui/input/drivers/hid-input-report/input-report.h"

namespace hid_input_report_dev {

const std::string kDeviceName = "InputReport";

class InputReportDriver : public fdf::DriverBase2 {
 public:
  explicit InputReportDriver() : fdf::DriverBase2(kDeviceName) {}

  zx::result<> Start(fdf::DriverContext context) override;
  void Stop(fdf::StopCompleter completer) override;

  // Public for testing
  hid_input_report_dev::InputReport& input_report_for_testing() { return *input_report_; }

 private:
  std::unique_ptr<hid_input_report_dev::InputReport> input_report_;
  fidl::ServerBindingGroup<fuchsia_input_report::InputDevice> input_report_bindings_;

  fdf::OwnedChildNode child_;
};

}  // namespace hid_input_report_dev

#endif  // SRC_UI_INPUT_DRIVERS_HID_INPUT_REPORT_DRIVER_H_
