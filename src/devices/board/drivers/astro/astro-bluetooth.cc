// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <fidl/fuchsia.hardware.serial/cpp/wire.h>
#include <lib/ddk/binding.h>
#include <lib/ddk/debug.h>
#include <lib/mmio/mmio.h>
#include <lib/zbi-format/zbi.h>
#include <unistd.h>

#include <bind/fuchsia/broadcom/platform/cpp/bind.h>
#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/gpio/cpp/bind.h>
#include <bind/fuchsia/pwm/cpp/bind.h>
#include <sdk/lib/driver/component/cpp/composite_node_spec.h>
#include <sdk/lib/driver/component/cpp/node_add_args.h>
#include <soc/aml-s905d2/s905d2-hw.h>

#include "astro.h"

namespace astro {
namespace fpbus = fuchsia_hardware_platform_bus;

static const std::vector<fpbus::Mmio> bt_uart_mmios{
    {{
        .base = S905D2_UART_A_BASE,
        .length = S905D2_UART_A_LENGTH,
    }},
};

static const std::vector<fpbus::Irq> bt_uart_irqs{
    {{
        .irq = S905D2_UART_A_IRQ,
        .mode = fpbus::ZirconInterruptMode::kEdgeHigh,
    }},
};

static const fuchsia_hardware_serial::wire::SerialPortInfo bt_uart_serial_info = {
    .serial_class = fuchsia_hardware_serial::Class::kBluetoothHci,
    .serial_vid = bind_fuchsia_broadcom_platform::BIND_PLATFORM_DEV_VID_BROADCOM,
    .serial_pid = bind_fuchsia_broadcom_platform::BIND_PLATFORM_DEV_PID_BCM43458,
};

static const std::vector<fpbus::BootMetadata> bt_uart_boot_metadata{
    {{
        .zbi_type = ZBI_TYPE_DRV_MAC_ADDRESS,
        .zbi_extra = MACADDR_BLUETOOTH,
    }},
};

zx_status_t Astro::BluetoothInit() {
  // set alternate functions to enable Bluetooth UART
  gpio_init_steps_.push_back(GpioFunction(S905D2_UART_TX_A, S905D2_UART_TX_A_FN));
  gpio_init_steps_.push_back(GpioFunction(S905D2_UART_RX_A, S905D2_UART_RX_A_FN));
  gpio_init_steps_.push_back(GpioFunction(S905D2_UART_CTS_A, S905D2_UART_CTS_A_FN));
  gpio_init_steps_.push_back(GpioFunction(S905D2_UART_RTS_A, S905D2_UART_RTS_A_FN));

  fdf::Arena arena('BLUE');

  fuchsia_driver_framework::wire::BindRule2 kPwmBindRules[] = {
      // TODO(https://fxbug.dev/42079489): Replace this with wire type function.
      fidl::ToWire(arena, fdf::MakeAcceptBindRule2(bind_fuchsia::INIT_STEP,
                                                   bind_fuchsia_pwm::BIND_INIT_STEP_PWM)),
  };

  fuchsia_driver_framework::wire::NodeProperty2 kPwmProperties[] = {
      fdf::MakeProperty2(arena, bind_fuchsia::INIT_STEP, bind_fuchsia_pwm::BIND_INIT_STEP_PWM),
  };

  fuchsia_driver_framework::wire::BindRule2 kGpioBindRules[] = {
      fidl::ToWire(arena, fdf::MakeAcceptBindRule2(bind_fuchsia::INIT_STEP,
                                                   bind_fuchsia_gpio::BIND_INIT_STEP_GPIO)),
  };

  fuchsia_driver_framework::wire::NodeProperty2 kGpioProperties[] = {
      fdf::MakeProperty2(arena, bind_fuchsia::INIT_STEP, bind_fuchsia_gpio::BIND_INIT_STEP_GPIO),
  };

  auto parents = std::vector{
      fuchsia_driver_framework::wire::ParentSpec2{
          .bind_rules = fidl::VectorView<fuchsia_driver_framework::wire::BindRule2>::FromExternal(
              kPwmBindRules, 1),
          .properties =
              fidl::VectorView<fuchsia_driver_framework::wire::NodeProperty2>::FromExternal(
                  kPwmProperties, 1),
      },
      fuchsia_driver_framework::wire::ParentSpec2{
          .bind_rules = fidl::VectorView<fuchsia_driver_framework::wire::BindRule2>::FromExternal(
              kGpioBindRules, 1),
          .properties =
              fidl::VectorView<fuchsia_driver_framework::wire::NodeProperty2>::FromExternal(
                  kGpioProperties, 1),
      },
  };

  auto builder =
      fuchsia_driver_framework::wire::CompositeNodeSpec::Builder(arena)
          .name("bluetooth-composite-spec")
          .parents2(fidl::VectorView<fuchsia_driver_framework::wire::ParentSpec2>(arena, parents));

  fit::result encoded = fidl::Persist(bt_uart_serial_info);
  if (encoded.is_error()) {
    zxlogf(ERROR, "Failed to encode serial metadata: %s",
           encoded.error_value().FormatDescription().c_str());
    return encoded.error_value().status();
  }

  const std::vector<fpbus::Metadata> bt_uart_metadata{
      {{
          .id = fuchsia_hardware_serial::wire::SerialPortInfo::kSerializableName,
          .data = *std::move(encoded),
      }},
  };

  const fpbus::Node bt_uart_dev = [&]() {
    fpbus::Node dev = {};
    dev.name() = "bt-uart";
    dev.vid() = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_VID_AMLOGIC;
    dev.pid() = bind_fuchsia_platform::BIND_PLATFORM_DEV_PID_GENERIC;
    dev.did() = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_DID_UART;
    dev.mmio() = bt_uart_mmios;
    dev.irq() = bt_uart_irqs;
    dev.metadata() = bt_uart_metadata;
    dev.boot_metadata() = bt_uart_boot_metadata;
    return dev;
  }();

  // Create composite spec for aml-uart based on UART and PWM nodes. The parent spec of bt_uart_dev
  // will be generated by the handler of AddCompositeNodeSpec.
  auto result =
      pbus_.buffer(arena)->AddCompositeNodeSpec(fidl::ToWire(arena, bt_uart_dev), builder.Build());
  if (!result.ok()) {
    zxlogf(ERROR, "AddCompositeNodeSpec Bluetooth(bt_uart_dev) request failed: %s",
           result.FormatDescription().data());
    return result.status();
  }
  if (result->is_error()) {
    zxlogf(ERROR, "AddCompositeNodeSpec Bluetooth(bt_uart_dev) failed: %s",
           zx_status_get_string(result->error_value()));
    return result->error_value();
  }

  return ZX_OK;
}

}  // namespace astro
