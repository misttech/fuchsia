// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/amlogic-display/mipi-phy.h"

#include <fidl/fuchsia.hardware.platform.device/cpp/wire.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/mmio/mmio-buffer.h>
#include <lib/zx/result.h>

#include <cstdint>
#include <memory>

#include <fbl/algorithm.h>
#include <fbl/alloc_checker.h>

#include "src/graphics/display/drivers/amlogic-display/board-resources.h"
#include "src/graphics/display/drivers/amlogic-display/common.h"
#include "src/graphics/display/drivers/amlogic-display/dsi.h"
#include "src/graphics/display/lib/designware-dsi/dsi-host-controller.h"

namespace amlogic_display {

template <typename T>
constexpr inline uint8_t NsToLaneByte(T x, int64_t lanebytetime) {
  return (static_cast<uint8_t>((x + lanebytetime - 1) / lanebytetime) & 0xFF);
}

constexpr int64_t kUnit = int64_t{1} * 1000 * 1000 * 100;

zx::result<> MipiPhy::PhyCfgLoad(int64_t bitrate) {
  // According to MIPI -PHY Spec, we need to define Unit Interval (UI).
  // This UI is defined as the time it takes to send a bit (i.e. bitrate)
  // The x100 is to ensure the ui is not rounded too much (i.e. 2.56 --> 256)
  // However, since we have introduced x100, we need to make sure we include x100
  // to all the PHY timings that are in ns units.
  const int64_t ui = kUnit / (bitrate / 1000);

  // Calculate values will be rounded by the lanebyteclk
  const int64_t lanebytetime = ui * 8;

  // lp_tesc:TX Escape Clock Division factor (from linebyteclk). Round up to units of ui
  dsi_phy_cfg_.lp_tesc = NsToLaneByte(DPHY_TIME_LP_TESC, lanebytetime);

  // lp_lpx: Transmit length of any LP state period
  dsi_phy_cfg_.lp_lpx = NsToLaneByte(DPHY_TIME_LP_LPX, lanebytetime);

  // lp_ta_sure
  dsi_phy_cfg_.lp_ta_sure = NsToLaneByte(DPHY_TIME_LP_TA_SURE, lanebytetime);

  // lp_ta_go
  dsi_phy_cfg_.lp_ta_go = NsToLaneByte(DPHY_TIME_LP_TA_GO, lanebytetime);

  // lp_ta_get
  dsi_phy_cfg_.lp_ta_get = NsToLaneByte(DPHY_TIME_LP_TA_GET, lanebytetime);

  // hs_exit
  dsi_phy_cfg_.hs_exit = NsToLaneByte(DPHY_TIME_HS_EXIT, lanebytetime);

  // clk-_prepare
  dsi_phy_cfg_.clk_prepare = NsToLaneByte(DPHY_TIME_CLK_PREPARE, lanebytetime);

  // clk_zero
  dsi_phy_cfg_.clk_zero = NsToLaneByte(DPHY_TIME_CLK_ZERO(ui), lanebytetime);

  // clk_pre
  dsi_phy_cfg_.clk_pre = NsToLaneByte(DPHY_TIME_CLK_PRE(ui), lanebytetime);

  // init
  dsi_phy_cfg_.init = NsToLaneByte(DPHY_TIME_INIT, lanebytetime);

  // wakeup
  dsi_phy_cfg_.wakeup = NsToLaneByte(DPHY_TIME_WAKEUP, lanebytetime);

  // clk_trail
  dsi_phy_cfg_.clk_trail = NsToLaneByte(DPHY_TIME_CLK_TRAIL, lanebytetime);

  // clk_post
  dsi_phy_cfg_.clk_post = NsToLaneByte(DPHY_TIME_CLK_POST(ui), lanebytetime);

  // hs_trail
  dsi_phy_cfg_.hs_trail = NsToLaneByte(DPHY_TIME_HS_TRAIL(ui), lanebytetime);

  // hs_prepare
  dsi_phy_cfg_.hs_prepare = NsToLaneByte(DPHY_TIME_HS_PREPARE(ui), lanebytetime);

  // hs_zero
  dsi_phy_cfg_.hs_zero = NsToLaneByte(DPHY_TIME_HS_ZERO(ui), lanebytetime);

  // Ensure both clk-trail and hs-trail do not exceed Teot (End of Transmission Time)
  const uint32_t time_req_max = NsToLaneByte(DPHY_TIME_EOT(ui), lanebytetime);
  if ((dsi_phy_cfg_.clk_trail > time_req_max) || (dsi_phy_cfg_.hs_trail > time_req_max)) {
    FDF_LOG(ERROR, "Invalid clk-trail and/or hs-trail exceed Teot!");
    FDF_LOG(ERROR, "clk-trail = 0x%02x, hs-trail =  0x%02x, Teot = 0x%02x", dsi_phy_cfg_.clk_trail,
            dsi_phy_cfg_.hs_trail, time_req_max);
    return zx::error(ZX_ERR_OUT_OF_RANGE);
  }

  FDF_LOG(TRACE,
          "lp_tesc     = 0x%02x"
          "lp_lpx      = 0x%02x"
          "lp_ta_sure  = 0x%02x"
          "lp_ta_go    = 0x%02x"
          "lp_ta_get   = 0x%02x"
          "hs_exit     = 0x%02x"
          "hs_trail    = 0x%02x"
          "hs_zero     = 0x%02x"
          "hs_prepare  = 0x%02x"
          "clk_trail   = 0x%02x"
          "clk_post    = 0x%02x"
          "clk_zero    = 0x%02x"
          "clk_prepare = 0x%02x"
          "clk_pre     = 0x%02x"
          "init        = 0x%02x"
          "wakeup      = 0x%02x",
          dsi_phy_cfg_.lp_tesc, dsi_phy_cfg_.lp_lpx, dsi_phy_cfg_.lp_ta_sure, dsi_phy_cfg_.lp_ta_go,
          dsi_phy_cfg_.lp_ta_get, dsi_phy_cfg_.hs_exit, dsi_phy_cfg_.hs_trail, dsi_phy_cfg_.hs_zero,
          dsi_phy_cfg_.hs_prepare, dsi_phy_cfg_.clk_trail, dsi_phy_cfg_.clk_post,
          dsi_phy_cfg_.clk_zero, dsi_phy_cfg_.clk_prepare, dsi_phy_cfg_.clk_pre, dsi_phy_cfg_.init,
          dsi_phy_cfg_.wakeup);
  return zx::ok();
}

void MipiPhy::PhyInit() {
  // Enable phy clock.
  dsi_phy_mmio_.Write32(PHY_CTRL_TXDDRCLK_EN | PHY_CTRL_DDRCLKPATH_EN | PHY_CTRL_CLK_DIV_COUNTER |
                            PHY_CTRL_CLK_DIV_EN | PHY_CTRL_BYTECLK_EN,
                        MIPI_DSI_PHY_CTRL);

  // Toggle PHY CTRL RST
  dsi_phy_mmio_.Write32(SetFieldValue32(dsi_phy_mmio_.Read32(MIPI_DSI_PHY_CTRL),
                                        /*field_begin_bit=*/PHY_CTRL_RST_START,
                                        /*field_size_bits=*/PHY_CTRL_RST_BITS, /*field_value=*/1),
                        MIPI_DSI_PHY_CTRL);
  dsi_phy_mmio_.Write32(SetFieldValue32(dsi_phy_mmio_.Read32(MIPI_DSI_PHY_CTRL),
                                        /*field_begin_bit=*/PHY_CTRL_RST_START,
                                        /*field_size_bits=*/PHY_CTRL_RST_BITS, /*field_value=*/0),
                        MIPI_DSI_PHY_CTRL);

  dsi_phy_mmio_.Write32((dsi_phy_cfg_.clk_trail | (dsi_phy_cfg_.clk_post << 8) |
                         (dsi_phy_cfg_.clk_zero << 16) | (dsi_phy_cfg_.clk_prepare << 24)),
                        MIPI_DSI_CLK_TIM);

  dsi_phy_mmio_.Write32(dsi_phy_cfg_.clk_pre, MIPI_DSI_CLK_TIM1);

  dsi_phy_mmio_.Write32((dsi_phy_cfg_.hs_exit | (dsi_phy_cfg_.hs_trail << 8) |
                         (dsi_phy_cfg_.hs_zero << 16) | (dsi_phy_cfg_.hs_prepare << 24)),
                        MIPI_DSI_HS_TIM);

  dsi_phy_mmio_.Write32((dsi_phy_cfg_.lp_lpx | (dsi_phy_cfg_.lp_ta_sure << 8) |
                         (dsi_phy_cfg_.lp_ta_go << 16) | (dsi_phy_cfg_.lp_ta_get << 24)),
                        MIPI_DSI_LP_TIM);

  dsi_phy_mmio_.Write32(ANA_UP_TIME, MIPI_DSI_ANA_UP_TIM);
  dsi_phy_mmio_.Write32(dsi_phy_cfg_.init, MIPI_DSI_INIT_TIM);
  dsi_phy_mmio_.Write32(dsi_phy_cfg_.wakeup, MIPI_DSI_WAKEUP_TIM);
  dsi_phy_mmio_.Write32(LPOK_TIME, MIPI_DSI_LPOK_TIM);
  dsi_phy_mmio_.Write32(ULPS_CHECK_TIME, MIPI_DSI_ULPS_CHECK);
  dsi_phy_mmio_.Write32(LP_WCHDOG_TIME, MIPI_DSI_LP_WCHDOG);
  dsi_phy_mmio_.Write32(TURN_WCHDOG_TIME, MIPI_DSI_TURN_WCHDOG);

  dsi_phy_mmio_.Write32(0, MIPI_DSI_CHAN_CTRL);
}

void MipiPhy::Shutdown() {
  if (!phy_enabled_) {
    return;
  }

  // Power down DSI
  designware_dsi_host_controller_.PowerDown();
  dsi_phy_mmio_.Write32(0x1f, MIPI_DSI_CHAN_CTRL);
  dsi_phy_mmio_.Write32(
      SetFieldValue32(dsi_phy_mmio_.Read32(MIPI_DSI_PHY_CTRL), /*field_begin_bit=*/7,
                      /*field_size_bits=*/1, /*field_value=*/0),
      MIPI_DSI_PHY_CTRL);
  phy_enabled_ = false;
}

zx::result<> MipiPhy::Startup() {
  if (phy_enabled_) {
    return zx::ok();
  }

  // Power up DSI
  designware_dsi_host_controller_.PowerUp();

  // Setup Parameters of DPHY
  // Below we are sending test code 0x44 with parameter 0x74. This means
  // we are setting up the phy to operate in 1050-1099 Mbps mode
  // TODO(payamm): Find out why 0x74 was selected
  designware_dsi_host_controller_.PhySendCode(0x00010044, 0x00000074);

  // Power up D-PHY
  designware_dsi_host_controller_.PhyPowerUp();

  // Setup PHY Timing parameters
  PhyInit();

  // Wait for PHY to be read
  zx_status_t status = designware_dsi_host_controller_.PhyWaitForReady();
  if (status != ZX_OK) {
    // no need to print additional info.
    return zx::error(status);
  }

  // Trigger a sync active for esc_clk
  dsi_phy_mmio_.Write32(
      SetFieldValue32(dsi_phy_mmio_.Read32(MIPI_DSI_PHY_CTRL), /*field_begin_bit=*/1,
                      /*field_size_bits=*/1, /*field_value=*/1),
      MIPI_DSI_PHY_CTRL);

  phy_enabled_ = true;
  return zx::ok();
}

// static
zx::result<std::unique_ptr<MipiPhy>> MipiPhy::Create(
    fidl::UnownedClientEnd<fuchsia_hardware_platform_device::Device> platform_device,
    designware_dsi::DsiHostController* designware_dsi_host_controller, bool enabled) {
  ZX_DEBUG_ASSERT(platform_device.is_valid());

  zx::result<fdf::MmioBuffer> d_phy_mmio_result = MapMmio(kMmioNameDsiPhy, platform_device);
  if (d_phy_mmio_result.is_error()) {
    return d_phy_mmio_result.take_error();
  }
  fdf::MmioBuffer d_phy_mmio = std::move(d_phy_mmio_result).value();

  fbl::AllocChecker alloc_checker;
  auto mipi_phy = fbl::make_unique_checked<MipiPhy>(&alloc_checker, std::move(d_phy_mmio),
                                                    designware_dsi_host_controller, enabled);
  if (!alloc_checker.check()) {
    FDF_LOG(ERROR, "Failed to allocate memory for Lcd");
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  return zx::ok(std::move(mipi_phy));
}

MipiPhy::MipiPhy(fdf::MmioBuffer d_phy_mmio,
                 designware_dsi::DsiHostController* designware_dsi_host_controller, bool enabled)
    : dsi_phy_mmio_(std::move(d_phy_mmio)),
      designware_dsi_host_controller_(*designware_dsi_host_controller),
      phy_enabled_(enabled) {
  ZX_DEBUG_ASSERT(designware_dsi_host_controller != nullptr);
}

void MipiPhy::Dump() {
  FDF_LOG(INFO, "%s: DUMPING PHY REGS", __func__);
  FDF_LOG(INFO, "MIPI_DSI_PHY_CTRL = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_PHY_CTRL));
  FDF_LOG(INFO, "MIPI_DSI_CHAN_CTRL = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_CHAN_CTRL));
  FDF_LOG(INFO, "MIPI_DSI_CHAN_STS = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_CHAN_STS));
  FDF_LOG(INFO, "MIPI_DSI_CLK_TIM = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_CLK_TIM));
  FDF_LOG(INFO, "MIPI_DSI_HS_TIM = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_HS_TIM));
  FDF_LOG(INFO, "MIPI_DSI_LP_TIM = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_LP_TIM));
  FDF_LOG(INFO, "MIPI_DSI_ANA_UP_TIM = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_ANA_UP_TIM));
  FDF_LOG(INFO, "MIPI_DSI_INIT_TIM = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_INIT_TIM));
  FDF_LOG(INFO, "MIPI_DSI_WAKEUP_TIM = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_WAKEUP_TIM));
  FDF_LOG(INFO, "MIPI_DSI_LPOK_TIM = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_LPOK_TIM));
  FDF_LOG(INFO, "MIPI_DSI_LP_WCHDOG = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_LP_WCHDOG));
  FDF_LOG(INFO, "MIPI_DSI_ANA_CTRL = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_ANA_CTRL));
  FDF_LOG(INFO, "MIPI_DSI_CLK_TIM1 = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_CLK_TIM1));
  FDF_LOG(INFO, "MIPI_DSI_TURN_WCHDOG = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_TURN_WCHDOG));
  FDF_LOG(INFO, "MIPI_DSI_ULPS_CHECK = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_ULPS_CHECK));
  FDF_LOG(INFO, "MIPI_DSI_TEST_CTRL0 = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_TEST_CTRL0));
  FDF_LOG(INFO, "MIPI_DSI_TEST_CTRL1 = 0x%x", dsi_phy_mmio_.Read32(MIPI_DSI_TEST_CTRL1));
  FDF_LOG(INFO, "");

  FDF_LOG(INFO, "#############################");
  FDF_LOG(INFO, "Dumping dsi_phy_cfg structure:");
  FDF_LOG(INFO, "#############################");
  FDF_LOG(INFO, "lp_tesc = 0x%x (%u)", dsi_phy_cfg_.lp_tesc, dsi_phy_cfg_.lp_tesc);
  FDF_LOG(INFO, "lp_lpx = 0x%x (%u)", dsi_phy_cfg_.lp_lpx, dsi_phy_cfg_.lp_lpx);
  FDF_LOG(INFO, "lp_ta_sure = 0x%x (%u)", dsi_phy_cfg_.lp_ta_sure, dsi_phy_cfg_.lp_ta_sure);
  FDF_LOG(INFO, "lp_ta_go = 0x%x (%u)", dsi_phy_cfg_.lp_ta_go, dsi_phy_cfg_.lp_ta_go);
  FDF_LOG(INFO, "lp_ta_get = 0x%x (%u)", dsi_phy_cfg_.lp_ta_get, dsi_phy_cfg_.lp_ta_get);
  FDF_LOG(INFO, "hs_exit = 0x%x (%u)", dsi_phy_cfg_.hs_exit, dsi_phy_cfg_.hs_exit);
  FDF_LOG(INFO, "hs_trail = 0x%x (%u)", dsi_phy_cfg_.hs_trail, dsi_phy_cfg_.hs_trail);
  FDF_LOG(INFO, "hs_zero = 0x%x (%u)", dsi_phy_cfg_.hs_zero, dsi_phy_cfg_.hs_zero);
  FDF_LOG(INFO, "hs_prepare = 0x%x (%u)", dsi_phy_cfg_.hs_prepare, dsi_phy_cfg_.hs_prepare);
  FDF_LOG(INFO, "clk_trail = 0x%x (%u)", dsi_phy_cfg_.clk_trail, dsi_phy_cfg_.clk_trail);
  FDF_LOG(INFO, "clk_post = 0x%x (%u)", dsi_phy_cfg_.clk_post, dsi_phy_cfg_.clk_post);
  FDF_LOG(INFO, "clk_zero = 0x%x (%u)", dsi_phy_cfg_.clk_zero, dsi_phy_cfg_.clk_zero);
  FDF_LOG(INFO, "clk_prepare = 0x%x (%u)", dsi_phy_cfg_.clk_prepare, dsi_phy_cfg_.clk_prepare);
  FDF_LOG(INFO, "clk_pre = 0x%x (%u)", dsi_phy_cfg_.clk_pre, dsi_phy_cfg_.clk_pre);
  FDF_LOG(INFO, "init = 0x%x (%u)", dsi_phy_cfg_.init, dsi_phy_cfg_.init);
  FDF_LOG(INFO, "wakeup = 0x%x (%u)", dsi_phy_cfg_.wakeup, dsi_phy_cfg_.wakeup);
}

}  // namespace amlogic_display
