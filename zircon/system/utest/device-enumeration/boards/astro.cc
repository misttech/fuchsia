// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "zircon/system/utest/device-enumeration/common.h"

namespace {

TEST_F(DeviceEnumerationTest, AstroTest) {
  static const char* kNodeMonikers[] = {
      "astro",
      "astro.post-init",
      "gpio.aml_gpio.aml-gpio.gpio",
      "gpio.aml_gpio.aml-gpio.gpio-init",
      "astro-buttons.buttons",
      "i2c-0.aml-i2c",
      "i2c-1.aml-i2c",
      "i2c-2.aml-i2c",
      "aml_gpu.aml-gpu-composite.aml-gpu",
      "aml-usb-phy.aml_usb_phy",
      "bt-uart.bluetooth-composite-spec.aml-uart",

      // XHCI driver will not be loaded if we are in USB peripheral mode.
      // "xhci.xhci.usb-bus",

      "i2c-2.aml-i2c.i2c.i2c-2-44.backlight",
      "display.amlogic-display.display-coordinator",
      "canvas.aml_canvas",
      "tee.optee",
      "raw_nand.aml-raw_nand.nand.bl2.skip-block",
      "raw_nand.aml-raw_nand.nand.tpl.skip-block",
      "raw_nand.aml-raw_nand.nand.fts.skip-block",
      "raw_nand.aml-raw_nand.nand.factory.skip-block",
      "raw_nand.aml-raw_nand.nand.zircon-b.skip-block",
      "raw_nand.aml-raw_nand.nand.zircon-a.skip-block",
      "raw_nand.aml-raw_nand.nand.zircon-r.skip-block",
      "raw_nand.aml-raw_nand.nand.sys-config.skip-block",
      "raw_nand.aml-raw_nand.nand.migration.skip-block",
      "raw_nand.aml-raw_nand.nand.fvm.ftl",
      "aml-sdio.aml_sdio.aml-sd-emmc.sdmmc",
      "aml-sdio.aml_sdio.aml-sd-emmc.sdmmc.sdmmc-sdio",
      "aml-sdio.aml_sdio.aml-sd-emmc.sdmmc.sdmmc-sdio.sdmmc-sdio-1",
      "aml-sdio.aml_sdio.aml-sd-emmc.sdmmc.sdmmc-sdio.sdmmc-sdio-2",

      "i2c-0.aml-i2c.i2c.i2c-0-57.tcs3400_light.tcs-3400",
      "astro-clk.amlogic_clock",
      "astro-clk.amlogic_clock.clocks.clock-init",
      "astro-i2s-audio-out.aml_tdm.astro-audio-i2s-out",
      "astro-audio-pdm-in.aml_pdm.astro-audio-pdm-in",
      "aml-secure-mem.aml_securemem.aml-securemem",
      "pwm.amlogic_pwm.aml-pwm-device.pwm-4.pwm_init",

      // CPU Device.
      "aml-cpu",
      "aml-power-impl-composite.power-impl.power-core.power-0.aml_cpu",
      // LED.
      "gpio-light.aml_light",
      // RAM (DDR) control.
      "aml-ram-ctl.aml_ram.ram",

      // Power Device.
      "aml-power-impl-composite",
      "aml-power-impl-composite.power-impl.power-core",
      "aml-power-impl-composite.power-impl.power-core.power-0",

      // Thermal
      "aml-thermal-ddr.aml_thermal_ddr.thermal",
      "05_03_a.aml_thermal_pll.thermal",
      "aml-thermal-ddr.aml_thermal_ddr.thermal",

      // Thermistor.ADC
      "adc.aml_saradc.aml-saradc.ASTRO_THERMISTOR_SOC",
      "adc.aml_saradc.aml-saradc.ASTRO_THERMISTOR_WIFI",
      "adc.aml_saradc.aml-saradc.ASTRO_THERMISTOR_DSP",
      "adc.aml_saradc.aml-saradc.ASTRO_THERMISTOR_AMBIENT",
      "03_03_27.thermistor.thermistor-device.therm-soc",
      "03_03_27.thermistor.thermistor-device.therm-wifi",
      "03_03_27.thermistor.thermistor-device.therm-dsp",
      "03_03_27.thermistor.thermistor-device.therm-ambient",
      "05_03_a.aml_thermal_pll.thermal",
      "aml-thermal-ddr.aml_thermal_ddr.thermal",

      // Registers Device.
      "registers",
#ifdef include_packaged_drivers
      "05:03:e.aml_video",

      // WLAN
      "aml-sdio.aml_sdio.aml-sd-emmc.sdmmc.sdmmc-sdio.sdmmc-sdio-1.wifi.brcmfmac-wlanphyimpl",
      "aml-sdio.aml_sdio.aml-sd-emmc.sdmmc.sdmmc-sdio.sdmmc-sdio-1.wifi.brcmfmac-wlanphyimpl.wlanphy",

      // Bluetooth
      "bt-uart.bluetooth-composite-spec.aml-uart.bt-transport-uart",
      "bt-uart.bluetooth-composite-spec.aml-uart.bt-transport-uart.bt-hci-broadcom",
#endif

  };
  VerifyNodes(kNodeMonikers);

  static const char* kTouchscreenNodeMonikers[] = {
      "i2c-1.aml-i2c.i2c.i2c-1-56.focaltech_touch.focaltouch-HidDevice",
      "i2c-1.aml-i2c.i2c.i2c-1-93.gt92xx_touch.gt92xx-HidDevice",
  };
  VerifyOneOf(kTouchscreenNodeMonikers);
}

}  // namespace
