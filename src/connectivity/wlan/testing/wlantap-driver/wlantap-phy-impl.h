// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_WLAN_TESTING_WLANTAP_DRIVER_WLANTAP_PHY_IMPL_H_
#define SRC_CONNECTIVITY_WLAN_TESTING_WLANTAP_DRIVER_WLANTAP_PHY_IMPL_H_

#include <fidl/fuchsia.wlan.phyimpl/cpp/driver/fidl.h>
#include <lib/zx/channel.h>
#include <lib/zx/result.h>

#include <variant>

#include "wlantap-driver-context.h"
#include "wlantap-mac.h"
#include "wlantap-phy.h"

namespace wlan {

// Serves the WlanPhyImpl protocol. This also creates an instance of WlantapPhy, which lets the test
// suite control the state of the mock driver.
class WlanPhyImplDevice : public fdf::Server<fuchsia_wlan_phyimpl::WlanPhyImpl>,
                          public std::enable_shared_from_this<WlanPhyImplDevice> {
  using NodeControllerClient = fidl::ClientEnd<fuchsia_driver_framework::NodeController>;

 public:
  WlanPhyImplDevice() = delete;

  // Allocates a WlanPhyImplDevice into a std::shared_ptr so that WlanPhyImplDevice
  // in its implementation can create additional references to the std::shared_ptr
  // for use by WlantapPhy and shutdown callbacks.
  static std::shared_ptr<WlanPhyImplDevice> New(
      WlantapDriverContext context, zx::channel user_channel,
      const fuchsia_wlan_tap::WlantapPhyConfig& phy_config, NodeControllerClient phy_controller);

  // WlanPhyImpl protocol implementation
  void Init(InitRequest& request, InitCompleter::Sync& completer) override;
  void GetSupportedMacRoles(GetSupportedMacRolesCompleter::Sync& completer) override;
  void CreateIface(CreateIfaceRequest& request, CreateIfaceCompleter::Sync& completer) override;
  void DestroyIface(DestroyIfaceRequest& request, DestroyIfaceCompleter::Sync& completer) override;
  void SetCountry(SetCountryRequest& request, SetCountryCompleter::Sync& completer) override;
  void ClearCountry(ClearCountryCompleter::Sync& completer) override;
  void GetCountry(GetCountryCompleter::Sync& completer) override;
  void SetPowerSaveMode(SetPowerSaveModeRequest& request,
                        SetPowerSaveModeCompleter::Sync& completer) override;
  void GetPowerSaveMode(GetPowerSaveModeCompleter::Sync& completer) override;
  void PowerDown(PowerDownCompleter::Sync& completer) override;
  void PowerUp(PowerUpCompleter::Sync& completer) override;
  void Reset(ResetCompleter::Sync& completer) override;
  void GetPowerState(GetPowerStateCompleter::Sync& completer) override;
  void SetBtCoexistenceMode(SetBtCoexistenceModeRequest& request,
                            SetBtCoexistenceModeCompleter::Sync& completer) override;
  void SetTxPowerScenario(SetTxPowerScenarioRequest& request,
                          SetTxPowerScenarioCompleter::Sync& completer) override;
  void ResetTxPowerScenario(ResetTxPowerScenarioCompleter::Sync& completer) override;
  void GetTxPowerScenario(GetTxPowerScenarioCompleter::Sync& completer) override;
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_wlan_phyimpl::WlanPhyImpl> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  WlanPhyImplDevice(WlantapDriverContext context,
                    const fuchsia_wlan_tap::WlantapPhyConfig& phy_config);
  void Init(zx::channel user_channel, NodeControllerClient phy_controller);

  zx_status_t CreateWlanSoftmac(fuchsia_wlan_common::WlanMacRole role,
                                zx::channel mlme_channel) __TA_NO_THREAD_SAFETY_ANALYSIS;
  zx_status_t AddWlanSoftmacChild(std::string_view name,
                                  fidl::ServerEnd<fuchsia_driver_framework::NodeController> server);
  zx::result<std::unique_ptr<WlantapMac>> ServeWlanSoftmac(std::string_view name,
                                                           fuchsia_wlan_common::WlanMacRole role,
                                                           zx::channel mlme_channel);

  void ShutdownComplete();

  struct SlotEmpty {};
  struct SlotCreating {};
  struct SlotActive {
    std::unique_ptr<WlantapMac> mac;
    fidl::Client<fuchsia_driver_framework::NodeController> controller;
  };
  struct SlotDestroying {
    std::unique_ptr<WlantapMac> mac;
    fidl::Client<fuchsia_driver_framework::NodeController> controller;
  };
  using IfaceSlot = std::variant<SlotEmpty, SlotCreating, SlotActive, SlotDestroying>;

  WlantapDriverContext driver_context_;

  const fuchsia_wlan_tap::WlantapPhyConfig phy_config_{};

  std::string name_{"wlanphyimpl"};

  // Initialize in Init() with a shared_ptr to this instance.
  std::unique_ptr<WlantapPhy> wlantap_phy_ = nullptr;

  fidl::Client<fuchsia_driver_framework::NodeController> phy_controller_;

  IfaceSlot iface_slot_{SlotEmpty{}};

  std::optional<WlantapPhy::ShutdownCompleter::Async> wlantap_phy_shutdown_completer_;
};

}  // namespace wlan

#endif  // SRC_CONNECTIVITY_WLAN_TESTING_WLANTAP_DRIVER_WLANTAP_PHY_IMPL_H_
