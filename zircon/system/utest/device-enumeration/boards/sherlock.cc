// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "zircon/system/utest/device-enumeration/common.h"

namespace {

TEST_F(DeviceEnumerationTest, SherlockTest) {
  static const char* kNodeMonikers[] = {
      "sherlock",
      "sherlock.post-init",
      "gpio.aml_gpio.aml-gpio.gpio",
      "gpio.aml_gpio.aml-gpio.gpio-init",
      "sherlock-clk.amlogic_clock",
      "sherlock-clk.amlogic_clock.clocks.clock-init",
      "gpio-light.aml_light",
      "i2c-0.aml-i2c",
      "i2c-1.aml-i2c",
      "i2c-2.aml-i2c",
      "canvas.aml_canvas",
      "05_04_a.aml_thermal_pll.thermal",
      "display.amlogic-display.display-coordinator",
      "aml-usb-phy",

      // XHCI driver will not be loaded if we are in USB peripheral mode.
      // "xhci.usb-bus",

      "sherlock-emmc.sherlock_emmc.aml-sd-emmc.sdmmc.sdmmc-mmc.boot1",
      "sherlock-emmc.sherlock_emmc.aml-sd-emmc.sdmmc.sdmmc-mmc.boot2",
      "sherlock-emmc.sherlock_emmc.aml-sd-emmc.sdmmc.sdmmc-mmc.rpmb",
      "sherlock-emmc.sherlock_emmc.aml-sd-emmc.sdmmc.sdmmc-mmc.user",
      "sherlock-emmc.sherlock_emmc.aml-sd-emmc.sdmmc.sdmmc-mmc.user.fts",
      "sherlock-sd-emmc.sherlock_sd_emmc.aml-sd-emmc.sdmmc.sdmmc-sdio.sdmmc-sdio-1",
      "sherlock-sd-emmc.sherlock_sd_emmc.aml-sd-emmc.sdmmc.sdmmc-sdio.sdmmc-sdio-2",

      "aml-nna.aml_nna",
      "pwm",  // pwm
      "gpio-light.aml_light",
      "aml_gpu.aml-gpu-composite.aml-gpu",
      "sherlock-pdm-audio-in.aml_pdm.sherlock-audio-pdm-in",
      "sherlock-i2s-audio-out.aml_tdm.sherlock-audio-i2s-out",
      "i2c-1.aml-i2c.i2c.i2c-1-56.focaltech_touch",
      "tee.optee",
      "gpio-c.aml_gpio_c.aml-gpio.gpio.gpio-50.spi_0.aml-spi-0.spi.spi-0-0",
      "sherlock-buttons.buttons",
      "i2c-2.aml-i2c.i2c.i2c-2-44.backlight",
      "i2c-0.aml-i2c.i2c.i2c-0-57.tcs3400_light.tcs-3400",
      "aml-secure-mem.aml_securemem.aml-securemem",
      "pwm.amlogic_pwm.aml-pwm-device.pwm-4.pwm_init",
      "aml-ram-ctl.aml_ram.ram",
      "registers",  // registers device

      // CPU Devices.
      "aml-cpu",
      "05_04_a.aml_thermal_pll.thermal.aml_cpu_legacy.big-cluster",
      "05_04_a.aml_thermal_pll.thermal.aml_cpu_legacy.little-cluster",

      // Thermal devices.
      "05_04_a",
      "aml-thermal-ddr",
      "aml-thermal-ddr.thermal",

      "adc",
      "adc.aml_saradc.aml-saradc.0",
      "adc.aml_saradc.aml-saradc.SHERLOCK_THERMISTOR_BASE",
      "adc.aml_saradc.aml-saradc.SHERLOCK_THERMISTOR_AUDIO",
      "adc.aml_saradc.aml-saradc.SHERLOCK_THERMISTOR_AMBIENT",

      // Audio
      "i2c-0.aml-i2c.i2c.i2c-0-111.audio-tas5720-woofer",
      "i2c-0.aml-i2c.i2c.i2c-0-108.audio-tas5720-left-tweeter",
      "i2c-0.aml-i2c.i2c.i2c-0-109.audio-tas5720-right-tweeter",

      // LCD Bias
      "i2c-2.aml-i2c.i2c.i2c-2-62",

      // Touchscreen
      "i2c-1.aml-i2c.i2c.i2c-1-56.focaltech_touch.focaltouch-HidDevice",

      "bt-uart.bluetooth-composite-spec.aml-uart",

#ifdef include_packaged_drivers

      "mipi-csi2.aml-mipi",
      "mipi-csi2.aml-mipi.imx227_sensor",
      "mipi-csi2.aml-mipi.imx227_sensor.imx227.gdc",
      "mipi-csi2.aml-mipi.imx227_sensor.imx227.ge2d",

      "aml_video",
      "aml-video-enc",

      "gpio-c.aml_gpio_c.aml-gpio.gpio.gpio-50.spi_0.aml-spi-0.spi.spi-0-0.nrf52840_radio.ot-radio",

      // WLAN
      "sherlock-sd-emmc.sherlock_sd_emmc.aml-sd-emmc.sdmmc.sdmmc-sdio.sdmmc-sdio-1.wifi.brcmfmac-wlanphyimpl",
      "sherlock-sd-emmc.sherlock_sd_emmc.aml-sd-emmc.sdmmc.sdmmc-sdio.sdmmc-sdio-1.wifi.brcmfmac-wlanphyimpl.wlanphy",

      "mipi-csi2.aml-mipi.imx227_sensor.imx227.isp",
      "mipi-csi2.aml-mipi.imx227_sensor.imx227.isp.arm-isp.camera_controller",

      // Bluetooth
      "bt-uart.bluetooth-composite-spec.aml-uart.serial.bt-transport-uart",
      "bt-uart.bluetooth-composite-spec.aml-uart.serial.bt-transport-uart.bt-hci-broadcom",
#endif
  };
  VerifyNodes(kNodeMonikers);

  ASSERT_NO_FATAL_FAILURE(device_enumeration::WaitForClassDeviceCount("class/thermal", 2));
  ASSERT_NO_FATAL_FAILURE(device_enumeration::WaitForClassDeviceCount("class/adc", 4));
  ASSERT_NO_FATAL_FAILURE(device_enumeration::WaitForClassDeviceCount("class/temperature", 3));
}

}  // namespace
