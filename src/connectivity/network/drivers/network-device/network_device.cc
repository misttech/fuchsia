// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "network_device.h"

#include <lib/ddk/binding_driver.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/driver.h>

#include <ddktl/device.h>
#include <ddktl/fidl.h>
#include <fbl/alloc_checker.h>
#include <fbl/auto_lock.h>

#include "device/network_device_shim.h"

namespace network {
namespace {

// Creates a `NetworkDeviceImplFactory` based on the `parent` device type.
zx::result<std::unique_ptr<NetworkDeviceImplBinder>> CreateImplFactory(
    ddk::NetworkDeviceImplProtocolClient netdevice_impl, NetworkDevice* device,
    const ShimDispatchers& dispatchers) {
  fbl::AllocChecker ac;

  // If the `parent` is Banjo based, then we must use "shims" to translate
  // between Banjo and FIDL in order to leverage the netdevice core library.
  if (netdevice_impl.is_valid()) {
    auto shim = fbl::make_unique_checked<NetworkDeviceShim>(&ac, netdevice_impl, dispatchers);
    if (!ac.check()) {
      zxlogf(ERROR, "no memory");
      return zx::error(ZX_ERR_NO_MEMORY);
    }

    return zx::ok(std::move(shim));
  }

  // If the `parent` is FIDL based, then return a factory that connects to the
  // device with no extra translation layer.
  std::unique_ptr fidl = fbl::make_unique_checked<FidlNetworkDeviceImplFactory>(&ac, device);
  if (!ac.check()) {
    zxlogf(ERROR, "no memory");
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  return zx::ok(std::move(fidl));
}

}  // namespace

NetworkDevice::~NetworkDevice() {
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

zx_status_t NetworkDevice::Create(void* ctx, zx_device_t* parent) {
  fbl::AllocChecker ac;
  std::unique_ptr netdev = fbl::make_unique_checked<NetworkDevice>(&ac, parent);
  if (!ac.check()) {
    zxlogf(ERROR, "no memory");
    return ZX_ERR_NO_MEMORY;
  }
  auto impl_dispatcher = fdf::UnsynchronizedDispatcher::Create(
      {}, "netdevice-impl",
      [netdev = netdev.get()](fdf_dispatcher_t*) { netdev->impl_dispatcher_shutdown_.Signal(); });
  if (impl_dispatcher.is_error()) {
    zxlogf(ERROR, "failed to create impl dispatcher: %s", impl_dispatcher.status_string());
    return impl_dispatcher.status_value();
  }
  netdev->impl_dispatcher_ = std::move(impl_dispatcher.value());

  auto ifc_dispatcher = fdf::UnsynchronizedDispatcher::Create(
      {}, "netdevice-ifc",
      [netdev = netdev.get()](fdf_dispatcher_t*) { netdev->ifc_dispatcher_shutdown_.Signal(); });
  if (ifc_dispatcher.is_error()) {
    zxlogf(ERROR, "failed to create ifc dispatcher: %s", ifc_dispatcher.status_string());
    return ifc_dispatcher.status_value();
  }
  netdev->ifc_dispatcher_ = std::move(ifc_dispatcher.value());
  auto port_dispatcher = fdf::UnsynchronizedDispatcher::Create(
      {}, "netdevice-port",
      [netdev = netdev.get()](fdf_dispatcher_t*) { netdev->port_dispatcher_shutdown_.Signal(); });
  if (port_dispatcher.is_error()) {
    zxlogf(ERROR, "failed to create dispatcher: %s", port_dispatcher.status_string());
    return port_dispatcher.status_value();
  }
  netdev->port_dispatcher_ = std::move(port_dispatcher.value());

  ddk::NetworkDeviceImplProtocolClient netdevice_impl(parent);
  if (netdevice_impl.is_valid()) {
    // We only need these dispatchers for Banjo parents.
    auto shim_dispatcher = fdf::SynchronizedDispatcher::Create(
        fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "netdevice-shim",
        [netdev = netdev.get()](fdf_dispatcher_t*) { netdev->shim_dispatcher_shutdown_.Signal(); });
    if (shim_dispatcher.is_error()) {
      zxlogf(ERROR, "failed to create dispatcher: %s", shim_dispatcher.status_string());
      return shim_dispatcher.status_value();
    }
    netdev->shim_dispatcher_ = std::move(shim_dispatcher.value());

    auto shim_port_dispatcher = fdf::SynchronizedDispatcher::Create(
        fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "netdevice-shim-port",
        [netdev = netdev.get()](fdf_dispatcher_t*) {
          netdev->shim_port_dispatcher_shutdown_.Signal();
        });
    if (shim_port_dispatcher.is_error()) {
      zxlogf(ERROR, "failed to create dispatcher: %s", shim_port_dispatcher.status_string());
      return shim_port_dispatcher.status_value();
    }
    netdev->shim_port_dispatcher_ = std::move(shim_port_dispatcher.value());
  }

  zx::result<std::unique_ptr<NetworkDeviceImplBinder>> factory =
      CreateImplFactory(netdevice_impl, netdev.get(),
                        ShimDispatchers{&netdev->shim_dispatcher_, &netdev->shim_port_dispatcher_});
  if (factory.is_error()) {
    zxlogf(ERROR, "failed to create network device factory: %s", factory.status_string());
    return factory.status_value();
  }

  zx::result device = NetworkDeviceInterface::Create(
      DeviceInterfaceDispatchers{&netdev->impl_dispatcher_, &netdev->ifc_dispatcher_,
                                 &netdev->port_dispatcher_},
      std::move(factory.value()), netdev.get());

  if (device.is_error()) {
    zxlogf(ERROR, "failed to create inner device %s", device.status_string());
    return device.status_value();
  }
  netdev->device_ = std::move(device.value());

  if (zx_status_t status = netdev->DdkAdd(
          ddk::DeviceAddArgs("network-device").set_proto_id(ZX_PROTOCOL_NETWORK_DEVICE));
      status != ZX_OK) {
    zxlogf(ERROR, "failed to bind device: %s", zx_status_get_string(status));
    return status;
  }

  // On successful Add, Devmgr takes ownership (relinquished on DdkRelease),
  // so transfer our ownership to a local var, and let it go out of scope.
  [[maybe_unused]] auto temp_ref = netdev.release();
  return ZX_OK;
}

void NetworkDevice::DdkUnbind(ddk::UnbindTxn unbindTxn) {
  zxlogf(INFO, "%p DdkUnbind", zxdev());
  device_->Teardown([txn = std::move(unbindTxn), this]() mutable {
    zxlogf(INFO, "%p DdkUnbind Done", zxdev());
    txn.Reply();
  });
}

void NetworkDevice::DdkRelease() {
  zxlogf(INFO, "%p DdkRelease", zxdev());
  delete this;
}

void NetworkDevice::GetDevice(GetDeviceRequestView request, GetDeviceCompleter::Sync& _completer) {
  ZX_ASSERT_MSG(device_, "can't serve device if not bound to parent implementation");
  device_->Bind(std::move(request->device));
}

void NetworkDevice::NotifyThread(zx::unowned_thread thread, ThreadType type) {
  const std::string_view role = [type]() {
    switch (type) {
      case ThreadType::Tx:
        return "fuchsia.devices.network.core.tx";
      case ThreadType::Rx:
        return "fuchsia.devices.network.core.rx";
    }
  }();

  if (!thread->is_valid()) {
    zxlogf(INFO, "thread not present, scheduler role '%.*s' will not be applied",
           static_cast<int>(role.size()), role.data());
    return;
  }

  if (zx_status_t status =
          device_set_profile_by_role(parent(), thread->get(), role.data(), role.size());
      status != ZX_OK) {
    zxlogf(WARNING, "failed to set scheduler role '%.*s': %s", static_cast<int>(role.size()),
           role.data(), zx_status_get_string(status));
  }
}

static constexpr zx_driver_ops_t network_driver_ops = {
    .version = DRIVER_OPS_VERSION,
    .bind = [](void* ctx, zx_device_t* parent) -> zx_status_t {
      return NetworkDevice::Create(ctx, parent);
    },
};

zx::result<fdf::ClientEnd<fuchsia_hardware_network_driver::NetworkDeviceImpl>>
FidlNetworkDeviceImplFactory::Bind() {
  zx::result<fdf::ClientEnd<fuchsia_hardware_network_driver::NetworkDeviceImpl>> client_end =
      parent_->DdkConnectRuntimeProtocol<
          fuchsia_hardware_network_driver::Service::NetworkDeviceImpl>();
  if (client_end.is_error()) {
    zxlogf(ERROR, "failed to connect to parent device: %s", client_end.status_string());
    return client_end;
  }
  return client_end;
}

}  // namespace network

ZIRCON_DRIVER(network, network::network_driver_ops, "zircon", "0.1");
