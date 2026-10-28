// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_SIM_SMMU_HARDWARE_MODEL_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_SIM_SMMU_HARDWARE_MODEL_H_

#include <lib/boot-options/boot-options.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <zircon/assert.h>
#include <zircon/syscalls/iommu.h>
#include <zircon/types.h>

#include <array>
#include <optional>
#include <string>
#include <unordered_map>
#include <unordered_set>
#include <vector>

#include <dev/arm_smmu/constants.h>
#include <dev/arm_smmu/smmu_mode.h>
#include <dev/arm_smmu/smmu_registers.h>
#include <dev/arm_smmu/vmsav8_64.h>
#include <dev/interrupt.h>
#include <hwreg/mmio.h>
#include <vm/pmm.h>

namespace arm_smmu::testing {

// The model decodes VMSAv8-64 descriptors with the definitions below, written
// from the Arm ARM (DDI 0487, "VMSAv8-64 translation table format
// descriptors"). It deliberately does not use the driver's definitions from
// <dev/arm_smmu/vmsav8_64.h>: the model is the oracle for that code, and a
// consistent mistake in a shared definition would be invisible.

// Descriptor bit 0: valid, at every level.
inline constexpr uint64_t kDescValid = 1ULL << 0;
// Output / next-level table address field for the 4KB granule with 48-bit
// addresses: bits [47:12]. Also the BADDR field of TTBR0.
inline constexpr uint64_t kDescAddrMask = ((1ULL << 48) - 1) & ~0xfffULL;
// Table descriptor bits [63:59]: NSTable, APTable, UXNTable, PXNTable. The
// model does not apply hierarchical attributes, so a table descriptor must
// leave them all zero.
inline constexpr uint64_t kDescTableHierAttrMask = 0x1fULL << 59;
// Leaf descriptor bit 53: Privileged Execute Never.
inline constexpr uint64_t kDescPxn = 1ULL << 53;
// Leaf descriptor bit 54: Unprivileged Execute Never.
inline constexpr uint64_t kDescUxn = 1ULL << 54;

// Returns true if stage 1 leaf descriptor |pte| permits every access in |perms|
// (IOMMU_FLAG_PERM_*) for a transaction of the given privilege, with WXN clear.
//
// AP[2:1]: 00 privileged read/write only; 01 read/write at both; 10 privileged
// read-only; 11 read-only at both. Unprivileged execute needs UXN == 0.
// Privileged execute needs PXN == 0, and is never permitted for memory that is
// writable by unprivileged accesses.
inline constexpr bool DescLeafPermits(uint64_t pte, uint32_t perms, bool privileged) {
  const uint64_t ap = (pte >> 6) & 0x3ULL;
  const bool unpriv_accessible = (ap & 0x1ULL) != 0;
  const bool read_only = (ap & 0x2ULL) != 0;
  const bool can_read = privileged || unpriv_accessible;
  const bool can_write = privileged ? !read_only : (unpriv_accessible && !read_only);
  const bool can_exec = privileged ? ((pte & kDescPxn) == 0 && !(unpriv_accessible && !read_only))
                                   : ((pte & kDescUxn) == 0);
  return !(((perms & IOMMU_FLAG_PERM_READ) != 0 && !can_read) ||
           ((perms & IOMMU_FLAG_PERM_WRITE) != 0 && !can_write) ||
           ((perms & IOMMU_FLAG_PERM_EXECUTE) != 0 && !can_exec));
}

// A 4KB translation table holds 512 descriptors.
inline constexpr uint32_t kDescEntriesPerTable = 512;

enum class DmaFaultType : uint32_t {
  kNone = 0,
  kUnidentifiedStream,
  kStreamMatchConflict,
  kInvalidContext,
  kTranslation,
  // A next-level table address or page output address does not fit the
  // configured stage 1 output address size.
  kAddressSize,
  kAccessFlag,
  kPermission,
  kTlbMatchConflict,
  kStaleTlbHit,
};

struct DmaTranslationResult {
  DmaFaultType fault = DmaFaultType::kNone;
  paddr_t paddr = 0;

  static constexpr DmaTranslationResult Ok(paddr_t pa) {
    return DmaTranslationResult{.fault = DmaFaultType::kNone, .paddr = pa};
  }

  static constexpr DmaTranslationResult Fault(DmaFaultType f) {
    return DmaTranslationResult{.fault = f, .paddr = 0};
  }

  constexpr bool is_ok() const { return fault == DmaFaultType::kNone; }
  constexpr bool operator==(const DmaTranslationResult& other) const {
    return fault == other.fault && paddr == other.paddr;
  }
  constexpr bool operator!=(const DmaTranslationResult& other) const { return !(*this == other); }
};

// SimSmmuHardwareModel models an ARM SMMUv2 controller (IHI 0062) paired with:
// 1. Non-coherent table walks reading strictly from PmmMock's PoC DRAM view
//    (poc_dram_priv), catching any missing or partial CleanDataCacheRange.
// 2. An infinite-capacity, multi-version Page Walk Cache (PWC, L0..L2) and
//    IOTLB (L3) that retains all valid descriptor versions ever loaded until
//    an explicit invalidation (TLBIVA / TLBIASID / TLBIALLNSNH) followed by
//    synchronization (TLBSYNC / TLBGSYNC).
// 3. Eager speculative walking of poc_dram_view on every CleanDataCacheRange,
//    every TTBR0/TCR/SCTLR enable, and every TLBSYNC / TLBGSYNC.
// 4. Immediate detection of Make-Before-Break violations, Hardware-Writable
//    PTE hazards (requiring AF == 1 && DBM == 0 on all valid L3 PTEs and
//    TCR2.HA == 0 && TCR2.HD == 0), and premature page-table page frees to PMM
//    before TLBSYNC.
class SimSmmuHardwareModel final : public FakeMmioHandler {
 public:
  struct Config {
    paddr_t mmio_phys = 0x0900'0000;
    vaddr_t mmio_vaddr = 0x1900'0000;
    uint32_t num_smrgs = 32;
    uint32_t num_cbs = 8;
    uint32_t num_pages = 16;  // 1 << (numpagendxb + 1)
    uint32_t page_size = 4096;
    uint32_t num_sid_bits = 15;
    uint32_t major_version = 2;
    uint32_t minor_version = 1;
    bool s1ts = true;
    bool sms = true;
    uint32_t numirpt = 1;
    uint32_t global_irq_num = 40;
    uint32_t base_context_irq_num = 48;
    bool simulate_tlb_sync_poll_delay = true;
    // Whether to honour the fields of SMMU_CBn_TCR2 which the driver does not
    // program yet: PASize (stage 1 output address size) and AS (8-bit or
    // 16-bit ASIDs).
    //
    // The model's registers reset to zero. A zero TCR2 selects a 32-bit output
    // size, which would fail every mapping above 4 GiB, and 8-bit ASIDs, which
    // the driver's 16-bit ASID values violate. These checks are switched on
    // together with the driver fix (see the review's item 2).
    bool enforce_tcr2_fields = false;
  };

  struct TlbKey {
    uint16_t asid = 0;
    uint8_t level = 0;
    uint64_t iova_prefix = 0;

    bool operator==(const TlbKey& o) const {
      return asid == o.asid && level == o.level && iova_prefix == o.iova_prefix;
    }
  };

  struct TlbKeyHash {
    size_t operator()(const TlbKey& k) const {
      size_t h = std::hash<uint64_t>{}(k.iova_prefix);
      h ^= (static_cast<size_t>(k.asid) << 16) ^ (static_cast<size_t>(k.level) << 1);
      return h;
    }
  };

  SimSmmuHardwareModel() : SimSmmuHardwareModel(Config{}) {}
  explicit SimSmmuHardwareModel(const Config& cfg) : cfg_(cfg) { Reset(); }

  ~SimSmmuHardwareModel() override { Detach(); }

  void Reset() {
    Detach();
    hazards_.clear();
    multi_version_tlb_.clear();
    table_ownership_.clear();
    pending_global_tlbis_.clear();
    global_syncing_tlbis_.clear();
    global_sync_active_ = false;
    tlbg_poll_remaining_ = 0;
    clean_dcache_call_count_ = 0;
    tlb_sync_call_count_ = 0;
    tlb_gsync_call_count_ = 0;
    descriptor_write_count_ = 0;

    gr0_ = Gr0Regs{};
    gr1_ = Gr1Regs{};
    cbs_.fill(ContextBankRegs{});
    bootloader_smrg_.fill(false);

    // Default CR0 after hardware reset has CLIENTPD=1 (bypass until initialized).
    gr0_.cr0 = 0x1;

    // Populate IDR0..IDR7 according to cfg_.
    uint32_t idr0 = 0;
    if (cfg_.s1ts) {
      idr0 |= (1u << 30);  // S1TS
    }
    idr0 |= (1u << 29);  // S2TS
    idr0 |= (1u << 28);  // NTS
    if (cfg_.sms) {
      idr0 |= (1u << 27);  // SMS
    }
    idr0 |= ((cfg_.numirpt & 0xffu) << 16);     // NUMIRPT
    idr0 |= ((cfg_.num_sid_bits & 0xfu) << 9);  // NUMSIDB
    idr0 |= (cfg_.num_smrgs & 0xffu);           // NUMSMRG
    gr0_.idr0 = idr0;

    // IDR1: PAGESIZE (bit 31) is 1 for 64KB register pages, NUMPAGENDXB is
    // log2(num_pages) - 1, and NUMCB is the number of context banks. They are
    // derived from the same configuration the register decoding uses, so the
    // two cannot disagree.
    ZX_ASSERT(cfg_.page_size == 4096 || cfg_.page_size == 65536);
    ZX_ASSERT(cfg_.num_pages >= 2 && cfg_.num_pages <= 256 &&
              (cfg_.num_pages & (cfg_.num_pages - 1)) == 0);
    const uint32_t numpagendxb = static_cast<uint32_t>(__builtin_ctz(cfg_.num_pages)) - 1;
    gr0_.idr1 = ((cfg_.page_size == 65536) ? (1u << 31) : 0u) | ((numpagendxb & 0x7u) << 28) |
                (cfg_.num_cbs & 0xffu);

    // IDR2: PTFSv8_4kb=1 (bit 12), UBS=5, OAS=5, IAS=5.
    gr0_.idr2 = (1u << 12) | (5u << 8) | (5u << 4) | 5u;

    // IDR7: major, minor.
    gr0_.idr7 = ((cfg_.major_version & 0xfu) << 4) | (cfg_.minor_version & 0xfu);

    const uint32_t sid_mask = (1u << cfg_.num_sid_bits) - 1u;
    smr_implemented_mask_ = (sid_mask << 16) | sid_mask;

    const size_t total_mmio_size = static_cast<size_t>(cfg_.page_size) * cfg_.num_pages * 2;
    FakeSmmuMmioRouter::Get().RegisterRegion(cfg_.mmio_phys, cfg_.mmio_vaddr, total_mmio_size,
                                             this);

    PmmMock::Get().SetObservers(
        [this](paddr_t table_pa, uint32_t index, uint64_t old_val, uint64_t new_val,
               uint32_t level) { OnPteWrite(table_pa, index, old_val, new_val, level); },
        [this](paddr_t page_pa, size_t offset, size_t size) {
          OnCacheClean(page_pa, offset, size);
        },
        [this](paddr_t page_pa) { OnPageFree(page_pa); });
  }

