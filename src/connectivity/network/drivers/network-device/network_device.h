// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_NETWORK_DEVICE_H_
#define SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_NETWORK_DEVICE_H_

#include <fidl/fuchsia.hardware.network/cpp/wire.h>
#include <fuchsia/hardware/network/driver/cpp/banjo.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/ddk/driver.h>
#include <lib/sync/cpp/completion.h>

#include <memory>

#include <ddktl/device.h>
#include <ddktl/fidl.h>
#include <ddktl/protocol/empty-protocol.h>

#include "device/public/network_device.h"

namespace network {

class NetworkDevice;
using DeviceType =
    ddk::Device<NetworkDevice, ddk::Messageable<fuchsia_hardware_network::DeviceInstance>::Mixin,
                ddk::Unbindable>;

// Creates `fuchsia_hardware_network_driver::NetworkDeviceImpl` endpoints for a
// parent device that is backed by the FIDL based driver runtime.
class FidlNetworkDeviceImplFactory : public NetworkDeviceImplBinder {
 public:
  explicit FidlNetworkDeviceImplFactory(NetworkDevice* parent) : parent_(parent) {}

  zx::result<fdf::ClientEnd<fuchsia_hardware_network_driver::NetworkDeviceImpl>> Bind() override;

 private:
  NetworkDevice* parent_;
};

class NetworkDevice : public DeviceType,
                      public ddk::EmptyProtocol<ZX_PROTOCOL_NETWORK_DEVICE>,
                      NetworkDeviceInterface::Sys {
 public:
  explicit NetworkDevice(zx_device_t* parent) : DeviceType(parent) {}
  ~NetworkDevice() override;

  static zx_status_t Create(void* ctx, zx_device_t* parent);

  void NotifyThread(zx::unowned_thread thread, ThreadType type) override;

  void DdkUnbind(ddk::UnbindTxn unbindTxn);

  void DdkRelease();

  void GetDevice(GetDeviceRequestView request, GetDeviceCompleter::Sync& _completer) override;

  NetworkDeviceInterface* GetInterface() { return device_.get(); }

 private:
  fdf::Dispatcher impl_dispatcher_;
  libsync::Completion impl_dispatcher_shutdown_;
  fdf::Dispatcher ifc_dispatcher_;
  libsync::Completion ifc_dispatcher_shutdown_;
  fdf::Dispatcher port_dispatcher_;
  libsync::Completion port_dispatcher_shutdown_;
  fdf::Dispatcher shim_dispatcher_;
  libsync::Completion shim_dispatcher_shutdown_;
  fdf::Dispatcher shim_port_dispatcher_;
  libsync::Completion shim_port_dispatcher_shutdown_;

  std::unique_ptr<NetworkDeviceInterface> device_;
};

}  // namespace network

#endif  // SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_NETWORK_DEVICE_H_
