// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "buttons-device.h"

#include <lib/driver/logging/cpp/structured_logger.h>
#include <lib/zx/clock.h>

#include <cinttypes>
#include <cstddef>

#include <fbl/alloc_checker.h>

namespace buttons {

void ButtonsDevice::ButtonsInputReport::ToFidlInputReport(
    fidl::WireTableBuilder<::fuchsia_input_report::wire::InputReport>& input_report,
    fidl::AnyArena& allocator) const {
  fidl::VectorView<fuchsia_input_report::wire::ConsumerControlButton> buttons_rpt(
      allocator, fuchsia_input_report::wire::kConsumerControlMaxNumButtons);
  size_t count = 0;
  bool mic_mute = false;
  bool cam_mute = false;
  for (uint32_t id = 0; id < buttons.size(); id++) {
    if (!buttons[id]) {
      continue;
    }

    switch (static_cast<fuchsia_buttons::GpioButtonId>(id)) {
      case fuchsia_buttons::GpioButtonId::kPower:
        buttons_rpt[count] = fuchsia_input_report::ConsumerControlButton::kPower;
        count++;
        break;
      case fuchsia_buttons::GpioButtonId::kVolumeUp:
        buttons_rpt[count] = fuchsia_input_report::ConsumerControlButton::kVolumeUp;
        count++;
        break;
      case fuchsia_buttons::GpioButtonId::kVolumeDown:
        buttons_rpt[count] = fuchsia_input_report::ConsumerControlButton::kVolumeDown;
        count++;
        break;
      case fuchsia_buttons::GpioButtonId::kFdr:
        buttons_rpt[count] = fuchsia_input_report::ConsumerControlButton::kFactoryReset;
        count++;
        break;
      case fuchsia_buttons::GpioButtonId::kMicMute:
        if (!mic_mute) {
          buttons_rpt[count] = fuchsia_input_report::ConsumerControlButton::kMicMute;
          count++;
          mic_mute = true;
        }
        break;
      case fuchsia_buttons::GpioButtonId::kCamMute:
        if (!cam_mute) {
          buttons_rpt[count] = fuchsia_input_report::ConsumerControlButton::kCameraDisable;
          count++;
          cam_mute = true;
        }
        break;
      case fuchsia_buttons::GpioButtonId::kMicAndCamMute:
        if (!mic_mute) {
          buttons_rpt[count] = fuchsia_input_report::ConsumerControlButton::kMicMute;
          count++;
          mic_mute = true;
        }
        if (!cam_mute) {
          buttons_rpt[count] = fuchsia_input_report::ConsumerControlButton::kCameraDisable;
          count++;
          cam_mute = true;
        }
        break;
      default:
        FDF_LOG(ERROR, "Invalid Button ID encountered %d", id);
    }
  }
  buttons_rpt.set_size(count);

  auto consumer_control =
      fuchsia_input_report::wire::ConsumerControlInputReport::Builder(allocator).pressed_buttons(
          buttons_rpt);
  input_report.event_time(event_time.get()).consumer_control(consumer_control.Build());
}

void ButtonsDevice::Notify(size_t button_index) {
  auto result = GetInputReportInternal();
  if (result.is_error()) {
    FDF_LOG(ERROR, "GetInputReport failed %s", zx_status_get_string(result.error_value()));
  } else if (!last_report_.has_value() || *last_report_ != result.value()) {
    last_report_ = result.value();
    readers_.SendReportToAllReaders(*last_report_);

    if (debounce_states_[button_index].timestamp != zx::time::infinite_past()) {
      const zx::duration latency =
          zx::clock::get_monotonic() - debounce_states_[button_index].timestamp;

      total_latency_ += latency;
      report_count_++;
      average_latency_usecs_.Set(total_latency_.to_usecs() / report_count_);

      if (latency > max_latency_) {
        max_latency_ = latency;
        max_latency_usecs_.Set(max_latency_.to_usecs());
      }
    }

    if (!last_report_->empty()) {
      total_report_count_.Add(1);
      last_event_timestamp_.Set(last_report_->event_time.get());
    }
  }
  if (buttons_[button_index].id() == fuchsia_buttons::GpioButtonId::kFdr) {
    FDF_LOG(INFO, "FDR (up and down buttons) pressed");
  }

  debounce_states_[button_index].enqueued = false;
  debounce_states_[button_index].timestamp = zx::time::infinite_past();
}

int ButtonsDevice::Thread() {
  thread_started_.Signal();
  if (poll_period_ != zx::duration::infinite()) {
    poll_timer_.set(zx::deadline_after(poll_period_), zx::duration(0));
    poll_timer_.wait_async(port_, kPortKeyPollTimer, ZX_TIMER_SIGNALED, 0);
  }

  while (true) {
    zx_port_packet_t packet;
    zx_status_t status = port_.wait(zx::time::infinite(), &packet);
    FDF_LOG(DEBUG, "msg received on port key %zu", packet.key);
    if (status != ZX_OK) {
      FDF_LOG(ERROR, "port wait failed %d", status);
      return thrd_error;
    }

    if (packet.key == kPortKeyShutDown) {
      FDF_LOG(INFO, "shutting down");
      return thrd_success;
    }

    if (packet.key >= kPortKeyInterruptStart &&
        packet.key < (kPortKeyInterruptStart + buttons_.size())) {
      uint32_t type = static_cast<uint32_t>(packet.key - kPortKeyInterruptStart);
      if (gpios_[type].config.type == BUTTONS_GPIO_TYPE_INTERRUPT) {
        const zx::duration kLeaseTimeout = zx::msec(100);
        wake_lease_.HandleInterrupt(kLeaseTimeout);

        // We need to reconfigure the GPIO to catch the opposite polarity.
        auto reconfig_result = ReconfigurePolarity(type, packet.key);
        if (!reconfig_result.is_ok()) {
          return reconfig_result.error_value();
        }
        debounce_states_[type].value = *reconfig_result;

        // Notify
        debounce_states_[type].timer.set(zx::deadline_after(zx::duration(kDebounceThresholdNs)),
                                         zx::duration(0));
        if (!debounce_states_[type].enqueued) {
          debounce_states_[type].timer.wait_async(port_, kPortKeyTimerStart + type,
                                                  ZX_TIMER_SIGNALED, 0);
          debounce_states_[type].timestamp = zx::time(packet.interrupt.timestamp);
        }
        debounce_states_[type].enqueued = true;
      }

      gpios_[type].irq.ack();
    }

    if (packet.key >= kPortKeyTimerStart && packet.key < (kPortKeyTimerStart + buttons_.size())) {
      Notify(packet.key - kPortKeyTimerStart);
    }

    if (packet.key == kPortKeyPollTimer) {
      for (size_t i = 0; i < gpios_.size(); i++) {
        if (gpios_[i].config.type != BUTTONS_GPIO_TYPE_POLL) {
          continue;
        }

        fidl::WireResult read_result = gpios_[i].client->Read();
        if (!read_result.ok()) {
          FDF_LOG(ERROR, "Failed to send Read request to gpio %zu: %s", i,
                  read_result.status_string());
          return read_result.status();
        }
        if (read_result->is_error()) {
          FDF_LOG(ERROR, "Failed to read gpio %zu: %s", i,
                  zx_status_get_string(read_result->error_value()));
          return read_result->error_value();
        }
        if (read_result.value()->value != debounce_states_[i].value) {
          Notify(i);
        }
        debounce_states_[i].value = read_result.value()->value;
      }

      poll_timer_.set(zx::deadline_after(poll_period_), zx::duration(0));
      poll_timer_.wait_async(port_, kPortKeyPollTimer, ZX_TIMER_SIGNALED, 0);
    }
  }
  return thrd_success;
}

void ButtonsDevice::GetInputReportsReader(GetInputReportsReaderRequestView request,
                                          GetInputReportsReaderCompleter::Sync& completer) {
  auto initial_report = GetInputReportInternal();
  if (initial_report.is_error()) {
    FDF_LOG(ERROR, "Failed to get initial report %d", initial_report.error_value());
  }
  auto status = readers_.CreateReader(
      dispatcher_, std::move(request->reader),
      initial_report.is_ok() ? std::make_optional(initial_report.value()) : std::nullopt);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to create a reader %d", status);
  }
}

