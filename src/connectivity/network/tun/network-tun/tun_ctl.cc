// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "tun_ctl.h"

#include <lib/async/cpp/task.h>
#include <lib/fit/defer.h>
#include <lib/syslog/global.h>
#include <zircon/status.h>

#include "tun_device.h"

namespace network {
namespace tun {

zx::result<std::unique_ptr<TunCtl>> TunCtl::Create(async_dispatcher_t* fidl_dispatcher) {
  std::unique_ptr<TunCtl> tun_ctl(new TunCtl(fidl_dispatcher));
  auto impl_dispatcher = fdf::UnsynchronizedDispatcher::Create(
      {}, "tun-ctl-impl", [tun_ctl = tun_ctl.get()](fdf_dispatcher_t*) {
        tun_ctl->impl_dispatcher_shutdown_.Signal();
      });
  if (impl_dispatcher.is_error()) {
    FX_LOGF(ERROR, "tun", "TunCtl::Create failed to create dispatcher: %s",
            impl_dispatcher.status_string());
    return impl_dispatcher.take_error();
  }
  tun_ctl->impl_dispatcher_ = std::move(impl_dispatcher.value());

  auto ifc_dispatcher = fdf::UnsynchronizedDispatcher::Create(
      {}, "tun-ctl-ifc",
      [tun_ctl = tun_ctl.get()](fdf_dispatcher_t*) { tun_ctl->ifc_dispatcher_shutdown_.Signal(); });
  if (ifc_dispatcher.is_error()) {
    FX_LOGF(ERROR, "tun", "TunCtl::Create failed to create dispatcher: %s",
            ifc_dispatcher.status_string());
    return ifc_dispatcher.take_error();
  }
  tun_ctl->ifc_dispatcher_ = std::move(ifc_dispatcher.value());

  auto port_dispatcher = fdf::UnsynchronizedDispatcher::Create(
      {}, "tun-ctl-port", [tun_ctl = tun_ctl.get()](fdf_dispatcher_t*) {
        tun_ctl->port_dispatcher_shutdown_.Signal();
      });
  if (port_dispatcher.is_error()) {
    FX_LOGF(ERROR, "tun", "TunCtl::Create failed to create dispatcher: %s",
            port_dispatcher.status_string());
    return port_dispatcher.take_error();
  }
  tun_ctl->port_dispatcher_ = std::move(port_dispatcher.value());

  auto shim_dispatcher = fdf::SynchronizedDispatcher::Create(
      fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "tun-ctl-shim",
      [tun_ctl = tun_ctl.get()](fdf_dispatcher_t*) {
        tun_ctl->shim_dispatcher_shutdown_.Signal();
      });
  if (shim_dispatcher.is_error()) {
    FX_LOGF(ERROR, "tun", "TunCtl::Create failed to create dispatcher: %s",
            shim_dispatcher.status_string());
    return shim_dispatcher.take_error();
  }
  tun_ctl->shim_dispatcher_ = std::move(shim_dispatcher.value());

  auto shim_port_dispatcher = fdf::SynchronizedDispatcher::Create(
      fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "tun-ctl-shim-port",
      [tun_ctl = tun_ctl.get()](fdf_dispatcher_t*) {
        tun_ctl->shim_port_dispatcher_shutdown_.Signal();
      });
  if (shim_port_dispatcher.is_error()) {
    FX_LOGF(ERROR, "tun", "TunCtl::Create failed to create dispatcher: %s",
            shim_port_dispatcher.status_string());
    return shim_port_dispatcher.take_error();
  }
  tun_ctl->shim_port_dispatcher_ = std::move(shim_port_dispatcher.value());

  return zx::ok(std::move(tun_ctl));
}

TunCtl::~TunCtl() {
  if (impl_dispatcher_.get()) {
    impl_dispatcher_.ShutdownAsync();
    impl_dispatcher_shutdown_.Wait();
  }
  if (ifc_dispatcher_.get()) {
    ifc_dispatcher_.ShutdownAsync();
    ifc_dispatcher_shutdown_.Wait();
  }
  if (port_dispatcher_.get()) {
    port_dispatcher_.ShutdownAsync();
    port_dispatcher_shutdown_.Wait();
  }
  if (shim_dispatcher_.get()) {
    shim_dispatcher_.ShutdownAsync();
    shim_dispatcher_shutdown_.Wait();
  }
  if (shim_port_dispatcher_.get()) {
    shim_port_dispatcher_.ShutdownAsync();
    shim_port_dispatcher_shutdown_.Wait();
  }
}

void TunCtl::CreateDevice(CreateDeviceRequestView request, CreateDeviceCompleter::Sync& completer) {
  zx::result tun_device = TunDevice::Create(
      DeviceInterfaceDispatchers{&impl_dispatcher_, &ifc_dispatcher_, &port_dispatcher_},
      ShimDispatchers{&shim_dispatcher_, &shim_port_dispatcher_},
      [this](TunDevice* dev) {
        // If this is posted on fdf_dispatcher then there's a lockup because we're
        // then creating a double lock in DevicePort.
        async::PostTask(fidl_dispatcher_, [this, dev]() {
          devices_.erase(*dev);
          TryFireShutdownCallback();
        });
      },
      DeviceConfig(request->config));

  if (tun_device.is_error()) {
    FX_LOGF(ERROR, "tun", "TunCtl: TunDevice creation failed: %s", tun_device.status_string());
    request->device.Close(tun_device.error_value());
    return;
  }
  auto& value = tun_device.value();
  value->Bind(std::move(request->device));
  devices_.push_back(std::move(value));
  FX_LOG(INFO, "tun", "TunCtl: Created TunDevice");
}

void TunCtl::CreatePair(CreatePairRequestView request, CreatePairCompleter::Sync& completer) {
  zx::result tun_pair = TunPair::Create(
      DeviceInterfaceDispatchers{&impl_dispatcher_, &ifc_dispatcher_, &port_dispatcher_},
      ShimDispatchers{&shim_dispatcher_, &shim_port_dispatcher_}, fidl_dispatcher_,
      [this](TunPair* pair) {
        async::PostTask(fidl_dispatcher_, [this, pair]() {
          device_pairs_.erase(*pair);
          TryFireShutdownCallback();
        });
      },
      DevicePairConfig(request->config));

  if (tun_pair.is_error()) {
    FX_LOGF(ERROR, "tun", "TunCtl: TunPair creation failed: %s", tun_pair.status_string());
    request->device_pair.Close(tun_pair.status_value());
    return;
  }
  auto& value = tun_pair.value();
  value->Bind(std::move(request->device_pair));
  device_pairs_.push_back(std::move(value));
  FX_LOG(INFO, "tun", "TunCtl: Created TunPair");
}

void TunCtl::SetSafeShutdownCallback(fit::callback<void()> shutdown_callback) {
  async::PostTask(fidl_dispatcher_, [this, callback = std::move(shutdown_callback)]() mutable {
    ZX_ASSERT_MSG(!shutdown_callback_, "Shutdown callback already installed");
    shutdown_callback_ = std::move(callback);
    TryFireShutdownCallback();
  });
}

void TunCtl::TryFireShutdownCallback() {
  if (shutdown_callback_ && device_pairs_.is_empty() && devices_.is_empty()) {
    shutdown_callback_();
  }
}

}  // namespace tun
}  // namespace network
