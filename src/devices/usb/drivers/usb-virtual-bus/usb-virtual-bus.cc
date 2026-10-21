// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-virtual-bus/usb-virtual-bus.h"

#include <assert.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/logging/cpp/logger.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/test/platform/cpp/bind.h>
#include <fbl/alloc_checker.h>
#include <fbl/auto_lock.h>
#include <usb/usb.h>

namespace usb_virtual_bus {

namespace fvirt = fuchsia_hardware_usb_virtual_bus;
namespace fhci = fuchsia_hardware_usb_hci;
namespace fdci = fuchsia_hardware_usb_dci;
namespace fusb = fuchsia_hardware_usb_descriptor;
namespace fdfw = fuchsia_driver_framework;
namespace fdfs = fuchsia_device_fs;

const uint32_t kDeviceSlotId = 0;
const uint32_t kDeviceHubId = 0;
const fusb::UsbSpeed kDeviceSpeed = fusb::UsbSpeed::kHigh;

zx::result<std::unique_ptr<UsbVirtualHost>> UsbVirtualBus::CreateHost() {
  fbl::AllocChecker ac;
  auto child = fbl::make_unique_checked<UsbVirtualHost>(&ac, this);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  {
    zx::result result =
        outgoing()->AddService<UsbVirtualHost::Service>(child->GetInstanceHandler());
    if (result.is_error()) {
      fdf::error("Failed to add service {}", result);
      return result.take_error();
    }
  }

  {
    zx::result result = child->compat_server().Initialize(
        incoming(), outgoing(), node_name(), UsbVirtualHost::kName.c_str(),
        compat::ForwardMetadata::None(), child->GetBanjoConfig());
    if (result.is_error()) {
      return result.take_error();
    }
  }

  std::vector offers = child->compat_server().CreateOffers2();
  offers.push_back(fdf::MakeOffer2<UsbVirtualHost::Service>());
  auto properties = UsbVirtualHost::GetProperties();
  properties.push_back(fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_VID,
                                          bind_fuchsia_test_platform::BIND_PLATFORM_DEV_VID_TEST));
  properties.push_back(fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_PID,
                                          bind_fuchsia_test_platform::BIND_PLATFORM_DEV_PID_USB));

  {
    zx::result result = AddChild(UsbVirtualHost::kName.c_str(), properties, offers);
    if (result.is_error()) {
      fdf::error("Failed to add child: {}", result);
      return result.take_error();
    }
    child->controller().Bind(std::move(*result));
  }
  return zx::ok(std::move(child));
}

zx::result<std::unique_ptr<UsbVirtualDevice>> UsbVirtualBus::CreateDevice() {
  fbl::AllocChecker ac;
  auto child = fbl::make_unique_checked<UsbVirtualDevice>(&ac, this);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  {
    zx::result result =
        outgoing()->AddService<UsbVirtualDevice::Service>(child->GetInstanceHandler());
    if (result.is_error()) {
      fdf::error("Failed to add service {}", result);
      return result.take_error();
    }
  }

  std::vector<fdfw::Offer> offers;
  offers.push_back(fdf::MakeOffer2<UsbVirtualDevice::Service>());
  auto properties = UsbVirtualDevice::GetProperties();
  properties.push_back(fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_VID,
                                          bind_fuchsia_test_platform::BIND_PLATFORM_DEV_VID_TEST));
  properties.push_back(fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_PID,
                                          bind_fuchsia_test_platform::BIND_PLATFORM_DEV_PID_USB));

  {
    zx::result result = AddChild(UsbVirtualDevice::kName.c_str(), properties, offers);
    if (result.is_error()) {
      fdf::error("Failed to add child: {}", result);
      return result.take_error();
    }
    child->controller().Bind(std::move(*result));
  }
  return zx::ok(std::move(child));
}

template <typename T>
zx::result<> UsbVirtualBus::RemoveChild(std::unique_ptr<T>& child) {
  if (!child) {
    return zx::ok();
  }

  if (child->controller()) {
    auto result = child->controller()->Remove();
    if (!result.ok()) {
      fdf::error("Failed to remove child: {}", result.FormatDescription().c_str());
      return zx::error(result.status());
    }
    removed_ = true;
  }
  return zx::ok();
}

