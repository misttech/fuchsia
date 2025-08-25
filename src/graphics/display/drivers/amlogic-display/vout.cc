// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/amlogic-display/vout.h"

#include <fidl/fuchsia.hardware.platform.device/cpp/wire.h>
#include <lib/device-protocol/display-panel.h>
#include <lib/driver/incoming/cpp/namespace.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fit/result.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/stdcompat/span.h>
#include <lib/zx/result.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <algorithm>
#include <cinttypes>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <optional>
#include <ranges>
#include <span>
#include <utility>

#include <fbl/alloc_checker.h>
#include <fbl/vector.h>

#include "src/graphics/display/drivers/amlogic-display/clock.h"
#include "src/graphics/display/drivers/amlogic-display/common.h"
#include "src/graphics/display/drivers/amlogic-display/display-timing-mode-conversion.h"
#include "src/graphics/display/drivers/amlogic-display/dsi-host.h"
#include "src/graphics/display/drivers/amlogic-display/hdmi-host.h"
#include "src/graphics/display/drivers/amlogic-display/logging.h"
#include "src/graphics/display/drivers/amlogic-display/panel-config.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/display-timing.h"
#include "src/graphics/display/lib/api-types/cpp/mode-and-id.h"
#include "src/graphics/display/lib/api-types/cpp/mode-id.h"
#include "src/graphics/display/lib/api-types/cpp/mode.h"
#include "src/graphics/display/lib/edid/edid.h"