void ButtonsDevice::GetDescriptor(GetDescriptorCompleter::Sync& completer) {
  fidl::Arena<kFeatureAndDescriptorBufferSize> arena;

  auto device_info = fuchsia_input_report::wire::DeviceInformation::Builder(arena);
  device_info.vendor_id(static_cast<uint32_t>(fuchsia_input_report::VendorId::kGoogle));
  // Product id is "HID" buttons only for backward compatibility with users of this driver.
  // There is no HID support in this driver anymore.
  device_info.product_id(
      static_cast<uint32_t>(fuchsia_input_report::VendorGoogleProductId::kHidButtons));

  const std::vector<fuchsia_input_report::ConsumerControlButton> buttons = {
      fuchsia_input_report::ConsumerControlButton::kVolumeUp,
      fuchsia_input_report::ConsumerControlButton::kVolumeDown,
      fuchsia_input_report::ConsumerControlButton::kFactoryReset,
      fuchsia_input_report::ConsumerControlButton::kCameraDisable,
      fuchsia_input_report::ConsumerControlButton::kMicMute,
      fuchsia_input_report::ConsumerControlButton::kPower};

  const auto input = fuchsia_input_report::wire::ConsumerControlInputDescriptor::Builder(arena)
                         .buttons(buttons)
                         .Build();

  const auto consumer_control =
      fuchsia_input_report::wire::ConsumerControlDescriptor::Builder(arena).input(input).Build();

  completer.Reply(fuchsia_input_report::wire::DeviceDescriptor::Builder(arena)
                      .device_information(device_info.Build())
                      .consumer_control(consumer_control)
                      .Build());
}

