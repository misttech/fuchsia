// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_INCLUDE_DEV_ARM_SMMU_CONTEXT_BANK_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_INCLUDE_DEV_ARM_SMMU_CONTEXT_BANK_H_

#include <stdint.h>

#include <dev/arm_smmu/smmu.h>
#include <dev/arm_smmu/smmu_bti.h>
#include <fbl/intrusive_double_list.h>
#include <ktl/unique_ptr.h>

namespace arm_smmu {

class SmmuBti;

class ContextBank : public fbl::DoublyLinkedListable<ktl::unique_ptr<ContextBank>> {
 public:
  static zx::result<ktl::unique_ptr<ContextBank>> CreateAndLockdown(Smmu& smmu, uint32_t cb_ndx)
      TA_REQ(smmu.get_lock());

  static zx::result<ktl::unique_ptr<ContextBank>> CreateAndAdopt(Smmu& smmu, uint32_t cb_ndx)
      TA_REQ(smmu.get_lock());

  ContextBank(const ContextBank&) = delete;
  ContextBank operator=(const ContextBank&) = delete;
  ContextBank(ContextBank&&) = delete;
  ContextBank operator=(ContextBank&&) = delete;

  void SetMode(SmmuBti& owner, BtiMode mode) TA_REQ(owner.get_lock());

  static void Disable(Smmu& smmu, uint32_t cb_ndx) TA_REQ(smmu.get_lock()) {
    DEBUG_ASSERT(cb_ndx < smmu.num_cbs());
    DEBUG_ASSERT(smmu.available_cbs_.TestBit(cb_ndx));
    DisableRegs(smmu.gr1_base_, smmu.get_cb_base(cb_ndx), cb_ndx);
  }

  uint32_t cb_ndx() const { return cb_ndx_; }
  BtiMode mode() const { return mode_; }
  AddrMode addr_mode() const { return addr_mode_; }

 private:
  friend class std::default_delete<ContextBank>;
  friend class SmmuBti;

  // Info fetched from the TCR which tells us a few different things about how a
  // TTBR is configured.
  struct TTBRInfo {
    bool enabled{false};
    uint32_t granule_size_bits{0};
    uint64_t first_valid_addr{0};
    uint64_t last_valid_addr{0};
    uint64_t ttbr_paddr{0};
  };

  static ktl::unique_ptr<ContextBank> Create(Smmu& smmu, uint32_t cb_ndx) TA_REQ(smmu.get_lock());
  static zx::result<> ValidateNdx(Smmu& smmu, uint32_t cb_ndx) TA_REQ(smmu.get_lock());
  static void DisableRegs(hwreg::RegisterMmio gr1_base, hwreg::RegisterMmio cb_base,
                          uint32_t cb_ndx);

  ContextBank(uint32_t cb_ndx);
  ~ContextBank();

  // aspace size accesses registers and should only be called from a
  // ContextBank's owning BTI, with the BTI's lock held.
  uint64_t aspace_size();
  uint32_t DecodeGranuleSizeBits(uint32_t reg_bits) const;
  void DecodeTtbrRegions(uint32_t t0sz, uint32_t t1sz);
  zx::result<> AdoptRegisterState(Smmu& smmu) TA_REQ(smmu.get_lock());
  void LogFaultInfo() const;

  const uint32_t cb_ndx_;
  BtiMode mode_{BtiMode::kInvalid};
  hwreg::RegisterMmio gr1_base_{0};
  hwreg::RegisterMmio cb_base_{0};

  AddrMode addr_mode_{AddrMode::kInvalid};
  ktl::array<TTBRInfo, 2> ttbrs_;
};

}  // namespace arm_smmu

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_INCLUDE_DEV_ARM_SMMU_CONTEXT_BANK_H_
