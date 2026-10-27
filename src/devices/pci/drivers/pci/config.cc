// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/pci/drivers/pci/config.h"

#include <zircon/assert.h>

namespace pci {

// Proxy Config Implementation
zx::result<std::unique_ptr<Config>> ProxyConfig::Create(
    fuchsia_hardware_pci::wire::Bdf bdf,
    const fidl::WireSyncClient<fuchsia_hardware_pci_root::Config>& client) {
  if (!client.is_valid()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  // Can't use std::make_unique because the constructor is private.
  return zx::ok(std::unique_ptr<ProxyConfig>(new ProxyConfig(bdf, client)));
}

uint8_t ProxyConfig::Read(const PciReg8 addr) const {
  const fidl::WireResult result = client_->Read8(bdf(), addr.offset());
  ZX_ASSERT(result.ok());
  ZX_ASSERT(result->is_ok());
  return result->value()->value;
}

uint16_t ProxyConfig::Read(const PciReg16 addr) const {
  const fidl::WireResult result = client_->Read16(bdf(), addr.offset());
  ZX_ASSERT(result.ok());
  ZX_ASSERT(result->is_ok());
  return result->value()->value;
}

uint32_t ProxyConfig::Read(const PciReg32 addr) const {
  const fidl::WireResult result = client_->Read32(bdf(), addr.offset());
  ZX_ASSERT(result.ok());
  ZX_ASSERT(result->is_ok());
  return result->value()->value;
}

void ProxyConfig::Write(PciReg8 addr, uint8_t val) const {
  const fidl::WireResult result = client_->Write8(bdf(), addr.offset(), val);
  ZX_ASSERT(result.ok());
  ZX_ASSERT(result->is_ok());
}

void ProxyConfig::Write(PciReg16 addr, uint16_t val) const {
  const fidl::WireResult result = client_->Write16(bdf(), addr.offset(), val);
  ZX_ASSERT(result.ok());
  ZX_ASSERT(result->is_ok());
}

void ProxyConfig::Write(PciReg32 addr, uint32_t val) const {
  const fidl::WireResult result = client_->Write32(bdf(), addr.offset(), val);
  ZX_ASSERT(result.ok());
  ZX_ASSERT(result->is_ok());
}

const char* ProxyConfig::type() const { return "proxy"; }

}  // namespace pci
