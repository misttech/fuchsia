// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "device.h"

#include <fidl/fuchsia.wlan.device/cpp/fidl.h>
#include <fidl/fuchsia.wlan.device/cpp/wire.h>
#include <fuchsia/wlan/common/cpp/fidl.h>
#include <fuchsia/wlan/internal/cpp/fidl.h>
#include <inttypes.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/fidl/cpp/wire/arena.h>
#include <net/ethernet.h>
#include <zircon/status.h>

#include <iterator>

#include <sdk/lib/driver/logging/cpp/logger.h>
#include <wlan/common/channel.h>
#include <wlan/common/element.h>
#include <wlan/common/phy.h>

#include "debug.h"
#include "wlan/drivers/log_instance.h"

namespace {

std::optional<fuchsia_wlan_phyimpl::wire::TxPowerScenario> ConvertPowerScenario(
    fuchsia_wlan_internal::wire::TxPowerScenario scenario) {
  switch (scenario) {
    case fuchsia_wlan_internal::TxPowerScenario::kDefault:
      return fuchsia_wlan_phyimpl::wire::TxPowerScenario::kDefault;
    case fuchsia_wlan_internal::TxPowerScenario::kVoiceCall:
      return fuchsia_wlan_phyimpl::wire::TxPowerScenario::kVoiceCall;
    case fuchsia_wlan_internal::TxPowerScenario::kHeadCellOff:
      return fuchsia_wlan_phyimpl::wire::TxPowerScenario::kHeadCellOff;
    case fuchsia_wlan_internal::TxPowerScenario::kHeadCellOn:
      return fuchsia_wlan_phyimpl::wire::TxPowerScenario::kHeadCellOn;
    case fuchsia_wlan_internal::TxPowerScenario::kBodyCellOff:
      return fuchsia_wlan_phyimpl::wire::TxPowerScenario::kBodyCellOff;
    case fuchsia_wlan_internal::TxPowerScenario::kBodyCellOn:
      return fuchsia_wlan_phyimpl::wire::TxPowerScenario::kBodyCellOn;
    case fuchsia_wlan_internal::TxPowerScenario::kBodyBtActive:
      return fuchsia_wlan_phyimpl::wire::TxPowerScenario::kBodyBtActive;
    default:
      return std::nullopt;
  }
}

std::optional<fuchsia_wlan_internal::wire::TxPowerScenario> ConvertPowerScenario(
    fuchsia_wlan_phyimpl::wire::TxPowerScenario scenario) {
  switch (scenario) {
    case fuchsia_wlan_phyimpl::TxPowerScenario::kDefault:
      return fuchsia_wlan_internal::wire::TxPowerScenario::kDefault;
    case fuchsia_wlan_phyimpl::TxPowerScenario::kVoiceCall:
      return fuchsia_wlan_internal::wire::TxPowerScenario::kVoiceCall;
    case fuchsia_wlan_phyimpl::TxPowerScenario::kHeadCellOff:
      return fuchsia_wlan_internal::wire::TxPowerScenario::kHeadCellOff;
    case fuchsia_wlan_phyimpl::TxPowerScenario::kHeadCellOn:
      return fuchsia_wlan_internal::wire::TxPowerScenario::kHeadCellOn;
    case fuchsia_wlan_phyimpl::TxPowerScenario::kBodyCellOff:
      return fuchsia_wlan_internal::wire::TxPowerScenario::kBodyCellOff;
    case fuchsia_wlan_phyimpl::TxPowerScenario::kBodyCellOn:
      return fuchsia_wlan_internal::wire::TxPowerScenario::kBodyCellOn;
    case fuchsia_wlan_phyimpl::TxPowerScenario::kBodyBtActive:
      return fuchsia_wlan_internal::wire::TxPowerScenario::kBodyBtActive;
    default:
      return std::nullopt;
  }
}

fuchsia_wlan_phyimpl::WlanPhyImplNotifyError ConvertToPhyImplNotifyError(zx_status_t status) {
  switch (status) {
    case ZX_ERR_INTERNAL:
      return fuchsia_wlan_phyimpl::WlanPhyImplNotifyError::kInternal;
    case ZX_ERR_INVALID_ARGS:
      return fuchsia_wlan_phyimpl::WlanPhyImplNotifyError::kInvalidArgs;
    case ZX_ERR_SHOULD_WAIT:
      return fuchsia_wlan_phyimpl::WlanPhyImplNotifyError::kShouldWait;
    case ZX_ERR_NOT_SUPPORTED:
      return fuchsia_wlan_phyimpl::WlanPhyImplNotifyError::kNotSupported;
    default:
      lerror("Unknown phyimplnotify error: %zu", status);
      return fuchsia_wlan_phyimpl::WlanPhyImplNotifyError::kNotSupported;
  }
}

}  // namespace

