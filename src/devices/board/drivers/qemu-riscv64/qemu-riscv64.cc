// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "qemu-riscv64.h"

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <fidl/fuchsia.kernel/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fdf/cpp/arena.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>

#include <bind/fuchsia/pci/cpp/bind.h>
#include <fbl/algorithm.h>

#include "pciroot.h"

namespace board_qemu_riscv64 {
namespace fpbus = fuchsia_hardware_platform_bus;

namespace {

template <class ResourceMoniker>
zx::result<zx::resource> GetResource(const fdf::Namespace& incoming) {
  zx::result result = incoming.Connect<ResourceMoniker>();
  if (result.is_error()) {
    return result.take_error();
  }
  fidl::WireResult wire_result = fidl::WireCall(result.value())->Get();
  if (!wire_result.ok()) {
    return zx::error(wire_result.status());
  }
  return zx::ok(std::move(wire_result.value().resource));
}

}  // namespace

void QemuRiscv64::SysinfoInit() {
  fdf::Arena arena('RISC');
  fpbus::BootloaderInfo bootloader_info;
  bootloader_info.vendor() = "<unknown>";
  {
    fidl::Arena fidl_arena;
    auto result = pbus_.buffer(arena)->SetBootloaderInfo(fidl::ToWire(fidl_arena, bootloader_info));
    if (!result.ok()) {
      fdf::error("SetBootloaderInfo request failed: {}", result.FormatDescription().data());
    } else if (result->is_error()) {
      fdf::error("SetBootloaderInfo failed: {}", zx_status_get_string(result->error_value()));
    }
  }
}

zx::result<> QemuRiscv64::PcirootInit(fdf::DriverContext& context, const std::string& node_name) {
  zx::result<zx::resource> msi = GetResource<fuchsia_kernel::MsiResource>(context.incoming());
  if (msi.is_error()) {
    fdf::error("Couldn't obtain MSI resource: {}", msi.status_string());
    return msi.take_error();
  }
  msi_resource_ = *std::move(msi);

  zx::result<zx::resource> mmio = GetResource<fuchsia_kernel::MmioResource>(context.incoming());
  if (mmio.is_error()) {
    fdf::error("Couldn't obtain MMIO resource: {}", mmio.status_string());
    return mmio.take_error();
  }
  mmio_resource_ = *std::move(mmio);

  // Note: fuchsia.kernel.IoportResource is only routed on x86 platforms; on RISC-V (as on
  // ARM/crosvm), ioport_resource_ remains an unbacked/invalid zx::resource handle.
  zx::result<zx::resource> ioport = GetResource<fuchsia_kernel::IoportResource>(context.incoming());
  if (ioport.is_ok()) {
    ioport_resource_ = *std::move(ioport);
  }

  zx::result<zx::resource> irq = GetResource<fuchsia_kernel::IrqResource>(context.incoming());
  if (irq.is_error()) {
    fdf::error("Couldn't obtain IRQ resource: {}", irq.status_string());
    return irq.take_error();
  }

  zx::result<zx::resource> iommu = GetResource<fuchsia_kernel::IommuResource>(context.incoming());
  if (iommu.is_error()) {
    fdf::error("Couldn't obtain IOMMU resource: {}", iommu.status_string());
    return iommu.take_error();
  }

  pci_root_host_.emplace(msi_resource_.borrow(), mmio_resource_.borrow(), ioport_resource_.borrow(),
                         PCI_ADDRESS_SPACE_MEMORY);

  pci_root_host_->mcfgs().push_back(kVirtPcieMcfg);

  if (zx_status_t status = pci_root_host_->Io().AddRegion(kVirtPciePio); status != ZX_OK) {
    fdf::error("Failed to add IO region {{ {:#x} - {:#x} }} to the PCI root allocator: {}",
               kVirtPciePio.base, kVirtPciePio.base + kVirtPciePio.size,
               zx_status_get_string(status));
    return zx::error(status);
  }

  if (zx::result<> result = pci_root_host_->AddMmioRange(kVirtPcieMmio.base, kVirtPcieMmio.size);
      result.is_error()) {
    fdf::error("Failed to add MMIO region {{ {:#x} - {:#x} }} to PCI root allocator: {}",
               kVirtPcieMmio.base, kVirtPcieMmio.base + kVirtPcieMmio.size, result.status_string());
    return result.take_error();
  }

  zx::vmo ecam;
  const size_t vmo_size = fbl::round_up<size_t>(kVirtPcieEcam.size, zx_system_get_page_size());
  zx_status_t status = zx::vmo::create_physical(mmio_resource_, /*paddr=*/kVirtPcieEcam.base,
                                                /*size=*/vmo_size, /*result=*/&ecam);
  if (status != ZX_OK) {
    fdf::error("Failed to allocate ecam vmo for [{:#x}, {:#x}): {}", kVirtPcieEcam.base,
               kVirtPcieEcam.base + kVirtPcieEcam.size, zx_status_get_string(status));
    return zx::error(status);
  }

  pciroot_.emplace(&*pci_root_host_, std::move(ecam), *std::move(irq), *std::move(iommu));
  inspector_ = context.CreateInspector(this, pciroot_->inspect());
  if (zx::result<> result = pciroot_->CreateInterrupts(); result.is_error()) {
    fdf::error("Failed to create interrupts: {}", result.status_string());
    return result.take_error();
  }

  banjo_server_.emplace(bind_fuchsia_pci::BIND_PROTOCOL_ROOT, &*pciroot_,
                        pciroot_->pciroot_protocol_ops());
  compat::DeviceServer::BanjoConfig banjo_config{
      .default_proto_id = bind_fuchsia_pci::BIND_PROTOCOL_ROOT,
  };
  banjo_config.callbacks[bind_fuchsia_pci::BIND_PROTOCOL_ROOT] = banjo_server_->callback();

  auto incoming = std::shared_ptr<fdf::Namespace>(context.take_incoming());
  zx::result<> result =
      compat_server_.Initialize(incoming, outgoing(), node_name, kPcirootName,
                                compat::ForwardMetadata::None(), std::move(banjo_config));
  if (result.is_error()) {
    fdf::error("Failed to initialize compat server: {}", result.status_string());
    return result.take_error();
  }

  std::vector offers = compat_server_.CreateOffers2();
  zx::result child = AddChild(kPcirootName, {{banjo_server_->property()}}, offers);
  if (child.is_error()) {
    fdf::error("Failed to add PCI0 child: {}", child.status_string());
    return child.take_error();
  }

  pciroot_controller_.Bind(std::move(child.value()), dispatcher());
  return zx::ok();
}

zx::result<> QemuRiscv64::Start(fdf::DriverContext context) {
  auto node_name = context.node_name().value_or("");

  zx::result pbus =
      context.incoming().Connect<fuchsia_hardware_platform_bus::Service::PlatformBus>();
  if (pbus.is_error()) {
    fdf::error("Failed to connect to platform bus: {}", pbus.status_string());
    return pbus.take_error();
  }
  pbus_ = fdf::WireSyncClient<fuchsia_hardware_platform_bus::PlatformBus>(std::move(pbus.value()));

  SysinfoInit();

  if (zx::result<> result = RtcInit(); result.is_error()) {
    fdf::error("Couldn't initialize rtc: {}", result.status_string());
    return result.take_error();
  }

  if (zx::result<> result = PcirootInit(context, node_name); result.is_error()) {
    fdf::error("Couldn't initialize pciroot: {}", result.status_string());
    return result.take_error();
  }

  fdf::info("QEMU RISC-V board driver initialized successfully");
  return zx::ok();
}

}  // namespace board_qemu_riscv64

FUCHSIA_DRIVER_EXPORT2(board_qemu_riscv64::QemuRiscv64);