void UsbVirtualBus::Serve(fidl::ServerEnd<fvirt::Bus> request) {
  bindings_.AddBinding(dispatcher(), std::move(request), this, fidl::kIgnoreBindingClosure);
}

void UsbVirtualBus::SetConnectedState(ConnectedState state) {
  if (connected_ == state) {
    return;
  }
  fdf::debug("State transition: {} -> {}", connected_, state);
  connected_ = state;
}

zx::result<> UsbVirtualBus::Start(fdf::DriverContext context) {
  incoming_ = std::shared_ptr<fdf::Namespace>(context.take_incoming());
  node_name_ = context.node_name().value_or("");
  for (uint8_t i = 0; i < USB_MAX_EPS; i++) {
    eps_[i].Init(this, i);
  }

  zx::result connector = devfs_connector_.Bind(dispatcher());
  if (connector.is_error()) {
    fdf::error("Error creating devfs node");
    return connector.take_error();
  }

  fdfw::DevfsAddArgs devfs_args{{
      .connector = std::move(connector.value()),
      .class_name = "usb-virtual-bus",
      .connector_supports = fdfs::ConnectionType::kController,
  }};

  zx::result child = AddOwnedChild(kName, devfs_args);
  if (child.is_error()) {
    fdf::error("Failed to add child {}", child);
    return child.take_error();
  }
  child_ = std::move(*child);
  return zx::ok();
}

void UsbVirtualBus::OnStartDci(fit::callback<void(zx_status_t)> callback) {
  // Trigger the connection sequence in the background.
  // We don't wait for it to complete before acknowledging the DCI start to avoid deadlocks.
  ConnectInternal([](zx_status_t status) {});
  if (callback) {
    callback(ZX_OK);
  }
}

void UsbVirtualBus::OnStopDci(fit::callback<void(zx_status_t)> callback) {
  // Trigger the disconnection sequence in the background.
  DisconnectInternal([](zx_status_t status) {});
  if (callback) {
    callback(ZX_OK);
  }
}

void UsbVirtualBus::ConnectInternal(fit::callback<void(zx_status_t)> callback) {
  if (!disable_callbacks_.empty()) {
    if (callback) {
      callback(ZX_ERR_BAD_STATE);
    }
    return;
  }

  if (connected_ == ConnectedState::kConnected) {
    if (callback) {
      callback(ZX_OK);
    }
    return;
  }

  if (connected_ != ConnectedState::kDisconnected && connected_ != ConnectedState::kConnecting) {
    fdf::error("ConnectInternal: failing with BAD_STATE, current state={}", connected_);
    if (callback) {
      callback(ZX_ERR_BAD_STATE);
    }
    return;
  }

  if (!pending_ops_.empty() && pending_ops_.back().type == OpType::kConnect) {
    if (callback) {
      pending_ops_.back().callbacks.push_back(std::move(callback));
    }
    return;
  }

  PendingOp op{.type = OpType::kConnect, .callbacks = {}};
  if (callback) {
    op.callbacks.push_back(std::move(callback));
  }
  pending_ops_.push_back(std::move(op));

  if (pending_ops_.size() == 1) {
    StartNextPendingOp();
  }
}

void UsbVirtualBus::DisconnectInternal(fit::callback<void(zx_status_t)> callback) {
  if (pending_ops_.empty()) {
    if (connected_ == ConnectedState::kDisconnected) {
      if (callback) {
        callback(ZX_OK);
      }
      return;
    }
    if (connected_ != ConnectedState::kConnected) {
      if (callback) {
        callback(ZX_ERR_BAD_STATE);
      }
      return;
    }
  }

  if (!pending_ops_.empty() && pending_ops_.back().type == OpType::kDisconnect) {
    if (callback) {
      pending_ops_.back().callbacks.push_back(std::move(callback));
    }
    return;
  }

  PendingOp op{.type = OpType::kDisconnect, .callbacks = {}};
  if (callback) {
    op.callbacks.push_back(std::move(callback));
  }
  pending_ops_.push_back(std::move(op));

  if (pending_ops_.size() == 1) {
    StartNextPendingOp();
  }
}

