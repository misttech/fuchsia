// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "buttons.h"

#include <fidl/fuchsia.buttons/cpp/fidl.h>
#include <fidl/fuchsia.driver.compat/cpp/wire.h>
#include <lib/ddk/metadata.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/compat/cpp/metadata.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/platform-device/cpp/pdev.h>

#include <fbl/alloc_checker.h>

namespace buttons {

zx::result<> Buttons::Start() {
  zx::result pdev_client_end =
      incoming()->Connect<fuchsia_hardware_platform_device::Service::Device>("pdev");
  if (pdev_client_end.is_error()) {
    FDF_LOG(ERROR, "Failed to connect to platform device: %s", pdev_client_end.status_string());
    return pdev_client_end.take_error();
  }
  fdf::PDev pdev{std::move(pdev_client_end.value())};

  zx::result buttons_metadata_result = pdev.GetFidlMetadata<fuchsia_buttons::GpioButtonsMetadata>();
  if (buttons_metadata_result.is_error()) {
    FDF_LOG(ERROR, "Failed to get buttons metadata: %s", buttons_metadata_result.status_string());
    return buttons_metadata_result.take_error();
  }
  fuchsia_buttons::GpioButtonsMetadata& buttons_metadata = buttons_metadata_result.value();

  if (!buttons_metadata.buttons().has_value()) {
    FDF_LOG(ERROR, "Metadata missing buttons");
    return zx::error(ZX_ERR_INTERNAL);
  }
  std::vector<fuchsia_buttons::GpioButtonConfig> buttons =
      std::move(buttons_metadata.buttons().value());

  zx::result gpio_metadata =
      compat::GetMetadataArray<buttons_gpio_config_t>(incoming(), DEVICE_METADATA_BUTTONS_GPIOS);
  if (gpio_metadata.is_error()) {
    FDF_LOG(ERROR, "Failed to get gpio metadata: %s", gpio_metadata.status_string());
    return gpio_metadata.take_error();
  }
  std::vector gpio_configs = std::move(gpio_metadata.value());
  std::vector<ButtonsDevice::Gpio> gpios;
  gpios.reserve(gpio_configs.size());

  for (size_t i = 0; i < gpio_configs.size(); ++i) {
    const char* name;
    const auto& button = buttons[i];
    if (!button.id().has_value()) {
      FDF_LOG(ERROR, "Button %zu missing id", i);
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
        name = "mic-privacy";
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
        FDF_LOG(ERROR, "Button %zu has unknown id: %" PRIu32, i, static_cast<uint32_t>(button_id));
        return zx::error(ZX_ERR_NOT_SUPPORTED);
    };
    zx::result gpio_client = incoming()->Connect<fuchsia_hardware_gpio::Service::Device>(name);
    if (gpio_client.is_error() || !gpio_client->is_valid()) {
      FDF_LOG(ERROR, "Connect to GPIO %s failed: %s", name, gpio_client.status_string());
      return gpio_client.take_error();
    }
    gpios.emplace_back(ButtonsDevice::Gpio{
        .client{std::move(gpio_client.value())}, .irq{}, .config = gpio_configs[i]});
  }

  fidl::ClientEnd<fuchsia_power_system::ActivityGovernor> sag_client;
  if (config_.suspend_enabled()) {
    auto sag_result = incoming()->Connect<fuchsia_power_system::ActivityGovernor>();
    if (sag_result.is_ok() && sag_result->is_valid()) {
      sag_client = std::move(sag_result.value());
    } else {
      FDF_LOG(
          ERROR,
          "Failed to connect to fuchsia.power.system.ActivityGovernor: %s; system may incorrectly enter suspend.",
          sag_result.status_string());
    }
  }

  device_ = std::make_unique<buttons::ButtonsDevice>(dispatcher(), std::move(buttons),
                                                     std::move(gpios), std::move(sag_client));

  auto result = outgoing()->component().AddUnmanagedProtocol<fuchsia_input_report::InputDevice>(
      input_report_bindings_.CreateHandler(device_.get(), dispatcher(),
                                           fidl::kIgnoreBindingClosure),
      kDeviceName);
  if (result.is_error()) {
    FDF_LOG(ERROR, "Failed to add input report service: %s", result.status_string());
    return result.take_error();
  }

  if (zx::result result = CreateDevfsNode(); result.is_error()) {
    FDF_LOG(ERROR, "Failed to export to devfs: %s", result.status_string());
    return result.take_error();
  }

  return zx::ok();
}

void Buttons::PrepareStop(fdf::PrepareStopCompleter completer) {
  device_->ShutDown();
  completer(zx::ok());
}

zx::result<> Buttons::CreateDevfsNode() {
  fidl::Arena arena;
  zx::result connector = devfs_connector_.Bind(dispatcher());
  if (connector.is_error()) {
    return connector.take_error();
  }

  auto devfs = fuchsia_driver_framework::wire::DevfsAddArgs::Builder(arena)
                   .connector(std::move(connector.value()))
                   .class_name("input-report");

  auto args = fuchsia_driver_framework::wire::NodeAddArgs::Builder(arena)
                  .name(arena, kDeviceName)
                  .devfs_args(devfs.Build())
                  .Build();

  auto controller_endpoints = fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();

  zx::result node_endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::Node>();
  ZX_ASSERT_MSG(node_endpoints.is_ok(), "Failed to create node endpoints: %s",
                node_endpoints.status_string());

  fidl::WireResult result = fidl::WireCall(node())->AddChild(
      args, std::move(controller_endpoints.server), std::move(node_endpoints->server));
  if (!result.ok()) {
    FDF_LOG(ERROR, "Failed to add child %s", result.status_string());
    return zx::error(result.status());
  }
  controller_.Bind(std::move(controller_endpoints.client));
  node_.Bind(std::move(node_endpoints->client));
  return zx::ok();
}

}  // namespace buttons

FUCHSIA_DRIVER_EXPORT(buttons::Buttons);
