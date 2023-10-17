// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_PUBLIC_NETWORK_DEVICE_H_
#define SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_PUBLIC_NETWORK_DEVICE_H_

#include <fidl/fuchsia.hardware.network.driver/cpp/driver/wire.h>
#include <fidl/fuchsia.hardware.network/cpp/wire.h>
#include <fuchsia/hardware/network/driver/cpp/banjo.h>
#include <lib/async/dispatcher.h>
#include <lib/fidl_driver/cpp/transport.h>
#include <lib/fit/function.h>
#include <lib/zx/thread.h>

#include <memory>

#include <fbl/alloc_checker.h>

namespace network {

namespace netdev = fuchsia_hardware_network;

// TODO(https://fxbug.dev/133736): Remove this and related artifacts once all parents have migrated
// to FIDL.
class NetworkDeviceImplBinder {
 public:
  enum class Synchronicity { Sync, Async };
  virtual ~NetworkDeviceImplBinder() = default;
  virtual zx::result<fdf::ClientEnd<fuchsia_hardware_network_driver::NetworkDeviceImpl>> Bind() = 0;
  // Use this for factory specific teardown if needed. The return value indicates if the teardown
  // is synchronous or asynchronous. Call |on_teardown_complete| when an asynchronous teardown has
  // completed. If teardown is synchronous then |on_teardown_complete| should NOT be called, as seen
  // in the base implementation here.
  virtual Synchronicity Teardown(fit::callback<void()>&& on_teardown_complete) {
    return Synchronicity::Sync;
  }
};

struct DeviceInterfaceDispatchers {
  // Used for the NetworkDeviceImpl client as well as some async tasks and FIDL servers.
  const fdf::Dispatcher* const impl_ = nullptr;
  // Used to serve the the NetworkDeviceIfc protocol to vendor drivers.
  const fdf::Dispatcher* const ifc_ = nullptr;
  // Used for the NetworkPort client. This MUST be a synchronous dispatcher that allows sync calls.
  // This requirement is enforced at runtime, adding ports with an incorrect dispatcher will return
  // an error.
  const fdf::Dispatcher* const port_ = nullptr;
};

struct ShimDispatchers {
  // This is used by NetworkDeviceShim to serve the NetworkDeviceImpl protocol.
  const fdf::Dispatcher* const shim_ = nullptr;
  // This is used by NetworkDeviceShim to serve the NetworkPort protocol.
  const fdf::Dispatcher* const port_ = nullptr;
};

class NetworkDeviceInterface {
 public:
  // Abstracts system operations needed by the interface.
  class Sys {
   public:
    enum class ThreadType { Tx, Rx };

    // Notifies system of thread creation.
    //
    // Applies scheduler roles to created threads.
    virtual void NotifyThread(zx::unowned_thread thread, ThreadType type) = 0;
  };

  virtual ~NetworkDeviceInterface() = default;
  // Creates a new NetworkDeviceInterface that will bind to the provided parent. This is the Banjo
  // version of this call. The multiple dispatchers required should be owned externally so that
  // components that use multiple instances of NetworkDeviceInterface can re-use these dispatchers
  // between instances. Otherwise those components may run into the limitations on the number of
  // dispatcher threads that can be created.
  //
  // |sys| is an unowned pointer to Sys that may be nullptr if thread roles are unneeded.
  static zx::result<std::unique_ptr<NetworkDeviceInterface>> Create(
      const DeviceInterfaceDispatchers& dispatchers, const ShimDispatchers& shim_dispatchers,
      ddk::NetworkDeviceImplProtocolClient parent, Sys* sys = nullptr);

  // Creates a new NetworkDeviceInterface that will bind to the provided parent. This is the FIDL
  // version of this call. The multiple dispatchers required should be owned externally so that
  // components that use multiple instances of NetworkDeviceInterface can re-use these dispatchers
  // between instances. Otherwise those components may run into the limitations on the number of
  // dispatcher threads that can be created.
  static zx::result<std::unique_ptr<NetworkDeviceInterface>> Create(
      const DeviceInterfaceDispatchers& dispatchers,
      std::unique_ptr<NetworkDeviceImplBinder>&& factory, Sys* sys = nullptr);

  // Tears down the NetworkDeviceInterface.
  // A NetworkDeviceInterface must not be destroyed until the callback provided to teardown is
  // triggered, doing so may cause an assertion error. Immediately destroying a NetworkDevice that
  // never succeeded Init is allowed.
  virtual void Teardown(fit::callback<void()>) = 0;

  // Binds the request channel req to this NetworkDeviceInterface. Requests will be handled on the
  // dispatcher given to the device on creation.
  virtual zx_status_t Bind(fidl::ServerEnd<netdev::Device> req) = 0;

  // Binds the request channel req to a port belonging to this NetworkDeviceInterface. Requests will
  // be handled on the dispatcher given to the device on creation.
  virtual zx_status_t BindPort(uint8_t port_id, fidl::ServerEnd<netdev::Port> req) = 0;

 protected:
  NetworkDeviceInterface() = default;
};

}  // namespace network

#endif  // SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_PUBLIC_NETWORK_DEVICE_H_