void UsbVirtualBus::StartNextPendingOp() {
  if (connected_ == ConnectedState::kConnecting || connected_ == ConnectedState::kDisconnecting) {
    return;
  }

  while (!pending_ops_.empty()) {
    if (pending_ops_.front().type == OpType::kConnect) {
      if (!disable_callbacks_.empty()) {
        auto op = std::move(pending_ops_.front());
        pending_ops_.pop_front();
        for (auto& cb : op.callbacks) {
          cb(ZX_ERR_BAD_STATE);
        }
        continue;
      }
      if (connected_ == ConnectedState::kConnected) {
        auto op = std::move(pending_ops_.front());
        pending_ops_.pop_front();
        for (auto& cb : op.callbacks) {
          cb(ZX_OK);
        }
        continue;
      }
      SetConnectedState(ConnectedState::kConnecting);
      // We connect phases sequentially: DCI first, then HCI.
      // This ensures the device is ready to handle requests before the host sees it.
      DciSetConnected();
      return;
    }

    if (connected_ == ConnectedState::kDisconnected) {
      auto op = std::move(pending_ops_.front());
      pending_ops_.pop_front();
      for (auto& cb : op.callbacks) {
        cb(ZX_OK);
      }
      continue;
    }
    SetConnectedState(ConnectedState::kDisconnecting);
    for (auto& ep : eps_) {
      ep.host_.CommonCancelAll();
      ep.device_.CommonCancelAll();
    }
    HciSetDisconnected();
    return;
  }
}

void UsbVirtualBus::CompleteCurrentPendingOp(OpType expected_type, zx_status_t status) {
  std::vector<fit::callback<void(zx_status_t)>> callbacks;
  if (!pending_ops_.empty() && pending_ops_.front().type == expected_type) {
    callbacks = std::move(pending_ops_.front().callbacks);
    pending_ops_.pop_front();
  } else {
    fdf::error("CompleteCurrentPendingOp: unexpected front op (empty={}, expected={})",
               pending_ops_.empty(), static_cast<uint8_t>(expected_type));
    ZX_DEBUG_ASSERT_MSG(false, "CompleteCurrentPendingOp state desynchronization");
  }
  for (auto& cb : callbacks) {
    cb(status);
  }
  StartNextPendingOp();
}

void UsbVirtualBus::HciSetDisconnected() {
  if (!hci_intf_.is_valid() || !hci_connected_) {
    OnHciDisconnectCompleted(ZX_OK);
    return;
  }
  hci_intf_->RemoveDevice({kDeviceSlotId})
      .Then([this](fidl::Result<fhci::UsbHciInterface::RemoveDevice>& result) mutable {
        zx_status_t status = ZX_OK;
        if (result.is_error()) {
          status = result.error_value().is_domain_error()
                       ? ZX_ERR_INTERNAL
                       : result.error_value().framework_error().status();
        }
        OnHciDisconnectCompleted(status);
      });
}

void UsbVirtualBus::OnHciDisconnectCompleted(zx_status_t status) {
  if (status != ZX_OK) {
    fdf::warn("Failed to remove HCI device: {}", zx_status_get_string(status));
  }
  hci_connected_ = false;
  DciSetDisconnected(status);
}

void UsbVirtualBus::DciSetDisconnected(zx_status_t previous_status) {
  if (!dci_intf_.is_valid() || !dci_connected_) {
    OnDciDisconnectCompleted(previous_status);
    return;
  }
  dci_intf_->SetConnected(false).Then(
      [this, previous_status](fidl::Result<fdci::UsbDciInterface::SetConnected>& result) mutable {
        zx_status_t status = previous_status;
        if (result.is_error()) {
          zx_status_t dci_status = result.error_value().is_domain_error()
                                       ? ZX_ERR_INTERNAL
                                       : result.error_value().framework_error().status();
          if (status == ZX_OK) {
            status = dci_status;
          }
        }
        OnDciDisconnectCompleted(status);
      });
}

void UsbVirtualBus::OnDciDisconnectCompleted(zx_status_t status) {
  if (status != ZX_OK) {
    fdf::warn("Failed to disconnect DCI: {}", zx_status_get_string(status));
  }
  dci_connected_ = false;

  SetConnectedState(ConnectedState::kDisconnected);
  CompleteCurrentPendingOp(OpType::kDisconnect, status);
}

