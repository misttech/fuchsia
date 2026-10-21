// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_PCI_DRIVERS_PCIROOT_BANJO_ADAPTER_ADAPTER_H_
#define SRC_DEVICES_PCI_DRIVERS_PCIROOT_BANJO_ADAPTER_ADAPTER_H_

#include <fidl/fuchsia.hardware.pci.root/cpp/wire.h>
#include <fidl/fuchsia.hardware.pci/cpp/fidl.h>
#include <fuchsia/hardware/pciroot/cpp/banjo.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/metadata/cpp/metadata_server.h>

#include <vector>

namespace pciroot_banjo_adapter {

// A transitional bridge that adapts a Banjo pciroot parent into FIDL, deleted once all boards serve
// FIDL.
class Adapter : public fdf::DriverBase2, public fidl::WireServer<fuchsia_hardware_pci_root::Root> {
 public:
  Adapter() : fdf::DriverBase2("pciroot-banjo-adapter") {}

  zx::result<> Start(fdf::DriverContext context) override;

  // fuchsia_hardware_pci_root::Root implementations:
  void GetBti(GetBtiRequestView request, GetBtiCompleter::Sync& completer) override;
  void GetPlatformInfo(GetPlatformInfoCompleter::Sync& completer) override;
  void GetCam(GetCamCompleter::Sync& completer) override;
  void GetLegacyInterrupt(GetLegacyInterruptRequestView request,
                          GetLegacyInterruptCompleter::Sync& completer) override;
  void GetAddressSpace(GetAddressSpaceRequestView request,
                       GetAddressSpaceCompleter::Sync& completer) override;
  void AllocateMsi(AllocateMsiRequestView request, AllocateMsiCompleter::Sync& completer) override;
  void GetMsiHandle(GetMsiHandleRequestView request,
                    GetMsiHandleCompleter::Sync& completer) override;

 private:
  zx::result<> ServeMetadata(std::vector<fuchsia_driver_framework::Offer>& offers,
                             fidl::UnownedClientEnd<fuchsia_io::Directory> svc_dir);

  ddk::PcirootProtocolClient pciroot_;
  // Banjo boards disagree on who owns cam.vmo: ACPI and qemu-arm64 lend their
  // own handle, while qemu-riscv64 and crosvm duplicate per call. The adapter
  // deliberately never closes it, accepting one leaked handle per Start on
  // boards that duplicate.
  pci_platform_info_t banjo_info_{};

  fidl::ServerBindingGroup<fuchsia_hardware_pci_root::Root> root_bindings_;
  fdf_metadata::MetadataServer<fuchsia_hardware_pci::BoardConfiguration> metadata_server_;
  fidl::ClientEnd<fuchsia_driver_framework::NodeController> node_controller_;
};

}  // namespace pciroot_banjo_adapter

#endif  // SRC_DEVICES_PCI_DRIVERS_PCIROOT_BANJO_ADAPTER_ADAPTER_H_