// Requires interrupts to be disabled for all rows/cols.
zx::result<bool> ButtonsDevice::MatrixScan(uint32_t row, uint32_t col, zx_duration_t delay) {
  auto& gpio_col = gpios_[col];
  {
    fidl::WireResult result = gpio_col.client->SetBufferMode(
        fuchsia_hardware_gpio::BufferMode::kInput);  // Float column to find row in use.
    if (!result.ok()) {
      FDF_LOG(ERROR, "Failed to send SetBufferMode request to gpio %u: %s", col,
              result.status_string());
      return zx::error(result.status());
    }
    if (result->is_error()) {
      FDF_LOG(ERROR, "Failed to configuire gpio %u to input: %s", col,
              zx_status_get_string(result->error_value()));
      return zx::error(result->error_value());
    }
  }
  zx::nanosleep(zx::deadline_after(zx::duration(delay)));

  fidl::WireResult read_result = gpios_[row].client->Read();
  if (!read_result.ok()) {
    FDF_LOG(ERROR, "Failed to send Read request to gpio %u: %s", row, read_result.status_string());
    return zx::error(read_result.status());
  }
  if (read_result->is_error()) {
    FDF_LOG(ERROR, "Failed to read gpio %u: %s", row,
            zx_status_get_string(read_result->error_value()));
    return zx::error(read_result->error_value());
  }

  {
    fidl::WireResult result = gpio_col.client->SetBufferMode(
        gpio_col.config.matrix.output_value ? fuchsia_hardware_gpio::BufferMode::kOutputHigh
                                            : fuchsia_hardware_gpio::BufferMode::kOutputLow);
    if (!result.ok()) {
      FDF_LOG(ERROR, "Failed to send SetBufferMode request to gpio %u: %s", col,
              result.status_string());
      return zx::error(read_result.status());
    }
    if (result->is_error()) {
      FDF_LOG(ERROR, "Failed to configuire gpio %u to output: %s", col,
              zx_status_get_string(result->error_value()));
      return zx::error(result->error_value());
    }
  }
  FDF_LOG(DEBUG, "row %u col %u val %u", row, col, read_result.value()->value);
  return zx::ok(read_result.value()->value);
}

