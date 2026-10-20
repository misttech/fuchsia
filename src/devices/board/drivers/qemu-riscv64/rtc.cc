// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <lib/driver/logging/cpp/logger.h>

#include <bind/fuchsia/google/platform/cpp/bind.h>

#include "qemu-riscv64.h"

namespace fpbus = fuchsia_hardware_platform_bus;
namespace board_qemu_riscv64 {

zx::result<> QemuRiscv64::RtcInit() {
  static const std::vector<fpbus::Mmio> kGoldfishRtcMmios{
      {{
          .base = kGoldfishRtcMmioBase,
          .length = kGoldfishRtcMmioSize,
      }},
  };
  fpbus::Node goldfish_rtc_dev;
  goldfish_rtc_dev.name() = "goldfish-rtc";
  goldfish_rtc_dev.vid() = bind_fuchsia_google_platform::BIND_PLATFORM_DEV_VID_GOOGLE;
  goldfish_rtc_dev.pid() = bind_fuchsia_google_platform::BIND_PLATFORM_DEV_PID_GOLDFISH;
  goldfish_rtc_dev.did() = bind_fuchsia_google_platform::BIND_PLATFORM_DEV_DID_GOLDFISH_RTC;
  goldfish_rtc_dev.mmio() = kGoldfishRtcMmios;

  fidl::Arena<> fidl_arena;
  fdf::Arena arena('RTC_');
  auto result = pbus_.buffer(arena)->NodeAdd(fidl::ToWire(fidl_arena, goldfish_rtc_dev));
  if (!result.ok()) {
    fdf::error("NodeAdd goldfish-rtc request failed: {}", result.FormatDescription().data());
    return zx::error(result.status());
  }
  if (result->is_error()) {
    fdf::error("NodeAdd goldfish-rtc failed: {}", zx_status_get_string(result->error_value()));
    return zx::error(result->error_value());
  }

  return zx::ok();
}

}  // namespace board_qemu_riscv64
