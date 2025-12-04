// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include "src/connectivity/ethernet/drivers/virtio/virtio_net_driver.h"

#include <lib/driver/component/cpp/driver_export.h>
#include <lib/virtio/driver_utils.h>

namespace {

constexpr char kDriverName[] = "virtio-net";

}  // namespace

namespace virtio {

VirtioNetDriver::VirtioNetDriver(fdf::DriverStartArgs start_args,
                                 fdf::UnownedSynchronizedDispatcher dispatcher)
    : fdf::DriverBase(kDriverName, std::move(start_args), std::move(dispatcher)) {}

zx::result<> VirtioNetDriver::Start() {
  zx::result netdevice = CreateNetworkDevice();
  if (netdevice.is_error()) {
    FDF_LOG(ERROR, "Failed to create net device: %s", netdevice.status_string());
    return netdevice.take_error();
  }
  netdevice_ = std::move(netdevice.value());

  if (zx_status_t status = netdevice_->Init(); status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to initialize net device: %s", zx_status_get_string(status));
    // Call Shutdown to clean up any device state. The driver framework will not call PrepareStop
    // if Start fails so we need to perform this cleanup here.
    netdevice_->Shutdown();
    return zx::error(status);
  }

  return zx::ok();
}

void VirtioNetDriver::PrepareStop(fdf::PrepareStopCompleter completer) {
  if (netdevice_) {
    netdevice_->Shutdown();
  }
  completer(zx::ok());
}

zx::result<std::unique_ptr<NetworkDevice>> VirtioNetDriver::CreateNetworkDevice() {
  zx::result<fidl::ClientEnd<fuchsia_hardware_pci::Device>> pci =
      incoming()->Connect<fuchsia_hardware_pci::Service::Device>();
  if (pci.is_error()) {
    FDF_LOG(ERROR, "Failed to get pci client: %s", pci.status_string());
    return pci.take_error();
  }

  zx::result<std::pair<zx::bti, std::unique_ptr<virtio::Backend>>> bti_and_backend =
      virtio::GetBtiAndBackend(std::move(pci).value());
  if (!bti_and_backend.is_ok()) {
    FDF_LOG(ERROR, "GetBtiAndBackend failed: %s", bti_and_backend.status_string());
    return bti_and_backend.take_error();
  }
  auto [bti, backend] = std::move(bti_and_backend).value();

  return zx::ok(std::make_unique<NetworkDevice>(this, std::move(bti), std::move(backend)));
}

}  // namespace virtio