zx::result<ButtonsDevice::ButtonsInputReport> ButtonsDevice::GetInputReportInternal() {
  ButtonsInputReport input_rpt;

  for (size_t i = 0; i < buttons_.size(); ++i) {
    bool new_value = false;  // A value true means a button is pressed.
    const fuchsia_buttons::GpioButtonConfig& button = buttons_[i];
    const std::optional<fuchsia_buttons::GpioButtonType>& button_type = button.type();
    if (!button_type.has_value()) {
      FDF_LOG(ERROR, "Button %zu missing type", i);
      return zx::error(ZX_ERR_BAD_STATE);
    }
    const std::optional<uint8_t>& gpio_a_index = button.gpio_a_index();
    if (!gpio_a_index.has_value()) {
      FDF_LOG(ERROR, "Button %zu missing gpio A index", i);
      return zx::error(ZX_ERR_BAD_STATE);
    }
    const std::optional<uint8_t>& gpio_delay = button.gpio_a_index();
    if (!gpio_delay.has_value()) {
      FDF_LOG(ERROR, "Button %zu missing gpio delay", i);
      return zx::error(ZX_ERR_BAD_STATE);
    }
    switch (button_type.value().Which()) {
      case fuchsia_buttons::GpioButtonType::Tag::kMatrix: {
        const fuchsia_buttons::MatrixGpioButton& matrix = button_type.value().matrix().value();
        const std::optional<uint8_t>& gpio_b_index = matrix.gpio_b_index();
        if (!gpio_b_index.has_value()) {
          FDF_LOG(ERROR, "Button %zu missing gpio B index", i);
          return zx::error(ZX_ERR_BAD_STATE);
        }

        zx::result scan_result =
            MatrixScan(gpio_a_index.value(), gpio_b_index.value(), gpio_delay.value());
        if (!scan_result.is_ok()) {
          return zx::error(scan_result.error_value());
        }
        new_value = *scan_result;
        break;
      }
      case fuchsia_buttons::GpioButtonType::Tag::kDirect: {
        const uint8_t gpio_index = gpio_a_index.value();
        fidl::WireResult read_result = gpios_[gpio_index].client->Read();
        if (!read_result.ok()) {
          FDF_LOG(ERROR, "Failed to send Read request to gpio %u: %s", gpio_index,
                  read_result.status_string());
          return zx::error(read_result.status());
        }
        if (read_result->is_error()) {
          FDF_LOG(ERROR, "Failed to read gpio %u: %s", gpio_index,
                  zx_status_get_string(read_result->error_value()));
          return zx::error(read_result->error_value());
        }

        new_value = read_result.value()->value;
        FDF_LOG(DEBUG, "GPIO direct read %u for button %zu", new_value, i);
        break;
      }
      default:
        FDF_LOG(ERROR, "Button %zu has unknown type %u", i, button_type->Which());
        return zx::error(ZX_ERR_INTERNAL);
    }

    if (gpios_[i].config.flags & BUTTONS_GPIO_FLAG_INVERTED) {
      new_value = !new_value;
    }

    FDF_LOG(DEBUG, "GPIO new value %u for button %zu", new_value, i);
    const std::optional<fuchsia_buttons::GpioButtonId>& button_id = button.id();
    if (!button_id.has_value()) {
      FDF_LOG(ERROR, "Button %zu missing id", i);
      return zx::error(ZX_ERR_BAD_STATE);
    }
    input_rpt.set(static_cast<uint32_t>(button_id.value()), new_value);
  }
  input_rpt.event_time = zx::clock::get_monotonic();

  return zx::ok(input_rpt);
}

void ButtonsDevice::GetInputReport(GetInputReportRequestView request,
                                   GetInputReportCompleter::Sync& completer) {
  if (request->device_type != fuchsia_input_report::DeviceType::kConsumerControl) {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
    return;
  }
  auto result = GetInputReportInternal();
  if (!result.is_ok()) {
    completer.ReplyError(result.error_value());
    return;
  }

  fidl::Arena<> arena;
  auto input_report = fuchsia_input_report::wire::InputReport::Builder(arena);
  result->ToFidlInputReport(input_report, arena);
  completer.ReplySuccess(input_report.Build());
}