  void Detach() {
    FakeSmmuMmioRouter::Get().UnregisterRegion(cfg_.mmio_phys);
    PmmMock::Get().ClearObservers();
  }

  zbi_dcfg_arm_smmu_driver_t MakeZbiConfig(const std::vector<uint32_t>& handoff_smrs = {},
                                           uint32_t num_cb_override = 0,
                                           uint32_t num_smr_override = 0) const {
    zbi_dcfg_arm_smmu_driver_t zbi{};
    zbi.mmio_phys = cfg_.mmio_phys;
    zbi.num_context_banks_override = num_cb_override;
    zbi.num_smr_override = num_smr_override;
    zbi.global_irq_cnt = 1;
    zbi.irq_cnt = 1 + cfg_.num_cbs;
    zbi.irqs[0] = zbi_dcfg_arm_smmu_irq_t{
        .num = cfg_.global_irq_num,
        .flags = ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_HIGH,
    };
    for (uint32_t i = 0; i < cfg_.num_cbs; ++i) {
      zbi.irqs[1 + i] = zbi_dcfg_arm_smmu_irq_t{
          .num = cfg_.base_context_irq_num + i,
          .flags = ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_HIGH,
      };
    }
    zbi.handoff_smr_cnt = static_cast<uint32_t>(handoff_smrs.size());
    for (size_t i = 0; i < handoff_smrs.size() && i < 16; ++i) {
      zbi.handoff_smrs[i] = handoff_smrs[i];
    }
    return zbi;
  }

  const Config& config() const { return cfg_; }
  // The mask of implemented stream ID bits (IDR0.NUMSIDB of them).
  uint32_t SidMask() const { return smr_implemented_mask_ & 0xffffu; }
  bool has_hazard() const { return !hazards_.empty(); }
  const std::vector<std::string>& hazards() const { return hazards_; }
  void ClearHazards() { hazards_.clear(); }
  void set_assert_on_hazard(bool v) { assert_on_hazard_ = v; }
  size_t clean_dcache_call_count() const { return clean_dcache_call_count_; }
  size_t tlb_sync_call_count() const { return tlb_sync_call_count_; }
  size_t tlb_gsync_call_count() const { return tlb_gsync_call_count_; }
  // The number of descriptor stores observed in translation tables. A zero-fill
  // of a table counts once for each descriptor it clears.
  size_t descriptor_write_count() const { return descriptor_write_count_; }

  // Pre-configure a bootloader SMRG and optional Context Bank prior to Smmu::Create
  // to test AdoptSmrg in Passthru and Enforced modes.
  void PreconfigureBootloaderSmrg(uint32_t smrg_ndx, uint16_t stream_id, uint16_t mask,
                                  S2CR_Type type, uint32_t cb_ndx = 0,
                                  CBAR_Type cbar_type = CBAR_Type::kS1TS2Bypass,
                                  bool sctlr_m = false, bool epd0 = true, bool epd1 = true) {
    ZX_ASSERT(smrg_ndx < cfg_.num_smrgs);
    bootloader_smrg_[smrg_ndx] = true;
    gr0_.smr[smrg_ndx] =
        (1u << 31) | ((uint32_t{mask} & SidMask()) << 16) | (stream_id & SidMask());
    uint32_t s2cr = (static_cast<uint32_t>(type) & 0x3u) << 16;
    if (type == S2CR_Type::kTranslation) {
      ZX_ASSERT(cb_ndx < cfg_.num_cbs);
      s2cr |= (cb_ndx & 0xffu);
      gr1_.cbar[cb_ndx] = (static_cast<uint32_t>(cbar_type) & 0x3u) << 16;
      gr1_.cba2r[cb_ndx] = 1u;  // VA64 = 1
      cbs_[cb_ndx].sctlr = sctlr_m ? 1u : 0u;
      cbs_[cb_ndx].tcr = (epd1 ? (1u << 23) : 0u) | (epd0 ? (1u << 7) : 0u) | 16u;
    }
    gr0_.s2cr[smrg_ndx] = s2cr;
  }

  // Which fault interrupt handlers ServicePendingInterrupts() ran.
  struct ServicedInterrupts {
    bool global = false;
    std::vector<uint32_t> contexts;

    bool ran_context(uint32_t cb_ndx) const {
      for (uint32_t cb : contexts) {
        if (cb == cb_ndx) {
          return true;
        }
      }
      return false;
    }
  };

  // Runs the driver's handler for every fault interrupt which is asserted,
  // enabled and unmasked, and reports which ones ran.
  //
  // Fault interrupts are level-sensitive: one stays asserted for as long as its
  // status register is non-zero. A fault recorded while the interrupt was
  // disabled or masked is therefore delivered later, as soon as the driver
  // enables and unmasks it again. Tests call this after a driver operation,
  // which is when such a deferred interrupt would reach the CPU. (It cannot be
  // called from inside a register write: the driver may hold the locks its
  // handler takes.)
  ServicedInterrupts ServicePendingInterrupts() {
    ServicedInterrupts serviced;
    if (GlobalIrqDeliverable()) {
      serviced.global = InterruptMock::Get().TriggerIrq(cfg_.global_irq_num);
      if (serviced.global && GlobalIrqDeliverable()) {
        RecordHazard(
            "Global fault handler returned with the interrupt still asserted, enabled and "
            "unmasked (GFSR=0x%x): it would fire again immediately",
            gr0_.gfsr);
      }
    }
    for (uint32_t cb = 0; cb < cfg_.num_cbs; ++cb) {
      if (!ContextIrqDeliverable(cb)) {
        continue;
      }
      if (InterruptMock::Get().TriggerIrq(cfg_.base_context_irq_num + cb)) {
        serviced.contexts.push_back(cb);
        if (ContextIrqDeliverable(cb)) {
          RecordHazard(
              "Context %u fault handler returned with the interrupt still asserted, enabled and "
              "unmasked (FSR=0x%x): it would fire again immediately",
              cb, cbs_[cb].fsr);
        }
      }
    }
    return serviced;
  }

  // Records a global fault with a syndrome chosen by the test, then delivers
  // the global fault interrupt if the driver has it enabled and unmasked.
  // Returns true if the handler ran.
  bool InjectGlobalFaultIrq(uint32_t gfsr, uint64_t gfar, uint32_t gfsynr0, uint16_t stream_id) {
    LatchGlobalFault(gfsr, gfar, gfsynr0, stream_id);
    return ServicePendingInterrupts().global;
  }

  // Records a context fault on |cb_ndx| with a syndrome chosen by the test,
  // then delivers every fault interrupt the driver has enabled and unmasked.
  // Returns true if |cb_ndx|'s handler ran.
  bool InjectContextFaultIrq(uint32_t cb_ndx, uint32_t fsr, uint64_t far, uint32_t fsynr0,
                             uint16_t stream_id) {
    ZX_ASSERT(cb_ndx < cfg_.num_cbs);
    LatchContextFault(cb_ndx, fsr, far, fsynr0, stream_id);
    return ServicePendingInterrupts().ran_context(cb_ndx);
  }

  // Simulates one access by a device: translates it and, if the access faults,
  // records the fault as the hardware would and delivers the fault interrupt if
  // the driver has it enabled and unmasked. Unlike the Inject*() operations,
  // the reported syndrome is whatever the hardware rules produce for this
  // access, not values chosen by the test.
  DmaTranslationResult DeviceAccess(uint16_t stream_id, uint64_t iova, uint32_t access_perms,
                                    ServicedInterrupts* out_serviced = nullptr) {
    const DmaTranslationResult res = TranslateDma(stream_id, iova, access_perms);
    const uint32_t is_write = (access_perms & IOMMU_FLAG_PERM_WRITE) ? 1u : 0u;
    const uint32_t is_exec = (access_perms & IOMMU_FLAG_PERM_EXECUTE) ? 1u : 0u;
    switch (res.fault) {
      // Not an access the hardware reports: it succeeded, or the result is one
      // of the model's own diagnostics for a stale cache.
      case DmaFaultType::kNone:
      case DmaFaultType::kStaleTlbHit:
      case DmaFaultType::kTlbMatchConflict:
      // A stream whose S2CR type is "fault" (or which is routed to a context
      // that is not stage 1) is terminated without a context to report in.
      case DmaFaultType::kInvalidContext:
        break;
      // Global faults: sGFSR.USF is bit 1 and sGFSR.SMCF is bit 2. sGFSYNR0.WNR
      // is bit 1 and IND is bit 3. sGFSYNR1 holds the stream ID.
      case DmaFaultType::kUnidentifiedStream:
      case DmaFaultType::kStreamMatchConflict:
        LatchGlobalFault(res.fault == DmaFaultType::kUnidentifiedStream ? (1u << 1) : (1u << 2),
                         iova, (is_write << 1) | (is_exec << 3), stream_id);
        break;
      // Context faults: FSR.TF is bit 1, AFF bit 2, PF bit 3, ASF bit 7. FSYNR0
      // holds the level in PLVL (bits [1:0]), WNR in bit 4, IND in bit 6 and the
      // context bank index in S1CBNDX (bits [23:16]). CBFRSYNRA holds the
      // stream ID.
      case DmaFaultType::kTranslation:
      case DmaFaultType::kPermission:
      case DmaFaultType::kAccessFlag:
      case DmaFaultType::kAddressSize: {
        const std::optional<uint32_t> cb = FindContextBankForStreamId(stream_id);
        if (cb.has_value() && *cb < cfg_.num_cbs) {
          uint32_t fsr = 1u << 1;
          if (res.fault == DmaFaultType::kPermission) {
            fsr = 1u << 3;
          } else if (res.fault == DmaFaultType::kAccessFlag) {
            fsr = 1u << 2;
          } else if (res.fault == DmaFaultType::kAddressSize) {
            fsr = 1u << 7;
          }
          const uint32_t fsynr0 =
              (last_walk_level_ & 0x3u) | (is_write << 4) | (is_exec << 6) | (*cb << 16);
          LatchContextFault(*cb, fsr, iova, fsynr0, stream_id);
        }
        break;
      }
    }
    ServicedInterrupts serviced = ServicePendingInterrupts();
    if (out_serviced != nullptr) {
      *out_serviced = std::move(serviced);
    }
    return res;
  }

