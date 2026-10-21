// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/pci/drivers/pciroot-banjo-adapter/adapter.h"

#include <lib/driver/compat/cpp/banjo_client.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/zx/bti.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/msi.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>
#include <string.h>

#include <string_view>
#include <utility>
#include <vector>

#include <bind/fuchsia/cpp/bind.h>

namespace pciroot_banjo_adapter {

zx::result<> Adapter::Start(fdf::DriverContext context) {
  auto incoming = std::shared_ptr<fdf::Namespace>(context.take_incoming());

  zx::result pciroot = compat::ConnectBanjo<ddk::PcirootProtocolClient>(incoming);
  if (pciroot.is_error()) {
    fdf::error("Failed to connect to Banjo pciroot protocol: {}", pciroot);
    return pciroot.take_error();
  }
  pciroot_ = *pciroot;

  if (zx_status_t status = pciroot_.GetPciPlatformInfo(&banjo_info_); status != ZX_OK) {
    fdf::error("Failed to get PCI platform info from Banjo parent: {}", zx::make_result(status));
    return zx::error(status);
  }

  if (banjo_info_.acpi_bdfs_count > 0 && banjo_info_.devicetree_bdfs_count > 0) {
    fdf::error("Both ACPI ({}) and devicetree ({}) BDFs were provided", banjo_info_.acpi_bdfs_count,
               banjo_info_.devicetree_bdfs_count);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  fuchsia_hardware_pci_root::RootService::InstanceHandler root_handler({
      .root = root_bindings_.CreateHandler(this, dispatcher(), fidl::kIgnoreBindingClosure),
  });
  if (zx::result result =
          outgoing()->AddService<fuchsia_hardware_pci_root::RootService>(std::move(root_handler));
      result.is_error()) {
    fdf::error("Failed to add RootService: {}", result);
    return result.take_error();
  }

  std::vector<fuchsia_driver_framework::Offer> offers = {
      fdf::MakeOffer2<fuchsia_hardware_pci_root::RootService>(),
  };

  if (zx::result result = ServeMetadata(offers, incoming->svc_dir()); result.is_error()) {
    return result.take_error();
  }

  std::vector<fuchsia_driver_framework::NodeProperty2> properties = {
      fdf::MakeProperty2(bind_fuchsia::SERVICE, fuchsia_hardware_pci_root::RootService::Name),
  };

  zx::result child = AddChild("pci-root", properties, offers);
  if (child.is_error()) {
    fdf::error("Failed to add child node 'pci-root': {}", child);
    return child.take_error();
  }
  node_controller_ = std::move(child.value());

  return zx::ok();
}

zx::result<> Adapter::ServeMetadata(std::vector<fuchsia_driver_framework::Offer>& offers,
                                    fidl::UnownedClientEnd<fuchsia_io::Directory> svc_dir) {
  zx::result is_serving = metadata_server_.ForwardAndServe(*outgoing(), dispatcher(), svc_dir);
  if (is_serving.is_error()) {
    fdf::error("Failed to forward and serve metadata: {}", is_serving);
    return is_serving.take_error();
  }
  if (std::optional metadata_offer = metadata_server_.CreateOffer(); metadata_offer.has_value()) {
    offers.push_back(std::move(*metadata_offer));
  }
  return zx::ok();
}

void Adapter::GetBti(GetBtiRequestView request, GetBtiCompleter::Sync& completer) {
  if (request->bdf.device_id >= 32 || request->bdf.function_id >= 8) {
    completer.ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }
  uint32_t packed_bdf = (static_cast<uint32_t>(request->bdf.bus_id) << 8) |
                        (static_cast<uint32_t>(request->bdf.device_id) << 3) |
                        static_cast<uint32_t>(request->bdf.function_id);
  zx::bti bti;
  if (zx_status_t status = pciroot_.GetBti(packed_bdf, request->index, &bti); status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }
  completer.ReplySuccess(std::move(bti));
}

void Adapter::GetPlatformInfo(GetPlatformInfoCompleter::Sync& completer) {
  fidl::Arena arena;

  fidl::VectorView<fuchsia_hardware_pci_root::wire::IrqRoutingEntry> irq_routing(
      arena, banjo_info_.irq_routing_count);
  for (size_t i = 0; i < banjo_info_.irq_routing_count; ++i) {
    const auto& entry = banjo_info_.irq_routing_list[i];
    irq_routing[i] = fuchsia_hardware_pci_root::wire::IrqRoutingEntry{
        .port_device_id = entry.port_device_id,
        .port_function_id = entry.port_function_id,
        .device_id = entry.device_id,
        .pins = {entry.pins[0], entry.pins[1], entry.pins[2], entry.pins[3]},
    };
  }

  std::vector<fuchsia_hardware_pci::wire::Bdf> bdfs;
  fidl::WireOptional<fuchsia_hardware_pci_root::wire::ParentBdfs> parent_bdfs;
  if (banjo_info_.acpi_bdfs_count > 0) {
    bdfs.reserve(banjo_info_.acpi_bdfs_count);
    for (size_t i = 0; i < banjo_info_.acpi_bdfs_count; ++i) {
      const auto& bdf = banjo_info_.acpi_bdfs_list[i];
      bdfs.push_back(fuchsia_hardware_pci::wire::Bdf{
          .bus_id = bdf.bus_id,
          .device_id = bdf.device_id,
          .function_id = bdf.function_id,
      });
    }
    parent_bdfs = fuchsia_hardware_pci_root::wire::ParentBdfs::WithAcpi(
        arena, fidl::VectorView<fuchsia_hardware_pci::wire::Bdf>::FromExternal(bdfs));
  } else if (banjo_info_.devicetree_bdfs_count > 0) {
    bdfs.reserve(banjo_info_.devicetree_bdfs_count);
    for (size_t i = 0; i < banjo_info_.devicetree_bdfs_count; ++i) {
      const auto& bdf = banjo_info_.devicetree_bdfs_list[i];
      bdfs.push_back(fuchsia_hardware_pci::wire::Bdf{
          .bus_id = bdf.bus_id,
          .device_id = bdf.device_id,
          .function_id = bdf.function_id,
      });
    }
    parent_bdfs = fuchsia_hardware_pci_root::wire::ParentBdfs::WithDevicetree(
        arena, fidl::VectorView<fuchsia_hardware_pci::wire::Bdf>::FromExternal(bdfs));
  }

  fuchsia_hardware_pci_root::wire::PlatformInfo info{
      .name = fidl::StringView(
          arena,
          std::string_view(banjo_info_.name, strnlen(banjo_info_.name, sizeof(banjo_info_.name)))),
      .start_bus_num = banjo_info_.start_bus_num,
      .end_bus_num = banjo_info_.end_bus_num,
      .segment_group = banjo_info_.segment_group,
      .irq_routing = irq_routing,
      .parent_bdfs = parent_bdfs,
  };
  completer.ReplySuccess(info);
}

void Adapter::GetCam(GetCamCompleter::Sync& completer) {
  if (banjo_info_.cam.vmo == ZX_HANDLE_INVALID) {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
    return;
  }
  zx::vmo dup;
  if (zx_status_t status =
          zx::unowned_vmo(banjo_info_.cam.vmo)->duplicate(ZX_RIGHT_SAME_RIGHTS, &dup);
      status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }
  completer.ReplySuccess(std::move(dup), banjo_info_.cam.is_extended);
}

void Adapter::GetLegacyInterrupt(GetLegacyInterruptRequestView request,
                                 GetLegacyInterruptCompleter::Sync& completer) {
  for (size_t i = 0; i < banjo_info_.legacy_irqs_count; ++i) {
    const auto& irq = banjo_info_.legacy_irqs_list[i];
    if (irq.vector == request->vector) {
      zx::interrupt dup;
      if (zx_status_t status =
              zx::unowned_interrupt(irq.interrupt)->duplicate(ZX_RIGHT_SAME_RIGHTS, &dup);
          status != ZX_OK) {
        completer.ReplyError(status);
        return;
      }
      completer.ReplySuccess(std::move(dup));
      return;
    }
  }
  completer.ReplyError(ZX_ERR_NOT_FOUND);
}

void Adapter::GetAddressSpace(GetAddressSpaceRequestView request,
                              GetAddressSpaceCompleter::Sync& completer) {
  pci_address_space_t banjo_type = PCI_ADDRESS_SPACE_NONE;
  switch (request->type) {
    case fuchsia_hardware_pci_root::wire::AddressSpace::kMemory:
      banjo_type = PCI_ADDRESS_SPACE_MEMORY;
      break;
    case fuchsia_hardware_pci_root::wire::AddressSpace::kIo:
      banjo_type = PCI_ADDRESS_SPACE_IO;
      break;
  }
  uint64_t base = 0;
  zx::resource resource;
  zx::eventpair token;
  if (zx_status_t status = pciroot_.GetAddressSpace(request->in_base, request->size, banjo_type,
                                                    request->low, &base, &resource, &token);
      status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }
  completer.ReplySuccess(base, std::move(resource), std::move(token));
}

void Adapter::AllocateMsi(AllocateMsiRequestView request, AllocateMsiCompleter::Sync& completer) {
  zx::msi banjo_allocation;
  msi_allocation_info_t banjo_info{};
  if (zx_status_t status = pciroot_.AllocateMsi(request->msi_count, request->can_target_64bit,
                                                &banjo_allocation, &banjo_info);
      status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }
  completer.ReplySuccess(zx::handle(banjo_allocation.release()),
                         fuchsia_hardware_pci_root::wire::MsiAllocationInfo{
                             .target_addr = banjo_info.target_addr,
                             .target_data = banjo_info.target_data,
                             .irq_count = banjo_info.irq_count,
                         });
}

void Adapter::GetMsiHandle(GetMsiHandleRequestView request,
                           GetMsiHandleCompleter::Sync& completer) {
  zx::interrupt interrupt;
  if (zx_status_t status = pciroot_.GetMsiHandle(
          zx::msi(request->allocation.release()), request->options, request->msi_id,
          std::move(request->cfg_vmo), request->cfg_offset, &interrupt);
      status != ZX_OK) {
    completer.ReplyError(status);
    return;
  }
  completer.ReplySuccess(std::move(interrupt));
}

}  // namespace pciroot_banjo_adapter

FUCHSIA_DRIVER_EXPORT2(pciroot_banjo_adapter::Adapter);