void UsbVirtualBus::DciSetConnected() {
  if (!dci_intf_.is_valid()) {
    SetConnectedState(ConnectedState::kDisconnected);
    CompleteCurrentPendingOp(OpType::kConnect, ZX_ERR_BAD_STATE);
    return;
  }
  if (dci_connected_) {
    OnDciConnectCompleted(ZX_OK);
    return;
  }

  // Set the flag early to prevent multiple SetConnected calls if we're called concurrently.
  // If it fails, we'll reset it in the error block below or in OnDciConnectCompleted.
  dci_connected_ = true;

  dci_intf_->SetConnected(true).Then(
      [this](fidl::Result<fdci::UsbDciInterface::SetConnected>& result) mutable {
        if (result.is_error()) {
          fdf::error("Failed to set DCI connected: {}",
                     result.error_value().FormatDescription().c_str());
          zx_status_t status = result.error_value().is_domain_error()
                                   ? ZX_ERR_INTERNAL
                                   : result.error_value().framework_error().status();
          OnDciConnectCompleted(status);
        } else {
          OnDciConnectCompleted(ZX_OK);
        }
      });
}

void UsbVirtualBus::OnDciConnectCompleted(zx_status_t status) {
  fdf::debug(
      "OnDciConnectCompleted: status={}, connected_={}, dci_connected_={}, hci_connected_={}",
      zx_status_get_string(status), connected_, dci_connected_, hci_connected_);

  if (status != ZX_OK) {
    SetConnectedState(ConnectedState::kDisconnected);
    dci_connected_ = false;
    CompleteCurrentPendingOp(OpType::kConnect, status);
    return;
  }

  dci_connected_ = true;

  if (connected_ != ConnectedState::kConnecting) {
    CompleteCurrentPendingOp(OpType::kConnect, ZX_OK);
    return;
  }
  if (hci_connected_) {
    OnHciConnectCompleted(ZX_OK);
    return;
  }

  HciSetConnected();
}

void UsbVirtualBus::HciSetConnected() {
  if (!hci_intf_.is_valid() || hci_connected_) {
    OnHciConnectCompleted(ZX_OK);
    return;
  }

  // Set the flag early to prevent multiple AddDevice calls if we're called concurrently.
  // If it fails, OnHciConnectCompleted will reset it.
  hci_connected_ = true;

  hci_intf_->AddDevice({kDeviceSlotId, kDeviceHubId, kDeviceSpeed})
      .Then([this](fidl::Result<fhci::UsbHciInterface::AddDevice>& result) mutable {
        zx_status_t status = ZX_OK;
        if (result.is_error()) {
          status = result.error_value().is_domain_error()
                       ? ZX_ERR_INTERNAL
                       : result.error_value().framework_error().status();
        }
        OnHciConnectCompleted(status);
      });
}

void UsbVirtualBus::OnHciConnectCompleted(zx_status_t status) {
  if (status == ZX_OK) {
    fdf::debug("HCI device added successfully");
    hci_connected_ = true;
    SetConnectedState(ConnectedState::kConnected);
  } else {
    fdf::warn("Failed to add HCI device: {}", zx_status_get_string(status));
    hci_connected_ = false;
    SetConnectedState(ConnectedState::kDisconnected);
  }

  CompleteCurrentPendingOp(OpType::kConnect, status);
}

void UsbVirtualBus::Stop(fdf::StopCompleter completer) {
  Disable([completer = std::move(completer)](zx_status_t status) mutable {
    if (status != ZX_OK) {
      completer(zx::error(status));
      return;
    }
    completer(zx::ok());
  });
}

zx::result<> UsbVirtualBus::SetBusInterface(fidl::ClientEnd<fhci::UsbHciInterface> client_end) {
  if (hci_intf_.is_valid()) {
    return zx::error(ZX_ERR_ALREADY_BOUND);
  }

  hci_intf_.Bind(std::move(client_end), dispatcher());

  if (connected_ == ConnectedState::kConnected) {
    hci_intf_->AddDevice({kDeviceSlotId, kDeviceHubId, kDeviceSpeed})
        .Then([](fidl::Result<fhci::UsbHciInterface::AddDevice>& result) {
          if (result.is_error()) {
            fdf::error("Failed to add device");
          }
        });
  }

  return zx::ok();
}

zx::result<> UsbVirtualBus::SetDciInterface(fidl::ClientEnd<fdci::UsbDciInterface> client_end) {
  if (dci_intf_.is_valid()) {
    return zx::error(ZX_ERR_ALREADY_BOUND);
  }

  dci_intf_.Bind(std::move(client_end), dispatcher());
  return zx::ok();
}

