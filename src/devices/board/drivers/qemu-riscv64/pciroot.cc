// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "pciroot.h"

#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <fuchsia/hardware/pciroot/c/banjo.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/zx/bti.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/iommu.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>
#include <stdint.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/status.h>
#include <zircon/syscalls/iommu.h>
#include <zircon/syscalls/types.h>

#include <cstring>

namespace fpci = fuchsia_hardware_pci::wire;
namespace board_qemu_riscv64 {

zx_status_t QemuRiscv64Pciroot::PcirootGetBti(uint32_t bdf, uint32_t index, zx::bti* bti) {
  // We are creating a fake iommu because we currently don't support a real one. This will
  // eventually need to change.
  zx_iommu_desc_stub_t desc{};
  zx::iommu iommu;
  zx_status_t status =
      zx::iommu::create(iommu_resource_, ZX_IOMMU_TYPE_STUB, &desc, sizeof(desc), &iommu);
  if (status != ZX_OK) {
    return status;
  }
  return zx::bti::create(iommu, 0, bdf, bti);
}

zx_status_t QemuRiscv64Pciroot::PcirootGetPciPlatformInfo(pci_platform_info_t* info) {
  *info = {};

  zx::vmo ecam;
  if (zx_status_t status = ecam_.duplicate(ZX_RIGHT_SAME_RIGHTS, &ecam); status != ZX_OK) {
    fdf::error("couldn't duplicate ecam handle: {}", zx_status_get_string(status));
    return status;
  }

  // bus-pci takes ownership of the handles in legacy_irqs_list, so duplicate owned_interrupts_.
  std::vector<zx::interrupt> dup_interrupts;
  dup_interrupts.reserve(owned_interrupts_.size());
  for (const auto& irq : owned_interrupts_) {
    zx::interrupt dup;
    if (zx_status_t status = irq.duplicate(ZX_RIGHT_SAME_RIGHTS, &dup); status != ZX_OK) {
      fdf::error("couldn't duplicate interrupt handle: {}", zx_status_get_string(status));
      return status;
    }
    dup_interrupts.push_back(std::move(dup));
  }
  interrupts_.clear();
  for (size_t i = 0; i < dup_interrupts.size(); i++) {
    interrupts_.push_back(pci_legacy_irq_t{
        .interrupt = dup_interrupts[i].release(),
        .vector = static_cast<uint32_t>(kVirtPcieIrqBase + i),
    });
  }

  info->start_bus_num = kVirtPcieMcfg.start_bus_number;
  info->end_bus_num = kVirtPcieMcfg.end_bus_number;
  info->segment_group = kVirtPcieMcfg.pci_segment;
  info->legacy_irqs_list = interrupts_.data();
  info->legacy_irqs_count = interrupts_.size();
  info->irq_routing_list = irq_routing_entries_.data();
  info->irq_routing_count = irq_routing_entries_.size();
  strncpy(info->name, kPcirootName, sizeof(info->name));
  info->cam = {.vmo = ecam.release(), .is_extended = true};
  return ZX_OK;
}

zx::result<> QemuRiscv64Pciroot::CreateInterrupts() {
  for (uint32_t vector_offset = 0; vector_offset < kVirtPcieIrqCount; vector_offset++) {
    zx::interrupt interrupt;
    zx_status_t status = zx::interrupt::create(irq_resource_,
                                               /*vector=*/kVirtPcieIrqBase + vector_offset,
                                               /*options=*/0, /*result=*/&interrupt);
    if (status != ZX_OK) {
      return zx::error(status);
    }

    owned_interrupts_.push_back(std::move(interrupt));
  }

  // All QEMU devices are plugged into the root port and use a pre-defined pin swizzle.
  for (uint8_t device_id = 0; device_id < DEVICES_PER_BUS; device_id++) {
    pci_irq_routing_entry_t entry = {.port_device_id = PCI_IRQ_ROUTING_NO_PARENT,
                                     .port_function_id = PCI_IRQ_ROUTING_NO_PARENT,
                                     .device_id = device_id};
    for (uint32_t pin = 0; pin < PINS_PER_FUNCTION; pin++) {
      entry.pins[pin] = kVirtPcieIrqBase + ((pin + device_id) % PINS_PER_FUNCTION);
    }
    irq_routing_entries_.push_back(entry);
  }

  return zx::ok();
}

}  // namespace board_qemu_riscv64
