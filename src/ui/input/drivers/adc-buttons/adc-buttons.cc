// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "adc-buttons.h"

#include <fidl/fuchsia.buttons/cpp/fidl.h>
#include <fidl/fuchsia.driver.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/driver/fidl.h>
#include <fidl/fuchsia.input.report/cpp/fidl.h>
#include <fidl/fuchsia.input/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/driver/platform-device/cpp/pdev.h>

#include "src/ui/input/drivers/adc-buttons/adc-buttons_parser.h"

namespace adc_buttons {

namespace {

constexpr uint32_t kDefaultPollingRateUsec = 1'000;

struct MetadataValues {
  uint32_t polling_rate_usec = kDefaultPollingRateUsec;
  std::map<uint32_t, std::vector<fuchsia_buttons::Button>> configs;
  std::set<fuchsia_input::ConsumerControlButton> buttons;
};

fuchsia_input::ConsumerControlButton ConvertConsumerControlButton(
    adc_buttons::ConsumerControlButton type) {
  switch (type) {
    case adc_buttons::ConsumerControlButton::kCameraDisable:
      return fuchsia_input::ConsumerControlButton::kCameraDisable;
    case adc_buttons::ConsumerControlButton::kFactoryReset:
      return fuchsia_input::ConsumerControlButton::kFactoryReset;
    case adc_buttons::ConsumerControlButton::kFunction:
      return fuchsia_input::ConsumerControlButton::kFunction;
    case adc_buttons::ConsumerControlButton::kMicMute:
      return fuchsia_input::ConsumerControlButton::kMicMute;
    case adc_buttons::ConsumerControlButton::kPause:
      return fuchsia_input::ConsumerControlButton::kPause;
    case adc_buttons::ConsumerControlButton::kPower:
      return fuchsia_input::ConsumerControlButton::kPower;
    case adc_buttons::ConsumerControlButton::kReboot:
      return fuchsia_input::ConsumerControlButton::kReboot;
    case adc_buttons::ConsumerControlButton::kVolumeDown:
      return fuchsia_input::ConsumerControlButton::kVolumeDown;
    case adc_buttons::ConsumerControlButton::kVolumeUp:
      return fuchsia_input::ConsumerControlButton::kVolumeUp;
  }
}

fuchsia_buttons::AdcButtonsMetadata ConvertMetadata(
    const adc_buttons::Adc_buttonsMetadata& parsed) {
  fuchsia_buttons::AdcButtonsMetadata metadata;

  if (parsed.polling_rate_usec.has_value()) {
    metadata.polling_rate_usec(*parsed.polling_rate_usec);
  }

  if (parsed.buttons.has_value()) {
    std::vector<fuchsia_buttons::Button> buttons;
    for (const auto& btn : *parsed.buttons) {
      fuchsia_buttons::Button button;

      if (btn.types.has_value()) {
        std::vector<fuchsia_input::ConsumerControlButton> button_types;
        for (const auto& type : *btn.types) {
          button_types.push_back(ConvertConsumerControlButton(type));
        }
        button.types(std::move(button_types));
      }

      if (btn.button_config.has_value() && btn.button_config->adc.has_value()) {
        const auto& adc = *btn.button_config->adc;
        fuchsia_buttons::AdcButtonConfig adc_config;
        if (adc.channel_idx.has_value()) {
          adc_config.channel_idx(*adc.channel_idx);
        }
        if (adc.release_threshold.has_value()) {
          adc_config.release_threshold(*adc.release_threshold);
        }
        if (adc.press_threshold.has_value()) {
          adc_config.press_threshold(*adc.press_threshold);
        }
        button.button_config(fuchsia_buttons::ButtonConfig::WithAdc(std::move(adc_config)));
      }

      buttons.push_back(std::move(button));
    }
    metadata.buttons(std::move(buttons));
  }

  return metadata;
}

zx::result<MetadataValues> ParseMetadata(const fuchsia_buttons::AdcButtonsMetadata& metadata) {
  if (!metadata.polling_rate_usec().has_value()) {
    fdf::error("Metadata missing `polling_rate_usec` field");
    return zx::error(ZX_ERR_INTERNAL);
  }

  if (!metadata.buttons().has_value()) {
    fdf::error("Metadata missing `buttons` field");
    return zx::error(ZX_ERR_INTERNAL);
  }

  MetadataValues ret{.polling_rate_usec = metadata.polling_rate_usec().value()};

  const auto& buttons = metadata.buttons().value();
  for (size_t i = 0; i < buttons.size(); ++i) {
    const auto& button = buttons[i];
    if (button.button_config()->Which() != fuchsia_buttons::ButtonConfig::Tag::kAdc) {
      fdf::warn("Ignoring button {}: Button config not of type adc", i);
      continue;
    }
    ret.buttons.insert(button.types()->begin(), button.types()->end());
    ret.configs[*button.button_config()->adc()->channel_idx()].emplace_back(std::move(button));
  }
  if (ret.buttons.size() >= fuchsia_input::kConsumerControlMaxNumButtons) {
    fdf::error(
        "{}: More buttons than expected (max = {}). Please increase kConsumerControlMaxNumButtons",
        __func__, fuchsia_input::kConsumerControlMaxNumButtons);
    return zx::error(ZX_ERR_INTERNAL);
  }

  if (ret.configs.empty() || ret.buttons.empty()) {
    fdf::error("{}: failed to get button configs in metadata.", __func__);
    return zx::error(ZX_ERR_INTERNAL);
  }

  return zx::ok(std::move(ret));
}

}  // namespace

zx::result<> AdcButtons::Start(fdf::DriverContext context) {
  zx::result pdev_client_end =
      context.incoming().Connect<fuchsia_hardware_platform_device::Service::Device>("pdev");
  if (pdev_client_end.is_error()) {
    fdf::error("Failed to connect to platform device: {}", pdev_client_end);
    return pdev_client_end.take_error();
  }
  fdf::PDev pdev{std::move(pdev_client_end.value())};

  // Get metadata.
  fuchsia_buttons::AdcButtonsMetadata metadata;
  zx::result metadata_result = pdev.GetFidlMetadata<fuchsia_buttons::AdcButtonsMetadata>();
  if (metadata_result.is_ok()) {
    metadata = std::move(metadata_result.value());
  } else {
    // Fall back to reading fuchsia_driver_metadata::Dictionary
    auto dict_result = pdev.GetFidlMetadata<fuchsia_driver_metadata::Dictionary>(
        "fuchsia.buttons.AdcButtonsMetadata");
    if (dict_result.is_error()) {
      fdf::error("Failed to get metadata as AdcButtonsMetadata ({}) or Dictionary ({})",
                 metadata_result, dict_result);
      return metadata_result.take_error();
    }
    auto parsed = adc_buttons::Adc_buttonsMetadata::Parse(dict_result.value());
    if (!parsed.has_value()) {
      fdf::error("Failed to parse AdcButtonsMetadata from Dictionary");
      return zx::error(ZX_ERR_INTERNAL);
    }
    metadata = ConvertMetadata(*parsed);
  }
  zx::result values = ParseMetadata(metadata);
  if (values.is_error()) {
    fdf::error("Failed to parse metadata: {}", values);
    return values.take_error();
  }

  // Get adc.
  std::vector<adc_buttons_device::AdcButtonsDevice::AdcButtonClient> clients;
  for (auto& [idx, buttons] : values.value().configs) {
    char adc_name[32];
    sprintf(adc_name, "adc-%u", idx);
    zx::result result = context.incoming().Connect<fuchsia_hardware_adc::Service::Device>(adc_name);
    if (result.is_error()) {
      fdf::error("Failed to open adc service: {}", result);
      return result.take_error();
    }
    clients.emplace_back(std::move(result.value()), std::move(buttons));
  }

  device_ = std::make_unique<adc_buttons_device::AdcButtonsDevice>(
      dispatcher(), std::move(clients), zx::usec(values.value().polling_rate_usec),
      std::move(values.value().buttons));

  zx::result result = outgoing()->AddService<fuchsia_input_report::Service>(
      fuchsia_input_report::Service::InstanceHandler({
          .input_device = input_report_bindings_.CreateHandler(device_.get(), dispatcher(),
                                                               fidl::kIgnoreBindingClosure),
      }));
  if (result.is_error()) {
    fdf::error("Failed to add input report service: {}", result);
    return result.take_error();
  }

  return zx::ok();
}

void AdcButtons::Stop(fdf::StopCompleter completer) {
  device_->Shutdown();
  completer(zx::ok());
}

}  // namespace adc_buttons

FUCHSIA_DRIVER_EXPORT2(adc_buttons::AdcButtons);
