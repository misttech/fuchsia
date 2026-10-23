// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.driver.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.clock/cpp/fidl.h>
#include <fidl/fuchsia.hardware.interrupt/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/platform-device/cpp/pdev.h>
#include <lib/fidl/cpp/wire/channel.h>
#include <lib/fit/function.h>
#include <lib/zx/clock.h>
#include <lib/zx/interrupt.h>

namespace fake_clock {

class FakeClockDevice : public fidl::Server<fuchsia_hardware_clock::Clock> {
 public:
  FakeClockDevice(uint32_t id, std::string name, fit::closure on_enable = nullptr)
      : id_(id), name_(std::move(name)), on_enable_(std::move(on_enable)) {}

  void Enable(EnableCompleter::Sync& completer) override {
    fdf::info("Clock {} (id {}) Enabled", name_, id_);
    if (on_enable_) {
      on_enable_();
    }
    completer.Reply(zx::ok());
  }

  void Disable(DisableCompleter::Sync& completer) override {
    fdf::info("Clock {} (id {}) Disabled", name_, id_);
    completer.Reply(zx::ok());
  }

  void IsEnabled(IsEnabledCompleter::Sync& completer) override { completer.Reply(zx::ok(true)); }

  void SetRate(SetRateRequest& request, SetRateCompleter::Sync& completer) override {
    fdf::info("Clock {} (id {}) rate set to {} Hz", name_, id_, request.hz());
    completer.Reply(zx::ok());
  }

  void QuerySupportedRate(QuerySupportedRateRequest& request,
                          QuerySupportedRateCompleter::Sync& completer) override {
    completer.Reply(zx::ok(request.hz_in()));
  }

  void GetRate(GetRateCompleter::Sync& completer) override {
    completer.Reply(zx::ok(1000000000));  // 1 GHz
  }

  void SetInput(SetInputRequest& request, SetInputCompleter::Sync& completer) override {
    completer.Reply(zx::ok());
  }

  void GetNumInputs(GetNumInputsCompleter::Sync& completer) override { completer.Reply(zx::ok(0)); }

  void GetInput(GetInputCompleter::Sync& completer) override { completer.Reply(zx::ok(0)); }

  void GetProperties(GetPropertiesCompleter::Sync& completer) override {
    completer.Reply({{.id = id_, .name = name_}});
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_clock::Clock> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    fdf::error("Unknown method ordinal {}", metadata.method_ordinal);
  }

