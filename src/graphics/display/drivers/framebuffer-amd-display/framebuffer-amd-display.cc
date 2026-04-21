// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/component/cpp/driver_export.h>

#include <utility>

#include "src/graphics/display/lib/framebuffer-display/framebuffer-pci-boot-display-driver.h"

namespace framebuffer_display {

namespace {

constexpr uint32_t kFramebufferPciBarIndex = 0;

class FramebufferAmdDisplayDriver final : public FramebufferPciBootDisplayDriver {
 public:
  explicit FramebufferAmdDisplayDriver(fdf::DriverStartArgs start_args,
                                       fdf::UnownedSynchronizedDispatcher driver_dispatcher);

  FramebufferAmdDisplayDriver(const FramebufferAmdDisplayDriver&) = delete;
  FramebufferAmdDisplayDriver(FramebufferAmdDisplayDriver&&) = delete;
  FramebufferAmdDisplayDriver& operator=(const FramebufferAmdDisplayDriver&) = delete;
  FramebufferAmdDisplayDriver& operator=(FramebufferAmdDisplayDriver&&) = delete;

  ~FramebufferAmdDisplayDriver() override;
};

FramebufferAmdDisplayDriver::FramebufferAmdDisplayDriver(
    fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
    : FramebufferPciBootDisplayDriver("framebuffer-amd-display", kFramebufferPciBarIndex,
                                      std::move(start_args), std::move(driver_dispatcher)) {}

FramebufferAmdDisplayDriver::~FramebufferAmdDisplayDriver() = default;

}  // namespace

}  // namespace framebuffer_display

FUCHSIA_DRIVER_EXPORT(framebuffer_display::FramebufferAmdDisplayDriver);
