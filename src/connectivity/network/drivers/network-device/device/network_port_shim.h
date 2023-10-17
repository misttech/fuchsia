// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_NETWORK_PORT_SHIM_H_
#define SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_NETWORK_PORT_SHIM_H_

#include <lib/sync/cpp/completion.h>

#include "definitions.h"
#include "public/network_device.h"

namespace network {

// Translates calls between the parent device and the underlying netdevice.
//
// Usage of this type assumes that the parent device speaks Banjo while the underlying netdevice
// port speaks FIDL. This type translates calls from from netdevice into the parent from FIDL to
// Banjo. The NetworkPort protocol does not have corresponding Ifc protocol in the other direction
// so this type only needs to work in one direction.
class NetworkPortShim : public fdf::WireServer<netdriver::NetworkPort> {
 public:
  static void Bind(ddk::NetworkPortProtocolClient client_impl, const fdf::Dispatcher* dispatcher,
                   fdf::ServerEnd<netdriver::NetworkPort> server_end);

  void GetInfo(fdf::Arena& arena, GetInfoCompleter::Sync& completer) override;
  void GetStatus(fdf::Arena& arena, GetStatusCompleter::Sync& completer) override;
  void SetActive(netdriver::wire::NetworkPortSetActiveRequest* request, fdf::Arena& arena,
                 SetActiveCompleter::Sync& completer) override;
  void GetMac(fdf::Arena& arena, GetMacCompleter::Sync& completer) override;
  void Removed(fdf::Arena& arena, RemovedCompleter::Sync& completer) override;

 private:
  NetworkPortShim(ddk::NetworkPortProtocolClient impl, const fdf::Dispatcher* dispatcher);

  std::optional<fdf::ServerBindingRef<netdriver::NetworkPort>> binding_;
  ddk::NetworkPortProtocolClient impl_;
  const fdf::Dispatcher* dispatcher_;
  libsync::Completion dispatcher_shutdown_;
};

#endif  // SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_NETWORK_PORT_SHIM_H_

}  // namespace network