namespace wlanphy {
using fuchsia_wlan_phyimpl::WlanPhyImplNotifyError;

Device::Device()
    : fdf::DriverBase2("wlanphy"), devfs_connector_(fit::bind_member<&Device::Serve>(this)) {}

zx::result<> Device::Start(fdf::DriverContext context) {
  wlan::drivers::log::Instance::Init(0);

  auto client_dispatcher =
      fdf::SynchronizedDispatcher::Create({}, "wlanphy", [&](fdf_dispatcher_t*) {});

  ZX_ASSERT_MSG(!client_dispatcher.is_error(), "Creating dispatcher error: %s",
                zx_status_get_string(client_dispatcher.status_value()));
  client_dispatcher_ = std::move(*client_dispatcher);
  auto phyimplnotify_disp =
      fdf::SynchronizedDispatcher::Create({}, "wlanphyimplnotify", [this](fdf_dispatcher_t*) {
        ldebug_device("phyimplnotify dispatcher shutdown handler");
        phyimplnotify_dispatcher_.close();
        phyimplnotify_shutdown_complete_.Signal();
      });

  ZX_ASSERT_MSG(!phyimplnotify_disp.is_error(), "Creating phyimplnotify dispatcher error: %s",
                zx_status_get_string(phyimplnotify_disp.status_value()));
  phyimplnotify_dispatcher_ = std::move(*phyimplnotify_disp);

  zx_status_t status;
  if ((status = ConnectToWlanPhyImpl(context.incoming())) != ZX_OK) {
    lerror("Connect to WlanPhyImpl failed: %s", zx_status_get_string(status));
    return zx::error(status);
  }
  if ((status = AddWlanDeviceConnector()) != ZX_OK) {
    lerror("Adding WlanPhy service failed: %s", zx_status_get_string(status));
    return zx::error(status);
  }
  return zx::ok();
}

void Device::Stop(fdf::StopCompleter completer) {
  client_.AsyncTeardown();
  if (phyimplnotify_dispatcher_.get()) {
    ldebug_device("shutting down phyimplnotify dispatcher");
    phyimplnotify_dispatcher_.ShutdownAsync();
    phyimplnotify_shutdown_complete_.Wait();
  }
  completer(zx::ok());
}

void Device::Connect(ConnectRequestView request, ConnectCompleter::Sync& completer) {
  ConnectPhyServerEnd(std::move(request->request));
}

void Device::ConnectPhyServerEnd(fidl::ServerEnd<fuchsia_wlan_device::Phy> server_end) {
  ltrace_fn();
  phy_servers_.AddBinding(dispatcher(), std::move(server_end), this, fidl::kIgnoreBindingClosure);
}

zx_status_t Device::ConnectToWlanPhyImpl(fdf::Namespace& incoming) {
  auto client_end = incoming.Connect<fuchsia_wlan_phyimpl::Service::WlanPhyImpl>();
  if (client_end.is_error()) {
    lerror("Connect to wlanphyimpl service Failed = %s", client_end.status_string());
    return client_end.status_value();
  }
  client_.Bind(std::move(*client_end), client_dispatcher_.get());
  if (!client_.is_valid()) {
    lerror("WlanPhyImpl Client is not valid");
    return ZX_ERR_BAD_HANDLE;
  }

  // All the errors are logged in SetupWlanPhyImplNotifyServer(), no need to log here.
  return SetupWlanPhyImplNotifyServer();
}

zx_status_t Device::SetupWlanPhyImplNotifyServer() {
  auto [client, server] = fidl::Endpoints<fuchsia_wlan_phyimpl::WlanPhyImplNotify>::Create();

  auto phyimplnotify_add_binding = [&]() {
    phyimplnotify_bindings_.AddBinding(phyimplnotify_dispatcher_.async_dispatcher(),
                                       std::move(server), this, fidl::kIgnoreBindingClosure);
  };

  libsync::Completion complete;
  async::PostTask(phyimplnotify_dispatcher_.async_dispatcher(), [&]() mutable {
    phyimplnotify_add_binding();
    complete.Signal();
  });
  complete.Wait();
  fdf::Arena fdf_arena(0u);

  fidl::Arena fidl_arena;
  auto builder = fuchsia_wlan_phyimpl::wire::WlanPhyImplInitRequest::Builder(fidl_arena);
  builder.notify_client(std::move(client));

  client_.buffer(fdf_arena)
      ->Init(builder.Build())
      .ThenExactlyOnce(
          [&](fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::Init>& result) mutable {
            if (!result.ok()) {
              lerror("Init failed with FIDL error %s", result.status_string());
              return;
            }
            if (result->is_error()) {
              lerror("Init failed with error %s", zx_status_get_string(result->error_value()));
              return;
            }
            linfo("Successfully sent phyimplifc client end to wlan driver");
          });
  return ZX_OK;
}

zx_status_t Device::AddWlanDeviceConnector() {
  zx::result connector = devfs_connector_.Bind(dispatcher());
  if (connector.is_error()) {
    return connector.status_value();
  }

  fuchsia_driver_framework::DevfsAddArgs devfs_args{
      {.connector = std::move(connector.value()), .class_name = "wlanphy"}};

  zx::result child = AddOwnedChild("wlanphy", devfs_args);
  if (child.is_error()) {
    lerror("Failed to add child: %s", child.status_string());
    return child.status_value();
  }
  child_ = std::move(child.value());

  return ZX_OK;
}

void Device::GetSupportedMacRoles(GetSupportedMacRolesCompleter::Sync& completer) {
  ltrace_fn();
  fdf::Arena fdf_arena(0u);

  client_.buffer(fdf_arena)->GetSupportedMacRoles().ThenExactlyOnce(
      [completer = completer.ToAsync()](
          fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::GetSupportedMacRoles>&
              result) mutable {
        if (!result.ok()) {
          completer.ReplyError(result.status());
          return;
        }
        if (result->is_error()) {
          completer.ReplyError(result->error_value());
          return;
        }
        if (!result->value()->has_supported_mac_roles()) {
          completer.ReplyError(ZX_ERR_UNAVAILABLE);
        }
        if (result->value()->supported_mac_roles().size() >
            fuchsia::wlan::common::MAX_SUPPORTED_MAC_ROLES) {
          completer.ReplyError(ZX_ERR_OUT_OF_RANGE);
          return;
        }

        completer.ReplySuccess(result->value()->supported_mac_roles());
      });
}

const fidl::Array<uint8_t, 6> NULL_MAC_ADDR{0x00, 0x00, 0x00, 0x00, 0x00, 0x00};

void Device::CreateIface(CreateIfaceRequestView request, CreateIfaceCompleter::Sync& completer) {
  ltrace_fn();
  if (request->req.role.IsUnknown()) {
    lerror("CreateIface failed: invalid mac role %u", static_cast<uint32_t>(request->req.role));
    completer.ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }

  fdf::Arena fdf_arena(0u);

  fidl::Arena fidl_arena;
  auto builder = fuchsia_wlan_phyimpl::wire::WlanPhyImplCreateIfaceRequest::Builder(fidl_arena);
  builder.role(request->req.role);
  builder.mlme_channel(std::move(request->req.mlme_channel));

  if (!std::equal(std::begin(NULL_MAC_ADDR), std::end(NULL_MAC_ADDR),
                  request->req.init_sta_addr.data())) {
    builder.init_sta_addr(request->req.init_sta_addr);
  }

  client_.buffer(fdf_arena)
      ->CreateIface(builder.Build())
      .ThenExactlyOnce([completer = completer.ToAsync()](
                           fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::CreateIface>&
                               result) mutable {
        if (!result.ok()) {
          lerror("CreateIface failed with FIDL error %s", result.status_string());
          completer.ReplyError(result.status());
          return;
        }
        if (result->is_error()) {
          lerror("CreateIface failed with error %s", zx_status_get_string(result->error_value()));
          completer.ReplyError(result->error_value());
          return;
        }

        if (!result->value()->has_iface_id()) {
          lerror("CreateIface failed. Response missing iface_id");
          completer.ReplyError(ZX_ERR_INTERNAL);
          return;
        }
        completer.ReplySuccess(result->value()->iface_id());
      });
}

void Device::DestroyIface(DestroyIfaceRequestView request, DestroyIfaceCompleter::Sync& completer) {
  ltrace_fn();
  fdf::Arena fdf_arena(0u);

  fidl::Arena fidl_arena;
  auto builder = fuchsia_wlan_phyimpl::wire::WlanPhyImplDestroyIfaceRequest::Builder(fidl_arena);
  builder.iface_id(request->req.id);

  client_.buffer(fdf_arena)
      ->DestroyIface(builder.Build())
      .ThenExactlyOnce([completer = completer.ToAsync()](
                           fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::DestroyIface>&
                               result) mutable {
        if (!result.ok()) {
          lerror("DestroyIface failed with FIDL error %s", result.status_string());
          completer.ReplyError(result.status());
          return;
        }
        if (result->is_error()) {
          if (result->error_value() != ZX_ERR_NOT_FOUND) {
            lerror("DestroyIface failed with error %s",
                   zx_status_get_string(result->error_value()));
          }
          completer.ReplyError(result->error_value());
          return;
        }

        completer.ReplySuccess();
      });
}

void Device::SetCountry(SetCountryRequestView request, SetCountryCompleter::Sync& completer) {
  ltrace_fn();
  ldebug_device("SetCountry to %s", wlan::common::Alpha2ToStr(request->req.alpha2).c_str());
  fdf::Arena fdf_arena(0u);

  auto alpha2 = ::fidl::Array<uint8_t, fuchsia_wlan_phyimpl::wire::kWlanphyAlpha2Len>();
  memcpy(alpha2.data(), request->req.alpha2.data(), fuchsia_wlan_phyimpl::wire::kWlanphyAlpha2Len);

  auto out_country = fuchsia_wlan_phyimpl::wire::WlanPhyCountry::WithAlpha2(alpha2);
  client_.buffer(fdf_arena)
      ->SetCountry(out_country)
      .ThenExactlyOnce([completer = completer.ToAsync()](
                           fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::SetCountry>&
                               result) mutable {
        if (!result.ok()) {
          lerror("SetCountry failed with FIDL error %s", result.status_string());
          completer.Reply(result.status());
          return;
        }
        if (result->is_error()) {
          lerror("SetCountry failed with error %s", zx_status_get_string(result->error_value()));
          completer.Reply(result->error_value());
          return;
        }

        completer.Reply(ZX_OK);
      });
}

void Device::GetCountry(GetCountryCompleter::Sync& completer) {
  ltrace_fn();
  fdf::Arena fdf_arena(0u);

  client_.buffer(fdf_arena)->GetCountry().ThenExactlyOnce(
      [completer = completer.ToAsync()](
          fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::GetCountry>& result) mutable {
        fuchsia_wlan_device::wire::CountryCode resp;
        zx_status_t status;
        if (!result.ok()) {
          lerror("GetCountry failed with FIDL error %s", result.status_string());
          status = result.status();
          completer.ReplyError(status);
          return;
        }
        if (result->is_error()) {
          lerror("GetCountry failed with error %s", zx_status_get_string(result->error_value()));
          status = result->error_value();
          completer.ReplyError(status);
          return;
        }
        if (!result->value()->is_alpha2()) {
          lerror("GetCountry failed. Response union is not an alpha2: %" PRIu64,
                 result->value()->Which());
          completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
          return;
        }
        memcpy(resp.alpha2.data(), result->value()->alpha2().data(),
               fuchsia_wlan_phyimpl::wire::kWlanphyAlpha2Len);

        completer.ReplySuccess(resp);
      });
}

void Device::ClearCountry(ClearCountryCompleter::Sync& completer) {
  ltrace_fn();
  fdf::Arena fdf_arena(0u);

  client_.buffer(fdf_arena)->ClearCountry().ThenExactlyOnce(
      [completer = completer.ToAsync()](
          fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::ClearCountry>& result) mutable {
        if (!result.ok()) {
          lerror("ClearCountry failed with FIDL error %s", result.status_string());
          completer.Reply(result.status());
          return;
        }
        if (result->is_error()) {
          lerror("ClearCountry failed with error %s", zx_status_get_string(result->error_value()));
          completer.Reply(result->error_value());
          return;
        }

        completer.Reply(ZX_OK);
      });
}

void Device::SetPowerSaveMode(SetPowerSaveModeRequestView request,
                              SetPowerSaveModeCompleter::Sync& completer) {
  ltrace_fn();
  ldebug_device("SetPowerSaveMode to %d", request->req);
  fdf::Arena fdf_arena(0u);

  fidl::Arena fidl_arena;
  auto builder =
      fuchsia_wlan_phyimpl::wire::WlanPhyImplSetPowerSaveModeRequest::Builder(fidl_arena);
  builder.ps_mode(request->req);

  client_.buffer(fdf_arena)
      ->SetPowerSaveMode(builder.Build())
      .ThenExactlyOnce(
          [completer = completer.ToAsync()](
              fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::SetPowerSaveMode>&
                  result) mutable {
            if (!result.ok()) {
              lerror("SetPowerSaveMode failed with FIDL error %s", result.status_string());
              completer.Reply(result.status());
              return;
            }
            if (result->is_error()) {
              lerror("SetPowerSaveMode failed with error %s",
                     zx_status_get_string(result->error_value()));
              completer.Reply(result->error_value());
              return;
            }

            completer.Reply(ZX_OK);
          });
}

void Device::GetPowerSaveMode(GetPowerSaveModeCompleter::Sync& completer) {
  ltrace_fn();
  fdf::Arena fdf_arena(0u);

  client_.buffer(fdf_arena)->GetPowerSaveMode().ThenExactlyOnce(
      [completer = completer.ToAsync()](
          fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::GetPowerSaveMode>&
              result) mutable {
        if (!result.ok()) {
          lerror("GetPowerSaveMode failed with FIDL error %s", result.status_string());
          completer.ReplyError(result.status());
          return;
        }
        if (result->is_error()) {
          lerror("GetPowerSaveMode failed with error %s",
                 zx_status_get_string(result->error_value()));
          completer.ReplyError(result->error_value());
          return;
        }

        if (!result->value()->has_ps_mode()) {
          lerror("GetPowerSaveMode failed. Response missing ps_mode.");
          completer.ReplyError(ZX_ERR_INTERNAL);
          return;
        }

        completer.ReplySuccess(result->value()->ps_mode());
      });
}

void Device::PowerDown(PowerDownCompleter::Sync& completer) {
  ltrace_fn();
  fdf::Arena fdf_arena(0u);

  client_.buffer(fdf_arena)->PowerDown().ThenExactlyOnce(
      [completer = completer.ToAsync()](
          fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::PowerDown>& result) mutable {
        if (!result.ok()) {
          lerror("PowerDown failed with FIDL error %s", result.FormatDescription().c_str());
          completer.ReplyError(result.status());
          return;
        }
        if (result->is_error()) {
          lerror("PowerDown failed with error %s", zx_status_get_string(result->error_value()));
          completer.ReplyError(result->error_value());
          return;
        }

        completer.ReplySuccess();
      });
}

void Device::PowerUp(PowerUpCompleter::Sync& completer) {
  ltrace_fn();
  fdf::Arena fdf_arena(0u);

  client_.buffer(fdf_arena)->PowerUp().ThenExactlyOnce(
      [completer = completer.ToAsync()](
          fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::PowerUp>& result) mutable {
        if (!result.ok()) {
          lerror("PowerUp failed with FIDL error %s", result.FormatDescription().c_str());
          completer.ReplyError(result.status());
          return;
        }
        if (result->is_error()) {
          lerror("PowerUp failed with error %s", zx_status_get_string(result->error_value()));
          completer.ReplyError(result->error_value());
          return;
        }

        completer.ReplySuccess();
      });
}

void Device::Reset(ResetCompleter::Sync& completer) {
  ltrace_fn();
  fdf::Arena fdf_arena(0u);

  client_.buffer(fdf_arena)->Reset().ThenExactlyOnce(
      [completer = completer.ToAsync()](
          fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::Reset>& result) mutable {
        if (!result.ok()) {
          lerror("Reset failed with FIDL error %s", result.FormatDescription().c_str());
          completer.ReplyError(result.status());
          return;
        }
        if (result->is_error()) {
          lerror("Reset failed with error %s", zx_status_get_string(result->error_value()));
          completer.ReplyError(result->error_value());
          return;
        }

        completer.ReplySuccess();
      });
}

void Device::GetPowerState(GetPowerStateCompleter::Sync& completer) {
  ltrace_fn();
  fdf::Arena fdf_arena(0u);

  client_.buffer(fdf_arena)->GetPowerState().ThenExactlyOnce(
      [completer = completer.ToAsync()](
          fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::GetPowerState>&
              result) mutable {
        if (!result.ok()) {
          lerror("GetPowerState failed with FIDL error %s", result.FormatDescription().c_str());
          completer.ReplyError(result.status());
          return;
        }
        if (result->is_error()) {
          lerror("GetPowerState failed with error %s", zx_status_get_string(result->error_value()));
          completer.ReplyError(result.status());
          return;
        }
        if (!result->value()->has_power_on()) {
          lerror("GetPowerState failed. power_on value not found");
          completer.ReplyError(ZX_ERR_BAD_STATE);
          return;
        }
        completer.ReplySuccess(result->value()->power_on());
      });
}

void Device::SetBtCoexistenceMode(SetBtCoexistenceModeRequestView request,
                                  SetBtCoexistenceModeCompleter::Sync& completer) {
  ltrace_fn();
  ldebug_device("SetBtCoexistenceMode to %d", request->mode);
  fdf::Arena fdf_arena(0u);

  fidl::Arena fidl_arena;
  auto builder =
      fuchsia_wlan_phyimpl::wire::WlanPhyImplSetBtCoexistenceModeRequest::Builder(fidl_arena);
  switch (request->mode) {
    case ::fuchsia_wlan_internal::wire::BtCoexistenceMode::kModeAuto:
      builder.mode(fuchsia_wlan_phyimpl::wire::BtCoexistenceMode::kModeAuto);
      break;
    case ::fuchsia_wlan_internal::wire::BtCoexistenceMode::kModeOff:
      builder.mode(fuchsia_wlan_phyimpl::wire::BtCoexistenceMode::kModeOff);
      break;
    default:
      lwarn("Unknown BtCoexistenceMode: %d, defaulting to kModeAuto", request->mode);
      builder.mode(fuchsia_wlan_phyimpl::wire::BtCoexistenceMode::kModeAuto);
      break;
  }

  client_.buffer(fdf_arena)
      ->SetBtCoexistenceMode(builder.Build())
      .ThenExactlyOnce(
          [completer = completer.ToAsync()](
              fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::SetBtCoexistenceMode>&
                  result) mutable {
            if (!result.ok()) {
              lerror("SetBtCoexistenceMode failed with FIDL error %s", result.status_string());
              completer.ReplyError(result.status());
              return;
            }
            if (result->is_error()) {
              lerror("SetPowerSaveMode failed with error %s",
                     zx_status_get_string(result->error_value()));
              completer.ReplyError(result->error_value());
              return;
            }

            completer.ReplySuccess();
          });
}

void Device::OnCriticalError(OnCriticalErrorRequestView request,
                             OnCriticalErrorCompleter::Sync& completer) {
  if (!request->has_reason_code()) {
    lerror("OnCriticalError does not contain reason code");
    completer.ReplyError(ConvertToPhyImplNotifyError(ZX_ERR_INVALID_ARGS));
    return;
  }

  ldebug_device("Received Critical error with reason: %d", request->reason_code());
  // Forward the critical error to the wlanphy client.
  zx_status_t status = SendCriticalErrorEvent(request->reason_code());
  if (status == ZX_OK) {
    completer.ReplySuccess();
    return;
  }
  completer.ReplyError(ConvertToPhyImplNotifyError(status));
}

void Device::OnCountryCodeChange(OnCountryCodeChangeRequestView request,
                                 OnCountryCodeChangeCompleter::Sync& completer) {
  ldebug_device("sending ccode change event to wlanphy client");
  if (!request->has_phy_country()) {
    lerror("Country code not present");
    completer.ReplyError(ConvertToPhyImplNotifyError(ZX_ERR_INVALID_ARGS));
    return;
  }
  fuchsia_wlan_device::wire::CountryCode country_code{};
  ldebug_device("Country code changed to: %c%c", request->phy_country().alpha2().data()[0],
                request->phy_country().alpha2().data()[1]);
  country_code.alpha2 = request->phy_country().alpha2();

  if (!phy_servers_.size()) {
    lerror("Cannot forward country code phy server binding not set");
    completer.ReplyError(ConvertToPhyImplNotifyError(ZX_ERR_SHOULD_WAIT));
    return;
  }

  zx_status_t notification_status = ZX_OK;

  phy_servers_.ForEachBinding([&](const fidl::ServerBinding<fuchsia_wlan_device::Phy>& binding) {
    auto result = fidl::WireSendEvent(binding)->OnCountryCodeChange(country_code);
    if (!result.ok()) {
      lerror("Failed to send country code event: %s", result.FormatDescription().c_str());
      // Note the first failure.
      if (notification_status == ZX_OK) {
        notification_status = result.status();
      }
    } else {
      ldebug_device("country code change event forwarded to wlanphy client successfully");
    }
  });
  if (notification_status != ZX_OK) {
    // If even one client failed, report error to the caller.
    completer.ReplyError(ConvertToPhyImplNotifyError(notification_status));
  } else {
    // All clients were notified successfully.
    completer.ReplySuccess();
  }
}

zx_status_t Device::SendCriticalErrorEvent(fuchsia_wlan_phyimpl::CriticalErrorReason reason) {
  fuchsia_wlan_device::wire::CriticalErrorReason reason_code;
  switch (reason) {
    case fuchsia_wlan_phyimpl::CriticalErrorReason::kFwCrash:
      reason_code = fuchsia_wlan_device::CriticalErrorReason::kFwCrash;
      break;
    default:
      lerror("unknown reason code in OnCriticalError: %d", reason);
      return ZX_ERR_INVALID_ARGS;
  }

  if (!phy_servers_.size()) {
    lerror("Cannot forward critical error event phy server binding not set");
    return ZX_ERR_SHOULD_WAIT;
  }
  zx_status_t status = ZX_OK;

  phy_servers_.ForEachBinding([&](const fidl::ServerBinding<fuchsia_wlan_device::Phy>& binding) {
    auto result = fidl::WireSendEvent(binding)->OnCriticalError(reason_code);
    if (!result.ok()) {
      lerror("Failed to send critical error event: %s", result.status_string());
      status = result.status();
    } else {
      ldebug_device("critical event forwarded to wlanphy client successfully");
    }
  });
  return status;
}

void Device::SetTxPowerScenario(SetTxPowerScenarioRequestView request,
                                SetTxPowerScenarioCompleter::Sync& completer) {
  fdf::Arena arena(0u);

  auto builder = fuchsia_wlan_phyimpl::wire::WlanPhyImplSetTxPowerScenarioRequest::Builder(arena);
  auto scenario = ConvertPowerScenario(request->scenario);
  if (!scenario.has_value()) {
    lerror("Invalid TX power scenario %u", request->scenario);
    completer.ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }
  builder.scenario(scenario.value());
  client_.buffer(arena)
      ->SetTxPowerScenario(builder.Build())
      .ThenExactlyOnce(
          [completer = completer.ToAsync()](
              fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::SetTxPowerScenario>&
                  result) mutable {
            if (!result.ok()) {
              lerror("SetTxPowerScenario failed with FIDL error %s",
                     result.FormatDescription().c_str());
              completer.ReplyError(result.status());
              return;
            }
            if (result->is_error()) {
              lerror("SetTxPowerScenario failed with error %s",
                     zx_status_get_string(result->error_value()));
              completer.ReplyError(result->error_value());
              return;
            }
            completer.ReplySuccess();
          });
}

void Device::ResetTxPowerScenario(ResetTxPowerScenarioCompleter::Sync& completer) {
  fdf::Arena arena(0u);

  client_.buffer(arena)->ResetTxPowerScenario().ThenExactlyOnce(
      [completer = completer.ToAsync()](
          fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::ResetTxPowerScenario>&
              result) mutable {
        if (!result.ok()) {
          lerror("ResetTxPowerScenario failed with a FIDL error %s",
                 result.FormatDescription().c_str());
          completer.ReplyError(result.status());
          return;
        }
        if (result->is_error()) {
          lerror("ResetTxPowerScenario failed with error %s",
                 zx_status_get_string(result->error_value()));
          completer.ReplyError(result->error_value());
          return;
        }
        completer.ReplySuccess();
      });
}

void Device::GetTxPowerScenario(GetTxPowerScenarioCompleter::Sync& completer) {
  fdf::Arena arena(0u);

  client_.buffer(arena)->GetTxPowerScenario().ThenExactlyOnce(
      [completer = completer.ToAsync()](
          fdf::WireUnownedResult<fuchsia_wlan_phyimpl::WlanPhyImpl::GetTxPowerScenario>&
              result) mutable {
        if (!result.ok()) {
          lerror("GetTxPowerScenario failed with a FIDL error %s",
                 result.FormatDescription().c_str());
          completer.ReplyError(result.status());
          return;
        }
        if (result->is_error()) {
          lerror("GetTxPowerScenario failed with error %s",
                 zx_status_get_string(result->error_value()));
          completer.ReplyError(result->error_value());
          return;
        }
        auto scenario = ConvertPowerScenario(result.value()->scenario);
        if (!scenario.has_value()) {
          lerror("GetTxPowerScenario encountered invalid scenario %u", result.value()->scenario);
          completer.ReplyError(ZX_ERR_INTERNAL);
          return;
        }
        completer.ReplySuccess(scenario.value());
      });
}

}  // namespace wlanphy
FUCHSIA_DRIVER_EXPORT2(::wlanphy::Device);
