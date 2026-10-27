// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_DRIVERS_PCI_CONFIG_H_
#define SRC_DEVICES_PCI_DRIVERS_PCI_CONFIG_H_

#include <fidl/fuchsia.hardware.pci.root/cpp/wire.h>
#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <lib/zx/result.h>

#include <memory>

#include "src/devices/pci/lib/cpp/config.h"

namespace pci {

// ProxyConfig is used with PCI buses that do not support MMIO config space,
// or require special controller configuration before config access. Examples
// of this are IO config on x64 due to needing to synchronize CF8/CFC with
// ACPI, and Designware on ARM where the controller needs to be configured to
// map a given device's configuration space in before access.
//
// For proxy configuration access all operations are passed to the
// fuchsia.hardware.pci.root.Config protocol implementation.
class ProxyConfig final : public Config {
 public:
  static zx::result<std::unique_ptr<Config>> Create(
      fuchsia_hardware_pci::wire::Bdf bdf,
      const fidl::WireSyncClient<fuchsia_hardware_pci_root::Config>& client);
  uint8_t Read(PciReg8 addr) const final;
  uint16_t Read(PciReg16 addr) const final;
  uint32_t Read(PciReg32 addr) const final;
  void Write(PciReg8 addr, uint8_t val) const final;
  void Write(PciReg16 addr, uint16_t val) const final;
  void Write(PciReg32 addr, uint32_t val) const final;
  const char* type() const final;

 private:
  ProxyConfig(fuchsia_hardware_pci::wire::Bdf bdf,
              const fidl::WireSyncClient<fuchsia_hardware_pci_root::Config>& client)
      : Config(bdf), client_(client) {}
  // The bus driver outlives config objects.
  const fidl::WireSyncClient<fuchsia_hardware_pci_root::Config>& client_;
};

}  // namespace pci

#endif  // SRC_DEVICES_PCI_DRIVERS_PCI_CONFIG_H_