  uint32_t gfsr() const { return gr0_.gfsr; }
  uint32_t cb_fsr(uint32_t cb_ndx) const { return cbs_[cb_ndx].fsr; }
  uint32_t cb_sctlr(uint32_t cb_ndx) const { return cbs_[cb_ndx].sctlr; }
  uint32_t smr(uint32_t smrg_ndx) const { return gr0_.smr[smrg_ndx]; }
  uint32_t s2cr(uint32_t smrg_ndx) const { return gr0_.s2cr[smrg_ndx]; }

  // Checks the invariants that must hold whenever no driver operation is in
  // progress, recording a hazard for each violation. Returns true if no hazard
  // is recorded (now or earlier).
  //
  // 1. No TLB invalidation is posted but not yet synced.
  // 2. Every cached PWC/IOTLB descriptor is still what a fresh walk of PoC DRAM
  //    from an enabled context with that ASID would read. Anything else is a
  //    stale entry: the driver changed or removed a translation and returned
  //    without completing its invalidation.
  //
  // Without this, a missing invalidation is only noticed if a later step
  // happens to reuse or probe the same input address.
  bool VerifyQuiescent() {
    size_t posted = pending_global_tlbis_.size();
    size_t syncing = global_syncing_tlbis_.size();
    for (uint32_t i = 0; i < cfg_.num_cbs; ++i) {
      posted += cbs_[i].pending_tlbis.size();
      syncing += cbs_[i].syncing_tlbis.size();
    }
    if (posted + syncing != 0) {
      RecordHazard(
          "TLB invalidation posted but not synced to completion at the end of an operation "
          "(%zu posted, of which %zu have a sync in progress)",
          posted + syncing, syncing);
    }

    // Every context which is translating must be configured the way the model
    // (and the driver's own reasoning) assumes.
    for (uint32_t i = 0; i < cfg_.num_cbs; ++i) {
      if (!IsCbTranslationEnabled(i)) {
        continue;
      }
      if ((cbs_[i].tcr & (1u << 23)) == 0) {
        RecordHazard(
            "Context %u is translating with EPD1 == 0: upper-region addresses would walk a second "
            "translation table root which the driver never sets up",
            i);
      }
      if ((cbs_[i].tcr2 & ((1u << 10) | (1u << 11))) != 0) {
        RecordHazard(
            "Context %u is translating with hardware translation table updates enabled "
            "(TCR2=0x%x)",
            i, cbs_[i].tcr2);
      }
      if (((cbs_[i].tcr >> 14) & 0x3u) != 0) {
        RecordHazard("Context %u is translating with TG0=%u (only the 4KB granule is supported)", i,
                     (cbs_[i].tcr >> 14) & 0x3u);
      }
    }

    constexpr size_t kMaxReported = 4;
    size_t stale_count = 0;
    std::vector<std::string> reports;
    for (const auto& [key, versions] : multi_version_tlb_) {
      const std::optional<uint64_t> current = WalkPocToLevel(key.asid, key.level, key.iova_prefix);
      for (const auto& [pte, read_from] : versions) {
        if (current.has_value() && *current == pte) {
          continue;
        }
        if (stale_count++ < kMaxReported) {
          char buf[384];
          snprintf(buf, sizeof(buf),
                   "Stale cached descriptor at the end of an operation (missing TLBI/TLBSYNC) "
                   "[asid 0x%x level %u iova 0x%lx: cached 0x%lx read from table 0x%lx, PoC walk "
                   "now yields %s0x%lx]",
                   key.asid, key.level, key.iova_prefix, pte, read_from,
                   current.has_value() ? "" : "nothing, not ", current.value_or(0));
          reports.emplace_back(buf);
        }
      }
    }
    for (const std::string& report : reports) {
      RecordHazard("%s", report.c_str());
    }
    return !has_hazard();
  }

  // Find which Context Bank currently serves a given StreamID (if any).
  std::optional<uint32_t> FindContextBankForStreamId(uint16_t stream_id) const {
    for (uint32_t i = 0; i < cfg_.num_smrgs; ++i) {
      const uint32_t smr = gr0_.smr[i];
      if ((smr & (1u << 31)) == 0) {
        continue;
      }
      const uint16_t id = static_cast<uint16_t>(smr & SidMask());
      const uint16_t mask = static_cast<uint16_t>((smr >> 16) & SidMask());
      if (((stream_id ^ id) & ~mask) == 0) {
        const uint32_t s2cr = gr0_.s2cr[i];
        const uint32_t type = (s2cr >> 16) & 0x3u;
        if (type == static_cast<uint32_t>(S2CR_Type::kTranslation)) {
          return s2cr & 0xffu;
        }
        return std::nullopt;
      }
    }
    return std::nullopt;
  }

