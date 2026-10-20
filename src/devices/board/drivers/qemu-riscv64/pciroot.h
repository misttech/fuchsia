// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_BOARD_DRIVERS_QEMU_RISCV64_PCIROOT_H_
#define SRC_DEVICES_BOARD_DRIVERS_QEMU_RISCV64_PCIROOT_H_

#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <fuchsia/hardware/pciroot/c/banjo.h>
#include <fuchsia/hardware/pciroot/cpp/banjo.h>
#include <lib/pci/pciroot.h>
#include <lib/pci/root_host.h>
#include <lib/zx/bti.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>
#include <zircon/status.h>

#include <vector>

namespace board_qemu_riscv64 {

inline constexpr char kPcirootName[] = "PCI0";

// Per-architecture PCIe memory map and IRQ constants from QEMU hw/riscv/virt.c.
inline constexpr ralloc_region_t kVirtPcieEcam = {.base = 0x3000'0000, .size = 0x1000'0000};
inline constexpr ralloc_region_t kVirtPcieMmio = {.base = 0x4000'0000, .size = 0x4000'0000};
inline constexpr ralloc_region_t kVirtPciePio = {.base = 0x300'0000, .size = 0x0001'0000};
inline constexpr uint8_t kVirtPcieIrqBase = 0x20;
inline constexpr uint8_t kVirtPcieIrqCount = 4;

inline constexpr uint32_t kBytesPerPciBus =
    fuchsia_hardware_pci::wire::kExtendedConfigSize * FUNCTIONS_PER_DEVICE * DEVICES_PER_BUS;
inline constexpr uint16_t kEndBusNumber = (kVirtPcieEcam.size / kBytesPerPciBus) - 1;

inline constexpr McfgAllocation kVirtPcieMcfg = {
    .address = kVirtPcieEcam.base,
    .pci_segment = 0,
    .start_bus_number = 0,
    .end_bus_number = kEndBusNumber,
};

class QemuRiscv64Pciroot : public PcirootBase, public ddk::PcirootProtocol<QemuRiscv64Pciroot> {
 public:
  QemuRiscv64Pciroot() = delete;
  QemuRiscv64Pciroot(PciRootHost* root_host, zx::vmo ecam, zx::resource irq_resource,
                     zx::resource iommu_resource)
      : PcirootBase(root_host),
        ecam_(std::move(ecam)),
        irq_resource_(std::move(irq_resource)),
        iommu_resource_(std::move(iommu_resource)) {
    ZX_DEBUG_ASSERT(irq_resource_.is_valid());
    ZX_DEBUG_ASSERT(iommu_resource_.is_valid());
  }
  ~QemuRiscv64Pciroot() override = default;

  zx::result<> CreateInterrupts();

  // fuchsia.hardware.pciroot implementation fill-ins
  using PcirootBase::PcirootAllocateMsi;
  using PcirootBase::PcirootDriverShouldProxyConfig;
  using PcirootBase::PcirootGetAddressSpace;
  using PcirootBase::PcirootGetMsiHandle;
  using PcirootBase::PcirootReadConfig16;
  using PcirootBase::PcirootReadConfig32;
  using PcirootBase::PcirootReadConfig8;
  using PcirootBase::PcirootWriteConfig16;
  using PcirootBase::PcirootWriteConfig32;
  using PcirootBase::PcirootWriteConfig8;
  zx_status_t PcirootGetBti(uint32_t bdf, uint32_t index, zx::bti* bti);
  zx_status_t PcirootGetPciPlatformInfo(pci_platform_info_t* info);

  pciroot_protocol_ops_t* pciroot_protocol_ops() { return &pciroot_protocol_ops_; }

 private:
  zx::vmo ecam_;
  const zx::resource irq_resource_;
  const zx::resource iommu_resource_;
  std::vector<zx::interrupt> owned_interrupts_;
  std::vector<pci_legacy_irq_t> interrupts_;
  std::vector<pci_irq_routing_entry_t> irq_routing_entries_;
};

}  // namespace board_qemu_riscv64

#endif  // SRC_DEVICES_BOARD_DRIVERS_QEMU_RISCV64_PCIROOT_H_