zx::result<bool> ButtonsDevice::ReconfigurePolarity(size_t idx, uint64_t int_port) {
  FDF_LOG(DEBUG, "gpio %zu port %zu", idx, int_port);
  bool current = false, old;
  auto& gpio = gpios_[idx];

  fidl::WireResult read_result1 = gpio.client->Read();
  if (!read_result1.ok()) {
    FDF_LOG(ERROR, "Failed to send Read request to gpio %zu: %s", idx,
            read_result1.status_string());
    return zx::error(read_result1.status());
  }
  if (read_result1->is_error()) {
    FDF_LOG(ERROR, "Failed to read gpio %zu: %s", idx,
            zx_status_get_string(read_result1->error_value()));
    return zx::error(read_result1->error_value());
  }
  current = read_result1.value()->value;

  fidl::Arena arena;
  auto config = fuchsia_hardware_gpio::wire::InterruptConfiguration::Builder(arena)
                    .mode(current ? fuchsia_hardware_gpio::InterruptMode::kEdgeLow
                                  : fuchsia_hardware_gpio::InterruptMode::kEdgeHigh)
                    .Build();
  do {
    {
      fidl::WireResult result = gpio.client->ConfigureInterrupt(config);
      if (!result.ok()) {
        FDF_LOG(ERROR, "Failed to send ConfigureInterrupt request to gpio %zu: %s", idx,
                result.status_string());
        return zx::error(result.status());
      }
      if (result->is_error()) {
        FDF_LOG(ERROR, "Failed to set interrupt configuration of gpio %zu: %s", idx,
                zx_status_get_string(result->error_value()));
        return zx::error(result->error_value());
      }
    }

    old = current;
    fidl::WireResult read_result2 = gpio.client->Read();
    if (!read_result2.ok()) {
      FDF_LOG(ERROR, "Failed to send Read request to gpio %zu: %s", idx,
              read_result2.status_string());
      return zx::error(read_result2.status());
    }
    if (read_result2->is_error()) {
      FDF_LOG(ERROR, "Failed to read gpio %zu: %s", idx,
              zx_status_get_string(read_result2->error_value()));
      return zx::error(read_result2->error_value());
    }
    current = read_result2.value()->value;
    FDF_LOG(TRACE, "%zu old gpio %u new gpio %u", idx, old, current);
    // If current switches after setup, we setup a new trigger for it (opposite edge).
  } while (current != old);
  return zx::ok(current);
}

zx_status_t ButtonsDevice::ConfigureInterrupt(size_t idx, uint64_t int_port) {
  FDF_LOG(DEBUG, "gpio %zu port %zu", idx, int_port);
  zx_status_t status;
  bool current = false;
  auto& gpio = gpios_[idx];

  const fidl::WireResult read_result = gpio.client->Read();
  if (!read_result.ok()) {
    FDF_LOG(ERROR, "Failed to send Read request to gpio %zu: %s", idx, read_result.status_string());
    return read_result.status();
  }
  if (read_result->is_error()) {
    FDF_LOG(ERROR, "Failed to read gpio %zu: %s", idx,
            zx_status_get_string(read_result->error_value()));
    return read_result->error_value();
  }
  current = read_result.value()->value;

  {
    const fidl::WireResult result = gpio.client->ReleaseInterrupt();
    if (!result.ok()) {
      FDF_LOG(ERROR, "Failed to send ReleaseInterrupt request to gpio %zu: %s", idx,
              result.status_string());
      return result.status();
    }
    if (result->is_error() && result->error_value() != ZX_ERR_NOT_FOUND) {
      FDF_LOG(ERROR, "Failed to release interrupt for gpio %zu: %s", idx,
              zx_status_get_string(result->error_value()));
      return result->error_value();
    }
  }

  fidl::Arena arena;
  // We setup a trigger for the opposite of the current GPIO value.
  auto interrupt_config = fuchsia_hardware_gpio::wire::InterruptConfiguration::Builder(arena)
                              .mode(current ? fuchsia_hardware_gpio::InterruptMode::kEdgeLow
                                            : fuchsia_hardware_gpio::InterruptMode::kEdgeHigh)
                              .Build();
  fidl::WireResult configure_result = gpio.client->ConfigureInterrupt(interrupt_config);
  if (!configure_result.ok()) {
    FDF_LOG(ERROR, "Failed to send ConfigureInterrupt request to gpio %zu: %s", idx,
            configure_result.status_string());
    return configure_result.status();
  }
  if (configure_result->is_error()) {
    FDF_LOG(ERROR, "Failed to configure interrupt for gpio %zu: %s", idx,
            zx_status_get_string(configure_result->error_value()));
    return configure_result->error_value();
  }

  fuchsia_hardware_gpio::InterruptOptions interrupt_options = {};
  if (gpio.config.flags & BUTTONS_GPIO_FLAG_WAKE_VECTOR) {
    interrupt_options |= fuchsia_hardware_gpio::InterruptOptions::kWakeable;
  }

  fidl::WireResult interrupt_result = gpio.client->GetInterrupt(interrupt_options);
  if (!interrupt_result.ok()) {
    FDF_LOG(ERROR, "Failed to send GetInterrupt request to gpio %zu: %s", idx,
            interrupt_result.status_string());
    return interrupt_result.status();
  }
  if (interrupt_result->is_error()) {
    FDF_LOG(ERROR, "Failed to get interrupt for gpio %zu: %s", idx,
            zx_status_get_string(interrupt_result->error_value()));
    return interrupt_result->error_value();
  }
  gpio.irq = std::move(interrupt_result.value()->interrupt);

  status = gpios_[idx].irq.bind(port_, int_port, 0);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "zx_interrupt_bind failed %d", status);
    return status;
  }
  // To make sure polarity is correct in case it changed during configuration.
  auto reconfig_result = ReconfigurePolarity(idx, int_port);
  if (!reconfig_result.is_ok()) {
    return reconfig_result.error_value();
  }
  return ZX_OK;
}

