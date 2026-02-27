// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/ufs/ufs_pdev.h"

#include <fidl/fuchsia.hardware.platform.device/cpp/wire.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/platform-device/cpp/pdev.h>

namespace ufs {

zx::result<> UfsPdev::InitResources() {
  auto pdev = incoming()->Connect<fuchsia_hardware_platform_device::Service::Device>();
  if (!pdev.is_ok()) {
    FDF_LOG(ERROR, "Failed to connect to platform device service: %s", pdev.status_string());
    return pdev.take_error();
  }

  fdf::PDev dev{std::move(pdev.value())};

  {
    auto mmio_params = dev.GetMmio(0);
    if (mmio_params.is_error()) {
      FDF_LOG(ERROR, "Failed to get MMIO: %s", mmio_params.status_string());
      return mmio_params.take_error();
    }

    mmio_buffer_vmo_ = std::move(mmio_params->vmo);
    mmio_buffer_size_ = mmio_params->size;
  }

  {
    auto bti = dev.GetBti(0);
    if (bti.is_error()) {
      FDF_LOG(ERROR, "Failed to get BTI: %s", bti.status_string());
      return bti.take_error();
    }
    bti_ = std::move(bti.value());
  }

  {
    auto irq_result = dev.GetInterrupt(0);
    if (irq_result.is_error()) {
      FDF_LOG(ERROR, "Failed to get IRQ: %s", irq_result.status_string());
      return irq_result.take_error();
    }
    irq_ = std::move(irq_result.value());
  }

  return zx::ok();
}

zx_status_t UfsPdev::StopResources() { return ZX_OK; }

}  // namespace ufs