void UsbVirtualBus::Enable(EnableCompleter::Sync& completer) {
  if (host_ == nullptr) {
    zx::result result = CreateHost();
    if (result.is_error()) {
      fdf::error("Failed to create host {}", result);
      completer.Reply(result.error_value());
      return;
    }
    host_ = std::move(*result);
  }
  if (device_ == nullptr) {
    zx::result result = CreateDevice();
    if (result.is_error()) {
      fdf::error("Failed to create device {}", result);
      completer.Reply(result.error_value());
      return;
    }
    device_ = std::move(*result);
  }

  completer.Reply(ZX_OK);
}

void UsbVirtualBus::Disable(DisableCompleter::Sync& completer) {
  Disable(
      [completer = completer.ToAsync()](zx_status_t status) mutable { completer.Reply(status); });
}

void UsbVirtualBus::Disable(fit::callback<void(zx_status_t)> callback) {
  std::vector<fit::callback<void(zx_status_t)>> cancelled_connect_callbacks;
  for (auto it = pending_ops_.begin(); it != pending_ops_.end();) {
    if (it->type == OpType::kConnect) {
      for (auto& cb : it->callbacks) {
        cancelled_connect_callbacks.push_back(std::move(cb));
      }
      if (it == pending_ops_.begin()) {
        // Keep the in-flight front entry so OnDciConnectCompleted / OnHciConnectCompleted
        // can pop it cleanly when it finishes, then transition into the queued kDisconnect.
        it->callbacks.clear();
        ++it;
      } else {
        it = pending_ops_.erase(it);
      }
    } else {
      ++it;
    }
  }
  // Coalesce adjacent kDisconnect entries if erasing a middle kConnect brought two together.
  for (auto it = pending_ops_.begin(); it != pending_ops_.end();) {
    auto next = std::next(it);
    if (next != pending_ops_.end() && it->type == next->type) {
      for (auto& cb : next->callbacks) {
        it->callbacks.push_back(std::move(cb));
      }
      pending_ops_.erase(next);
    } else {
      ++it;
    }
  }
  for (auto& cb : cancelled_connect_callbacks) {
    cb(ZX_ERR_BAD_STATE);
  }

  if (callback) {
    disable_callbacks_.push_back(std::move(callback));
  }

  if (disable_callbacks_.size() > 1) {
    // Already in progress. The completion of the first request will notify us.
    return;
  }

  // We want to perform hardware cleanup after the bus has fully disconnected.
  DisconnectInternal([this](zx_status_t status) {
    for (auto& ep : eps_) {
      ep.host_.CommonCancelAll();
      ep.device_.CommonCancelAll();
    }
    hci_intf_ = {};
    dci_intf_ = {};
    zx_status_t final_status = status;

    if (host_) {
      zx::result host_result = RemoveChild(host_);
      if (host_result.is_error()) {
        fdf::error("Failed to remove host {}", host_result.status_string());
        if (final_status == ZX_OK) {
          final_status = host_result.error_value();
        }
      }
      host_ = nullptr;
    }

    if (device_) {
      zx::result device_result = RemoveChild(device_);
      if (device_result.is_error()) {
        fdf::error("Failed to remove device {}", device_result.status_string());
        if (final_status == ZX_OK) {
          final_status = device_result.error_value();
        }
      }
      device_ = nullptr;
    }

    auto callbacks = std::move(disable_callbacks_);
    for (auto& cb : callbacks) {
      cb(final_status);
    }
  });
}

void UsbVirtualBus::Connect(ConnectCompleter::Sync& completer) {
  if (!host_ || !device_) {
    completer.Reply(ZX_ERR_BAD_STATE);
    return;
  }

  ConnectInternal(
      [completer = completer.ToAsync()](zx_status_t status) mutable { completer.Reply(status); });
}

void UsbVirtualBus::Disconnect(DisconnectCompleter::Sync& completer) {
  if (!host_ || !device_ || connected_ == ConnectedState::kConnecting) {
    completer.Reply(ZX_ERR_BAD_STATE);
    return;
  }

  DisconnectInternal(
      [completer = completer.ToAsync()](zx_status_t status) mutable { completer.Reply(status); });
}

}  // namespace usb_virtual_bus

FUCHSIA_DRIVER_EXPORT2(usb_virtual_bus::UsbVirtualBus);
