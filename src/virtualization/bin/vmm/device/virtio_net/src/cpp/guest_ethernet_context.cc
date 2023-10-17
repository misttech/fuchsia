// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/virtualization/bin/vmm/device/virtio_net/src/cpp/guest_ethernet_context.h"

#include <lib/fdf/env.h>
#include <lib/syslog/cpp/macros.h>

zx::result<std::unique_ptr<GuestEthernetContext>> GuestEthernetContext::Create() {
  zx_status_t status = fdf_env_start();
  if (status != ZX_OK) {
    return zx::error(status);
  }

  std::unique_ptr<GuestEthernetContext> context(new GuestEthernetContext);

  fdf_env_register_driver_entry(context.get());

  auto sync_dispatcher = fdf::SynchronizedDispatcher::Create(
      fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "guest-ethernet-sync-dispatcher",
      [context = context.get()](fdf_dispatcher_t*) {
        context->sync_dispatcher_shutdown_.Signal();
      });
  if (sync_dispatcher.is_error()) {
    FX_LOGS(ERROR) << "Failed to create impl dispatcher: " << sync_dispatcher.status_string();
    return sync_dispatcher.take_error();
  }
  context->sync_dispatcher_ = std::move(sync_dispatcher.value());

  auto impl_dispatcher = fdf::UnsynchronizedDispatcher::Create(
      {}, "guest-ethernet-impl-dispatcher", [context = context.get()](fdf_dispatcher_t*) {
        context->impl_dispatcher_shutdown_.Signal();
      });
  if (impl_dispatcher.is_error()) {
    FX_LOGS(ERROR) << "Failed to create impl dispatcher: " << impl_dispatcher.status_string();
    return impl_dispatcher.take_error();
  }
  context->impl_dispatcher_ = std::move(impl_dispatcher.value());

  auto ifc_dispatcher = fdf::UnsynchronizedDispatcher::Create(
      {}, "guest-ethernet-ifc-dispatcher",
      [context = context.get()](fdf_dispatcher_t*) { context->ifc_dispatcher_shutdown_.Signal(); });
  if (ifc_dispatcher.is_error()) {
    FX_LOGS(ERROR) << "Failed to create ifc dispatcher: " << ifc_dispatcher.status_string();
    return ifc_dispatcher.take_error();
  }
  context->ifc_dispatcher_ = std::move(ifc_dispatcher.value());

  auto port_dispatcher = fdf::UnsynchronizedDispatcher::Create(
      {}, "guest-ethernet-port-dispatcher", [context = context.get()](fdf_dispatcher_t*) {
        context->port_dispatcher_shutdown_.Signal();
      });
  if (port_dispatcher.is_error()) {
    FX_LOGS(ERROR) << "Failed to create port dispatcher: " << port_dispatcher.status_string();
    return port_dispatcher.take_error();
  }
  context->port_dispatcher_ = std::move(port_dispatcher.value());

  auto shim_dispatcher = fdf::SynchronizedDispatcher::Create(
      fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "guest-ethernet-shim-dispatcher",
      [context = context.get()](fdf_dispatcher_t*) {
        context->shim_dispatcher_shutdown_.Signal();
      });
  if (shim_dispatcher.is_error()) {
    FX_LOGS(ERROR) << "Failed to create shim dispatcher: " << shim_dispatcher.status_string();
    return shim_dispatcher.take_error();
  }
  context->shim_dispatcher_ = std::move(shim_dispatcher.value());

  auto shim_port_dispatcher = fdf::SynchronizedDispatcher::Create(
      fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "guest-ethernet-shim-port-dispatcher",
      [context = context.get()](fdf_dispatcher_t*) {
        context->shim_port_dispatcher_shutdown_.Signal();
      });
  if (shim_port_dispatcher.is_error()) {
    FX_LOGS(ERROR) << "Failed to create shim port dispatcher: "
                   << shim_port_dispatcher.status_string();
    return shim_port_dispatcher.take_error();
  }
  context->shim_port_dispatcher_ = std::move(shim_port_dispatcher.value());

  return zx::ok(std::move(context));
}

GuestEthernetContext::~GuestEthernetContext() {
  if (sync_dispatcher_.get()) {
    sync_dispatcher_.ShutdownAsync();
    sync_dispatcher_shutdown_.Wait();
  }
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
  fdf_env_register_driver_exit();
  fdf_env_reset();
}