  // Translate a simulated DMA transaction from |stream_id| at |iova| with
  // requested |access_perms| (IOMMU_FLAG_PERM_READ / WRITE / EXECUTE), walking
  // strictly the PoC DRAM view and checking the multi-version IOTLB/PWC.
  DmaTranslationResult TranslateDma(uint16_t stream_id, uint64_t iova, uint32_t access_perms) {
    // 1. Check CR0.CLIENTPD (bit 0).
    if ((gr0_.cr0 & 0x1u) != 0) {
      return DmaTranslationResult::Ok(iova);
    }

    // 2. Match stream_id against valid SMRGs.
    std::optional<uint32_t> matched_smrg;
    for (uint32_t i = 0; i < cfg_.num_smrgs; ++i) {
      const uint32_t smr = gr0_.smr[i];
      if ((smr & (1u << 31)) == 0) {
        continue;
      }
      const uint16_t id = static_cast<uint16_t>(smr & SidMask());
      const uint16_t mask = static_cast<uint16_t>((smr >> 16) & SidMask());
      if (((stream_id ^ id) & ~mask) == 0) {
        if (matched_smrg.has_value()) {
          return DmaTranslationResult::Fault(DmaFaultType::kStreamMatchConflict);
        }
        matched_smrg = i;
      }
    }

    if (!matched_smrg.has_value()) {
      const bool usfcfg = (gr0_.cr0 & (1u << 10)) != 0;
      return usfcfg ? DmaTranslationResult::Fault(DmaFaultType::kUnidentifiedStream)
                    : DmaTranslationResult::Ok(iova);
    }

    // 3. Check S2CRn type.
    const uint32_t s2cr = gr0_.s2cr[*matched_smrg];
    const uint32_t s2cr_type = (s2cr >> 16) & 0x3u;
    if (s2cr_type == static_cast<uint32_t>(S2CR_Type::kBypass)) {
      return DmaTranslationResult::Ok(iova);
    }
    if (s2cr_type == static_cast<uint32_t>(S2CR_Type::kFault)) {
      return DmaTranslationResult::Fault(DmaFaultType::kInvalidContext);
    }
    if (s2cr_type != static_cast<uint32_t>(S2CR_Type::kTranslation)) {
      return DmaTranslationResult::Fault(DmaFaultType::kInvalidContext);
    }

    const uint32_t cb_ndx = s2cr & 0xffu;
    if (cb_ndx >= cfg_.num_cbs) {
      return DmaTranslationResult::Fault(DmaFaultType::kInvalidContext);
    }

    // 4. Inspect Context Bank cb_ndx.
    const ContextBankRegs& cb = cbs_[cb_ndx];
    const bool sctlr_m = (cb.sctlr & 0x1u) != 0;
    if (!sctlr_m) {
      return DmaTranslationResult::Ok(iova);
    }

    // CBAR.TYPE (bits [17:16]) selects what the context bank does. Only "stage
    // 1 with stage 2 bypass" (0b01) yields a stage 1 translation result.
    // "Stage 1 with stage 2 fault" (0b10) faults every transaction. A stage 2
    // or nested context is never what this driver intends to route a stream to.
    const uint32_t cbar_type = (gr1_.cbar[cb_ndx] >> 16) & 0x3u;
    if (cbar_type == 2) {
      return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
    }
    if (cbar_type != 1) {
      RecordHazard(
          "Stream 0x%x is routed to context bank %u whose CBAR.TYPE is %u (not a stage 1 context)",
          stream_id, cb_ndx, cbar_type);
      return DmaTranslationResult::Fault(DmaFaultType::kInvalidContext);
    }

    // S2CR.PRIVCFG (bits [25:24]) and S2CR.INSTCFG (bits [27:26]) may override
    // the privilege and instruction/data attributes the device presents.
    //
    // PRIVCFG 0b00 is "as presented" (0b01 is reserved while IDR2.DIPANS == 0,
    // which is what this model advertises). The model cannot know what the
    // device presents, so such an access is treated as permitted if either
    // privilege would permit it. A driver that relies on unprivileged
    // permissions without forcing the stream to unprivileged is then caught
    // when a probe that should be denied is not.
    const uint32_t privcfg = (s2cr >> 24) & 0x3u;
    const bool check_unprivileged = privcfg != 3;
    const bool check_privileged = privcfg != 2;
    switch ((s2cr >> 26) & 0x3u) {
      case 2:  // Forced to data: an instruction fetch is checked as a read.
        if (access_perms & IOMMU_FLAG_PERM_EXECUTE) {
          access_perms = (access_perms & ~IOMMU_FLAG_PERM_EXECUTE) | IOMMU_FLAG_PERM_READ;
        }
        break;
      case 3:  // Forced to instruction (writes are always data in SMMUv2).
        if (access_perms & IOMMU_FLAG_PERM_READ) {
          access_perms = (access_perms & ~IOMMU_FLAG_PERM_READ) | IOMMU_FLAG_PERM_EXECUTE;
        }
        break;
      default:
        break;
    }

    // TCR2.PASize (bits [2:0]); reserved encodings are treated as 48 bits.
    static constexpr uint32_t kPaSizeBits[8] = {32, 36, 40, 42, 44, 48, 48, 48};
    const uint32_t output_bits = cfg_.enforce_tcr2_fields ? kPaSizeBits[cb.tcr2 & 0x7u] : 64;
    const auto fits_output = [output_bits](paddr_t pa) {
      return output_bits >= 64 || (pa >> output_bits) == 0;
    };

    const bool va64 = (gr1_.cba2r[cb_ndx] & 0x1u) != 0;
    if (!va64) {
      return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
    }

    const bool epd0 = (cb.tcr & (1u << 7)) != 0;
    if (epd0) {
      return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
    }

    // Only the 4KB granule (TCR.TG0 == 0) is modelled.
    const uint32_t tg0 = (cb.tcr >> 14) & 0x3u;
    if (tg0 != 0) {
      return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
    }

    // An input address outside the TTBR0 region is a translation fault (VMSAv8-64), not an
    // address size fault.
    const uint32_t t0sz = cb.tcr & 0x3fu;
    const uint64_t max_iova = (t0sz == 0) ? ~uint64_t{0} : ((1ULL << (64 - t0sz)) - 1ULL);
    if (iova > max_iova) {
      // Outside the lower region. Addresses in the upper region are resolved
      // through a second root unless that is disabled (EPD1), and the model has
      // no second root.
      const bool epd1 = (cb.tcr & (1u << 23)) != 0;
      const uint32_t t1sz = (cb.tcr >> 16) & 0x3fu;
      const uint64_t upper_start = (t1sz == 0) ? 0 : (0 - (1ULL << (64 - t1sz)));
      if (!epd1 && iova >= upper_start) {
        RecordHazard(
            "Access to the upper input address region with EPD1 == 0: hardware would walk the "
            "second translation table root, which the driver never sets up [cb %u iova 0x%lx]",
            cb_ndx, iova);
      }
      return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
    }

    const uint16_t asid = EffectiveAsid(cb_ndx);
    const paddr_t root_pa = cb.ttbr0 & kDescAddrMask;
    if (root_pa == 0) {
      return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
    }

    // 5. Walk 4-level VMSAv8-64 Stage-1 translation tables in PoC DRAM view,
    // cross-checking against the multi-version PWC/IOTLB.
    paddr_t current_table_pa = root_pa;
    for (uint32_t level = 0; level < 4; ++level) {
      last_walk_level_ = level;
      const uint32_t shift = 12 + (3 - level) * 9;
      const uint32_t idx = static_cast<uint32_t>((iova >> shift) & 0x1ffu);
      const uint64_t prefix_mask = ~((1ULL << shift) - 1ULL);
      const TlbKey key{
          .asid = asid,
          .level = static_cast<uint8_t>(level),
          .iova_prefix = iova & prefix_mask,
      };

      const std::optional<uint64_t> maybe_pte =
          PmmMock::Get().ReadPocDram64(current_table_pa + idx * sizeof(uint64_t));
      if (!maybe_pte.has_value()) {
        return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
      }
      const uint64_t pte = *maybe_pte;

      auto tlb_it = multi_version_tlb_.find(key);
      if ((pte & kDescValid) == 0) {
        // If PoC DRAM says Invalid, but a stale valid version still exists in
        // the multi-version PWC/IOTLB (missing TLBIVA/TLBIASID + TLBSYNC),
        // report a stale TLB hit!
        if (tlb_it != multi_version_tlb_.end() && !tlb_it->second.empty()) {
          RecordHazard(
              "Stale PWC/IOTLB hit on a descriptor that is invalid in PoC DRAM (missing "
              "TLBI/TLBSYNC) [cb %u asid 0x%x level %u iova 0x%lx: cached 0x%lx]",
              cb_ndx, asid, level, iova, tlb_it->second.begin()->first);
          return DmaTranslationResult::Fault(DmaFaultType::kStaleTlbHit);
        }
        return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
      }

      // Any cached version that differs from PoC DRAM means hardware could use either.
      if (tlb_it != multi_version_tlb_.end()) {
        for (const auto& [cached_val, cached_from] : tlb_it->second) {
          if (cached_val != pte) {
            RecordHazard(
                "PWC/IOTLB holds a version that disagrees with PoC DRAM (missing cache clean or "
                "TLBI/TLBSYNC) [cb %u asid 0x%x level %u iova 0x%lx: cached 0x%lx vs poc 0x%lx]",
                cb_ndx, asid, level, iova, cached_val, pte);
            return DmaTranslationResult::Fault(DmaFaultType::kTlbMatchConflict);
          }
        }
      }

      if (level < 3) {
        // Must be a valid Table descriptor (bits[1:0] == 0b11).
        if ((pte & 0x3ULL) != 0x3ULL) {
          return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
        }
        current_table_pa = pte & kDescAddrMask;
        if (!fits_output(current_table_pa)) {
          return DmaTranslationResult::Fault(DmaFaultType::kAddressSize);
        }
      } else {
        // Level 3 Leaf Page descriptor (bits[1:0] == 0b11).
        if ((pte & 0x3ULL) != 0x3ULL) {
          return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
        }
        // Verify AF == 1 (bit 10) and DBM == 0 (bit 51).
        if ((pte & (1ULL << 10)) == 0) {
          RecordHazard("Hardware walked valid leaf PTE with AF=0 [cb %u iova 0x%lx: pte 0x%lx]",
                       cb_ndx, iova, pte);
          return DmaTranslationResult::Fault(DmaFaultType::kAccessFlag);
        }
        if ((pte & (1ULL << 51)) != 0) {
          RecordHazard("Hardware walked valid leaf PTE with DBM=1 [cb %u iova 0x%lx: pte 0x%lx]",
                       cb_ndx, iova, pte);
        }

        // Check permissions against access_perms for the privilege(s) the
        // stream's transactions can have.
        const bool permitted = (check_unprivileged && DescLeafPermits(pte, access_perms, false)) ||
                               (check_privileged && DescLeafPermits(pte, access_perms, true));
        if (!permitted) {
          return DmaTranslationResult::Fault(DmaFaultType::kPermission);
        }

        if (!fits_output(pte & kDescAddrMask)) {
          return DmaTranslationResult::Fault(DmaFaultType::kAddressSize);
        }
        const paddr_t leaf_pa = (pte & kDescAddrMask) | (iova & 0xfffULL);
        return DmaTranslationResult::Ok(leaf_pa);
      }
    }

    return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
  }

  // FakeMmioHandler implementation
  uint32_t Read32(uint64_t offset) override {
    const uint64_t page_size = cfg_.page_size;
    const uint64_t cb0_offset = page_size * cfg_.num_pages;

    if (offset < page_size) {
      return ReadGr0_32(offset);
    }
    if (offset >= page_size && offset < 2 * page_size) {
      return ReadGr1_32(offset - page_size);
    }
    if (offset >= cb0_offset) {
      const uint32_t cb_ndx = static_cast<uint32_t>((offset - cb0_offset) / page_size);
      const uint64_t cb_off = (offset - cb0_offset) % page_size;
      if (cb_ndx < cfg_.num_cbs) {
        return ReadCb_32(cb_ndx, cb_off);
      }
    }
    RecordHazard("Read32 out-of-range MMIO offset 0x%lx", offset);
    return 0;
  }

  void Write32(uint64_t offset, uint32_t val) override {
    const uint64_t page_size = cfg_.page_size;
    const uint64_t cb0_offset = page_size * cfg_.num_pages;

    if (offset < page_size) {
      WriteGr0_32(offset, val);
      return;
    }
    if (offset >= page_size && offset < 2 * page_size) {
      WriteGr1_32(offset - page_size, val);
      return;
    }
    if (offset >= cb0_offset) {
      const uint32_t cb_ndx = static_cast<uint32_t>((offset - cb0_offset) / page_size);
      const uint64_t cb_off = (offset - cb0_offset) % page_size;
      if (cb_ndx < cfg_.num_cbs) {
        WriteCb_32(cb_ndx, cb_off, val);
        return;
      }
    }
    RecordHazard("Write32 out-of-range MMIO offset 0x%lx (val=0x%x)", offset, val);
  }

  uint64_t Read64(uint64_t offset) override {
    const uint64_t page_size = cfg_.page_size;
    const uint64_t cb0_offset = page_size * cfg_.num_pages;

    if (offset < page_size) {
      if (offset == 0x040) {  // GFAR
        return gr0_.gfar;
      }
      RecordHazard("Read64 unexpected GR0 offset 0x%lx", offset);
      return 0;
    }
    if (offset >= cb0_offset) {
      const uint32_t cb_ndx = static_cast<uint32_t>((offset - cb0_offset) / page_size);
      const uint64_t cb_off = (offset - cb0_offset) % page_size;
      if (cb_ndx < cfg_.num_cbs) {
        return ReadCb_64(cb_ndx, cb_off);
      }
    }
    RecordHazard("Read64 out-of-range MMIO offset 0x%lx", offset);
    return 0;
  }

  void Write64(uint64_t offset, uint64_t val) override {
    const uint64_t page_size = cfg_.page_size;
    const uint64_t cb0_offset = page_size * cfg_.num_pages;

    if (offset < page_size) {
      if (offset == 0x040) {  // GFAR
        gr0_.gfar = val;
        return;
      }
      RecordHazard("Write64 unexpected GR0 offset 0x%lx (val=0x%lx)", offset, val);
      return;
    }
    if (offset >= cb0_offset) {
      const uint32_t cb_ndx = static_cast<uint32_t>((offset - cb0_offset) / page_size);
      const uint64_t cb_off = (offset - cb0_offset) % page_size;
      if (cb_ndx < cfg_.num_cbs) {
        WriteCb_64(cb_ndx, cb_off, val);
        return;
      }
    }
    RecordHazard("Write64 out-of-range MMIO offset 0x%lx (val=0x%lx)", offset, val);
  }

 private:
  struct PendingTlbi {
    bool everything = false;
    bool all_asid = false;
    // "Last level" invalidation by address: only the leaf entry is discarded.
    bool leaf_only = false;
    uint16_t asid = 0;
    uint64_t va = 0;
  };

