// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/component/cpp/driver_export.h>

#include "src/graphics/display/lib/framebuffer-display/framebuffer-pci-boot-display-driver.h"

namespace framebuffer_display {

namespace {

constexpr uint32_t kFramebufferPciBarIndex = 2;

class FramebufferIntelDisplayDriver final : public FramebufferPciBootDisplayDriver {
 public:
  explicit FramebufferIntelDisplayDriver(fdf::DriverStartArgs start_args,
                                         fdf::UnownedSynchronizedDispatcher driver_dispatcher);

  FramebufferIntelDisplayDriver(const FramebufferIntelDisplayDriver&) = delete;
  FramebufferIntelDisplayDriver(FramebufferIntelDisplayDriver&&) = delete;
  FramebufferIntelDisplayDriver& operator=(const FramebufferIntelDisplayDriver&) = delete;
  FramebufferIntelDisplayDriver& operator=(FramebufferIntelDisplayDriver&&) = delete;

  ~FramebufferIntelDisplayDriver() override;
};

FramebufferIntelDisplayDriver::FramebufferIntelDisplayDriver(
    fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
    : FramebufferPciBootDisplayDriver("framebuffer-intel-display", kFramebufferPciBarIndex,
                                      std::move(start_args), std::move(driver_dispatcher)) {}

FramebufferIntelDisplayDriver::~FramebufferIntelDisplayDriver() = default;

}  // namespace

}  // namespace framebuffer_display

FUCHSIA_DRIVER_EXPORT(framebuffer_display::FramebufferIntelDisplayDriver);