ButtonsDevice::ButtonsDevice(async_dispatcher_t* dispatcher,
                             std::vector<fuchsia_buttons::GpioButtonConfig> buttons,
                             std::vector<Gpio> gpios,
                             fidl::ClientEnd<fuchsia_power_system::ActivityGovernor> sag_client)
    : dispatcher_(dispatcher),
      buttons_(std::move(buttons)),
      gpios_(std::move(gpios)),
      wake_lease_(dispatcher, "buttons-driver-wake", std::move(sag_client), nullptr, true) {
  ZX_ASSERT(Init() == ZX_OK);
}

zx_status_t ButtonsDevice::Init() {
  zx_status_t status;
  fbl::AllocChecker ac;

  metrics_root_ = inspector_.GetRoot().CreateChild("hid-input-report-touch");
  average_latency_usecs_ = metrics_root_.CreateUint("average_latency_usecs", 0);
  max_latency_usecs_ = metrics_root_.CreateUint("max_latency_usecs", 0);
  total_report_count_ = metrics_root_.CreateUint("total_report_count", 0);
  last_event_timestamp_ = metrics_root_.CreateUint("last_event_timestamp", 0);

  status = zx::port::create(ZX_PORT_BIND_TO_INTERRUPT, &port_);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "port_create failed %d", status);
    return status;
  }

  debounce_states_ = fbl::Array(new (&ac) debounce_state[buttons_.size()], buttons_.size());
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  for (auto& i : debounce_states_) {
    i.enqueued = false;
    zx::timer::create(0, ZX_CLOCK_MONOTONIC, &(i.timer));
    i.value = false;
  }

  zx::timer::create(0, ZX_CLOCK_MONOTONIC, &poll_timer_);

  // Check the metadata.
  for (size_t i = 0; i < buttons_.size(); ++i) {
    const fuchsia_buttons::GpioButtonConfig& button = buttons_[i];
    if (!button.gpio_a_index()) {
      FDF_LOG(ERROR, "Button %zu missing gpio A index", i);
      return ZX_ERR_INTERNAL;
    }
    const uint8_t gpio_a_index = button.gpio_a_index().value();
    if (gpio_a_index >= gpios_.size()) {
      FDF_LOG(ERROR, "Invalid gpio A index %u", gpio_a_index);
      return ZX_ERR_INTERNAL;
    }
    if (!button.type().has_value()) {
      FDF_LOG(ERROR, "Button %zu missing id", i);
      return ZX_ERR_INTERNAL;
    }
    const fuchsia_buttons::GpioButtonType& button_type = button.type().value();
    if (button_type.Which() == fuchsia_buttons::GpioButtonType::Tag::kMatrix) {
      const fuchsia_buttons::MatrixGpioButton& matrix = button_type.matrix().value();
      if (!matrix.gpio_b_index().has_value()) {
        FDF_LOG(ERROR, "Button %zu missing gpio B index", i);
        return ZX_ERR_INTERNAL;
      }
      const uint8_t gpio_b_index = matrix.gpio_b_index().value();
      if (gpio_b_index >= gpios_.size()) {
        FDF_LOG(ERROR, "Invalid gpio B index %u", gpio_b_index);
        return ZX_ERR_INTERNAL;
      }
      const Gpio& gpio_b = gpios_[gpio_b_index];
      if (gpio_b.config.type != BUTTONS_GPIO_TYPE_MATRIX_OUTPUT) {
        FDF_LOG(ERROR, "Config for matrix gpio B has invalid type %u", gpio_b.config.type);
        return ZX_ERR_INTERNAL;
      }
    }
    const Gpio& gpio_a = gpios_[gpio_a_index];
    const uint8_t gpio_a_type = gpio_a.config.type;
    switch (gpio_a_type) {
      case BUTTONS_GPIO_TYPE_INTERRUPT:
        break;
      case BUTTONS_GPIO_TYPE_POLL: {
        const auto button_poll_period = zx::duration(gpios_[gpio_a_index].config.poll.period);
        if (poll_period_ == zx::duration::infinite()) {
          poll_period_ = button_poll_period;
        }
        if (button_poll_period != poll_period_) {
          FDF_LOG(ERROR, "GPIOs must have the same poll period");
          return ZX_ERR_INTERNAL;
        }
        break;
      }
      default:
        FDF_LOG(ERROR, "Config for gpio %u has invalid type %d", gpio_a_index, gpio_a_type);
        return ZX_ERR_INTERNAL;
    }
    if (!button.id().has_value()) {
      FDF_LOG(ERROR, "Button %zu missing id", i);
      return ZX_ERR_INTERNAL;
    }
    const fuchsia_buttons::GpioButtonId button_id = button.id().value();
    if (button_id == fuchsia_buttons::GpioButtonId::kFdr) {
      FDF_LOG(INFO, "FDR (up and down buttons) setup to GPIO %u", gpio_a_index);
    }
  }

  // Setup.
  for (size_t i = 0; i < gpios_.size(); ++i) {
    const Gpio& gpio = gpios_[i];

    if (gpio.config.type == BUTTONS_GPIO_TYPE_MATRIX_OUTPUT) {
      fidl::WireResult result = gpio.client->SetBufferMode(
          gpio.config.matrix.output_value ? fuchsia_hardware_gpio::BufferMode::kOutputHigh
                                          : fuchsia_hardware_gpio::BufferMode::kOutputLow);
      if (!result.ok()) {
        FDF_LOG(ERROR, "Failed to send SetBufferMode request to gpio %zu: %s", i,
                result.status_string());
        return result.status();
      }
      if (result->is_error()) {
        FDF_LOG(ERROR, "Failed to configure gpio %zu to output: %s", i,
                zx_status_get_string(result->error_value()));
        return ZX_ERR_NOT_SUPPORTED;
      }
    } else if (gpio.config.type == BUTTONS_GPIO_TYPE_INTERRUPT) {
      fidl::WireResult result =
          gpio.client->SetBufferMode(fuchsia_hardware_gpio::BufferMode::kInput);
      if (!result.ok()) {
        FDF_LOG(ERROR, "Failed to send SetBufferMode request to gpio %zu: %s", i,
                result.status_string());
        return result.status();
      }
      if (result->is_error()) {
        FDF_LOG(ERROR, "Failed to configure gpio %zu to input: %s", i,
                zx_status_get_string(result->error_value()));
        return ZX_ERR_NOT_SUPPORTED;
      }
      status = ConfigureInterrupt(i, kPortKeyInterruptStart + i);
      if (status != ZX_OK) {
        return status;
      }
    } else if (gpio.config.type == BUTTONS_GPIO_TYPE_POLL) {
      fidl::WireResult result =
          gpio.client->SetBufferMode(fuchsia_hardware_gpio::BufferMode::kInput);
      if (!result.ok()) {
        FDF_LOG(ERROR, "Failed to send SetBufferMode request to gpio %zu: %s", i,
                result.status_string());
        return result.status();
      }
      if (result->is_error()) {
        FDF_LOG(ERROR, "Failed to configure gpio %zu to input: %s", i,
                zx_status_get_string(result->error_value()));
        return ZX_ERR_NOT_SUPPORTED;
      }
    }
  }

  auto f = [](void* arg) -> int { return reinterpret_cast<ButtonsDevice*>(arg)->Thread(); };
  const int rc = thrd_create_with_name(&thread_, f, this, "buttons-thread");
  if (rc != thrd_success) {
    return ZX_ERR_INTERNAL;
  }

  return ZX_OK;
}

void ButtonsDevice::ShutDown() {
  zx_port_packet packet = {kPortKeyShutDown, ZX_PKT_TYPE_USER, ZX_OK, {}};
  const zx_status_t status = port_.queue(&packet);
  ZX_ASSERT(status == ZX_OK);
  thread_started_.Wait();
  thrd_join(thread_, NULL);
  for (Gpio& gpio : gpios_) {
    gpio.irq.destroy();
  }
}

}  // namespace buttons
