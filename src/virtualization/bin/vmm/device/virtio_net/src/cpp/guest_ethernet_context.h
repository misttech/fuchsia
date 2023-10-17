// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_VIRTUALIZATION_BIN_VMM_DEVICE_VIRTIO_NET_SRC_CPP_GUEST_ETHERNET_CONTEXT_H_
#define SRC_VIRTUALIZATION_BIN_VMM_DEVICE_VIRTIO_NET_SRC_CPP_GUEST_ETHERNET_CONTEXT_H_

#include <lib/fdf/cpp/dispatcher.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/result.h>

#include <memory>

class GuestEthernetContext {
 public:
  static zx::result<std::unique_ptr<GuestEthernetContext>> Create();
  ~GuestEthernetContext();

  fdf::Dispatcher* SyncDispatcher() { return &sync_dispatcher_; }
  fdf::Dispatcher* ImplDispatcher() { return &impl_dispatcher_; }
  fdf::Dispatcher* IfcDispatcher() { return &ifc_dispatcher_; }
  fdf::Dispatcher* PortDispatcher() { return &port_dispatcher_; }
  fdf::Dispatcher* ShimDispatcher() { return &shim_dispatcher_; }
  fdf::Dispatcher* ShimPortDispatcher() { return &shim_port_dispatcher_; }

 private:
  GuestEthernetContext() = default;

  fdf::Dispatcher sync_dispatcher_;
  libsync::Completion sync_dispatcher_shutdown_;
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
};

#endif  // SRC_VIRTUALIZATION_BIN_VMM_DEVICE_VIRTIO_NET_SRC_CPP_GUEST_ETHERNET_CONTEXT_H_
