// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/pci/drivers/pci/config.h"

#include <fuchsia/hardware/pciroot/cpp/banjo.h>
#include <zircon/assert.h>

namespace pci {

// Proxy Config Implementation
zx::result<std::unique_ptr<Config>> ProxyConfig::Create(pci_bdf_t bdf,
                                                        ddk::PcirootProtocolClient* proto) {
  // Can't use std::make_unique because the constructor is private.
  return zx::ok(std::unique_ptr<ProxyConfig>(new ProxyConfig(bdf, proto)));
}

uint8_t ProxyConfig::Read(const PciReg8 addr) const {
  uint8_t tmp;
  ZX_ASSERT(pciroot_->ReadConfig8(&bdf(), addr.offset(), &tmp) == ZX_OK);
  return tmp;
}

uint16_t ProxyConfig::Read(const PciReg16 addr) const {
  uint16_t tmp;
  ZX_ASSERT(pciroot_->ReadConfig16(&bdf(), addr.offset(), &tmp) == ZX_OK);
  return tmp;
}

uint32_t ProxyConfig::Read(const PciReg32 addr) const {
  uint32_t tmp;
  ZX_ASSERT(pciroot_->ReadConfig32(&bdf(), addr.offset(), &tmp) == ZX_OK);
  return tmp;
}

void ProxyConfig::Write(PciReg8 addr, uint8_t val) const {
  ZX_ASSERT(pciroot_->WriteConfig8(&bdf(), addr.offset(), val) == ZX_OK);
}

void ProxyConfig::Write(PciReg16 addr, uint16_t val) const {
  ZX_ASSERT(pciroot_->WriteConfig16(&bdf(), addr.offset(), val) == ZX_OK);
}

void ProxyConfig::Write(PciReg32 addr, uint32_t val) const {
  ZX_ASSERT(pciroot_->WriteConfig32(&bdf(), addr.offset(), val) == ZX_OK);
}

const char* ProxyConfig::type() const { return "proxy"; }

}  // namespace pci
