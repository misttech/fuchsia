// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "wlantap-phy-impl.h"

#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fidl/cpp/wire/status.h>
#include <lib/fit/defer.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <type_traits>
#include <utility>

#include <wlan/common/phy.h>
#include <wlan/drivers/log.h>

#include "utils.h"

namespace wlan {

template <class... Ts>
struct overloaded : Ts... {
  using Ts::operator()...;
};
template <class... Ts>
overloaded(Ts...) -> overloaded<Ts...>;

std::shared_ptr<WlanPhyImplDevice> WlanPhyImplDevice::New(
    const std::shared_ptr<const WlantapDriverContext>& context, zx::channel user_channel,
    const std::shared_ptr<const wlan_tap::WlantapPhyConfig>& phy_config,
    NodeControllerClient phy_controller) {
  WLAN_TRACE_DURATION();
  auto device = std::shared_ptr<WlanPhyImplDevice>(new WlanPhyImplDevice(context, phy_config));
  device->Init(std::move(user_channel), std::move(phy_controller));
  return device;
}

WlanPhyImplDevice::WlanPhyImplDevice(
    const std::shared_ptr<const WlantapDriverContext>& context,
    const std::shared_ptr<const wlan_tap::WlantapPhyConfig>& phy_config)
    : driver_context_(context), phy_config_(phy_config) {
  WLAN_TRACE_DURATION();
}

void WlanPhyImplDevice::Init(
    zx::channel user_channel,
    fidl::ClientEnd<fuchsia_driver_framework::NodeController> phy_controller) {
  WLAN_TRACE_DURATION();
  wlantap_phy_ = std::make_unique<WlantapPhy>(
      std::move(user_channel), phy_config_,
      [self = shared_from_this(),
       name = name_](WlantapPhy::ShutdownCompleter::Async wlantap_phy_shutdown_completer) mutable
          -> fit::result<zx_status_t> {
        // Return an error if |self| has already been reset(). This function
        // should only be called once, and |self| is reset() upon completion
        // to drop its reference.
        if (self == nullptr) {
          fdf::error("{}: shutdown callback called more than once", name);
          return fit::error(ZX_ERR_INTERNAL);
        }

        self->wlantap_phy_shutdown_completer_ = std::move(wlantap_phy_shutdown_completer);

        // Unbind the WlanPhyImpl child node. This effectively blocks iface
        // management from the outside.
        auto phy_removal_status = self->phy_controller_->Remove();
        fit::result<zx_status_t> result = fit::ok();
        if (phy_removal_status.is_error()) {
          fdf::error("{}: Could not remove phy: {}", name,
                     phy_removal_status.error_value().status_string());
          self->ShutdownComplete();

          result = fit::error(phy_removal_status.error_value().status());
        }
        self.reset();
        return result;
      });

  // The PhyControllerEventHandler class detects when NodeController server associated
  // with the phy is no longer available. This normally occurs during shutdown.
  class PhyControllerEventHandler
      : public fidl::AsyncEventHandler<fuchsia_driver_framework::NodeController> {
   public:
    explicit PhyControllerEventHandler(std::shared_ptr<WlanPhyImplDevice> device)
        : device_(std::move(device)) {}
    void on_fidl_error(::fidl::UnbindInfo error) override {
      WLAN_TRACE_DURATION();
      auto cleanup = fit::defer([this] { delete this; });

      auto device = std::move(device_);
      fdf::info("{}: phy node unbound: {}", device->name_, error.FormatDescription());
      device->phy_controller_ = {};

      std::optional<WlanPhyImplDevice::IfaceSlot> next_slot;
      std::visit(
          overloaded{
              [device, &next_slot](WlanPhyImplDevice::SlotActive& slot) {
                auto controller = std::move(slot.controller);
                auto mac = std::move(slot.mac);
                next_slot = WlanPhyImplDevice::SlotDestroying{.mac = std::move(mac),
                                                              .controller = std::move(controller)};
              },
              [device](WlanPhyImplDevice::SlotDestroying& slot) {
                auto status = slot.controller->Remove();
                if (status.is_error()) {
                  fdf::error("{}: Could not remove iface: {}", device->name_,
                             status.error_value().status_string());
                  device->ShutdownComplete();
                }
              },
              [device](WlanPhyImplDevice::SlotEmpty& slot) { device->ShutdownComplete(); },
              [device](WlanPhyImplDevice::SlotCreating& slot) { device->ShutdownComplete(); }},
          device->iface_slot_);

      if (next_slot.has_value()) {
        device->iface_slot_ = std::move(*next_slot);
        auto& destroying = std::get<WlanPhyImplDevice::SlotDestroying>(device->iface_slot_);
        auto status = destroying.controller->Remove();
        if (status.is_error()) {
          fdf::error("{}: Could not remove iface: {}", device->name_,
                     status.error_value().status_string());
          device->ShutdownComplete();
        }
      }
    }
    void handle_unknown_event(
        fidl::UnknownEventMetadata<fuchsia_driver_framework::NodeController> metadata) override {}

   private:
    std::shared_ptr<WlanPhyImplDevice> device_;
  };
  phy_controller_.Bind(std::move(phy_controller), fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                       new PhyControllerEventHandler(shared_from_this()));
}

void WlanPhyImplDevice::Init(InitRequestView request, fdf::Arena& arena,
                             InitCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void WlanPhyImplDevice::GetSupportedMacRoles(fdf::Arena& arena,
                                             GetSupportedMacRolesCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();  // wlantap-phy only supports a single mac role determined by the config
  wlan_common::WlanMacRole supported[1] = {phy_config_->mac_role};
  auto reply_vec = fidl::VectorView<wlan_common::WlanMacRole>::FromExternal(supported, 1);

  fdf::info("{}: received a 'GetSupportedMacRoles' DDK request. Responding with roles = {{{}}}",
            name_, static_cast<uint32_t>(phy_config_->mac_role));

  auto response =
      fuchsia_wlan_phyimpl::wire::WlanPhyImplGetSupportedMacRolesResponse::Builder(arena)
          .supported_mac_roles(reply_vec)
          .Build();
  completer.buffer(arena).ReplySuccess(response);
}

void WlanPhyImplDevice::CreateIface(CreateIfaceRequestView request, fdf::Arena& arena,
                                    CreateIfaceCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::info("{}: received a 'CreateIface' request", name_);

  std::string role_str = RoleToString(request->role());
  fdf::info("{}: received a 'CreateIface' for role: {}", name_, role_str);
  if (phy_config_->mac_role != request->role()) {
    fdf::error("{}: CreateIface({}): role not supported", name_, role_str);
    completer.buffer(arena).ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }

  if (!std::holds_alternative<SlotEmpty>(iface_slot_)) {
    fdf::error(
        "{}: CreateIface({}): Failed to create iface. wlantap only supports at most one iface.",
        name_, role_str);
    completer.buffer(arena).ReplyError(ZX_ERR_ALREADY_EXISTS);
    return;
  }

  if (!request->mlme_channel().is_valid()) {
    fdf::error("{}: CreateIface({}): MLME channel in request is invalid", name_, role_str);
    completer.buffer(arena).ReplyError(ZX_ERR_IO_INVALID);
    return;
  }

  iface_slot_ = SlotCreating{};

  zx_status_t status = CreateWlanSoftmac(request->role(), std::move(request->mlme_channel()));
  if (status != ZX_OK) {
    fdf::error("{}: CreateIface({}): Could not create softmac: {}", name_, role_str,
               zx_status_get_string(status));
    iface_slot_ = SlotEmpty{};
    completer.buffer(arena).ReplyError(status);
    return;
  }

  fidl::Arena fidl_arena;
  auto resp = fuchsia_wlan_phyimpl::wire::WlanPhyImplCreateIfaceResponse::Builder(fidl_arena)
                  .iface_id(0)
                  .Build();
  completer.buffer(arena).ReplySuccess(resp);
}

fit::result<zx_status_t> WlanPhyImplDevice::DestroyIface() {
  WLAN_TRACE_DURATION();
  if (!std::holds_alternative<SlotActive>(iface_slot_)) {
    fdf::error("{}: Iface doesn't exist or is not active", name_);
    return fit::error(ZX_ERR_NOT_FOUND);
  }

  auto& active = std::get<SlotActive>(iface_slot_);
  auto status = active.controller->Remove();
  if (status.is_error()) {
    fdf::error("{}: Failed to destroy iface: {}", name_, status.error_value().status_string());
    return fit::error(status.error_value().status());
  }

  iface_slot_ =
      SlotDestroying{.mac = std::move(active.mac), .controller = std::move(active.controller)};

  return fit::ok();
}

// Calls the stored ShutdownCompleter received through WlantapPhy.Shutdown().
void WlanPhyImplDevice::ShutdownComplete() {
  WLAN_TRACE_DURATION();
  if (this->wlantap_phy_shutdown_completer_.has_value()) {
    this->wlantap_phy_shutdown_completer_->Reply();
  }
}

void WlanPhyImplDevice::DestroyIface(DestroyIfaceRequestView request, fdf::Arena& arena,
                                     DestroyIfaceCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::info("{}: received a 'DestroyIface' DDK request", name_);
  completer.buffer(arena).Reply(DestroyIface());
}

void WlanPhyImplDevice::SetCountry(SetCountryRequestView request, fdf::Arena& arena,
                                   SetCountryCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::info("{}: SetCountry() to [{}] received", name_,
            wlan::common::Alpha2ToStr(request->alpha2()));

  wlan_tap::SetCountryArgs args{.alpha2 = request->alpha2()};
  zx_status_t status = wlantap_phy_->SetCountry(args);
  if (status != ZX_OK) {
    fdf::error("{}: SetCountry() failed: {}", name_, zx_status_get_string(status));
    completer.buffer(arena).ReplyError(status);
    return;
  }
  completer.buffer(arena).ReplySuccess();
}

void WlanPhyImplDevice::ClearCountry(fdf::Arena& arena, ClearCountryCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::warn("{}: ClearCountry() not supported", name_);
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void WlanPhyImplDevice::GetCountry(fdf::Arena& arena, GetCountryCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::warn("{}: GetCountry() not supported", name_);
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void WlanPhyImplDevice::SetPowerSaveMode(SetPowerSaveModeRequestView request, fdf::Arena& arena,
                                         SetPowerSaveModeCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::warn("{}: SetPowerSaveMode() not supported", name_);
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void WlanPhyImplDevice::GetPowerSaveMode(fdf::Arena& arena,
                                         GetPowerSaveModeCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::warn("{}: GetPowerSaveMode() not supported", name_);
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void WlanPhyImplDevice::PowerDown(fdf::Arena& arena, PowerDownCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::warn("{}: PowerDown() not supported", name_);
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}
void WlanPhyImplDevice::PowerUp(fdf::Arena& arena, PowerUpCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::warn("{}: PowerUp() not supported", name_);
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void WlanPhyImplDevice::Reset(fdf::Arena& arena, ResetCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::warn("{}: Reset() not supported", name_);
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}
void WlanPhyImplDevice::GetPowerState(fdf::Arena& arena, GetPowerStateCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::warn("{}: GetPowerState() not supported", name_);
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void WlanPhyImplDevice::SetBtCoexistenceMode(SetBtCoexistenceModeRequestView request,
                                             fdf::Arena& arena,
                                             SetBtCoexistenceModeCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::warn("{}: SetBtCoexistenceMode() not supported", name_);
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}

void WlanPhyImplDevice::SetTxPowerScenario(SetTxPowerScenarioRequestView request, fdf::Arena& arena,
                                           SetTxPowerScenarioCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::warn("{}: SetTxPowerScenario() not supported", name_);
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}
void WlanPhyImplDevice::ResetTxPowerScenario(fdf::Arena& arena,
                                             ResetTxPowerScenarioCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::warn("{}: ResetTxPowerScenario() not supported", name_);
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}
void WlanPhyImplDevice::GetTxPowerScenario(fdf::Arena& arena,
                                           GetTxPowerScenarioCompleter::Sync& completer) {
  WLAN_TRACE_DURATION();
  fdf::warn("{}: GetTxPowerScenario() not supported", name_);
  completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
}

zx_status_t WlanPhyImplDevice::CreateWlanSoftmac(wlan_common::WlanMacRole role,
                                                 zx::channel mlme_channel) {
  WLAN_TRACE_DURATION();
  static size_t n = 0;
  char name[ZX_MAX_NAME_LEN + 1];
  snprintf(name, sizeof(name), "wlansoftmac-%lu", n++);

  auto endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::NodeController>();
  if (endpoints.is_error()) {
    fdf::error("{}: Failed to create endpoints: {}", name_, endpoints);
    return endpoints.status_value();
  }

  auto serve_result = ServeWlanSoftmac(name, role, std::move(mlme_channel));
  if (serve_result.is_error()) {
    fdf::error("{}: ServeWlanSoftmac failed: {}", name_, serve_result.status_string());
    return serve_result.error_value();
  }
  std::unique_ptr<WlantapMac> mac = std::move(serve_result.value());

  zx_status_t status = AddWlanSoftmacChild(name, std::move(endpoints->server));
  if (status != ZX_OK) {
    fdf::error("{}: AddWlanSoftmacChild failed: {}", name_, zx_status_get_string(status));
    zx::result remove_res =
        driver_context_->outgoing()->RemoveService<fuchsia_wlan_softmac::Service>(name);
    if (remove_res.is_error()) {
      fdf::error("{}: Failed to remove service instance during rollback: {}", name_, remove_res);
    }
    return status;
  }

  // The IfaceControllerEventHandler class detects when NodeController server associated
  // with an iface is no longer available. This normally occurs during shutdown.
  class IfaceControllerEventHandler
      : public fidl::AsyncEventHandler<fuchsia_driver_framework::NodeController> {
   public:
    explicit IfaceControllerEventHandler(std::shared_ptr<WlanPhyImplDevice> device)
        : device_(std::move(device)) {}
    void on_fidl_error(::fidl::UnbindInfo error) override {
      auto cleanup = fit::defer([this] { delete this; });
      auto device = std::move(device_);
      fdf::info("{}: Iface node unbound: {}", device->name_, error.FormatDescription());

      std::visit(overloaded{[device](WlanPhyImplDevice::SlotActive& slot) {
                              fdf::warn("{}: Iface node unexpectedly unbound!", device->name_);
                            },
                            [device](WlanPhyImplDevice::SlotDestroying& slot) {
                              if (device->wlantap_phy_shutdown_completer_.has_value()) {
                                device->ShutdownComplete();
                              }
                            },
                            [](WlanPhyImplDevice::SlotEmpty& slot) {},
                            [](WlanPhyImplDevice::SlotCreating& slot) {}},
                 device->iface_slot_);

      device->iface_slot_ = WlanPhyImplDevice::SlotEmpty{};
    }
    void handle_unknown_event(
        fidl::UnknownEventMetadata<fuchsia_driver_framework::NodeController> metadata) override {}

   private:
    std::shared_ptr<WlanPhyImplDevice> device_;
  };

  fidl::Client<fuchsia_driver_framework::NodeController> controller;
  controller.Bind(std::move(endpoints->client), fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                  new IfaceControllerEventHandler(shared_from_this()));

  iface_slot_ = SlotActive{.mac = std::move(mac), .controller = std::move(controller)};
  return ZX_OK;
}

zx_status_t WlanPhyImplDevice::AddWlanSoftmacChild(
    std::string_view name, fidl::ServerEnd<fuchsia_driver_framework::NodeController> server) {
  WLAN_TRACE_DURATION();
  fidl::Arena arena;

  auto offers = std::vector{fdf::MakeOffer2<fuchsia_wlan_softmac::Service>(arena, name)};
  fdf::info("{}: Creating Child node", name_);
  auto args = fuchsia_driver_framework::wire::NodeAddArgs::Builder(arena)
                  .name(name)
                  .offers2(offers)
                  .Build();

  auto res = driver_context_->node_client()->AddChild(args, std::move(server), {});
  if (!res.ok()) {
    fdf::error("{}: Failed to add child: {}", name_, res.status_string());
    return res.status();
  }

  return ZX_OK;
}

zx::result<std::unique_ptr<WlantapMac>> WlanPhyImplDevice::ServeWlanSoftmac(
    std::string_view name, wlan_common::WlanMacRole role, zx::channel mlme_channel) {
  WLAN_TRACE_DURATION();
  auto out_mac =
      std::make_unique<WlantapMac>(wlantap_phy_.get(), role, phy_config_, std::move(mlme_channel));

  fdf::info("{}: Adding softmac outgoing service", name_);
  fuchsia_wlan_softmac::Service::InstanceHandler handler(
      {.wlan_softmac = out_mac->ProtocolHandler()});

  zx::result result = driver_context_->outgoing()->AddService<fuchsia_wlan_softmac::Service>(
      std::move(handler), name);

  if (result.is_error()) {
    fdf::error("{}: Failed To add WlanSoftmac service: {}", name_, result);
    return result.take_error();
  }

  return zx::ok(std::move(out_mac));
}

}  // namespace wlan
