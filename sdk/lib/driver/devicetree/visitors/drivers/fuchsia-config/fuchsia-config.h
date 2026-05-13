// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_FUCHSIA_CONFIG_FUCHSIA_CONFIG_H_
#define LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_FUCHSIA_CONFIG_FUCHSIA_CONFIG_H_

#include <lib/driver/devicetree/manager/visitor.h>

namespace fuchsia_config_dt {

class FuchsiaConfig : public fdf_devicetree::Visitor {
 public:
  FuchsiaConfig() = default;
  zx::result<> Visit(fdf_devicetree::Node& node,
                     const devicetree::PropertyDecoder& decoder) override;
};

}  // namespace fuchsia_config_dt

#endif  // LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_FUCHSIA_CONFIG_FUCHSIA_CONFIG_H_
