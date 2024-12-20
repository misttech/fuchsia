// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/metadata.h>

#include <ddk/metadata/gpio.h>
#include <soc/aml-s905d2/s905d2-gpio.h>
#include <soc/aml-s905d2/s905d2-hw.h>

#include "astro-gpios.h"
#include "astro.h"

// uncomment to disable LED blinky test
// #define GPIO_TEST

namespace astro {
namespace fpbus = fuchsia_hardware_platform_bus;

static const std::vector<fpbus::Mmio> gpio_mmios{
    {{
        .base = S905D2_GPIO_BASE,
        .length = S905D2_GPIO_LENGTH,
    }},
    {{
        .base = S905D2_GPIO_AO_BASE,
        .length = S905D2_GPIO_AO_LENGTH,
    }},
    {{
        .base = S905D2_GPIO_INTERRUPT_BASE,
        .length = S905D2_GPIO_INTERRUPT_LENGTH,
    }},
};

static const std::vector<fpbus::Irq> gpio_irqs{
    {{
        .irq = S905D2_GPIO_IRQ_0,
        .mode = fpbus::ZirconInterruptMode::kDefault,
    }},
    {{
        .irq = S905D2_GPIO_IRQ_1,
        .mode = fpbus::ZirconInterruptMode::kDefault,
    }},
    {{
        .irq = S905D2_GPIO_IRQ_2,
        .mode = fpbus::ZirconInterruptMode::kDefault,
    }},
    {{
        .irq = S905D2_GPIO_IRQ_3,
        .mode = fpbus::ZirconInterruptMode::kDefault,
    }},
    {{
        .irq = S905D2_GPIO_IRQ_4,
        .mode = fpbus::ZirconInterruptMode::kDefault,
    }},
    {{
        .irq = S905D2_GPIO_IRQ_5,
        .mode = fpbus::ZirconInterruptMode::kDefault,
    }},
    {{
        .irq = S905D2_GPIO_IRQ_6,
        .mode = fpbus::ZirconInterruptMode::kDefault,
    }},
    {{
        .irq = S905D2_GPIO_IRQ_7,
        .mode = fpbus::ZirconInterruptMode::kDefault,
    }},
};

// GPIOs to expose from generic GPIO driver.
static const gpio_pin_t gpio_pins[] = {
    // For wifi.
    DECL_GPIO_PIN(S905D2_WIFI_SDIO_WAKE_HOST),
    // For display.
    DECL_GPIO_PIN(GPIO_PANEL_DETECT),
    DECL_GPIO_PIN(GPIO_LCD_RESET),
    // For touch screen.
    DECL_GPIO_PIN(GPIO_TOUCH_INTERRUPT),
    DECL_GPIO_PIN(GPIO_TOUCH_RESET),
    // For light sensor.
    DECL_GPIO_PIN(GPIO_LIGHT_INTERRUPT),
    // For audio.
    DECL_GPIO_PIN(GPIO_AUDIO_SOC_FAULT_L),
    DECL_GPIO_PIN(GPIO_SOC_AUDIO_EN),
    // For buttons.
    DECL_GPIO_PIN(GPIO_VOLUME_UP),
    DECL_GPIO_PIN(GPIO_VOLUME_DOWN),
    DECL_GPIO_PIN(GPIO_VOLUME_BOTH),
    DECL_GPIO_PIN(GPIO_MIC_PRIVACY),
    // For SDIO.
    DECL_GPIO_PIN(GPIO_SDIO_RESET),
    // For Bluetooth.
    DECL_GPIO_PIN(GPIO_SOC_WIFI_LPO_32k768),
    DECL_GPIO_PIN(GPIO_SOC_BT_REG_ON),
    // For lights.
    DECL_GPIO_PIN(GPIO_AMBER_LED),

    // Board revision GPIOs.
    DECL_GPIO_PIN(GPIO_HW_ID0),
    DECL_GPIO_PIN(GPIO_HW_ID1),
    DECL_GPIO_PIN(GPIO_HW_ID2),
};

zx_status_t Astro::GpioInit() {
  fuchsia_hardware_pinimpl::Metadata metadata{{std::move(gpio_init_steps_)}};
  gpio_init_steps_.clear();

  const fit::result encoded_metadata = fidl::Persist(metadata);
  if (!encoded_metadata.is_ok()) {
    zxlogf(ERROR, "Failed to encode GPIO init metadata: %s",
           encoded_metadata.error_value().FormatDescription().c_str());
    return encoded_metadata.error_value().status();
  }

  const std::vector<fpbus::Metadata> gpio_metadata{
      {{
          .id = std::to_string(DEVICE_METADATA_GPIO_PINS),
          .data = std::vector<uint8_t>(
              reinterpret_cast<const uint8_t*>(&gpio_pins),
              reinterpret_cast<const uint8_t*>(&gpio_pins) + sizeof(gpio_pins)),
      }},
      {{
          .id = std::to_string(DEVICE_METADATA_GPIO_CONTROLLER),
          .data = encoded_metadata.value(),
      }},
  };

  fpbus::Node gpio_dev;
  gpio_dev.name() = "gpio";
  gpio_dev.vid() = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_VID_AMLOGIC;
  gpio_dev.pid() = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_PID_S905D2;
  gpio_dev.did() = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_DID_GPIO;
  gpio_dev.mmio() = gpio_mmios;
  gpio_dev.irq() = gpio_irqs;
  gpio_dev.metadata() = gpio_metadata;

  fidl::Arena<> fidl_arena;
  fdf::Arena arena('GPIO');
  auto result = pbus_.buffer(arena)->NodeAdd(fidl::ToWire(fidl_arena, gpio_dev));
  if (!result.ok()) {
    zxlogf(ERROR, "%s: NodeAdd Gpio(gpio_dev) request failed: %s", __func__,
           result.FormatDescription().data());
    return result.status();
  }
  if (result->is_error()) {
    zxlogf(ERROR, "%s: NodeAdd Gpio(gpio_dev) failed: %s", __func__,
           zx_status_get_string(result->error_value()));
    return result->error_value();
  }

#ifdef GPIO_TEST
  static const pbus_gpio_t gpio_test_gpios[] = {{
                                                    // SYS_LED
                                                    .gpio = S905D2_GPIOAO(11),
                                                },
                                                {
                                                    // JTAG Adapter Pin
                                                    .gpio = S905D2_GPIOAO(6),
                                                }};

  fpbus::Node gpio_test_dev;
  fpbus::Node dev = {};
  dev.name() = "astro-gpio-test";
  dev.vid() = bind_fuchsia_platform::BIND_PLATFORM_DEV_VID_GENERIC;
  dev.pid() = bind_fuchsia_platform::BIND_PLATFORM_DEV_PID_GENERIC;
  dev.did() = bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_GPIO_TEST;
  dev.gpio() = gpio_test_gpios;
  return dev;
}
();

result = pbus_.buffer(arena)->NodeAdd(fidl::ToWire(fidl_arena, gpio_test_dev));
if (!result.ok()) {
  zxlogf(ERROR, "%s: NodeAdd Gpio(gpio_test_dev) request failed: %s", __func__,
         result.FormatDescription().data());
  return result.status();
}
if (result->is_error()) {
  zxlogf(ERROR, "%s: NodeAdd Gpio(gpio_test_dev) failed: %s", __func__,
         zx_status_get_string(result->error_value()));
  return result->error_value();
}
#endif

return ZX_OK;
}

}  // namespace astro
