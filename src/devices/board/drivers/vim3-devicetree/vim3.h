// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#pragma once

#include <lib/async/dispatcher.h>
#include <lib/driver2/driver2_cpp.h>

#include "src/devices/board/lib/devicetree/manager.h"

namespace vim3_dt {

class Vim3Devicetree : public driver::DriverBase {
 public:
  Vim3Devicetree(driver::DriverStartArgs start_args, fdf::UnownedDispatcher dispatcher)
      : driver::DriverBase("vim3-devicetree", std::move(start_args), std::move(dispatcher)) {}

  zx::status<> Start() override;

  void Stop() override;

 private:
  std::optional<fdf_devicetree::Manager> manager_;
};

}  // namespace vim3_dt
