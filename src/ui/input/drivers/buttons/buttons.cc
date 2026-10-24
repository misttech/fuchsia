// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "buttons.h"

#include <fidl/fuchsia.buttons/cpp/fidl.h>
#include <fidl/fuchsia.driver.compat/cpp/wire.h>
#include <fidl/fuchsia.driver.metadata/cpp/fidl.h>
#include <lib/ddk/metadata.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/compat/cpp/metadata.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/platform-device/cpp/pdev.h>

#include <fbl/alloc_checker.h>

#include "src/ui/input/drivers/buttons/buttons_parser.h"

namespace buttons {

namespace {

fuchsia_buttons::GpioButtonsMetadata ConvertMetadata(
    const buttons_metadata::ButtonsMetadata& parsed) {
  fuchsia_buttons::GpioButtonsMetadata metadata;
  if (parsed.buttons.has_value()) {
    std::vector<fuchsia_buttons::GpioButtonConfig> buttons;
    for (const auto& btn : *parsed.buttons) {
      fuchsia_buttons::GpioButtonConfig config;
      if (btn.id.has_value()) {
        switch (*btn.id) {
          case buttons_metadata::GpioButtonId::kVolumeUp:
            config.id(fuchsia_buttons::GpioButtonId::kVolumeUp);
            break;
          case buttons_metadata::GpioButtonId::kVolumeDown:
            config.id(fuchsia_buttons::GpioButtonId::kVolumeDown);
            break;
          case buttons_metadata::GpioButtonId::kFdr:
            config.id(fuchsia_buttons::GpioButtonId::kFdr);
            break;
          case buttons_metadata::GpioButtonId::kMicMute:
            config.id(fuchsia_buttons::GpioButtonId::kMicMute);
            break;
          case buttons_metadata::GpioButtonId::kPlayPause:
            config.id(fuchsia_buttons::GpioButtonId::kPlayPause);
            break;
          case buttons_metadata::GpioButtonId::kKeyA:
            config.id(fuchsia_buttons::GpioButtonId::kKeyA);
            break;
          case buttons_metadata::GpioButtonId::kKeyM:
            config.id(fuchsia_buttons::GpioButtonId::kKeyM);
            break;
          case buttons_metadata::GpioButtonId::kCamMute:
            config.id(fuchsia_buttons::GpioButtonId::kCamMute);
            break;
          case buttons_metadata::GpioButtonId::kMicAndCamMute:
            config.id(fuchsia_buttons::GpioButtonId::kMicAndCamMute);
            break;
          case buttons_metadata::GpioButtonId::kFunction:
            config.id(fuchsia_buttons::GpioButtonId::kFunction);
            break;
          case buttons_metadata::GpioButtonId::kPower:
            config.id(fuchsia_buttons::GpioButtonId::kPower);
            break;
        }
      }
      if (btn.gpio_a_index.has_value()) {
        config.gpio_a_index(*btn.gpio_a_index);
      }
      if (btn.gpio_delay.has_value()) {
        config.gpio_delay(*btn.gpio_delay);
      }
      if (btn.type.has_value()) {
        if (btn.type->direct.has_value()) {
          config.type(
              fuchsia_buttons::GpioButtonType::WithDirect(fuchsia_buttons::DirectGpioButton{}));
        } else if (btn.type->matrix.has_value()) {
          fuchsia_buttons::MatrixGpioButton matrix;
          if (btn.type->matrix->gpio_b_index.has_value()) {
            matrix.gpio_b_index(*btn.type->matrix->gpio_b_index);
          }
          config.type(fuchsia_buttons::GpioButtonType::WithMatrix(std::move(matrix)));
        }
      }
      buttons.push_back(std::move(config));
    }
    metadata.buttons(std::move(buttons));
  }

  if (parsed.gpios.has_value()) {
    std::vector<fuchsia_buttons::GpioConfig> gpios;
    for (const auto& g : *parsed.gpios) {
      fuchsia_buttons::GpioConfig gpio;
      if (g.flags.has_value()) {
        gpio.flags(static_cast<fuchsia_buttons::GpioFlag>(*g.flags));
      }
      if (g.type.has_value()) {
        if (g.type->interrupt.has_value()) {
          gpio.type(fuchsia_buttons::GpioType::WithInterrupt(fuchsia_buttons::InterruptGpio{}));
        } else if (g.type->matrix_output.has_value()) {
          fuchsia_buttons::MatrixOutputGpio matrix_out;
          if (g.type->matrix_output->output_value.has_value()) {
            matrix_out.output_value(*g.type->matrix_output->output_value);
          }
          gpio.type(fuchsia_buttons::GpioType::WithMatrixOutput(std::move(matrix_out)));
        } else if (g.type->poll.has_value()) {
          fuchsia_buttons::PollGpio poll;
          if (g.type->poll->period.has_value()) {
            poll.period(*g.type->poll->period);
          }
          gpio.type(fuchsia_buttons::GpioType::WithPoll(std::move(poll)));
        }
      }
      gpios.push_back(std::move(gpio));
    }
    metadata.gpios(std::move(gpios));
  }
  return metadata;
}

}  // namespace

zx::result<> Buttons::Start(fdf::DriverContext context) {
  config_ = context.take_config<buttons_config::Config>();
  zx::result pdev_client_end =
      context.incoming().Connect<fuchsia_hardware_platform_device::Service::Device>("pdev");
  if (pdev_client_end.is_error()) {
    fdf::error("Failed to connect to platform device: {}", pdev_client_end);
    return pdev_client_end.take_error();
  }
  fdf::PDev pdev{std::move(pdev_client_end.value())};

  fuchsia_buttons::GpioButtonsMetadata metadata;
  zx::result metadata_result = pdev.GetFidlMetadata<fuchsia_buttons::GpioButtonsMetadata>();
  if (metadata_result.is_ok()) {
    metadata = std::move(metadata_result.value());
  } else {
    // Fall back to reading fuchsia_driver_metadata::Dictionary
    auto dict_result = pdev.GetFidlMetadata<fuchsia_driver_metadata::Dictionary>(
        "fuchsia.buttons.GpioButtonsMetadata");
    if (dict_result.is_error()) {
      fdf::error("Failed to get metadata as GpioButtonsMetadata ({}) or Dictionary ({})",
                 metadata_result, dict_result);
      return metadata_result.take_error();
    }
    auto parsed = buttons_metadata::ButtonsMetadata::Parse(dict_result.value());
    if (!parsed.has_value()) {
      fdf::error("Failed to parse GpioButtonsMetadata from Dictionary");
      return zx::error(ZX_ERR_INTERNAL);
    }
    metadata = ConvertMetadata(*parsed);
  }

  if (!metadata.gpios().has_value()) {
    fdf::error("Metadata missing gpios");
    return zx::error(ZX_ERR_INTERNAL);
  }
  const std::span<const fuchsia_buttons::GpioConfig>& gpio_configs = metadata.gpios().value();
  std::vector<ButtonsDevice::Gpio> gpios;
  gpios.reserve(gpio_configs.size());

  if (!metadata.buttons().has_value()) {
    fdf::error("Metadata missing buttons");
    return zx::error(ZX_ERR_INTERNAL);
  }
  std::vector<fuchsia_buttons::GpioButtonConfig> buttons = std::move(metadata.buttons().value());

  for (size_t i = 0; i < gpio_configs.size(); ++i) {
    const char* name;
    const auto& button = buttons[i];
    if (!button.id().has_value()) {
      fdf::error("Button {} missing id", i);
      return zx::error(ZX_ERR_INTERNAL);
    }
    const fuchsia_buttons::GpioButtonId& button_id = button.id().value();
    switch (button_id) {
      case fuchsia_buttons::GpioButtonId::kVolumeUp:
        name = "volume-up";
        break;
      case fuchsia_buttons::GpioButtonId::kVolumeDown:
        name = "volume-down";
        break;
      case fuchsia_buttons::GpioButtonId::kFdr:
        name = "volume-both";
        break;
      case fuchsia_buttons::GpioButtonId::kMicMute:
      case fuchsia_buttons::GpioButtonId::kMicAndCamMute:
        name = "mic-mute";
        break;
      case fuchsia_buttons::GpioButtonId::kCamMute:
        name = "cam-mute";
        break;
      case fuchsia_buttons::GpioButtonId::kPower:
        name = "power";
        break;
      case fuchsia_buttons::GpioButtonId::kPlayPause:
        name = "play-pause";
        break;
      case fuchsia_buttons::GpioButtonId::kKeyA:
        name = "key-a";
        break;
      case fuchsia_buttons::GpioButtonId::kKeyM:
        name = "key-m";
        break;
      case fuchsia_buttons::GpioButtonId::kFunction:
        name = "function";
        break;
      default:
        fdf::error("Button {} has unknown id: {}", i, static_cast<uint32_t>(button_id));
        return zx::error(ZX_ERR_NOT_SUPPORTED);
    };
    zx::result gpio_client =
        context.incoming().Connect<fuchsia_hardware_gpio::Service::Device>(name);
    if (gpio_client.is_error() || !gpio_client->is_valid()) {
      fdf::error("Connect to GPIO {} failed: {}", name, gpio_client);
      return gpio_client.take_error();
    }
    gpios.emplace_back(ButtonsDevice::Gpio{
        .client{std::move(gpio_client.value())}, .irq{}, .config = gpio_configs[i]});
  }

  fidl::ClientEnd<fuchsia_power_system::ActivityGovernor> sag_client;
  if (config_.suspend_enabled()) {
    auto sag_result = context.incoming().Connect<fuchsia_power_system::ActivityGovernor>();
    if (sag_result.is_ok() && sag_result->is_valid()) {
      sag_client = std::move(sag_result.value());
    } else {
      fdf::error(
          "Failed to connect to fuchsia.power.system.ActivityGovernor: {}; system may incorrectly enter suspend.",
          sag_result);
    }
  }

  device_ = std::make_unique<buttons::ButtonsDevice>(dispatcher(), std::move(buttons),
                                                     std::move(gpios), std::move(sag_client));

  zx::result result = outgoing()->AddService<fuchsia_input_report::Service>(
      fuchsia_input_report::Service::InstanceHandler({
          .input_device = input_report_bindings_.CreateHandler(device_.get(), dispatcher(),
                                                               fidl::kIgnoreBindingClosure),
      }));
  if (result.is_error()) {
    fdf::error("Failed to add input report service: {}", result);
    return result.take_error();
  }

  zx::result child = AddOwnedChild(kDeviceName);
  if (child.is_error()) {
    fdf::error("Failed to add child: {}", child);
    return child.take_error();
  }
  child_ = std::move(child.value());

  return zx::ok();
}

void Buttons::Stop(fdf::StopCompleter completer) {
  device_->ShutDown();
  completer(zx::ok());
}

}  // namespace buttons

FUCHSIA_DRIVER_EXPORT2(buttons::Buttons);
