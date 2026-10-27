// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include "src/devices/pci/drivers/pci/capabilities/msix.h"

#include <lib/pci/hw.h>
#include <lib/zx/result.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>

#include "src/devices/pci/lib/cpp/config.h"

namespace pci {

MsixCapability::MsixCapability(const Config& cfg, uint8_t base)
    : Capability(static_cast<uint8_t>(Capability::Id::kMsiX), base, cfg.addr()),
      ctrl_(PciReg16(static_cast<uint8_t>(base + kMsixControlRegisterOffset))),
      table_reg_(PciReg32(static_cast<uint8_t>(base + kMsixTableRegisterOffset))),
      pba_reg_(PciReg32(static_cast<uint8_t>(base + kMsixPbaRegisterOffset))) {
  uint8_t header_type = cfg.Read(Config::kHeaderType) & kHeaderTypeMask;
  max_bars_ = (header_type == kHeaderTypePciBridge) ? kBarRegsPerBridge : kMaxBarCount;

  MsixControlReg ctrl = {.value = cfg.Read(ctrl_)};
  // Table size is stored in the register as N-1 (PCIe Base Spec 7.7.2.2)
  table_size_ = static_cast<uint16_t>(ctrl.table_size() + 1);

  // Offset assumes a full 32bit width and is handled by the unshifted
  // field in the register structures.
  auto table = MsixTableReg::Get().FromValue(cfg.Read(table_reg_));
  table_bar_ = static_cast<uint8_t>(table.bir());
  table_offset_ = table.offset();

  auto pba = MsixPbaReg::Get().FromValue(cfg.Read(pba_reg_));
  pba_bar_ = static_cast<uint8_t>(pba.bir());
  pba_offset_ = pba.offset();
}

zx_status_t MsixCapability::Init(std::span<const std::optional<Bar>> bars) {
  if (inited_) {
    return ZX_ERR_BAD_STATE;
  }

  // Every vector has one entry in the table and in the pending bit array.
  size_t table_bytes = table_size_ * sizeof(MsixTable);
  // The PBA packs one pending bit per table entry into QWORDs, so it spans
  // ceil(N / 64) QWORDs with the last partly filled (PCIe Base Spec 7.7.2, 7.7.2.9).
  size_t pba_bytes =
      ((table_size_ + (kMsixPbaBitsPerEntry - 1)) / kMsixPbaBitsPerEntry) * sizeof(uint64_t);
  zxlogf(TRACE, "[%s] MSI-X supports %u vector%c", addr(), table_size_,
         (table_size_ == 1) ? ' ' : 's');
  zxlogf(TRACE, "[%s] MSI-X mask table bar %u @ %#x-%#zx", addr(), table_bar_, table_offset_,
         table_offset_ + table_bytes);
  zxlogf(TRACE, "[%s] MSI-X pending table bar %u @ %#x-%#zx", addr(), pba_bar_, pba_offset_,
         pba_offset_ + pba_bytes);

  // Reject reserved BIRs (2-7 on Type 1 bridge headers, 6-7 on Type 0 endpoint headers;
  // PCIe Base Spec 7.7.2.3-4) as well as unimplemented or unallocated BARs.
  if (table_bar_ >= max_bars_ || table_bar_ >= bars.size() || !bars[table_bar_].has_value() ||
      !bars[table_bar_]->allocation) {
    zxlogf(ERROR, "[%s] MSI-X table BAR %u is not allocated", addr(), table_bar_);
    return ZX_ERR_BAD_STATE;
  }
  const Bar& tbar = *bars[table_bar_];
  if (tbar.size < table_offset_ + table_bytes) {
    zxlogf(ERROR, "[%s] MSI-X table doesn't fit within BAR %u size of %#zx", addr(), table_bar_,
           tbar.size);
    return ZX_ERR_BAD_STATE;
  }

  if (pba_bar_ >= max_bars_ || pba_bar_ >= bars.size() || !bars[pba_bar_].has_value() ||
      !bars[pba_bar_]->allocation) {
    zxlogf(ERROR, "[%s] MSI-X pba BAR %u is not allocated", addr(), pba_bar_);
    return ZX_ERR_BAD_STATE;
  }
  const Bar& pbar = *bars[pba_bar_];
  if (pbar.size < pba_offset_ + pba_bytes) {
    zxlogf(ERROR, "[%s] MSI-X pba doesn't fit within BAR %u size of %#zx", addr(), pba_bar_,
           pbar.size);
    return ZX_ERR_BAD_STATE;
  }

  zx::result<zx::vmo> vmo_result = tbar.allocation->CreateVmo();
  if (!vmo_result.is_ok()) {
    zxlogf(ERROR, "[%s] Couldn't allocate VMO for MSI-X table bar: %s", addr(),
           vmo_result.status_string());
    return vmo_result.status_value();
  }
  zx::vmo table_vmo(std::move(vmo_result.value()));

  vmo_result = pbar.allocation->CreateVmo();
  if (!vmo_result.is_ok()) {
    zxlogf(ERROR, "[%s] Couldn't allocate VMO for MSI-X pba bar: %s", addr(),
           vmo_result.status_string());
    return vmo_result.status_value();
  }
  zx::vmo pba_vmo(std::move(vmo_result.value()));

  zx::result<fdf::MmioBuffer> mmio_result = fdf::MmioBuffer::Create(
      table_offset_, table_bytes, std::move(table_vmo), ZX_CACHE_POLICY_UNCACHED_DEVICE);
  if (mmio_result.is_error()) {
    zxlogf(ERROR, "[%s] Couldn't map MSI-X table: %s", addr(), mmio_result.status_string());
    return mmio_result.status_value();
  }
  table_mmio_ = std::move(mmio_result.value());
  table_ = static_cast<MMIO_PTR MsixTable*>(table_mmio_->get());

  mmio_result = fdf::MmioBuffer::Create(pba_offset_, pba_bytes, std::move(pba_vmo),
                                        ZX_CACHE_POLICY_UNCACHED_DEVICE);
  if (mmio_result.is_error()) {
    zxlogf(ERROR, "[%s] Couldn't map MSI-X pba: %s", addr(), mmio_result.status_string());
    return mmio_result.status_value();
  }
  pba_mmio_ = std::move(mmio_result.value());
  pba_ = static_cast<MMIO_PTR uint64_t*>(pba_mmio_->get());

  inited_ = true;
  return ZX_OK;
}

}  // namespace pci