namespace amlogic_display {

namespace {

// List of supported features
struct supported_features_t {
  bool hpd;
};

constexpr supported_features_t kDsiSupportedFeatures = supported_features_t{
    .hpd = false,
};

constexpr supported_features_t kHdmiSupportedFeatures = supported_features_t{
    .hpd = true,
};

std::optional<display::DisplayTiming> GetDisplayTimingFromModeId(
    std::span<const display::DisplayTiming> timings, display::ModeId mode_id) {
  ZX_DEBUG_ASSERT(mode_id != display::kInvalidModeId);
  size_t index = mode_id.value() - 1;
  if (index >= timings.size()) {
    return std::nullopt;
  }
  return timings[index];
}

constexpr display::ModeId kDsiDefaultModeId(1);

}  // namespace

Vout::Vout(std::unique_ptr<DsiHost> dsi_host, std::unique_ptr<Clock> dsi_clock,
           const PanelConfig* panel_config, inspect::Node node)
    : type_(VoutType::kDsi),
      supports_hpd_(kDsiSupportedFeatures.hpd),
      node_(std::move(node)),
      dsi_{
          .dsi_host = std::move(dsi_host),
          .clock = std::move(dsi_clock),
          .panel_config = *panel_config,
          .mode_and_id = display::ModeAndId({
              .id = kDsiDefaultModeId,
              .mode = ToDisplayMode(panel_config->display_timing),
          }),
      } {
  ZX_DEBUG_ASSERT(panel_config != nullptr);
  node_.RecordInt("vout_type", static_cast<int>(type()));
}

Vout::Vout(std::unique_ptr<HdmiHost> hdmi_host, inspect::Node node, uint8_t visual_debug_level)
    : type_(VoutType::kHdmi),
      supports_hpd_(kHdmiSupportedFeatures.hpd),
      node_(std::move(node)),
      hdmi_{.hdmi_host = std::move(hdmi_host)},
      visual_debug_level_(visual_debug_level) {
  node_.RecordInt("vout_type", static_cast<int>(type()));
}

zx::result<std::unique_ptr<Vout>> Vout::CreateDsiVout(fdf::Namespace& incoming,
                                                      display::PanelType panel_type,
                                                      inspect::Node node) {
  fdf::info("Fixed panel type is {}", static_cast<uint32_t>(panel_type));
  const PanelConfig* panel_config = GetPanelConfig(panel_type);
  if (panel_config == nullptr) {
    fdf::error("Failed to get panel config for panel {}", static_cast<uint32_t>(panel_type));
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  zx::result<std::unique_ptr<DsiHost>> dsi_host_result =
      DsiHost::Create(incoming, panel_type, panel_config);
  if (dsi_host_result.is_error()) {
    fdf::error("Could not create DSI host: {}", dsi_host_result);
    return dsi_host_result.take_error();
  }
  std::unique_ptr<DsiHost> dsi_host = std::move(dsi_host_result).value();

  static constexpr char kPdevFragmentName[] = "pdev";
  zx::result<fidl::ClientEnd<fuchsia_hardware_platform_device::Device>> pdev_result =
      incoming.Connect<fuchsia_hardware_platform_device::Service::Device>(kPdevFragmentName);
  if (pdev_result.is_error()) {
    fdf::error("Failed to get the pdev client: {}", pdev_result);
    return pdev_result.take_error();
  }
  fidl::ClientEnd<fuchsia_hardware_platform_device::Device> platform_device =
      std::move(pdev_result).value();
  if (!platform_device.is_valid()) {
    fdf::error("Failed to get a valid platform device client");
    return zx::error(ZX_ERR_INTERNAL);
  }

  zx::result<std::unique_ptr<Clock>> clock_result =
      Clock::Create(platform_device, kBootloaderDisplayEnabled);
  if (clock_result.is_error()) {
    fdf::error("Could not create Clock: {}", clock_result);
    return clock_result.take_error();
  }
  std::unique_ptr<Clock> clock = std::move(clock_result).value();

  fbl::AllocChecker alloc_checker;
  std::unique_ptr<Vout> vout = fbl::make_unique_checked<Vout>(
      &alloc_checker, std::move(dsi_host), std::move(clock), panel_config, std::move(node));
  if (!alloc_checker.check()) {
    fdf::error("Failed to allocate memory for Vout.");
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  return zx::ok(std::move(vout));
}

zx::result<std::unique_ptr<Vout>> Vout::CreateDsiVoutForTesting(display::PanelType panel_type) {
  const PanelConfig* panel_config = GetPanelConfig(panel_type);
  if (panel_config == nullptr) {
    fdf::error("Failed to get panel config for panel {}", static_cast<uint32_t>(panel_type));
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  fbl::AllocChecker alloc_checker;
  std::unique_ptr<Vout> vout = fbl::make_unique_checked<Vout>(
      &alloc_checker,
      /*dsi_host=*/nullptr, /*dsi_clock=*/nullptr, panel_config, inspect::Node{});
  if (!alloc_checker.check()) {
    fdf::error("Failed to allocate memory for Vout.");
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  return zx::ok(std::move(vout));
}

zx::result<std::unique_ptr<Vout>> Vout::CreateHdmiVout(fdf::Namespace& incoming, inspect::Node node,
                                                       uint8_t visual_debug_level) {
  zx::result<std::unique_ptr<HdmiHost>> hdmi_host_result = HdmiHost::Create(incoming);
  if (hdmi_host_result.is_error()) {
    fdf::error("Could not create HDMI host: {}", hdmi_host_result);
    return hdmi_host_result.take_error();
  }

  fbl::AllocChecker alloc_checker;
  std::unique_ptr<Vout> vout = fbl::make_unique_checked<Vout>(
      &alloc_checker, std::move(hdmi_host_result).value(), std::move(node), visual_debug_level);
  if (!alloc_checker.check()) {
    fdf::error("Failed to allocate memory for Vout.");
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  return zx::ok(std::move(vout));
}

AddedDisplayInfo Vout::CreateAddedDisplayInfo(display::DisplayId display_id) {
  switch (type_) {
    case VoutType::kDsi: {
      ZX_DEBUG_ASSERT(dsi_.mode_and_id.has_value());
      return {
          .display_id = display_id,
          .preferred_modes = {dsi_.mode_and_id.value()},
      };
    }
    case VoutType::kHdmi: {
      ZX_DEBUG_ASSERT(!hdmi_.timings.is_empty());
      fbl::Vector<display::ModeAndId> preferred_modes;
      fbl::AllocChecker alloc_checker;

      const size_t preferred_modes_count =
          std::min(hdmi_.timings.size(), size_t{AddedDisplayInfo::kMaxPreferredModes});
      preferred_modes.reserve(hdmi_.timings.size(), &alloc_checker);
      ZX_DEBUG_ASSERT(alloc_checker.check());

      for (uint16_t i = 0; i < preferred_modes_count; ++i) {
        ZX_DEBUG_ASSERT_MSG(
            preferred_modes.size() < preferred_modes.capacity(),
            "The push_back() below was not supposed to allocate memory, but it might");
        preferred_modes.push_back(display::ModeAndId({.id = display::ModeId(i + 1),
                                                      .mode = ToDisplayMode(hdmi_.timings[i])}),
                                  &alloc_checker);
        ZX_DEBUG_ASSERT_MSG(alloc_checker.check(),
                            "The push_back() above failed to allocate memory; "
                            "it was not supposed to allocate at all");
      }

      return {
          .display_id = display_id,
          .preferred_modes = std::move(preferred_modes),
      };
    }
  }
  ZX_ASSERT_MSG(false, "Invalid Vout type: %u", static_cast<uint8_t>(type_));
}

zx::result<> Vout::UpdateStateOnDisplayConnected() {
  switch (type_) {
    case VoutType::kHdmi: {
      auto read_extended_edid_result = hdmi_.hdmi_host->ReadExtendedEdid();
      if (read_extended_edid_result.is_error()) {
        // HdmiTransmitter::ReadExtendedEdid() already logs errors.
        return read_extended_edid_result.take_error();
      }
      fit::result<const char*, edid::Edid> edid_result =
          edid::Edid::Create(std::move(read_extended_edid_result).value());
      if (edid_result.is_error()) {
        fdf::error("Failed to parse EDID: {}", edid_result.error_value());
        return zx::error(ZX_ERR_INTERNAL);
      }
      edid::Edid edid = std::move(edid_result).value();

      zx::result<fbl::Vector<display::DisplayTiming>> edid_timings_result =
          edid.GetSupportedDisplayTimings();
      if (edid_timings_result.is_error()) {
        fdf::error("Failed to get supported display timings from EDID: {}",
                   edid_timings_result.status_string());
        return edid_timings_result.take_error();
      }
      fbl::Vector<display::DisplayTiming> edid_timings = std::move(edid_timings_result).value();

      // Filter and shrink edid_timings to contain only supported timings.
      auto removed_subrange =
          std::ranges::remove_if(edid_timings, [&](const display::DisplayTiming& timing) {
            return !hdmi_.hdmi_host->IsDisplayTimingSupported(timing);
          });
      ZX_DEBUG_ASSERT(removed_subrange.size() >= 0);
      ZX_DEBUG_ASSERT(removed_subrange.size() <= edid_timings.size());
      edid_timings.resize(edid_timings.size() - removed_subrange.size());

      if (edid_timings.is_empty()) {
        fdf::error("None of the EDID timings is supported. The new display cannot be added.");
        return zx::error(ZX_ERR_INTERNAL);
      }

      hdmi_.edid = std::move(edid);
      hdmi_.timings = std::move(edid_timings);

      // A new connected display is not yet set up with any display timing.
      hdmi_.current_mode_id = display::kInvalidModeId;
      return zx::ok();
    }
    case VoutType::kDsi:
      return zx::ok();
  }
  ZX_ASSERT_MSG(false, "Invalid Vout type: %u", static_cast<uint8_t>(type_));
}

void Vout::DisplayDisconnected() {
  switch (type_) {
    case VoutType::kHdmi:
      hdmi_.hdmi_host->HostOff();
      return;
    case VoutType::kDsi:
      return;
  }
  ZX_ASSERT_MSG(false, "Invalid Vout type: %u", static_cast<uint8_t>(type_));
}

zx::result<> Vout::PowerOff() {
  switch (type_) {
    case VoutType::kDsi: {
      dsi_.clock->Disable();
      dsi_.dsi_host->Disable();
      return zx::ok();
    }
    case VoutType::kHdmi: {
      hdmi_.hdmi_host->HostOff();
      return zx::ok();
    }
  }
  ZX_ASSERT_MSG(false, "Invalid Vout type: %u", static_cast<uint8_t>(type_));
}

zx::result<> Vout::PowerOn() {
  switch (type_) {
    case VoutType::kDsi: {
      zx::result<> clock_enable_result = dsi_.clock->Enable(dsi_.panel_config);
      if (!clock_enable_result.is_ok()) {
        fdf::error("Could not enable display clocks: {}", clock_enable_result);
        return clock_enable_result;
      }

      dsi_.clock->SetVideoOn(false);
      // Configure and enable DSI host interface.
      zx::result<> dsi_host_enable_result = dsi_.dsi_host->Enable(dsi_.clock->GetBitrate());
      if (!dsi_host_enable_result.is_ok()) {
        fdf::error("Could not enable DSI Host: {}", dsi_host_enable_result);
        return dsi_host_enable_result;
      }
      dsi_.clock->SetVideoOn(true);
      return zx::ok();
    }
    case VoutType::kHdmi: {
      zx::result<> hdmi_host_on_result = zx::make_result(hdmi_.hdmi_host->HostOn());
      if (!hdmi_host_on_result.is_ok()) {
        fdf::error("Could not enable HDMI host: {}", hdmi_host_on_result);
        return hdmi_host_on_result;
      }

      // Powering on the display panel also resets the display mode set on the
      // display. This clears the display mode set previously to force a Vout
      // modeset to be performed on the next ApplyConfiguration().
      hdmi_.current_mode_id = display::kInvalidModeId;
      return zx::ok();
    }
  }
  ZX_ASSERT_MSG(false, "Invalid Vout type: %u", static_cast<uint8_t>(type_));
}

zx::result<> Vout::SetFrameVisibility(bool frame_visible) {
  switch (type_) {
    case VoutType::kDsi:
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    case VoutType::kHdmi:
      // On HDMI video output, when the frames are invisible, the encoder
      // outputs a **green** background indicating that the display engine
      // front end is idle.
      static constexpr uint8_t kVisualDebugLevelInfoProduct = 1;

      // The following values are calculated using the conversion formulas defined
      // in the following standards:
      //
      // Rec. ITU-R BT.709-6, Parameter values for the HDTV1 standards for
      // production and international programme exchange, June 2015.
      // - Section 3 "Signal format", item 3.4 "Quantization of RGB, luminance
      //   and colour-difference signals", page 4.
      // - Section 3 "Signal format", item 3.5 "Derivation of luminance and
      //   colour difference signals via quantized RGB signals", page 4.

      // Black (R = 0, G = 0, B = 0) is (Y = 0, Cb = 512, Cr = 512) in YCbCr.
      static constexpr YCbCrColor kBlack = {.y = 0, .cb = 512, .cr = 512};

      // Green (R = 0, G = 128, B = 0) is (Y = 378, Cb = 339, Cr = 308) in YCbCr.
      static constexpr YCbCrColor kGreen = {.y = 378, .cb = 339, .cr = 308};

      if (visual_debug_level_ >= kVisualDebugLevelInfoProduct) {
        hdmi_.hdmi_host->ReplaceEncoderPixelColorWithColor(!frame_visible, kGreen);
      } else {
        hdmi_.hdmi_host->ReplaceEncoderPixelColorWithColor(!frame_visible, kBlack);
      }
      return zx::ok();
  }
}

std::optional<display::Mode> Vout::GetDisplayMode(display::ModeId mode_id) const {
  ZX_DEBUG_ASSERT(mode_id != display::kInvalidModeId);
  switch (type_) {
    case VoutType::kDsi: {
      ZX_DEBUG_ASSERT(dsi_.mode_and_id.has_value());
      if (mode_id == dsi_.mode_and_id->id()) {
        return dsi_.mode_and_id->mode();
      }
      return std::nullopt;
    }
    case VoutType::kHdmi: {
      std::optional<display::DisplayTiming> get_timing_result =
          GetDisplayTimingFromModeId(hdmi_.timings, mode_id);
      if (!get_timing_result.has_value()) {
        return std::nullopt;
      }
      return ToDisplayMode(get_timing_result.value());
    }
  }
  ZX_ASSERT_MSG(false, "Invalid Vout type: %u", static_cast<uint8_t>(type_));
}

zx::result<> Vout::ApplyConfiguration(display::ModeId mode_id) {
  switch (type_) {
    case VoutType::kDsi: {
      ZX_DEBUG_ASSERT(dsi_.mode_and_id.has_value());
      ZX_DEBUG_ASSERT_MSG(mode_id == dsi_.mode_and_id->id(), "Unsupported DSI mode ID: %" PRIu16,
                          mode_id.value());
      return zx::ok();
    }
    case VoutType::kHdmi: {
      ZX_DEBUG_ASSERT(mode_id != display::kInvalidModeId);
      if (mode_id == hdmi_.current_mode_id) {
        return zx::ok();
      }

      std::optional<display::DisplayTiming> timing_result =
          GetDisplayTimingFromModeId(hdmi_.timings, mode_id);
      ZX_DEBUG_ASSERT(timing_result.has_value());

      display::DisplayTiming timing = std::move(timing_result).value();
      ZX_DEBUG_ASSERT(hdmi_.hdmi_host->IsDisplayTimingSupported(timing));

      zx_status_t status = hdmi_.hdmi_host->ModeSet(timing);
      if (status != ZX_OK) {
        fdf::error("Failed to set HDMI display timing: {}", zx::make_result(status));
        return zx::error(status);
      }

      hdmi_.current_mode_id = mode_id;
      return zx::ok();
    }
  }
  ZX_ASSERT_MSG(false, "Invalid Vout type: %u", static_cast<uint8_t>(type_));
}

void Vout::Dump() {
  switch (type_) {
    case VoutType::kDsi: {
      LogPanelConfig(dsi_.panel_config);
      return;
    }
    case VoutType::kHdmi: {
      if (hdmi_.current_mode_id == display::kInvalidModeId) {
        fdf::info("No display mode is currently set.");
        return;
      }
      std::optional<display::DisplayTiming> current_timing_optional =
          GetDisplayTimingFromModeId(hdmi_.timings, hdmi_.current_mode_id);
      ZX_DEBUG_ASSERT(current_timing_optional.has_value());
      display::DisplayTiming timing = std::move(current_timing_optional).value();

      fdf::info("HDMI Display Timing:");

      fdf::info("horizontal_active_px = {}", timing.horizontal_active_px);
      fdf::info("horizontal_front_porch_px = {}", timing.horizontal_front_porch_px);
      fdf::info("horizontal_sync_width_px = {}", timing.horizontal_sync_width_px);
      fdf::info("horizontal_back_porch_px = {}", timing.horizontal_back_porch_px);
      fdf::info("vertical_active_lines = {}", timing.vertical_active_lines);
      fdf::info("vertical_front_porch_lines = {}", timing.vertical_front_porch_lines);
      fdf::info("vertical_sync_width_lines = {}", timing.vertical_sync_width_lines);
      fdf::info("vertical_back_porch_lines = {}", timing.vertical_back_porch_lines);
      fdf::info("pixel_clock_frequency_hz = {}", timing.pixel_clock_frequency_hz);
      fdf::info("fields_per_frame (enum) = {}", static_cast<uint32_t>(timing.fields_per_frame));
      fdf::info("hsync_polarity (enum) = {}", static_cast<uint32_t>(timing.hsync_polarity));
      fdf::info("vsync_polarity (enum) = {}", static_cast<uint32_t>(timing.vsync_polarity));
      fdf::info("vblank_alternates = {}", timing.vblank_alternates);
      fdf::info("pixel_repetition = {}", timing.pixel_repetition);
      return;
    }
  }
  ZX_ASSERT_MSG(false, "Invalid Vout type: %u", static_cast<uint8_t>(type_));
}

}  // namespace amlogic_display