  struct ContextBankRegs {
    uint32_t sctlr = 0;
    uint32_t actlr = 0;
    uint32_t tcr2 = 0;
    uint64_t ttbr0 = 0;
    uint64_t ttbr1 = 0;
    uint32_t tcr = 0;
    uint32_t prrr_mair0 = 0;
    uint32_t nmrr_mair1 = 0;
    uint32_t fsr = 0;
    uint64_t far = 0;
    uint32_t fsynr0 = 0;
    uint32_t fsynr1 = 0;
    uint32_t tlb_poll_remaining = 0;
    // Whether a TLBSYNC has been written whose completion has not yet been read back.
    bool sync_active = false;
    // Posted, not yet covered by a sync.
    std::vector<PendingTlbi> pending_tlbis;
    // Covered by a sync whose completion has not yet been observed.
    std::vector<PendingTlbi> syncing_tlbis;
  };

  struct TableOwnership {
    uint16_t asid = 0;
    uint8_t level = 0;
    uint64_t base_iova = 0;
  };

  void RecordHazard(const char* fmt, ...) __PRINTFLIKE(2, 3) {
    char buf[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    hazards_.emplace_back(buf);
    if (assert_on_hazard_) {
      ZX_ASSERT_MSG(false, "SimSmmuHardwareModel Hazard: %s", buf);
    }
  }

  uint32_t ReadGr0_32(uint64_t offset) {
    switch (offset) {
      case 0x000:
        return gr0_.cr0;
      case 0x008:
        return gr0_.cr2;
      case 0x010:
        return gr0_.acr;
      case 0x020:
        return gr0_.idr0;
      case 0x024:
        return gr0_.idr1;
      case 0x028:
        return gr0_.idr2;
      case 0x03c:
        return gr0_.idr7;
      case 0x048:
        return gr0_.gfsr;
      case 0x050:
        return gr0_.gfsynr0;
      case 0x054:
        return gr0_.gfsynr1;
      case 0x058:
        return gr0_.gfsynr2;
      case 0x074: {  // TLBGSTATUS
        // GSACTIVE describes the most recent sTLBGSYNC (IHI 0062D.c section
        // 9.6.21): with no sync outstanding it reads as idle. The
        // invalidations take effect at the read which first reports
        // completion, so a driver which does not poll never sees them applied.
        if (!global_sync_active_) {
          return 0u;
        }
        if (tlbg_poll_remaining_ > 0) {
          --tlbg_poll_remaining_;
          return 1u;
        }
        global_sync_active_ = false;
        CommitTlbis(global_syncing_tlbis_);
        return 0u;
      }
      default:
        break;
    }
    if (offset >= 0x800 && offset < 0x800 + 4 * 128 && (offset % 4 == 0)) {
      const uint32_t idx = static_cast<uint32_t>((offset - 0x800) / 4);
      return gr0_.smr[idx];
    }
    if (offset >= 0xc00 && offset < 0xc00 + 4 * 128 && (offset % 4 == 0)) {
      const uint32_t idx = static_cast<uint32_t>((offset - 0xc00) / 4);
      return gr0_.s2cr[idx];
    }
    // Registers which exist but which this model has no state for read as
    // zero: sCR1 (0x004), IDR3-IDR6 (0x02c-0x038) and the halves of the 64-bit
    // sGFAR (0x040, 0x044).
    switch (offset) {
      case 0x004:
      case 0x02c:
      case 0x030:
      case 0x034:
      case 0x038:
      case 0x040:
      case 0x044:
        return 0;
      default:
        break;
    }
    Unmodelled("32-bit read of", "GR0", offset, 0);
    return 0;
  }

  // Returns true if the global fault interrupt is asserted and would reach the
  // CPU: a global fault is recorded, sCR0.GFIE (bit 2) is set, and the
  // interrupt is not masked.
  bool GlobalIrqDeliverable() const {
    return gr0_.gfsr != 0 && (gr0_.cr0 & (1u << 2)) != 0 &&
           !InterruptMock::Get().IsMasked(cfg_.global_irq_num);
  }

  // Returns true if context bank |cb|'s fault interrupt is asserted and would
  // reach the CPU: a fault is recorded in FSR, SCTLR.CFIE (bit 6) is set, and
  // the interrupt is not masked.
  bool ContextIrqDeliverable(uint32_t cb) const {
    return cbs_[cb].fsr != 0 && (cbs_[cb].sctlr & (1u << 6)) != 0 &&
           !InterruptMock::Get().IsMasked(cfg_.base_context_irq_num + cb);
  }

  // Records a global fault. A fault which arrives while one is already recorded
  // sets MULTI (bit 31) and does not overwrite the first fault's syndrome.
  void LatchGlobalFault(uint32_t gfsr, uint64_t gfar, uint32_t gfsynr0, uint16_t stream_id) {
    if (gr0_.gfsr != 0) {
      gr0_.gfsr |= gfsr | (1u << 31);
      return;
    }
    gr0_.gfsr = gfsr;
    gr0_.gfar = gfar;
    gr0_.gfsynr0 = gfsynr0;
    gr0_.gfsynr1 = stream_id;
  }

  // Records a context fault. A fault which arrives while one is already
  // recorded sets MULTI (bit 31) and does not overwrite the first fault's
  // syndrome.
  void LatchContextFault(uint32_t cb_ndx, uint32_t fsr, uint64_t far, uint32_t fsynr0,
                         uint16_t stream_id) {
    if (cbs_[cb_ndx].fsr != 0) {
      cbs_[cb_ndx].fsr |= fsr | (1u << 31);
      return;
    }
    cbs_[cb_ndx].fsr = fsr;
    cbs_[cb_ndx].far = far;
    cbs_[cb_ndx].fsynr0 = fsynr0;
    gr1_.cbfrsynra[cb_ndx] = stream_id;
  }

  // Records a hazard for an access to a register this model does not
  // implement. Accepting such an access silently would make a wrong register
  // offset in the driver look like a successful access.
  void Unmodelled(const char* access, const char* space, uint64_t offset, uint64_t val) {
    RecordHazard("%s unmodelled %s register at offset 0x%lx (value 0x%lx)", access, space, offset,
                 val);
  }

  void WriteGr0_32(uint64_t offset, uint32_t val) {
    switch (offset) {
      case 0x000:
        gr0_.cr0 = val;
        EagerSpeculativeWalkAllEnabledCbs();
        CheckRouting("a write to sCR0");
        return;
      case 0x008:
        gr0_.cr2 = val;
        return;
      case 0x010:
        gr0_.acr = val;
        return;
      case 0x048:  // GFSR is Write-1-to-Clear (W1C)
        gr0_.gfsr &= ~val;
        return;
      case 0x04c:  // GFSRRESTORE
        gr0_.gfsr |= val;
        return;
      case 0x050:
        gr0_.gfsynr0 = val;
        return;
      case 0x054:
        gr0_.gfsynr1 = val;
        return;
      case 0x058:
        gr0_.gfsynr2 = val;
        return;
      case 0x060:  // STLBIALL
      case 0x068:  // TLBIALLNSNH
      case 0x06c:  // TLBIALLH
        pending_global_tlbis_.push_back(PendingTlbi{.everything = true});
        return;
      case 0x070:  // TLBGSYNC
        // A sync accepted in the global address space covers invalidations
        // accepted in the global address space and in every context bank
        // (section 5.4.2). Operations posted after this point are not covered.
        ++tlb_gsync_call_count_;
        MoveAppend(pending_global_tlbis_, global_syncing_tlbis_);
        for (uint32_t i = 0; i < cfg_.num_cbs; ++i) {
          MoveAppend(cbs_[i].pending_tlbis, global_syncing_tlbis_);
        }
        global_sync_active_ = true;
        tlbg_poll_remaining_ = cfg_.simulate_tlb_sync_poll_delay ? 1 : 0;
        return;
      default:
        break;
    }
    if (offset >= 0x800 && offset < 0x800 + 4 * 128 && (offset % 4 == 0)) {
      const uint32_t idx = static_cast<uint32_t>((offset - 0x800) / 4);
      if (idx >= cfg_.num_smrgs) {
        Unmodelled("write to unimplemented SMR beyond NUMSMRG:", "GR0", offset, val);
        return;
      }
      const uint32_t valid_bit = val & (1u << 31);
      gr0_.smr[idx] = valid_bit | (val & smr_implemented_mask_);
      if (valid_bit != 0) {
        VerifySmrgEnableOrdering(idx);
      }
      CheckRouting("a write to SMR");
      return;
    }
    if (offset >= 0xc00 && offset < 0xc00 + 4 * 128 && (offset % 4 == 0)) {
      const uint32_t idx = static_cast<uint32_t>((offset - 0xc00) / 4);
      if (idx >= cfg_.num_smrgs) {
        Unmodelled("write to unimplemented S2CR beyond NUMSMRG:", "GR0", offset, val);
        return;
      }
      gr0_.s2cr[idx] = val;
      if ((gr0_.smr[idx] & (1u << 31)) != 0) {
        VerifySmrgEnableOrdering(idx);
      }
      CheckRouting("a write to S2CR");
      return;
    }
    Unmodelled("32-bit write to", "GR0", offset, val);
  }

  uint32_t ReadGr1_32(uint64_t offset) {
    if (offset < 0x400 && (offset % 4 == 0)) {
      return gr1_.cbar[offset / 4];
    }
    if (offset >= 0x400 && offset < 0x800 && (offset % 4 == 0)) {
      return gr1_.cbfrsynra[(offset - 0x400) / 4];
    }
    if (offset >= 0x800 && offset < 0xc00 && (offset % 4 == 0)) {
      return gr1_.cba2r[(offset - 0x800) / 4];
    }
    Unmodelled("32-bit read of", "GR1", offset, 0);
    return 0;
  }

  void WriteGr1_32(uint64_t offset, uint32_t val) {
    if (offset < 0x400 && (offset % 4 == 0)) {
      gr1_.cbar[offset / 4] = val;
      CheckRouting("a write to CBAR");
      return;
    }
    if (offset >= 0x400 && offset < 0x800 && (offset % 4 == 0)) {
      gr1_.cbfrsynra[(offset - 0x400) / 4] = val;
      return;
    }
    if (offset >= 0x800 && offset < 0xc00 && (offset % 4 == 0)) {
      gr1_.cba2r[(offset - 0x800) / 4] = val;
      return;
    }
    Unmodelled("32-bit write to", "GR1", offset, val);
  }

  uint32_t ReadCb_32(uint32_t cb_ndx, uint64_t offset) {
    ContextBankRegs& cb = cbs_[cb_ndx];
    switch (offset) {
      case 0x000:
        return cb.sctlr;
      case 0x004:
        return cb.actlr;
      case 0x010:
        return cb.tcr2;
      case 0x030:
        return cb.tcr;
      case 0x038:
        return cb.prrr_mair0;
      case 0x03c:
        return cb.nmrr_mair1;
      case 0x058:
        return cb.fsr;
      case 0x068:
        return cb.fsynr0;
      case 0x06c:
        return cb.fsynr1;
      case 0x7f4: {  // TLBSTATUS
        // SACTIVE describes the most recent TLBSYNC (section 16.5.36): with no
        // sync outstanding it reads as idle. The invalidations take effect at
        // the read which first reports completion.
        if (!cb.sync_active) {
          return 0u;
        }
        if (cb.tlb_poll_remaining > 0) {
          --cb.tlb_poll_remaining;
          return 1u;
        }
        cb.sync_active = false;
        CommitTlbis(cb.syncing_tlbis);
        return 0u;
      }
      case 0x034:  // CONTEXTIDR exists but the model has no state for it.
        return 0;
      default:
        Unmodelled("32-bit read of", "context bank", offset, 0);
        return 0;
    }
  }

  void WriteCb_32(uint32_t cb_ndx, uint64_t offset, uint32_t val) {
    ContextBankRegs& cb = cbs_[cb_ndx];
    switch (offset) {
      case 0x000:  // SCTLR
        cb.sctlr = val;
        EagerSpeculativeWalkCb(cb_ndx);
        CheckRouting("a write to SCTLR");
        return;
      case 0x004:  // ACTLR
        cb.actlr = val;
        return;
      case 0x010: {  // TCR2
        // Check §4.4.1 item 8: TCR2.HA (bit 10) and TCR2.HD (bit 11) must be 0.
        if ((val & ((1u << 10) | (1u << 11))) != 0) {
          RecordHazard("CB %u wrote non-zero TCR2.HA/HD (TCR2=0x%x)", cb_ndx, val);
        }
        cb.tcr2 = val;
        return;
      }
      case 0x030:  // TCR
        cb.tcr = val;
        EagerSpeculativeWalkCb(cb_ndx);
        return;
      case 0x038:
        cb.prrr_mair0 = val;
        return;
      case 0x03c:
        cb.nmrr_mair1 = val;
        return;
      case 0x058:  // FSR is Write-1-to-Clear (W1C)
        cb.fsr &= ~val;
        return;
      case 0x05c:  // FSRRESTORE
        cb.fsr |= val;
        return;
      case 0x068:
        cb.fsynr0 = val;
        return;
      case 0x06c:
        cb.fsynr1 = val;
        return;
      case 0x610:  // TLBIASID
        cb.pending_tlbis.push_back(PendingTlbi{
            .all_asid = true,
            .asid = static_cast<uint16_t>(val & 0xffffu),
            .va = 0,
        });
        CheckTlbiAsid(cb_ndx, static_cast<uint16_t>(val & 0xffffu));
        return;
      case 0x618: {  // TLBIALL
        // TLBIALL takes no argument. For a non-secure stage 1 context it "only
        // has to apply to TLB entries associated with the VMID" of the context
        // bank (IHI 0062D.c section 16.5.30), so the least a driver can rely
        // on is: every entry of every context bank which shares this bank's
        // VMID (CBAR bits [7:0]).
        const uint32_t vmid = gr1_.cbar[cb_ndx] & 0xffu;
        for (uint32_t other = 0; other < cfg_.num_cbs; ++other) {
          if ((gr1_.cbar[other] & 0xffu) == vmid) {
            cb.pending_tlbis.push_back(PendingTlbi{
                .all_asid = true,
                .asid = EffectiveAsid(other),
                .va = 0,
            });
          }
        }
        return;
      }
      case 0x7f0:  // TLBSYNC
        // A sync accepted in a context bank covers only invalidations accepted
        // in that context bank (section 5.4.2).
        ++tlb_sync_call_count_;
        MoveAppend(cb.pending_tlbis, cb.syncing_tlbis);
        cb.sync_active = true;
        cb.tlb_poll_remaining = cfg_.simulate_tlb_sync_poll_delay ? 1 : 0;
        return;
      // 64-bit registers must be written with a single 64-bit access; the
      // model does not assemble them from two halves.
      case 0x020:
      case 0x024:
      case 0x028:
      case 0x02c:
      case 0x060:
      case 0x064:
      case 0x600:
      case 0x604:
      case 0x620:
      case 0x624:
        Unmodelled("32-bit write to 64-bit", "context bank", offset, val);
        return;
      default:
        Unmodelled("32-bit write to", "context bank", offset, val);
        return;
    }
  }

  uint64_t ReadCb_64(uint32_t cb_ndx, uint64_t offset) {
    const ContextBankRegs& cb = cbs_[cb_ndx];
    switch (offset) {
      case 0x020:
        return cb.ttbr0;
      case 0x028:
        return cb.ttbr1;
      case 0x060:
        return cb.far;
      case 0x050:  // PAR exists but the model has no state for it.
        return 0;
      default:
        Unmodelled("64-bit read of", "context bank", offset, 0);
        return 0;
    }
  }

  void WriteCb_64(uint32_t cb_ndx, uint64_t offset, uint64_t val) {
    ContextBankRegs& cb = cbs_[cb_ndx];
    switch (offset) {
      case 0x020: {  // TTBR0_64Bit
        cb.ttbr0 = val;
        const paddr_t root_pa = val & kDescAddrMask;
        const uint16_t asid = EffectiveAsid(cb_ndx);
        if (root_pa != 0 && asid != 0xffffu) {
          table_ownership_[root_pa] = TableOwnership{.asid = asid, .level = 0, .base_iova = 0};
        }
        EagerSpeculativeWalkCb(cb_ndx);
        return;
      }
      case 0x028:  // TTBR1_64Bit
        cb.ttbr1 = val;
        EagerSpeculativeWalkCb(cb_ndx);
        return;
      case 0x060:  // FAR
        cb.far = val;
        return;
      case 0x600: {  // TLBIVA_AArch64
        const uint16_t asid = static_cast<uint16_t>(val >> 48);
        const uint64_t va = (val & 0x0000'0fff'ffff'ffffULL) << 12;
        CheckTlbiAsid(cb_ndx, asid);
        cb.pending_tlbis.push_back(PendingTlbi{
            .all_asid = false,
            .asid = asid,
            .va = va,
        });
        return;
      }
      case 0x620: {  // TLBIVAL_AArch64: as TLBIVA, but last level only.
        const uint16_t asid = static_cast<uint16_t>(val >> 48);
        const uint64_t va = (val & 0x0000'0fff'ffff'ffffULL) << 12;
        CheckTlbiAsid(cb_ndx, asid);
        cb.pending_tlbis.push_back(PendingTlbi{
            .all_asid = false,
            .leaf_only = true,
            .asid = asid,
            .va = va,
        });
        return;
      }
      default:
        Unmodelled("64-bit write to", "context bank", offset, val);
        return;
    }
  }

  // Checks where every stream currently goes. Called after each register write
  // which can change routing, so that a state which exists only between two
  // writes is still seen.
  //
  // Once the SMMU is enabled for client transactions (sCR0.CLIENTPD == 0):
  // - No two valid stream match registers may match the same stream ID.
  // - A stream routed for translation must point at an implemented, stage 1
  //   context bank.
  // - When the driver is operating in enforced mode, no stream may be able to
  //   bypass translation: not through a bypass-type S2CR, not through a context
  //   bank whose SCTLR.M is clear, and not by matching nothing while
  //   sCR0.USFCFG is clear.
  //
  // Groups set up by the bootloader are exempt: the driver adopts those as they
  // are.
  void CheckRouting(const char* cause) {
    if ((gr0_.cr0 & 0x1u) != 0) {
      return;
    }
    const bool enforced = GetSmmuMode(BootOptions::Get()->arm_smmu_mode.data(), cfg_.mmio_phys) ==
                          ktl::optional<ArmSmmuMode>(ArmSmmuMode::kEnforced);

    if (enforced && (gr0_.cr0 & (1u << 10)) == 0) {
      RecordHazard(
          "After %s: client transactions are enabled with sCR0.USFCFG == 0 in enforced mode, so "
          "a stream which matches nothing bypasses translation",
          cause);
    }

    for (uint32_t i = 0; i < cfg_.num_smrgs; ++i) {
      const uint32_t smr = gr0_.smr[i];
      if ((smr & (1u << 31)) == 0) {
        continue;
      }
      const uint32_t id = smr & SidMask();
      const uint32_t mask = (smr >> 16) & SidMask();

      // Two match registers overlap if some stream ID satisfies both: their IDs
      // agree on every bit that neither one masks out.
      for (uint32_t j = i + 1; j < cfg_.num_smrgs; ++j) {
        const uint32_t other = gr0_.smr[j];
        if ((other & (1u << 31)) == 0) {
          continue;
        }
        const uint32_t other_id = other & SidMask();
        const uint32_t other_mask = (other >> 16) & SidMask();
        if (((id ^ other_id) & ~(mask | other_mask) & SidMask()) == 0) {
          RecordHazard(
              "After %s: stream match registers %u (0x%x) and %u (0x%x) are both valid and match "
              "a common stream ID",
              cause, i, smr, j, other);
        }
      }

      if (bootloader_smrg_[i]) {
        continue;
      }
      const uint32_t s2cr = gr0_.s2cr[i];
      const uint32_t type = (s2cr >> 16) & 0x3u;
      if (type == static_cast<uint32_t>(S2CR_Type::kTranslation)) {
        const uint32_t cb_ndx = s2cr & 0xffu;
        if (cb_ndx >= cfg_.num_cbs) {
          continue;  // Reported by VerifySmrgEnableOrdering.
        }
        const uint32_t cbar_type = (gr1_.cbar[cb_ndx] >> 16) & 0x3u;
        if (cbar_type != 1 && cbar_type != 2) {
          RecordHazard(
              "After %s: valid stream match register %u (0x%x) routes to context bank %u whose "
              "CBAR.TYPE is %u (not a stage 1 context)",
              cause, i, smr, cb_ndx, cbar_type);
        } else if (enforced && (cbs_[cb_ndx].sctlr & 0x1u) == 0) {
          RecordHazard(
              "After %s: valid stream match register %u (0x%x) routes to context bank %u whose "
              "SCTLR.M is 0, so its streams bypass translation in enforced mode",
              cause, i, smr, cb_ndx);
        }
      } else if (type == static_cast<uint32_t>(S2CR_Type::kBypass) && enforced) {
        RecordHazard(
            "After %s: valid stream match register %u (0x%x) has a bypass-type S2CR (0x%x) in "
            "enforced mode",
            cause, i, smr, s2cr);
      }
    }
  }

  void VerifySmrgEnableOrdering(uint32_t smrg_ndx) {
    const uint32_t s2cr = gr0_.s2cr[smrg_ndx];
    const uint32_t type = (s2cr >> 16) & 0x3u;
    if (type != static_cast<uint32_t>(S2CR_Type::kTranslation)) {
      return;
    }
    const uint32_t cb_ndx = s2cr & 0xffu;
    if (cb_ndx >= cfg_.num_cbs) {
      RecordHazard("SMRG %u enabled pointing to out-of-range CB %u", smrg_ndx, cb_ndx);
      return;
    }
    if (!cbs_[cb_ndx].pending_tlbis.empty() || !cbs_[cb_ndx].syncing_tlbis.empty()) {
      RecordHazard("SMRG %u enabled while CB %u still has unsynchronized TLBI operations", smrg_ndx,
                   cb_ndx);
    }
  }

  static void MoveAppend(std::vector<PendingTlbi>& from, std::vector<PendingTlbi>& to) {
    to.insert(to.end(), from.begin(), from.end());
    from.clear();
  }

  bool AnySyncInProgress() const {
    if (!global_syncing_tlbis_.empty()) {
      return true;
    }
    for (uint32_t i = 0; i < cfg_.num_cbs; ++i) {
      if (!cbs_[i].syncing_tlbis.empty()) {
        return true;
      }
    }
    return false;
  }

  // Applies and clears |ops|, then re-walks PoC DRAM.
  void CommitTlbis(std::vector<PendingTlbi>& ops) {
    for (const PendingTlbi& op : ops) {
      if (op.everything) {
        multi_version_tlb_.clear();
      } else if (op.all_asid) {
        for (auto it = multi_version_tlb_.begin(); it != multi_version_tlb_.end();) {
          if (it->first.asid == op.asid) {
            it = multi_version_tlb_.erase(it);
          } else {
            ++it;
          }
        }
      } else {
        // TLBIVA invalidates the L3 leaf entry for (asid, va) as well as any
        // cached L0..L2 walk cache entries covering va for that ASID.
        // TLBIVAL invalidates only the L3 leaf entry.
        for (uint8_t level = op.leaf_only ? 3 : 0; level < 4; ++level) {
          const uint32_t shift = 12 + (3 - level) * 9;
          const uint64_t prefix_mask = ~((1ULL << shift) - 1ULL);
          const TlbKey key{
              .asid = op.asid,
              .level = level,
              .iova_prefix = op.va & prefix_mask,
          };
          multi_version_tlb_.erase(key);
        }
      }
    }
    ops.clear();

    // Immediately perform an eager speculative walk of poc_dram_view after
    // committing TLB invalidations! If any cleared PTE was not flushed via
    // CleanDataCacheRange before TLBSYNC, poc_dram_view still holds the old
    // valid PTE and it will be immediately re-loaded here.
    EagerSpeculativeWalkAllEnabledCbs();
  }

  // Walks PoC DRAM from the root of the enabled context using |asid| and
  // returns the descriptor a walk for |iova| reads at |level|, or nullopt if no
  // enabled context uses |asid| or the walk cannot reach that level.
  std::optional<uint64_t> WalkPocToLevel(uint16_t asid, uint8_t level, uint64_t iova) const {
    for (uint32_t i = 0; i < cfg_.num_cbs; ++i) {
      if (!IsCbTranslationEnabled(i) || EffectiveAsid(i) != asid) {
        continue;
      }
      paddr_t table_pa = cbs_[i].ttbr0 & kDescAddrMask;
      for (uint8_t l = 0; l <= level; ++l) {
        const uint32_t shift = 12 + (3 - l) * 9;
        const uint64_t idx = (iova >> shift) & 0x1ffu;
        const std::optional<uint64_t> pte =
            PmmMock::Get().ReadPocDram64(table_pa + idx * sizeof(uint64_t));
        if (!pte.has_value()) {
          return std::nullopt;
        }
        if (l == level) {
          return pte;
        }
        if ((*pte & 0x3ULL) != 0x3ULL) {
          return std::nullopt;
        }
        table_pa = *pte & kDescAddrMask;
      }
    }
    return std::nullopt;
  }

  // Returns true if context bank |cb_ndx| uses 16-bit ASIDs. TCR2.AS (bit 4)
  // selects the width, but it is only honoured when the model is configured to
  // enforce TCR2 (see Config::enforce_tcr2_fields); otherwise ASIDs are taken
  // as 16-bit.
  bool AsidIs16Bit(uint32_t cb_ndx) const {
    return !cfg_.enforce_tcr2_fields || (cbs_[cb_ndx].tcr2 & (1u << 4)) != 0;
  }

  // Returns the ASID which tags context bank |cb_ndx|'s translations.
  //
  // TCR.A1 (bit 22) selects whether it comes from TTBR1 or TTBR0. With 8-bit
  // ASIDs the upper 8 bits are treated as zero for TLB matching (IHI 0062D.c
  // section 16.5.39).
  uint16_t EffectiveAsid(uint32_t cb_ndx) const {
    const ContextBankRegs& cb = cbs_[cb_ndx];
    const bool a1 = (cb.tcr & (1u << 22)) != 0;
    const uint16_t asid = static_cast<uint16_t>((a1 ? cb.ttbr1 : cb.ttbr0) >> 48);
    return AsidIs16Bit(cb_ndx) ? asid : static_cast<uint16_t>(asid & 0xffu);
  }

  // Checks the ASID operand of a TLB invalidation written to context bank
  // |cb_ndx|. With 8-bit ASIDs the upper 8 bits "must be written as 0"
  // (sections 16.5.31 and 16.5.32).
  void CheckTlbiAsid(uint32_t cb_ndx, uint16_t asid) {
    if (!AsidIs16Bit(cb_ndx) && (asid >> 8) != 0) {
      RecordHazard(
          "TLB invalidation on context bank %u names ASID 0x%x while TCR2.AS == 0: the upper 8 "
          "bits must be written as 0",
          cb_ndx, asid);
    }
  }

  bool IsCbTranslationEnabled(uint32_t cb_ndx) const {
    if (cb_ndx >= cfg_.num_cbs) {
      return false;
    }
    const ContextBankRegs& cb = cbs_[cb_ndx];
    const bool sctlr_m = (cb.sctlr & 0x1u) != 0;
    const bool epd0 = (cb.tcr & (1u << 7)) != 0;
    const paddr_t root_pa = cb.ttbr0 & kDescAddrMask;
    return sctlr_m && !epd0 && (root_pa != 0);
  }

  void EagerSpeculativeWalkAllEnabledCbs() {
    for (uint32_t i = 0; i < cfg_.num_cbs; ++i) {
      EagerSpeculativeWalkCb(i);
    }
  }

  void EagerSpeculativeWalkCb(uint32_t cb_ndx) {
    if (!IsCbTranslationEnabled(cb_ndx)) {
      return;
    }
    const ContextBankRegs& cb = cbs_[cb_ndx];
    const uint16_t asid = EffectiveAsid(cb_ndx);
    const paddr_t root_pa = cb.ttbr0 & kDescAddrMask;
    table_ownership_[root_pa] = TableOwnership{.asid = asid, .level = 0, .base_iova = 0};
    WalkTableInPocDram(asid, root_pa, 0, 0);
  }

  void WalkTableInPocDram(uint16_t asid, paddr_t table_pa, uint8_t level, uint64_t base_iova) {
    const uint32_t shift = 12 + (3 - level) * 9;
    for (uint32_t idx = 0; idx < kDescEntriesPerTable; ++idx) {
      const std::optional<uint64_t> maybe_pte =
          PmmMock::Get().ReadPocDram64(table_pa + idx * sizeof(uint64_t));
      if (!maybe_pte.has_value()) {
        RecordHazard("Speculative walk read unallocated table page 0x%lx at level %u", table_pa,
                     level);
        return;
      }
      const uint64_t pte = *maybe_pte;
      if ((pte & kDescValid) == 0) {
        if (pte != 0) {
          RecordHazard("Non-zero invalid PTE 0x%lx in PoC DRAM at table 0x%lx index %u level %u",
                       pte, table_pa, idx, level);
        }
        continue;
      }

      const uint64_t entry_iova = base_iova | (uint64_t{idx} << shift);
      const TlbKey key{.asid = asid, .level = level, .iova_prefix = entry_iova};
      auto& versions = multi_version_tlb_[key];
      const bool newly_loaded = versions.find(pte) == versions.end();
      versions[pte] = table_pa;
      // Report each conflicting version once, when it is first loaded.
      if (newly_loaded && versions.size() > 1) {
        RecordHazard(
            "Multi-version PWC/IOTLB conflict at ASID 0x%x level %u IOVA 0x%lx (new PTE 0x%lx)",
            asid, level, entry_iova, pte);
      }

      if (level < 3) {
        if ((pte & 0x3ULL) != 0x3ULL) {
          RecordHazard("Invalid non-table descriptor 0x%lx at level %u", pte, level);
          continue;
        }
        if ((pte & kDescTableHierAttrMask) != 0) {
          RecordHazard(
              "Speculative walk found table descriptor 0x%lx with hierarchical attribute bits "
              "[63:59] set at level %u",
              pte, level);
        }
        const paddr_t child_pa = pte & kDescAddrMask;
        table_ownership_[child_pa] = TableOwnership{
            .asid = asid,
            .level = static_cast<uint8_t>(level + 1),
            .base_iova = entry_iova,
        };
        WalkTableInPocDram(asid, child_pa, static_cast<uint8_t>(level + 1), entry_iova);
      } else {
        if ((pte & 0x3ULL) != 0x3ULL) {
          RecordHazard("Invalid L3 page descriptor bits[1:0] in 0x%lx", pte);
        }
        if ((pte & (1ULL << 10)) == 0 || (pte & (1ULL << 51)) != 0) {
          RecordHazard("Hardware-writable L3 PTE hazard in PoC DRAM (PTE=0x%lx: AF=%d, DBM=%d)",
                       pte, static_cast<int>((pte >> 10) & 1), static_cast<int>((pte >> 51) & 1));
        }
      }
    }
  }

  void OnPteWrite(paddr_t table_pa, uint32_t index, uint64_t old_val, uint64_t new_val,
                  uint32_t claimed_level) {
    // The driver says which level it believes it is writing. The model knows
    // the level of every table it has seen linked into a tree; when it does,
    // it trusts its own view and reports a disagreement.
    ++descriptor_write_count_;
    uint32_t level = claimed_level;
    if (auto it = table_ownership_.find(table_pa); it != table_ownership_.end()) {
      if (claimed_level != PmmMock::kPteLevelUnknown && it->second.level != claimed_level) {
        RecordHazard(
            "Descriptor write claims level %u but table 0x%lx is linked at level %u (index %u: "
            "new=0x%lx)",
            claimed_level, table_pa, it->second.level, index, new_val);
      }
      level = it->second.level;
    }

    // 1. Check Make-Before-Break: never overwrite a valid descriptor with a
    // different valid descriptor without breaking to 0 first. A valid value
    // that is still in PoC DRAM (cleared in the CPU view but never cleaned)
    // counts as "old".
    const uint64_t poc_val =
        PmmMock::Get().ReadPocDram64(table_pa + index * sizeof(uint64_t)).value_or(0);
    const uint64_t prev_val = (old_val & kDescValid) != 0 ? old_val : poc_val;
    if ((prev_val & kDescValid) != 0 && (new_val & kDescValid) != 0 && prev_val != new_val) {
      RecordHazard(
          "Make-Before-Break violation at table 0x%lx index %u level %u: cpu old=0x%lx, "
          "poc=0x%lx -> new=0x%lx",
          table_pa, index, level, old_val, poc_val, new_val);
    }

    if ((new_val & kDescValid) == 0) {
      if (new_val != 0) {
        RecordHazard("Wrote non-zero invalid descriptor 0x%lx at table 0x%lx index %u level %u",
                     new_val, table_pa, index, level);
      }
      return;
    }

    // 2. Check descriptor format and AF == 1 && DBM == 0 on valid L3 leaf PTEs.
    if (level < 3) {
      if ((new_val & 0x3ULL) != 0x3ULL) {
        RecordHazard("Wrote non-table valid descriptor 0x%lx at level %u", new_val, level);
      } else if ((new_val & kDescTableHierAttrMask) != 0) {
        RecordHazard(
            "Wrote table descriptor 0x%lx with hierarchical attribute bits [63:59] set, which the "
            "model does not apply (level %u)",
            new_val, level);
      }
    } else {
      if ((new_val & 0x3ULL) != 0x3ULL) {
        RecordHazard("Wrote invalid L3 page descriptor 0x%lx", new_val);
      }
      if ((new_val & (1ULL << 10)) == 0 || (new_val & (1ULL << 51)) != 0) {
        RecordHazard("Hardware-writable L3 PTE hazard on write (PTE=0x%lx: AF=%d, DBM=%d)", new_val,
                     static_cast<int>((new_val >> 10) & 1), static_cast<int>((new_val >> 51) & 1));
      }
    }

    // 3. Track table ownership and check for multi-version PWC/IOTLB conflicts
    // if the CPU cache spontaneously wrote back this dirty cache line now.
    auto own_it = table_ownership_.find(table_pa);
    if (own_it != table_ownership_.end()) {
      const TableOwnership own = own_it->second;
      const uint32_t shift = 12 + (3 - level) * 9;
      const uint64_t entry_iova = own.base_iova | (uint64_t{index} << shift);
      if (level < 3) {
        const paddr_t child_pa = new_val & kDescAddrMask;
        table_ownership_[child_pa] = TableOwnership{
            .asid = own.asid,
            .level = static_cast<uint8_t>(level + 1),
            .base_iova = entry_iova,
        };
      }
      const TlbKey key{
          .asid = own.asid,
          .level = static_cast<uint8_t>(level),
          .iova_prefix = entry_iova,
      };
      auto tlb_it = multi_version_tlb_.find(key);
      if (tlb_it != multi_version_tlb_.end()) {
        for (const auto& [cached_val, cached_from] : tlb_it->second) {
          if (cached_val != new_val) {
            RecordHazard(
                "Multi-version PWC/IOTLB conflict on PTE write at ASID 0x%x level %u IOVA 0x%lx: "
                "un-invalidated cached=0x%lx vs new=0x%lx",
                own.asid, level, entry_iova, cached_val, new_val);
          }
        }
      }

      // The dirty line may already have been written back and walked. From
      // this instant the new descriptor counts as cached until it is
      // invalidated and synced, even if the driver clears it again before
      // ever cleaning it.
      for (uint32_t i = 0; i < cfg_.num_cbs; ++i) {
        if (IsCbTranslationEnabled(i) && EffectiveAsid(i) == own.asid) {
          multi_version_tlb_[key][new_val] = table_pa;
          break;
        }
      }
    }
  }

  void OnCacheClean(paddr_t page_pa, size_t offset, size_t size) {
    ++clean_dcache_call_count_;
    EagerSpeculativeWalkAllEnabledCbs();
  }

  void OnPageFree(paddr_t page_pa) {
    // 1. Verify page_pa is not the active TTBR0 root of any enabled Context Bank.
    for (uint32_t i = 0; i < cfg_.num_cbs; ++i) {
      if (IsCbTranslationEnabled(i)) {
        const paddr_t root_pa = cbs_[i].ttbr0 & kDescAddrMask;
        if (root_pa == page_pa) {
          RecordHazard("Freed page 0x%lx while still set as active TTBR0 of CB %u", page_pa, i);
        }
      }
    }

    // Freeing translation table memory while a sync is still in progress means
    // the driver requested the sync but did not wait for it to complete.
    if (AnySyncInProgress()) {
      RecordHazard(
          "Page 0x%lx freed while a TLB sync is still in progress (completion was not awaited)",
          page_pa);
    }

    // 2. Verify no dirty CPU cache lines remain uncleaned relative to PoC DRAM.
    for (size_t offset = 0; offset < cfg_.page_size; offset += sizeof(uint64_t)) {
      const std::optional<uint64_t> cpu = PmmMock::Get().ReadCpuCache64(page_pa + offset);
      const std::optional<uint64_t> poc = PmmMock::Get().ReadPocDram64(page_pa + offset);
      if (cpu != poc) {
        RecordHazard(
            "Page 0x%lx freed with dirty CPU cache lines not cleaned to PoC (offset 0x%zx: "
            "cpu=0x%lx vs poc=0x%lx)",
            page_pa, offset, cpu.value_or(0), poc.value_or(0));
        break;
      }
    }

    // 3. Verify the PWC/IOTLB holds no descriptor that points at page_pa as a
    // next-level table, and no descriptor that was read out of page_pa. Either
    // means the page is being freed before its invalidation was issued and
    // synced.
    for (const auto& [key, versions] : multi_version_tlb_) {
      for (const auto& [pte, read_from] : versions) {
        const bool points_at = key.level < 3 && (pte & kDescAddrMask) == page_pa;
        if (points_at || read_from == page_pa) {
          RecordHazard(
              "Premature page-table free before TLBI/TLBSYNC: page 0x%lx still referenced by "
              "cached descriptor (ASID 0x%x, level %u, IOVA 0x%lx, PTE 0x%lx read from table "
              "0x%lx)",
              page_pa, key.asid, key.level, key.iova_prefix, pte, read_from);
        }
      }
    }

    table_ownership_.erase(page_pa);
  }

  Config cfg_;
  uint32_t smr_implemented_mask_ = 0x7fff'7fffu;
  bool assert_on_hazard_ = true;
  // Posted through the global register space, not yet covered by a sync.
  std::vector<PendingTlbi> pending_global_tlbis_;
  // Covered by an sTLBGSYNC whose completion has not yet been observed.
  std::vector<PendingTlbi> global_syncing_tlbis_;
  bool global_sync_active_ = false;
  uint32_t tlbg_poll_remaining_ = 0;
  size_t clean_dcache_call_count_ = 0;
  size_t tlb_sync_call_count_ = 0;
  size_t tlb_gsync_call_count_ = 0;
  size_t descriptor_write_count_ = 0;
  // Translation table level at which the most recent TranslateDma() walk ended
  // (the level a fault would be reported at).
  uint32_t last_walk_level_ = 0;

  struct Gr0Regs {
    uint32_t cr0 = 0;
    uint32_t cr2 = 0;
    uint32_t acr = 0;
    uint32_t idr0 = 0;
    uint32_t idr1 = 0;
    uint32_t idr2 = 0;
    uint32_t idr7 = 0;
    uint64_t gfar = 0;
    uint32_t gfsr = 0;
    uint32_t gfsynr0 = 0;
    uint32_t gfsynr1 = 0;
    uint32_t gfsynr2 = 0;
    std::array<uint32_t, 128> smr{};
    std::array<uint32_t, 128> s2cr{};
  } gr0_;

  struct Gr1Regs {
    std::array<uint32_t, 128> cbar{};
    std::array<uint32_t, 128> cbfrsynra{};
    std::array<uint32_t, 128> cba2r{};
  } gr1_;

  std::array<ContextBankRegs, 128> cbs_{};
  // Stream match register groups which were set up "by the bootloader" before
  // the driver started. The driver adopts these without changing how they
  // route, so the routing invariants in CheckRouting() do not apply to them.
  std::array<bool, 128> bootloader_smrg_{};
  // (asid, level, iova prefix) -> { cached descriptor value -> table page it was read from }.
  std::unordered_map<TlbKey, std::unordered_map<uint64_t, paddr_t>, TlbKeyHash> multi_version_tlb_;
  std::unordered_map<paddr_t, TableOwnership> table_ownership_;
  std::vector<std::string> hazards_;
};

}  // namespace arm_smmu::testing

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_SIM_SMMU_HARDWARE_MODEL_H_
