// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "bt_hci_broadcom.h"

#include <assert.h>
#include <endian.h>
#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.power/cpp/fidl.h>
#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/async/default.h>
#include <lib/ddk/binding_driver.h>
#include <lib/ddk/driver.h>
#include <lib/ddk/platform-defs.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/metadata/cpp/metadata.h>
#include <lib/driver/power/cpp/wake-lease.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/random.h>
#include <threads.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/status.h>
#include <zircon/threads.h>

#include "fidl/fuchsia.hardware.bluetooth/cpp/markers.h"
#include "lib/fidl/cpp/channel.h"
#include "lib/fit/function.h"
#include "lib/fpromise/promise.h"
#include "src/connectivity/bluetooth/hci/vendor/broadcom/bt_hci_broadcom_config.h"
#include "src/connectivity/bluetooth/hci/vendor/broadcom/packets.emb.h"
#include "src/connectivity/bluetooth/hci/vendor/broadcom/packets.h"
#include "tools/power_config/lib/cpp/power_config.h"

namespace bt_hci_broadcom {
namespace {

template <typename Container>
WriteSleepModeCmdView DisableLowPowerModeCmd(Container* container) {
  auto view = MakeWriteSleepModeCmdView(container);
  ZX_ASSERT(view.IsComplete());
  view.header().opcode().Write(BroadcomOpCode::WRITE_SLEEP_MODE);
  view.header().parameter_total_size().Write(WriteSleepModeCmd::parameter_size());
  view.mode().Write(SleepMode::DISABLED);
  view.idle_threshold_host().Write(0);
  view.idle_threshold_device().Write(0);
  view.bt_wake_polarity().Write(0);
  view.host_wake_polarity().Write(0);
  view.sleep_during_sco().Write(0);
  view.combine_sleep_and_lpm().Write(0);
  view.tri_state_uart_before_sleep().Write(0);
  for (auto usb_flag : view.usb_flags()) {
    usb_flag.Write(0);
  }
  view.pulsed_host_wake().Write(0);
  ZX_ASSERT(view.Ok());
  return view;
}

template <typename Container>
WriteSleepModeCmdView EnableLowPowerModeCmd(Container* container, zx::duration host_idle_threshold,
                                            zx::duration device_idle_threshold) {
  uint8_t host_idle_units = static_cast<uint8_t>(host_idle_threshold.to_nsecs() / 12500000);
  uint8_t device_idle_units = static_cast<uint8_t>(device_idle_threshold.to_nsecs() / 12500000);
  auto view = MakeWriteSleepModeCmdView(container);
  ZX_ASSERT(view.IsComplete());
  view.header().opcode().Write(BroadcomOpCode::WRITE_SLEEP_MODE);
  view.header().parameter_total_size().Write(WriteSleepModeCmd::parameter_size());
  view.mode().Write(SleepMode::UART);
  view.idle_threshold_host().Write(host_idle_units);
  view.idle_threshold_device().Write(device_idle_units);
  view.bt_wake_polarity().Write(1);
  view.host_wake_polarity().Write(1);
  view.sleep_during_sco().Write(1);
  view.combine_sleep_and_lpm().Write(1);
  view.tri_state_uart_before_sleep().Write(0);
  for (auto usb_flag : view.usb_flags()) {
    usb_flag.Write(0);
  }
  view.pulsed_host_wake().Write(0);
  ZX_ASSERT(view.Ok());
  return view;
}

namespace fhbt = fuchsia_hardware_bluetooth;
namespace fhsi = fuchsia_hardware_serialimpl::wire;

constexpr uint32_t kTargetBaudRate = 2000000;
constexpr uint32_t kDefaultBaudRate = 115200;

constexpr zx::duration kFirmwareDownloadDelay = zx::msec(50);

// Hardcoded. Better to parameterize on chipset. Broadcom chips need a few hundred msec delay after
// firmware load.
constexpr zx::duration kBaudRateSwitchDelay = zx::msec(200);

}  // namespace

const std::unordered_map<uint16_t, std::string> BtHciBroadcom::kFirmwareMap = {
    {PDEV_PID_BCM43458, "BCM4345C5.hcd"},
    {PDEV_PID_BCM4359, "BCM4359C0.hcd"},
    {PDEV_PID_BCM4381A1, "BCM4381A1.hcd"}};

HciEventHandler::HciEventHandler(fit::function<void(std::vector<uint8_t>&)> on_receive_callback)
    : on_receive_callback_(std::move(on_receive_callback)) {}

void HciEventHandler::OnReceive(fhbt::wire::ReceivedPacket* packet) {
  if (!on_receive_callback_) {
    FDF_LOG(ERROR, "No receive callback has been set.");
    return;
  }
  // Ignore packets if they are not event packets during initialization.
  if (packet->Which() != fhbt::wire::ReceivedPacket::Tag::kEvent) {
    FDF_LOG(ERROR, "Received non event packet: %d", packet->Which());
    return;
  }
  std::vector<uint8_t> buffer(packet->event().begin(), packet->event().end());
  on_receive_callback_(buffer);
}

class HciTransportPassthroughImpl : public fidl::Server<fhbt::HciTransport>,
                                    public fidl::AsyncEventHandler<fhbt::HciTransport> {
 public:
  using ActivityCallback = fit::function<void(ActivityType)>;

  explicit HciTransportPassthroughImpl(fidl::ClientEnd<fhbt::HciTransport> upstream_client_end,
                                       ActivityCallback activity_cb, async_dispatcher_t* dispatcher)
      : activity_cb_(std::move(activity_cb)),
        upstream_client_(std::move(upstream_client_end), dispatcher, this) {}

  static fidl::ServerBindingRef<fhbt::HciTransport> BindServer(
      async_dispatcher_t* dispatcher,
      fidl::ServerEnd<fuchsia_hardware_bluetooth::HciTransport> server_end,
      fidl::ClientEnd<fuchsia_hardware_bluetooth::HciTransport> upstream_client_end,
      ActivityCallback activity_cb) {
    std::unique_ptr impl = std::make_unique<HciTransportPassthroughImpl>(
        std::move(upstream_client_end), std::move(activity_cb), dispatcher);
    HciTransportPassthroughImpl* impl_ptr = impl.get();

    fidl::ServerBindingRef binding_ref =
        fidl::BindServer(dispatcher, std::move(server_end), std::move(impl),
                         std::mem_fn(&HciTransportPassthroughImpl::OnUnbound));
    impl_ptr->binding_ref_.emplace(binding_ref);
    return binding_ref;
  }

  void Send(SendRequest& request, SendCompleter::Sync& completed) override {
    activity_cb_(ActivityType::kSendPacket);
    upstream_client_->Send(request).Then(
        [completer = completed.ToAsync()](auto result) mutable { completer.Reply(); });
  }

  void AckReceive(AckReceiveCompleter::Sync& completer) override {
    auto result = upstream_client_->AckReceive();
    if (result.is_error()) {
      FDF_LOG(WARNING, "Failed to ack to upstream");
    }
  }

  void OnReceive(fidl::Event<fhbt::HciTransport::OnReceive>& event) override {
    activity_cb_(ActivityType::kReceivePacket);
    if (!binding_ref_.has_value()) {
      FDF_LOG(WARNING, "OnReceive with no server?!?");
    }
    fit::result result = fidl::SendEvent(*binding_ref_)->OnReceive(event);
    if (result.is_error()) {
      FDF_LOG(WARNING, "Failed to send OnReceive to client");
    }
  }

  void ConfigureSco(ConfigureScoRequest& request, ConfigureScoCompleter::Sync& completer) override {
    auto result = upstream_client_->ConfigureSco(std::move(request));
    if (result.is_error()) {
      FDF_LOG(WARNING, "ConfigureSco failed");
    }
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fhbt::HciTransport> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    fdf::error("Unknown method in HciTransport client, closing with ZX_ERR_NOT_SUPPORTED");
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void handle_unknown_event(fidl::UnknownEventMetadata<fhbt::HciTransport> metadata) override {
    fdf::error("Unknown event in upstream HciTransport protocol, ignoring");
  }

  void OnUnbound(fidl::UnbindInfo info, fidl::ServerEnd<fhbt::HciTransport> server_end) {
    if (info.is_user_initiated()) {
      FDF_LOG(INFO, "Shutting down HciTransport");
    } else if (info.is_peer_closed()) {
      FDF_LOG(INFO, "HciTransport Client closed");
    } else {
      FDF_LOG(WARNING, "HciTransport Server error: %s", info.status_string());
    }
    // Upstream client end should be dropped when the server is deallocated.
  }

 private:
  ActivityCallback activity_cb_;
  fidl::Client<fhbt::HciTransport> upstream_client_;
  std::optional<fidl::ServerBindingRef<fuchsia_hardware_bluetooth::HciTransport>> binding_ref_;
};

BtHciBroadcom::BtHciBroadcom(fdf::DriverStartArgs start_args,
                             fdf::UnownedSynchronizedDispatcher driver_dispatcher)
    : DriverBase("bt-hci-broadcom", std::move(start_args), std::move(driver_dispatcher)),
      hci_event_handler_([this](std::vector<uint8_t>& packet) { OnReceivePacket(packet); }),
      node_(fidl::WireClient(std::move(node()), dispatcher())),
      devfs_connector_(fit::bind_member<&BtHciBroadcom::Connect>(this)) {}

void BtHciBroadcom::Start(fdf::StartCompleter completer) {
  // BT_HOST_WAKE and BT_DEV_WAKE, when they are available, are used to

  zx_status_t status = ConnectToHciTransportFidlProtocol();
  if (status != ZX_OK) {
    completer(zx::error(status));
    return;
  }
  status = ConnectToSerialFidlProtocol();
  if (status == ZX_OK) {
    is_uart_ = true;
  }

  fdf::Arena arena('INFO');
  auto result = serial_client_.buffer(arena)->GetInfo();
  if (!result.ok()) {
    FDF_LOG(ERROR, "Read failed FIDL error: %s", result.status_string());
    completer(zx::error(result.status()));
    return;
  }

  if (result->is_error()) {
    FDF_LOG(ERROR, "Read failed : %s", zx_status_get_string(result->error_value()));
    completer(zx::error(result->error_value()));
    return;
  }

  serial_pid_ = result.value()->info.serial_pid;

  if (serial_pid_ == PDEV_PID_BCM4381A1) {
    // BCM4381 board requires flow control by default.
    fdf::Arena config_arena('CONF');
    const uint32_t flags = fhsi::kSerialDataBits8 | fhsi::kSerialStopBits1 |
                           fhsi::kSerialParityNone | fhsi::kSerialFlowCtrlCtsRts;
    fdf::WireUnownedResult<fuchsia_hardware_serialimpl::Device::Config> result =
        serial_client_.buffer(config_arena)->Config(kDefaultBaudRate, flags);
    if (!result.ok()) {
      FDF_LOG(ERROR, "Initial UART configuration failed, FIDL error: %s",
              zx_status_get_string(result.status()));
      completer(zx::error(result.status()));
      return;
    }
    if (result->is_error()) {
      FDF_LOG(ERROR, "Initial UART configuration failed, domain error: %s",
              zx_status_get_string(result->error_value()));
      completer(zx::error(result->error_value()));
      return;
    }
  }

  const auto config = take_config<bt_hci_broadcom_config::Config>();

  if (config.enable_suspend()) {
    zx::result<> power_init_result = InitPowerManagement();
    if (power_init_result.is_ok()) {
      FDF_LOG(INFO, "Initialized power management");
    } else {
      FDF_LOG(ERROR, "Failed to initialize power management: %s",
              power_init_result.status_string());
      CompleteStart(power_init_result.error_value());
      return;
    }
  }

  // Continue initialization through the fpromise executor.
  start_completer_.emplace(std::move(completer));
  executor_.emplace(dispatcher());
  executor_->schedule_task(Initialize().then([this](fpromise::result<void, zx_status_t>& result) {
    if (result.is_ok()) {
      CompleteStart(ZX_OK);
    } else {
      CompleteStart(result.take_error());
    }
  }));
}

void BtHciBroadcom::PrepareStop(fdf::PrepareStopCompleter completer) { completer(zx::ok()); }

void BtHciBroadcom::GetFeatures(GetFeaturesCompleter::Sync& completer) {
  fidl::Arena arena;
  auto builder = fhbt::wire::VendorFeatures::Builder(arena);
  builder.acl_priority_command(true);
  builder.android_vendor_extensions(fhbt::wire::AndroidVendorSupport::Builder(arena).Build());
  completer.Reply(builder.Build());
}

void BtHciBroadcom::EncodeCommand(EncodeCommandRequestView request,
                                  EncodeCommandCompleter::Sync& completer) {
  uint8_t data_buffer[kBcmSetAclPriorityCmdSize];
  switch (request->Which()) {
    case fhbt::wire::VendorCommand::Tag::kSetAclPriority: {
      EncodeSetAclPriorityCommand(request->set_acl_priority(), data_buffer);
      auto encoded_cmd =
          fidl::VectorView<uint8_t>::FromExternal(data_buffer, kBcmSetAclPriorityCmdSize);
      completer.ReplySuccess(encoded_cmd);
      return;
    }
    default: {
      completer.ReplyError(ZX_ERR_INVALID_ARGS);
      return;
    }
  }
}

void BtHciBroadcom::OpenHci(OpenHciCompleter::Sync& completer) {
  completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
}

fidl::ClientEnd<fuchsia_hardware_bluetooth::HciTransport> BtHciBroadcom::AddHciTransportClient(
    fidl::ClientEnd<fuchsia_hardware_bluetooth::HciTransport> upstream_client_end) {
  auto [client_end, server_end] = fidl::Endpoints<fhbt::HciTransport>::Create();
  auto binding_ref = HciTransportPassthroughImpl::BindServer(
      executor_->dispatcher(), std::move(server_end), std::move(upstream_client_end),
      fit::bind_member<&BtHciBroadcom::NoteActivity>(this));
  active_clients_.push_back(binding_ref);
  return std::move(client_end);
}

void BtHciBroadcom::OpenHciTransport(OpenHciTransportCompleter::Sync& completer) {
  fidl::ClientEnd<fhbt::HciTransport> client_end;
  if (hci_transport_client_end_.is_valid()) {
    client_end = std::move(hci_transport_client_end_);
  } else {
    // We need a new client end, because we already gave away the initialization one.
    zx::result<fidl::ClientEnd<fhbt::HciTransport>> client_end_result =
        incoming()->Connect<fhbt::HciService::HciTransport>();
    if (client_end_result.is_error()) {
      FDF_LOG(ERROR, "Connect to fhbt::HciTransport protocol failed: %s",
              client_end_result.status_string());
      completer.ReplyError(client_end_result.status_value());
      return;
    }
    client_end = std::move(*client_end_result);
  }
  fidl::ClientEnd<fhbt::HciTransport> passthrough_client =
      AddHciTransportClient(std::move(client_end));
  completer.ReplySuccess(std::move(passthrough_client));
}

void BtHciBroadcom::OpenSnoop(OpenSnoopCompleter::Sync& completer) {
  zx::result<fidl::ClientEnd<fhbt::Snoop>> client_end =
      incoming()->Connect<fhbt::HciService::Snoop>();
  if (client_end.is_error()) {
    FDF_LOG(ERROR, "Connect to Snoop protocol failed: %s", client_end.status_string());
    completer.ReplyError(client_end.status_value());
    return;
  }
  completer.ReplySuccess(std::move(*client_end));
}

void BtHciBroadcom::handle_unknown_method(fidl::UnknownMethodMetadata<fhbt::Vendor> metadata,
                                          fidl::UnknownMethodCompleter::Sync& completer) {
  FDF_LOG(ERROR, "Unknown method in Vendor protocol, closing with ZX_ERR_NOT_SUPPORTED");
  completer.Close(ZX_ERR_NOT_SUPPORTED);
}

// driver_devfs::Connector<fhbt::Vendor>
void BtHciBroadcom::Connect(fidl::ServerEnd<fhbt::Vendor> request) {
  vendor_binding_group_.AddBinding(dispatcher(), std::move(request), this,
                                   fidl::kIgnoreBindingClosure);
}

zx_status_t BtHciBroadcom::ConnectToHciTransportFidlProtocol() {
  zx::result<fidl::ClientEnd<fhbt::HciTransport>> client_end =
      incoming()->Connect<fhbt::HciService::HciTransport>();
  if (client_end.is_error()) {
    FDF_LOG(ERROR, "Connect to fhbt::HciTransport protocol failed: %s", client_end.status_string());
    return client_end.status_value();
  }

  hci_transport_client_ = fidl::WireSyncClient(*std::move(client_end));

  return ZX_OK;
}

zx_status_t BtHciBroadcom::ConnectToSerialFidlProtocol() {
  zx::result<fdf::ClientEnd<fuchsia_hardware_serialimpl::Device>> client_end =
      incoming()->Connect<fuchsia_hardware_serialimpl::Service::Device>();
  if (client_end.is_error()) {
    FDF_LOG(ERROR, "Connect to fuchsia_hardware_serialimpl::Device protocol failed: %s",
            client_end.status_string());
    return client_end.status_value();
  }

  serial_client_ = fdf::WireSyncClient(*std::move(client_end));
  return ZX_OK;
}

void BtHciBroadcom::EncodeSetAclPriorityCommand(fhbt::wire::VendorSetAclPriorityParams params,
                                                void* out_buffer) {
  if (!params.has_connection_handle() || !params.has_priority() || !params.has_direction()) {
    FDF_LOG(ERROR,
            "The command cannot be encoded because the following fields are missing: %s %s %s",
            params.has_connection_handle() ? "" : "connection_handle",
            params.has_priority() ? "" : "priority", params.has_direction() ? "" : "direction");
    return;
  }
  BcmSetAclPriorityCmd command = {
      .header =
          {
              .opcode = htole16(kBcmSetAclPriorityCmdOpCode),
              .parameter_total_size = sizeof(BcmSetAclPriorityCmd) - sizeof(HciCommandHeader),
          },
      .connection_handle = htole16(params.connection_handle()),
      .priority = (params.priority() == fhbt::VendorAclPriority::kNormal) ? kBcmAclPriorityNormal
                                                                          : kBcmAclPriorityHigh,
      .direction = (params.direction() == fhbt::VendorAclDirection::kSource)
                       ? kBcmAclDirectionSource
                       : kBcmAclDirectionSink,
  };

  memcpy(out_buffer, &command, sizeof(command));
}

void BtHciBroadcom::OnReceivePacket(std::vector<uint8_t>& packet) {
  event_receive_buffer_ = packet;
  auto result = hci_transport_client_->AckReceive();
  if (result.status() != ZX_OK) {
    FDF_LOG(ERROR, "Failed to ack receive: %s", result.status_string());
  }
}

template <typename CmdView>
fpromise::promise<std::vector<uint8_t>, zx_status_t> BtHciBroadcom::SendCommand(CmdView view) {
  ZX_ASSERT(view.Ok());
  auto storage = view.BackingStorage();
  return SendCommand(storage.data(), view.IntrinsicSizeInBytes().Read());
}

fpromise::promise<std::vector<uint8_t>, zx_status_t> BtHciBroadcom::SendCommand(const void* command,
                                                                                size_t length) {
  // send HCI command
  fidl::Arena arena;
  auto command_vec = std::vector<uint8_t>(static_cast<const uint8_t*>(command),
                                          static_cast<const uint8_t*>(command) + length);
  auto command_view = fidl::VectorView<uint8_t>::FromExternal(command_vec);
  auto result =
      hci_transport_client_->Send(fhbt::wire::SentPacket::WithCommand(arena, command_view));
  if (result.status() != ZX_OK) {
    FDF_LOG(ERROR, "Failed to send command: %s", result.status_string());
    return fpromise::make_result_promise<std::vector<uint8_t>, zx_status_t>(
        fpromise::error(result.status()));
  }

  return ReadEvent();
}

fpromise::promise<std::vector<uint8_t>, zx_status_t> BtHciBroadcom::ReadEvent() {
  zx::result<std::vector<uint8_t>> result = ReadEventSync();
  if (result.is_error()) {
    FDF_LOG(ERROR, "Failed to read event");
    return fpromise::make_result_promise<std::vector<uint8_t>, zx_status_t>(
        fpromise::error(result.status_value()));
  }

  return fpromise::make_result_promise<std::vector<uint8_t>, zx_status_t>(
      fpromise::ok(std::move(result.value())));
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::SetBaudRate(uint32_t baud_rate) {
  BcmSetBaudRateCmd command = {
      .header =
          {
              .opcode = kBcmSetBaudRateCmdOpCode,
              .parameter_total_size = sizeof(BcmSetBaudRateCmd) - sizeof(HciCommandHeader),
          },
      .unused = 0,
      .baud_rate = htole32(baud_rate),
  };

  return SendCommand(&command, sizeof(command))
      .and_then(
          [this, baud_rate](const std::vector<uint8_t>&) -> fpromise::result<void, zx_status_t> {
            fdf::Arena arena('CONF');
            fdf::WireUnownedResult<fuchsia_hardware_serialimpl::Device::Config> result =
                serial_client_.buffer(arena)->Config(baud_rate, fhsi::kSerialSetBaudRateOnly);
            if (!result.ok()) {
              return fpromise::error(result.status());
            }
            if (result->is_error()) {
              return fpromise::error(result->error_value());
            }
            return fpromise::ok();
          });
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::EnableLowPowerMode(
    zx::duration host_idle_threshold, zx::duration device_idle_threshold) {
  if (serial_pid_ != PDEV_PID_BCM4381A1) {
    FDF_LOG(INFO, "skipping low power settings on non-4381");
    return fpromise::make_promise(
        []() { return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok()); });
  }
  // These are in 12.5ms increments.

  std::array<std::byte, WriteSleepModeCmd::MaxSizeInBytes()> storage;
  return SendCommand(EnableLowPowerModeCmd(&storage, host_idle_threshold, device_idle_threshold))
      .and_then([](const std::vector<uint8_t>& cmd_complete) {
        if (sizeof(HciCommandComplete) <= cmd_complete.size()) {
          HciCommandComplete event;
          std::memcpy(&event, cmd_complete.data(), sizeof(event));
          if (event.return_code == 0x00) {
            FDF_LOG(INFO, "set low power mode settings");
          } else {
            FDF_LOG(WARNING, "failed to set low power mode: 0x%02x", event.return_code);
          }
        } else {
          FDF_LOG(WARNING, "LowPowerMode CmdComplete is too small: %lu < %lu", cmd_complete.size(),
                  sizeof(HciCommandComplete));
        }
      });
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::DisableLowPowerMode() {
  if (serial_pid_ != PDEV_PID_BCM4381A1) {
    FDF_LOG(INFO, "skipping low power settings on non-4381");
    return fpromise::make_promise(
        []() { return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok()); });
  }
  std::array<std::byte, WriteSleepModeCmd::MaxSizeInBytes()> storage;
  return SendCommand(DisableLowPowerModeCmd(&storage))
      .and_then([](const std::vector<uint8_t>& cmd_complete) {
        if (sizeof(HciCommandComplete) <= cmd_complete.size()) {
          HciCommandComplete event;
          std::memcpy(&event, cmd_complete.data(), sizeof(event));
          if (event.return_code != 0x00) {
            FDF_LOG(WARNING, "failed to disable low power mode: 0x%02x", event.return_code);
          }
        } else {
          FDF_LOG(WARNING, "LowPowerMode CmdComplete is too small: %lu < %lu", cmd_complete.size(),
                  sizeof(HciCommandComplete));
        }
      });
}

zx::result<> BtHciBroadcom::InitPowerManagement() {
  zx::result open_result = incoming()->Open<fuchsia_io::File>("/pkg/data/broadcom_power.fidl",
                                                              fuchsia_io::Flags::kPermReadBytes);
  if (!open_result.is_ok() || !open_result->is_valid()) {
    return zx::error(ZX_ERR_INTERNAL);
  }
  zx::result<fuchsia_hardware_power::ComponentPowerConfiguration> load_result =
      power_config::Load(std::move(open_result.value()));
  if (load_result.is_error()) {
    FDF_LOG(ERROR, "Loading Power config failed: %s", load_result.status_string());
    return load_result.take_error();
  }

  std::vector<fdf_power::PowerElementConfiguration> element_configs;
  for (const fuchsia_hardware_power::PowerElementConfiguration& element_config :
       load_result.value().power_elements()) {
    auto converted = fdf_power::PowerElementConfiguration::FromFidl(element_config);
    if (converted.is_error()) {
      FDF_LOG(ERROR, "Converting power element config failed: %s", converted.status_string());
      return converted.take_error();
    }
    element_configs.push_back(converted.value());
  }

  zx::result<fdf_power::ElementDesc> element_desc =
      ApplyPowerConfiguration(std::move(element_configs));
  if (element_desc.is_error()) {
    return element_desc.take_error();
  }

  assertive_token_ = std::move(element_desc->assertive_token);
  element_control_client_end_ = *std::move(element_desc->element_control_client);
  element_runner_server_binding_.emplace(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                         std::move(element_desc->element_runner_server.value()),
                                         this, fidl::kIgnoreBindingClosure);
  element_lessor_client_ = std::move(*element_desc->lessor_client);

  fidl::WireResult lease = fidl::WireCall(element_lessor_client_)->Lease(PowerLevel::kBoot);
  if (!lease.ok()) {
    FDF_LOG(ERROR, "Call to Lease failed: %s", lease.error().FormatDescription().c_str());
    return zx::error(lease.error().status());
  }
  if (lease->is_error()) {
    FDF_LOG(ERROR, "Failed to acquire lease: %s",
            fdf_power::LeaseErrorToString(lease->error_value()));
    return fdf_power::LeaseErrorToZxError(lease->error_value());
  }
  level_lease_client_.emplace(std::move(lease->value()->lease_control), dispatcher());
  return zx::ok();
}

zx::result<fdf_power::ElementDesc> BtHciBroadcom::ApplyPowerConfiguration(
    std::vector<fdf_power::PowerElementConfiguration> element_configs) {
  // One for the power element
  constexpr size_t kExpectedPowerElementConfigs = 1;

  if (element_configs.size() != kExpectedPowerElementConfigs) {
    FDF_LOG(ERROR, "Unexpected number of power element configs: %zu != %zu", element_configs.size(),
            kExpectedPowerElementConfigs);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  fit::result<fdf_power::Error, std::vector<fdf_power::ElementDesc>> result =
      fdf_power::ApplyPowerConfiguration(*incoming(), element_configs,
                                         /*use_element_runner=*/true);
  if (result.is_error()) {
    FDF_LOG(INFO, "Failed to apply power config: %s",
            fdf_power::ErrorToString(result.error_value()));
    return fdf_power::ErrorToZxError(result.error_value());
  }
  if (result->size() != 1) {
    FDF_LOG(ERROR, "Unexpected element desc count %zu", result->size());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  fdf_power::ElementDesc& element_desc = result->at(0);
  FDF_LOG(INFO, "Power element applied: \"%s\"", element_desc.element_config.element.name.c_str());

  if (element_desc.element_config.element.levels.size() != PowerLevel::kPowerLevelCount) {
    FDF_LOG(ERROR, "Got %zu power levels, expected %u",
            element_desc.element_config.element.levels.size(), PowerLevel::kPowerLevelCount);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  return zx::ok(std::move(element_desc));
}

void BtHciBroadcom::SetLevel(fuchsia_power_broker::wire::ElementRunnerSetLevelRequest* request,
                             SetLevelCompleter::Sync& completer) {
  FDF_LOG(DEBUG, "SetLevel %u ?-> %u ", power_level_, request->level);

  if (power_level_ == request->level) {
    completer.Reply();
    return;
  }

  if (power_level_ == PowerLevel::kBoot) {
    if (request->level == PowerLevel::kOff) {
      FDF_LOG(DEBUG, "Initial powerlevel off request (but we are trying to boot), ignoring..");
      completer.Reply();
      return;
    }
    // We don't expect another transition within Boot mode, until we drop the Boot lease at the end
    // of initialization.
    FDF_LOG(WARNING, "Within boot mode, got unexpected SetLevel(%d) - ignoring..", request->level);
  }

  // The only two transitions we expect are from OFF -> ON, and ON -> OFF.
  // These are caused by our self-lease (in AcquirePowerElementLease) and signal that
  // dependent power elements are at the correct level when ON.
  // Log any other transitions.
  if ((power_level_ == PowerLevel::kOff && request->level != PowerLevel::kOn) ||
      (power_level_ == PowerLevel::kOn && request->level != PowerLevel::kOff)) {
    FDF_LOG(WARNING, "Got unexpected SetLevel Transition: %d -> %d", power_level_, request->level);
  }

  completer.Reply();
}

void BtHciBroadcom::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementRunner> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  FDF_LOG(ERROR, "Unexpected ElementRunner method ordinal 0x%016lx", metadata.method_ordinal);
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::AssertLevel(PowerLevel requested_level) {
  if (!element_lessor_client_.is_valid() || requested_level == power_level_) {
    // We are not using power framework or no change is needed.
    return fpromise::make_promise(
        []() { return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok()); });
  }
  switch (requested_level) {
    case PowerLevel::kOn:
    case PowerLevel::kBoot:
      return AcquirePowerElementLease();
    case PowerLevel::kOff:
      if (level_lease_client_) {
        // We are not asserting anymore, release any lease we have.
        auto result = level_lease_client_->UnbindMaybeGetEndpoint();
        if (result.is_error()) {
          FDF_LOG(ERROR, "Tried to unbind when we have a pending call?!");
        }
        level_lease_client_.reset();
      } else {
        FDF_LOG(WARNING, "Would have unbound, but we don't have a valid cilent");
      }
      break;
    default:
      FDF_LOG(ERROR, "Unexpected level %u", requested_level);
      return fpromise::make_error_promise(ZX_ERR_INVALID_ARGS);
  }

  power_level_ = requested_level;
  return fpromise::make_promise(
      []() { return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok()); });
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::AcquirePowerElementLease() {
  if (level_lease_client_) {
    FDF_LOG(DEBUG, "Not acquiring a lease due to already having a lease");
    return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
  }

  // Request dependent power nodes rise to kPowerLevelOn
  fidl::WireResult lease = fidl::WireCall(element_lessor_client_)->Lease(kOn);
  if (!lease.ok()) {
    FDF_LOG(ERROR, "Call to Lease failed: %s", lease.error().FormatDescription().c_str());
    return fpromise::make_result_promise<void, zx_status_t>(fpromise::error(ZX_ERR_IO));
  }
  if (lease->is_error()) {
    FDF_LOG(ERROR, "Failed to acquire lease: %s",
            fdf_power::LeaseErrorToString(lease->error_value()));
    return fpromise::make_result_promise<void, zx_status_t>(fpromise::error(ZX_ERR_IO));
  }

  level_lease_client_.emplace(std::move(lease->value()->lease_control), dispatcher());

  fpromise::bridge<void, zx_status_t> bridge;
  (*level_lease_client_)
      ->WatchStatus(fuchsia_power_broker::wire::LeaseStatus::kPending)
      .Then([this, completer = std::move(bridge.completer)](auto& lease_satisfied_result) mutable {
        if (!lease_satisfied_result.ok()) {
          FDF_LOG(ERROR, "Call to Lease WatchStatus failed: %s",
                  lease_satisfied_result.error().FormatDescription().c_str());
          completer.complete_error(ZX_ERR_INTERNAL);
          return;
        }
        if (lease_satisfied_result->status != fuchsia_power_broker::LeaseStatus::kSatisfied) {
          FDF_LOG(ERROR, "Call to Lease WatchStatus did not result in kSatisfied!?");
          completer.complete_error(ZX_ERR_BAD_STATE);
          return;
        }
        FDF_LOG(DEBUG, "Lease is satisfied");
        power_level_ = PowerLevel::kOn;
        completer.complete_ok();
        return;
      });
  return bridge.consumer.promise();
}

void BtHciBroadcom::HandleWakeLeaseTimeout() {
  executor_->schedule_task(AssertLevel(PowerLevel::kOff));
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::SetBdaddr(
    const std::array<uint8_t, kMacAddrLen>& bdaddr) {
  BcmSetBdaddrCmd command = {
      .header =
          {
              .opcode = kBcmSetBdaddrCmdOpCode,
              .parameter_total_size = sizeof(BcmSetBdaddrCmd) - sizeof(HciCommandHeader),
          },
      .bdaddr =
          {// HCI expects little endian. Swap bytes
           bdaddr[5], bdaddr[4], bdaddr[3], bdaddr[2], bdaddr[1], bdaddr[0]},
  };

  return SendCommand(&command.header, sizeof(command)).and_then([](std::vector<uint8_t>&) {});
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::SetDefaultPowerCaps() {
  if (serial_pid_ != PDEV_PID_BCM4381A1) {
    return fpromise::make_promise(
        []() { return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok()); });
  }
  return SendCommand(&kDefaultPowerCapCmd, sizeof(kDefaultPowerCapCmd))
      .and_then([](std::vector<uint8_t>& cmd_complete) {
        if (sizeof(HciCommandComplete) <= cmd_complete.size()) {
          HciCommandComplete event;
          std::memcpy(&event, cmd_complete.data(), sizeof(event));
          if (event.return_code == 0x00) {
            FDF_LOG(INFO, "set default power caps");
          } else {
            FDF_LOG(WARNING, "failed to set default power caps: 0x%02x", event.return_code);
          }
        }
      });
}

void BtHciBroadcom::NoteActivity(ActivityType activity) {
  drop_level_task_.Cancel();
  drop_level_task_.PostDelayed(dispatcher(), 2 * kDefaultIdleThreshold);
  executor_->schedule_task(AssertLevel(PowerLevel::kOn));
}

constexpr auto kOpenFlags = fuchsia_io::Flags::kPermReadBytes | fuchsia_io::Flags::kProtocolFile;

fpromise::promise<void, zx_status_t> BtHciBroadcom::LoadFirmware() {
  zx::vmo fw_vmo;
  size_t fw_size;

  // If there's no firmware for this PID, we don't expect the bind to happen without a
  // corresponding entry in the firmware table. Please double-check the PID value and add an entry
  // to the firmware table if it's valid.
  ZX_ASSERT_MSG(kFirmwareMap.find(serial_pid_) != kFirmwareMap.end(), "no mapping for PID: %u",
                serial_pid_);

  std::string full_filename = "/pkg/lib/firmware/";
  full_filename.append(kFirmwareMap.at(serial_pid_));

  auto client = incoming()->Open<fuchsia_io::File>(full_filename.c_str(), kOpenFlags);
  if (client.is_error()) {
    FDF_LOG(WARNING, "Open firmware file failed: %s", zx_status_get_string(client.error_value()));
    return fpromise::make_error_promise(client.error_value());
  }

  fidl::WireResult backing_memory_result =
      fidl::WireCall(*client)->GetBackingMemory(fuchsia_io::wire::VmoFlags::kRead);
  if (!backing_memory_result.ok()) {
    if (backing_memory_result.is_peer_closed()) {
      FDF_LOG(WARNING, "Failed to get backing memory: Peer closed");
      return fpromise::make_error_promise(ZX_ERR_NOT_FOUND);
    }
    FDF_LOG(WARNING, "Failed to get backing memory: %s",
            zx_status_get_string(backing_memory_result.status()));
    return fpromise::make_error_promise(backing_memory_result.status());
  }

  const auto* backing_memory = backing_memory_result.Unwrap();
  if (backing_memory->is_error()) {
    FDF_LOG(WARNING, "Failed to get backing memory: %s",
            zx_status_get_string(backing_memory->error_value()));
    return fpromise::make_error_promise(backing_memory->error_value());
  }

  zx::vmo& backing_vmo = backing_memory->value()->vmo;
  if (zx_status_t status = backing_vmo.get_prop_content_size(&fw_size); status != ZX_OK) {
    FDF_LOG(WARNING, "Failed to get vmo size: %s", zx_status_get_string(status));
    return fpromise::make_error_promise(status);
  }
  fw_vmo.reset(backing_vmo.release());

  return SendCommand(&kStartFirmwareDownloadCmd, sizeof(kStartFirmwareDownloadCmd))
      .or_else([](zx_status_t& status) -> fpromise::result<std::vector<uint8_t>, zx_status_t> {
        FDF_LOG(ERROR, "could not load firmware file");
        return fpromise::error(status);
      })
      .and_then([this](std::vector<uint8_t>& /*event*/) mutable {
        // give time for placing firmware in download mode
        return executor_->MakeDelayedPromise(zx::duration(kFirmwareDownloadDelay))
            .then([](fpromise::result<>& /*result*/) {
              return fpromise::result<void, zx_status_t>(fpromise::ok());
            });
      })
      .and_then([this, fw_vmo = std::move(fw_vmo), fw_size]() mutable {
        // The firmware is a sequence of HCI commands containing the firmware data as payloads.
        return SendVmoAsCommands(std::move(fw_vmo), fw_size);
      })
      .and_then([this]() -> fpromise::promise<void, zx_status_t> {
        if (is_uart_) {
          // firmware switched us back to 115200. switch back to kTargetBaudRate.
          fdf::Arena arena('CONF');
          fdf::WireUnownedResult<fuchsia_hardware_serialimpl::Device::Config> result =
              serial_client_.buffer(arena)->Config(kDefaultBaudRate, fhsi::kSerialSetBaudRateOnly);
          if (!result.ok()) {
            return fpromise::make_result_promise(fpromise::error(result.status()));
          }
          if (result->is_error()) {
            return fpromise::make_result_promise(fpromise::error(result->error_value()));
          }

          return executor_->MakeDelayedPromise(kBaudRateSwitchDelay)
              .then(
                  [this](fpromise::result<>& /*result*/) { return SetBaudRate(kTargetBaudRate); });
        }
        return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
      })
      .and_then([]() { FDF_LOG(INFO, "firmware loaded"); });
}

zx_status_t BtHciBroadcom::SendCommandSync(const void* command, size_t length) {
  // send HCI command
  fidl::Arena arena;
  auto command_vec = std::vector<uint8_t>(static_cast<const uint8_t*>(command),
                                          static_cast<const uint8_t*>(command) + length);
  auto command_view = fidl::VectorView<uint8_t>::FromExternal(command_vec);
  auto result =
      hci_transport_client_->Send(fhbt::wire::SentPacket::WithCommand(arena, command_view));
  if (result.status() != ZX_OK) {
    FDF_LOG(ERROR, "Failed to send command: %s", result.status_string());
    return result.status();
  }

  return ReadEventSync().status_value();
}

zx::result<std::vector<uint8_t>> BtHciBroadcom::ReadEventSync() {
  fidl::Status result = hci_transport_client_.HandleOneEvent(hci_event_handler_);
  if (result.status() != ZX_OK) {
    FDF_LOG(ERROR, "Failed to get event packet: %s", zx_status_get_string(result.status()));
    return zx::error(result.status());
  }

  // Read result will be stored in |event_receive_buffer_|.
  std::vector<uint8_t> packet_bytes = std::move(event_receive_buffer_);
  // Copy out the data from buffer and clear the buffer.
  event_receive_buffer_.clear();

  if (packet_bytes.size() < sizeof(HciCommandComplete)) {
    FDF_LOG(ERROR, "command channel read too short: %zu < %lu", packet_bytes.size(),
            sizeof(HciCommandComplete));
    return zx::error(ZX_ERR_INTERNAL);
  }

  HciCommandComplete event;
  std::memcpy(&event, packet_bytes.data(), sizeof(HciCommandComplete));
  if (event.header.event_code != kHciEvtCommandCompleteEventCode ||
      event.header.parameter_total_size < kMinEvtParamSize) {
    FDF_LOG(ERROR, "did not receive command complete or params too small");
    return zx::error(ZX_ERR_INTERNAL);
  }

  if (event.return_code != 0) {
    FDF_LOG(ERROR, "got command complete error %u", event.return_code);
    return zx::error(ZX_ERR_INTERNAL);
  }

  return zx::ok(std::move(packet_bytes));
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::SendVmoAsCommands(zx::vmo vmo, size_t size) {
  size_t offset = 0;

  while (offset < size) {
    uint8_t buffer[kMaxHciCommandSize];

    size_t remaining = size - offset;
    size_t read_amount = (remaining > sizeof(buffer) ? sizeof(buffer) : remaining);

    if (read_amount < sizeof(HciCommandHeader)) {
      FDF_LOG(ERROR, "short HCI command in firmware download");
      return fpromise::make_error_promise(ZX_ERR_INTERNAL);
    }

    zx_status_t status = vmo.read(buffer, offset, read_amount);
    if (status != ZX_OK) {
      return fpromise::make_error_promise(status);
    }

    HciCommandHeader header;
    std::memcpy(&header, buffer, sizeof(HciCommandHeader));
    size_t length = header.parameter_total_size + sizeof(header);
    if (read_amount < length) {
      FDF_LOG(ERROR, "short HCI command in firmware download");
      return fpromise::make_error_promise(ZX_ERR_INTERNAL);
    }

    offset += length;
    if (zx_status_t status = SendCommandSync(buffer, length); status != ZX_OK) {
      FDF_LOG(ERROR, "SendCommand failed in firmware download: %s", zx_status_get_string(status));
      return fpromise::make_error_promise(status);
    }
  }

  return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::Initialize() {
  FDF_LOG(DEBUG, "sending initial reset command");
  return SendCommand(&kResetCmd, sizeof(kResetCmd))
      .and_then([this](std::vector<uint8_t>&) -> fpromise::promise<void, zx_status_t> {
        if (is_uart_) {
          FDF_LOG(DEBUG, "setting baud rate to %u", kTargetBaudRate);
          // switch baud rate to TARGET_BAUD_RATE
          return SetBaudRate(kTargetBaudRate);
        }
        return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
      })
      .and_then([this]() {
        FDF_LOG(DEBUG, "loading firmware");
        return LoadFirmware();
      })
      .and_then([this]() {
        FDF_LOG(DEBUG, "sending reset command");
        return SendCommand(&kResetCmd, sizeof(kResetCmd));
      })
      .and_then([this](std::vector<uint8_t>&) -> fpromise::promise<void, zx_status_t> {
        FDF_LOG(DEBUG, "Getting mac address");
        zx::result metadata =
            fdf_metadata::GetMetadata<fuchsia_boot_metadata::MacAddressMetadata>(incoming());
        if (metadata.is_error()) {
          FDF_LOG(ERROR, "Error reading metadata: %s", metadata.status_string());
          return fpromise::make_error_promise(ZX_ERR_INTERNAL);
        }

        if (!metadata.value().mac_address().has_value()) {
          FDF_LOG(ERROR, "Mac address metadata missing mac address");
          return fpromise::make_error_promise(ZX_ERR_INTERNAL);
        }
        const auto& octets = metadata.value().mac_address().value().octets();
        FDF_LOG(INFO, "Got mac address %02x:%02x:%02x:%02x:%02x:%02x", octets[0], octets[1],
                octets[2], octets[3], octets[4], octets[5]);

        // send Set BDADDR command
        return SetBdaddr(octets);
      })
      .and_then([this]() { return SetDefaultPowerCaps(); })
      .and_then(
          [this]() { return EnableLowPowerMode(kDefaultIdleThreshold, kDefaultIdleThreshold); })
      .and_then([this]() { return AddNode(); })
      .then([this](fpromise::result<void, zx_status_t>& result) {
        zx_status_t status = result.is_ok() ? ZX_OK : result.error();
        return OnInitializeComplete(status);
      });
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::OnInitializeComplete(zx_status_t status) {
  // We're done with the HciTransport client end. Allow the HciTransport clients we vend to use it.
  hci_transport_client_end_ = hci_transport_client_.TakeClientEnd();

  if (status != ZX_OK) {
    FDF_LOG(ERROR, "device initialization failed: %s", zx_status_get_string(status));
    return fpromise::make_error_promise(status);
  }

  // We are done booting, we can drop our boot power needs.
  FDF_LOG(DEBUG, "dropping boot power lease");
  executor_->schedule_task(AssertLevel(PowerLevel::kOff));
  FDF_LOG(INFO, "initialization completed successfully.");
  return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
}

fpromise::promise<void, zx_status_t> BtHciBroadcom::AddNode() {
  zx::result connector = devfs_connector_.Bind(dispatcher());
  if (connector.is_error()) {
    FDF_LOG(ERROR, "Failed to bind devfs connecter to dispatcher: %s", connector.status_string());
    return fpromise::make_error_promise(connector.error_value());
  }

  fidl::Arena args_arena;
  auto devfs = fuchsia_driver_framework::wire::DevfsAddArgs::Builder(args_arena)
                   .connector(std::move(connector.value()))
                   .class_name("bt-hci")
                   .Build();

  auto args = fuchsia_driver_framework::wire::NodeAddArgs::Builder(args_arena)
                  .name("bt-hci-broadcom")
                  .devfs_args(devfs)
                  .Build();

  auto controller_endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::NodeController>();
  if (controller_endpoints.is_error()) {
    FDF_LOG(ERROR, "Create node controller end points failed: %s",
            zx_status_get_string(controller_endpoints.error_value()));
    return fpromise::make_error_promise(controller_endpoints.error_value());
  }

  // Create the endpoints of fuchsia_driver_framework::Node protocol for the child node, and hold
  // the client end of it, because no driver will bind to the child node.
  auto child_node_endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::Node>();
  if (child_node_endpoints.is_error()) {
    FDF_LOG(ERROR, "Create child node end points failed: %s",
            zx_status_get_string(child_node_endpoints.error_value()));
    return fpromise::make_error_promise(child_node_endpoints.error_value());
  }

  // Add bt-hci-broadcom child node.
  fpromise::bridge<void, zx_status_t> bridge;
  node_
      ->AddChild(args, std::move(controller_endpoints->server),
                 std::move(child_node_endpoints->server))
      .Then([this, completer = std::move(bridge.completer),
             child_node_client = std::move(child_node_endpoints->client),
             child_controller_client = std::move(controller_endpoints->client)](
                fidl::WireUnownedResult<fuchsia_driver_framework::Node::AddChild>&
                    child_result) mutable {
        if (!child_result.ok()) {
          FDF_LOG(ERROR, "Failed to add bt-hci-broadcom node, FIDL error: %s",
                  child_result.status_string());
          completer.complete_error(child_result.status());
          return;
        }

        if (child_result->is_error()) {
          FDF_LOG(ERROR, "Failed to add bt-hci-broadcom node: %u",
                  static_cast<uint32_t>(child_result->error_value()));
          completer.complete_error(ZX_ERR_INTERNAL);
          return;
        }

        child_node_.Bind(std::move(child_node_client), dispatcher(), this);
        node_controller_.Bind(std::move(child_controller_client), dispatcher(), this);
        completer.complete_ok();
      });

  return bridge.consumer.promise();
}

void BtHciBroadcom::CompleteStart(zx_status_t status) {
  if (start_completer_.has_value()) {
    start_completer_.value()(zx::make_result(status));
    start_completer_.reset();
  } else {
    FDF_LOG(ERROR, "CompleteStart called without start_completer_.");
  }
}

}  // namespace bt_hci_broadcom

FUCHSIA_DRIVER_EXPORT(bt_hci_broadcom::BtHciBroadcom);