 private:
  uint32_t id_;
  std::string name_;
  fit::closure on_enable_;
};

class FakeClockDriver : public fdf::DriverBase2,
                        public fidl::Server<fuchsia_hardware_interrupt::Controller> {
 public:
  FakeClockDriver() : fdf::DriverBase2("fake-clock") {}

  void RegisterInterrupt(RegisterInterruptRequest& request,
                         RegisterInterruptCompleter::Sync& completer) override {
    fdf::info("fake-clock: RegisterInterrupt called with irq {}", request.irq());
    if (request.irq() != 42) {
      fdf::error("Unexpected irq: expected 42, got {}", request.irq());
      completer.Reply(fit::error(ZX_ERR_INVALID_ARGS));
      return;
    }
    irq_ = std::move(request.interrupt());
    TriggerInterrupt();
    completer.Reply(fit::ok());
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_interrupt::Controller> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    fdf::error("FakeClockDriver unknown method ordinal {}", metadata.method_ordinal);
  }

  void TriggerInterrupt() {
    if (irq_.is_valid()) {
      zx_status_t status = irq_.trigger(0, zx::clock::get_monotonic());
      fdf::info("fake-clock TriggerInterrupt: status {}", zx_status_get_string(status));
    }
  }

  zx::result<> Start(fdf::DriverContext context) final {
    fdf::info("Starting fake-clock driver");

    incoming_ = context.take_incoming();
    auto pdev_result = fdf::PDev::Connect(incoming_, "pdev");
    if (pdev_result.is_error()) {
      fdf::error("Failed to connect to pdev: {}", pdev_result);
      return pdev_result.take_error();
    }
    auto pdev = std::move(pdev_result.value());

    // Register as interrupt controller with platform bus.
    auto registry_connect =
        incoming_->Connect<fuchsia_hardware_interrupt::ControllerRegistryService::Registry>("pdev");
    if (registry_connect.is_error()) {
      fdf::error("Failed to connect to ControllerRegistryService: {}", registry_connect);
      return registry_connect.take_error();
    }
    auto [controller_client_end, controller_server_end] =
        fidl::Endpoints<fuchsia_hardware_interrupt::Controller>::Create();

    controller_bindings_.AddBinding(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                    std::move(controller_server_end), this,
                                    fidl::kIgnoreBindingClosure);

    fidl::WireSyncClient<fuchsia_hardware_interrupt::ControllerRegistry> registry(
        std::move(registry_connect.value()));
    fidl::WireResult reg_result = registry->RegisterController(std::move(controller_client_end));
    if (!reg_result.ok()) {
      fdf::error("Failed to call RegisterController: {}", reg_result.FormatDescription());
      return zx::error(reg_result.status());
    }
    if (reg_result->is_error()) {
      fdf::error("RegisterController failed: {}", zx_status_get_string(reg_result->error_value()));
      return zx::error(reg_result->error_value());
    }
    fdf::info("fake-clock registered as interrupt controller successfully!");

    std::vector<fuchsia_driver_framework::NodeProperty2> irq_props{
        fdf::MakeProperty2("fuchsia.NAME", "interrupt-controller"),
        fdf::MakeProperty2("fuchsia.ID", 1u),
    };
    auto irq_child = AddChild("interrupt-controller", irq_props, {});
    if (irq_child.is_error()) {
      fdf::error("Failed to add interrupt-controller child: {}", irq_child);
      return irq_child.take_error();
    }

    // Default clock config: name="my-clock", clock_id=1
    std::string clock_name = "my-clock";
    uint32_t clock_id = 1;
    uint32_t node_id = clock_id;
    zx::result metadata = pdev.GetFidlMetadata<fuchsia_driver_metadata::Dictionary>(
        "fuchsia.hardware.clockimpl.ClockIdsMetadata");
    if (metadata.is_ok() && metadata->entries().has_value()) {
      for (const auto& entry : *metadata->entries()) {
        if (entry.key() == "clock_nodes.0.id" && entry.value().int64().has_value()) {
          node_id = static_cast<uint32_t>(entry.value().int64().value());
        } else if (entry.key() == "clock_nodes.0.clock_id" && entry.value().int64().has_value()) {
          clock_id = static_cast<uint32_t>(entry.value().int64().value());
        }
      }
    }

    auto clock_device =
        std::make_unique<FakeClockDevice>(clock_id, clock_name, [this] { TriggerInterrupt(); });

    // Expose service
    fuchsia_hardware_clock::Service::InstanceHandler instance_handler{
        {.clock = bindings_.CreateHandler(clock_device.get(),
                                          fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                          fidl::kIgnoreBindingClosure)}};

    auto result = outgoing()->AddService<fuchsia_hardware_clock::Service>(
        std::move(instance_handler), clock_name);
    if (result.is_error()) {
      fdf::error("Failed to add service {}: {}", clock_name, result);
      return result.take_error();
    }

    std::vector<fuchsia_driver_framework::NodeProperty2> props{
        fdf::MakeProperty2("fuchsia.NAME", clock_name),
        fdf::MakeProperty2("fuchsia.ID", node_id),
    };

    auto offers = std::vector{fdf::MakeOffer2<fuchsia_hardware_clock::Service>(clock_name)};

    auto child = AddChild(clock_name, props, offers);
    if (child.is_error()) {
      fdf::error("Failed to add child {}: {}", clock_name, child);
      return child.take_error();
    }

    clock_devices_.push_back(std::move(clock_device));

    return zx::ok();
  }

 private:
  fidl::ServerBindingGroup<fuchsia_hardware_clock::Clock> bindings_;
  fidl::ServerBindingGroup<fuchsia_hardware_interrupt::Controller> controller_bindings_;
  zx::interrupt irq_;
  std::vector<std::unique_ptr<FakeClockDevice>> clock_devices_;
  std::shared_ptr<fdf::Namespace> incoming_;
};

}  // namespace fake_clock

FUCHSIA_DRIVER_EXPORT2(fake_clock::FakeClockDriver);
