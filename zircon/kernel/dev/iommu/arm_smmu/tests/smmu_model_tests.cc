// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <stdio.h>
#include <zircon/errors.h>
#include <zircon/syscalls/iommu.h>
#include <zircon/types.h>

#include <chrono>
#include <map>
#include <memory>
#include <optional>
#include <random>
#include <vector>

#include <dev/arm_smmu/context_bank.h>
#include <dev/arm_smmu/smmu.h>
#include <dev/arm_smmu/smmu_bti.h>
#include <dev/arm_smmu/smmu_mode.h>
#include <dev/arm_smmu/smmu_pmt.h>
#include <dev/arm_smmu/stream_match_reg_group.h>
#include <dev/iommu/bti.h>
#include <dev/iommu/iommu.h>
#include <dev/iommu/pmt.h>
#include <fbl/ref_ptr.h>
#include <vm/pinned_vm_object.h>
#include <vm/pmm.h>
#include <zxtest/zxtest.h>

#include "sim_smmu_hardware_model.h"

namespace arm_smmu {
namespace testing {
namespace {

// Abstract reference specification of expected IOMMU translation behavior,
// verified after every operation against SimSmmuHardwareModel::TranslateDma.
class AbstractIommuSpec {
 public:
  struct MappedPage {
    paddr_t paddr = 0;
    uint32_t perms = 0;
  };

  struct BtiSpec {
    uint64_t bti_id = 0;
    uint16_t sid_val = 0;
    uint16_t sid_mask = 0;
    bool passthru = false;
    bool in_fault = false;
    std::map<uint64_t, MappedPage> iova_to_page;

    bool MatchesStreamId(uint16_t sid) const { return ((sid ^ sid_val) & ~sid_mask) == 0; }
  };

  explicit AbstractIommuSpec(ArmSmmuMode mode) : mode_(mode) {}

  void AddBti(uint64_t bti_id) {
    BtiSpec spec;
    spec.bti_id = bti_id;
    spec.sid_val = static_cast<uint16_t>(bti_id & 0x7fffu);
    spec.sid_mask = static_cast<uint16_t>((bti_id >> 16) & 0x7fffu);
    spec.passthru = (mode_ == ArmSmmuMode::kPassthru);
    btis_[bti_id] = std::move(spec);
  }

  void RemoveBti(uint64_t bti_id) { btis_.erase(bti_id); }

  void SetBtiFault(uint64_t bti_id, bool in_fault) {
    auto it = btis_.find(bti_id);
    if (it != btis_.end()) {
      it->second.in_fault = in_fault;
    }
  }

  void AddMapping(uint64_t bti_id, uint64_t base_iova, const std::vector<paddr_t>& paddrs,
                  uint32_t perms) {
    auto& bti = btis_.at(bti_id);
    for (size_t i = 0; i < paddrs.size(); ++i) {
      // The specification learns each mapping's device address from the
      // driver, so it cannot predict the address. When translating, it can
      // still require that the driver never hands out a device address which
      // is already part of a live mapping of the same BTI. (Without
      // translation the device address is the physical address, and two pins
      // of the same memory legitimately coincide.)
      if (!bti.passthru) {
        ZX_ASSERT_MSG(bti.iova_to_page.find(base_iova + i * kPageSize) == bti.iova_to_page.end(),
                      "driver mapped device address 0x%lx of BTI 0x%lx twice",
                      base_iova + i * kPageSize, bti_id);
      }
      bti.iova_to_page[base_iova + i * kPageSize] = MappedPage{
          .paddr = paddrs[i],
          .perms = perms,
      };
    }
  }

  void RemoveMapping(uint64_t bti_id, uint64_t base_iova, size_t num_pages) {
    auto it = btis_.find(bti_id);
    if (it == btis_.end()) {
      return;
    }
    for (size_t i = 0; i < num_pages; ++i) {
      it->second.iova_to_page.erase(base_iova + i * kPageSize);
    }
  }

  DmaTranslationResult TranslateDma(uint16_t stream_id, uint64_t iova,
                                    uint32_t access_perms) const {
    if (mode_ == ArmSmmuMode::kDisabled) {
      return DmaTranslationResult::Ok(iova);
    }

    const BtiSpec* matched = nullptr;
    for (const auto& [id, bti] : btis_) {
      if (bti.MatchesStreamId(stream_id)) {
        if (matched != nullptr) {
          return DmaTranslationResult::Fault(DmaFaultType::kStreamMatchConflict);
        }
        matched = &bti;
      }
    }

    if (matched == nullptr) {
      return DmaTranslationResult::Fault(DmaFaultType::kUnidentifiedStream);
    }

    if (matched->passthru) {
      if (matched->in_fault) {
        return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
      }
      return DmaTranslationResult::Ok(iova);
    }

    // Enforced Stage-1 translation mode:
    const uint64_t page_iova = iova & ~uint64_t{kPageSize - 1};
    const uint64_t page_off = iova & uint64_t{kPageSize - 1};
    auto it = matched->iova_to_page.find(page_iova);
    if (it == matched->iova_to_page.end()) {
      return DmaTranslationResult::Fault(DmaFaultType::kTranslation);
    }

    const uint32_t pte_perms = it->second.perms;
    // Note: in VMSAv8-64 (see GetPageEntryPerms in vmsav8_64.h), Write permission
    // implies Read permission, while Execute requires IOMMU_FLAG_PERM_EXECUTE and
    // Write requires IOMMU_FLAG_PERM_WRITE.
    const bool has_read = (pte_perms & (IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE)) != 0;
    const bool has_write = (pte_perms & IOMMU_FLAG_PERM_WRITE) != 0;
    const bool has_exec = (pte_perms & IOMMU_FLAG_PERM_EXECUTE) != 0;

    if ((access_perms & IOMMU_FLAG_PERM_READ) && !has_read) {
      return DmaTranslationResult::Fault(DmaFaultType::kPermission);
    }
    if ((access_perms & IOMMU_FLAG_PERM_WRITE) && !has_write) {
      return DmaTranslationResult::Fault(DmaFaultType::kPermission);
    }
    if ((access_perms & IOMMU_FLAG_PERM_EXECUTE) && !has_exec) {
      return DmaTranslationResult::Fault(DmaFaultType::kPermission);
    }

    return DmaTranslationResult::Ok(it->second.paddr | page_off);
  }

  // Verifies that the hardware model translates every mapped page of every
  // translating BTI the way this specification says, for the mapping's own
  // permissions and for each kind of access on its own (which, for a mapping
  // that does not grant it, is a probe that must be denied), and that the
  // pages just outside each mapping behave as the specification says too.
  bool VerifyAllMappings(SimSmmuHardwareModel& hw) const {
    bool ok = true;
    const auto check = [&](uint16_t sid, uint64_t iova, uint32_t perms) {
      const DmaTranslationResult expected = TranslateDma(sid, iova, perms);
      const DmaTranslationResult actual = hw.TranslateDma(sid, iova, perms);
      if (actual != expected) {
        printf(
            "spec mismatch: sid=0x%x iova=0x%lx perms=0x%x: actual fault %u pa 0x%lx, expected "
            "fault %u pa 0x%lx\n",
            sid, iova, perms, static_cast<uint32_t>(actual.fault), actual.paddr,
            static_cast<uint32_t>(expected.fault), expected.paddr);
        ok = false;
      }
    };
    for (const auto& [id, bti] : btis_) {
      if (bti.passthru) {
        continue;
      }
      size_t n = 0;
      for (const auto& [iova, page] : bti.iova_to_page) {
        // Vary the offset within the page so that it is not always the same low bits.
        const uint64_t offset = (n++ & 1) == 0 ? 0x40 : 0xfc8;
        check(bti.sid_val, iova + offset, page.perms);
        for (uint32_t perm :
             {IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE, IOMMU_FLAG_PERM_EXECUTE}) {
          check(bti.sid_val, iova + 0x10, perm);
        }
        if (iova >= kPageSize &&
            bti.iova_to_page.find(iova - kPageSize) == bti.iova_to_page.end()) {
          check(bti.sid_val, iova - 8, IOMMU_FLAG_PERM_READ);
        }
        if (bti.iova_to_page.find(iova + kPageSize) == bti.iova_to_page.end()) {
          check(bti.sid_val, iova + kPageSize, IOMMU_FLAG_PERM_READ);
        }
      }
    }
    return ok;
  }

  // Verifies that no two BTIs share a translation context. Each BTI owns its
  // own context; two BTIs resolving to the same one means a device can reach
  // another device's mappings.
  bool VerifyDistinctContexts(const SimSmmuHardwareModel& hw) const {
    if (mode_ == ArmSmmuMode::kDisabled) {
      return true;
    }
    std::map<uint32_t, uint64_t> owner_of_context;
    for (const auto& [id, bti] : btis_) {
      const std::optional<uint32_t> cb = hw.FindContextBankForStreamId(bti.sid_val);
      if (!cb.has_value()) {
        continue;
      }
      const auto [it, inserted] = owner_of_context.emplace(*cb, id);
      if (!inserted) {
        printf("BTIs 0x%lx and 0x%lx are both routed to context bank %u\n", it->second, id, *cb);
        return false;
      }
    }
    return true;
  }

 private:
  ArmSmmuMode mode_;
  std::map<uint64_t, BtiSpec> btis_;
};

// ============================================================================
// Detector self-tests
//
// The hardware model exists to catch driver mistakes, so each of its detectors
// needs a test that makes the mistake on purpose and checks that the detector
// fires. The driver does not make these mistakes, so the tests program the
// simulated registers and write descriptors directly. They mirror the Rust
// detector self-tests in v2/test_model.rs.
// ============================================================================

class DetectorFixture {
 public:
  static constexpr uint32_t kCb = 0;
  static constexpr uint16_t kAsid = 0x5a00;

  explicit DetectorFixture(const SimSmmuHardwareModel::Config& cfg = {}) : hw_(cfg) {
    PmmMock::Get().ClearAllocationLimit();
    // The routing checks depend on the operating mode the driver was asked for.
    BootOptions::Get()->SetArmSmmuMode("enforced");
    hw_.set_assert_on_hazard(false);
  }

  ~DetectorFixture() {
    // Stop routing and translation and flush the cache first so that tearing
    // the tables down is not itself reported.
    hw_.Write32(0x000, 1);  // sCR0.CLIENTPD
    WriteCb32(0x000, 0);
    TlbiAsid();
    Sync();
    while (!pages_.empty()) {
      const paddr_t pa = pages_.back()->paddr();
      ZeroAndClean(pa);
      Free(pa);
    }
  }

  // Allocates a translation table page which is zero in both the CPU and PoC views.
  paddr_t AllocTable() {
    vm_page_t* page = nullptr;
    paddr_t pa = 0;
    ZX_ASSERT(pmm_alloc_page(0, &page, &pa) == ZX_OK);
    pages_.push_back(page);
    ZeroAndClean(pa);
    return pa;
  }

  // Writes |val| into the CPU view of |table_pa|[idx], as the driver would.
  void Write(paddr_t table_pa, size_t idx, uint64_t val, uint32_t level) {
    vmsav8_64::WritePte(&Table(table_pa)[idx], val, level);
  }

  void Clean(paddr_t table_pa) {
    arch::CleanDataCacheRange(reinterpret_cast<vaddr_t>(Table(table_pa)), kPageSize);
  }

  // Returns |table_pa| to the PMM. The page must already be zero in both views.
  void Free(paddr_t table_pa) {
    for (auto it = pages_.begin(); it != pages_.end(); ++it) {
      if ((*it)->paddr() == table_pa) {
        VmPageDoublyLinkedList free_list;
        free_list.push_back(*it);
        pages_.erase(it);
        pmm_free(&free_list);
        return;
      }
    }
    ZX_PANIC("fixture does not own page 0x%lx", table_pa);
  }

  // Programs context bank kCb for stage 1 AArch64 translation through |root_pa|.
  void EnableContext(paddr_t root_pa) {
    hw_.Write32(GR1Offset() + 4 * kCb, 1u << 16);         // CBAR.TYPE = S1 with S2 bypass
    hw_.Write32(GR1Offset() + 0x800 + 4 * kCb, 1);        // CBA2R.VA64
    WriteCb64(0x020, (uint64_t{kAsid} << 48) | root_pa);  // TTBR0
    WriteCb32(0x030, (1u << 23) | 16u);                   // TCR: EPD1, T0SZ = 16
    WriteCb32(0x000, 1);                                  // SCTLR.M
    // Route kStreamId to the context so TranslateDma() reaches it.
    hw_.Write32(0x000, 1u << 10);                // sCR0: CLIENTPD = 0, USFCFG
    RouteStream(2);                              // S2CR0, forced to unprivileged as the driver does
    hw_.Write32(0x800, (1u << 31) | kStreamId);  // SMR0: valid
  }

  // Points S2CR0 at the fixture's context with the given PRIVCFG encoding.
  void RouteStream(uint32_t privcfg) { hw_.Write32(0xc00, (privcfg << 24) | kCb); }

  void WriteCbar(uint32_t val) { hw_.Write32(GR1Offset() + 4 * kCb, val); }
  void WriteTcr(uint32_t val) { WriteCb32(0x030, val); }
  void WriteTcr2(uint32_t val) { WriteCb32(0x010, val); }
  void WriteTtbr1(uint64_t val) { WriteCb64(0x028, val); }
  void WriteSctlr(uint32_t val) { WriteCb32(0x000, val); }
  void WriteGr0(uint64_t reg, uint32_t val) { hw_.Write32(reg, val); }
  static constexpr uint16_t kStreamId = 0x0010;

  void TlbiVa(uint64_t iova) { WriteCb64(0x600, (uint64_t{kAsid} << 48) | (iova >> 12)); }
  void TlbiAsid() { WriteCb32(0x610, kAsid); }
  // Posts a "last level only" invalidation by address (TLBIVAL).
  void TlbiVaLeaf(uint64_t iova) { WriteCb64(0x620, (uint64_t{kAsid} << 48) | (iova >> 12)); }
  void WriteCbRaw32(uint64_t reg, uint32_t val) { WriteCb32(reg, val); }

  // Issues TLBSYNC and polls TLBSTATUS until the sync is complete.
  void Sync() { SyncOn(kCb); }

  // Issues TLBSYNC on context bank |cb| and polls that bank's TLBSTATUS to completion.
  void SyncOn(uint32_t cb) {
    hw_.Write32(CbOffset(cb) + 0x7f0, 0);
    while (hw_.Read32(CbOffset(cb) + 0x7f4) != 0) {
    }
  }

  // Writes TLBSYNC without waiting for it to complete.
  void SyncWithoutWaiting() { WriteCb32(0x7f0, 0); }

  // Polls TLBSTATUS until it reads idle.
  void PollSync() {
    while (hw_.Read32(CbOffset() + 0x7f4) != 0) {
    }
  }

  // Issues sTLBGSYNC and polls sTLBGSTATUS until the sync is complete.
  void GlobalSync() {
    hw_.Write32(0x070, 0);
    while (hw_.Read32(0x074) != 0) {
    }
  }

  DmaTranslationResult Translate(uint64_t iova, uint32_t perms = IOMMU_FLAG_PERM_READ) {
    return hw_.TranslateDma(kStreamId, iova, perms);
  }

  bool HasNoHazard() const { return !hw_.has_hazard(); }

  // Returns true if a hazard whose text contains |needle| was recorded, then clears all hazards.
  bool TakeHazard(const char* needle) {
    bool found = false;
    for (const std::string& h : hw_.hazards()) {
      if (h.find(needle) != std::string::npos) {
        found = true;
      }
    }
    if (!found) {
      printf("expected a hazard containing \"%s\"; %zu recorded%s%s\n", needle,
             hw_.hazards().size(), hw_.hazards().empty() ? "" : ", first: ",
             hw_.hazards().empty() ? "" : hw_.hazards().front().c_str());
    }
    hw_.ClearHazards();
    return found;
  }

  SimSmmuHardwareModel& hw() { return hw_; }

 private:
  uint64_t GR1Offset() const { return hw_.config().page_size; }
  uint64_t CbOffset(uint32_t cb = kCb) const {
    return uint64_t{hw_.config().page_size} * (hw_.config().num_pages + cb);
  }
  void WriteCb32(uint64_t reg, uint32_t val) { hw_.Write32(CbOffset() + reg, val); }
  void WriteCb64(uint64_t reg, uint64_t val) { hw_.Write64(CbOffset() + reg, val); }

  static uint64_t* Table(paddr_t table_pa) {
    return reinterpret_cast<uint64_t*>(paddr_to_physmap(table_pa));
  }

  void ZeroAndClean(paddr_t table_pa) {
    memset(Table(table_pa), 0, kPageSize);
    Clean(table_pa);
  }

  SimSmmuHardwareModel hw_;
  std::vector<vm_page_t*> pages_;
};

// Links root -> l1 -> l2 -> l3 for IOVA 0 with one valid leaf at IOVA 0, all
// visible in PoC DRAM, and enables the context.
std::array<paddr_t, 4> BuildOneMapping(DetectorFixture& f, paddr_t leaf_pa = 0x8000'0000,
                                       uint32_t perms = IOMMU_FLAG_PERM_READ) {
  std::array<paddr_t, 4> t = {f.AllocTable(), f.AllocTable(), f.AllocTable(), f.AllocTable()};
  f.Write(t[3], 0, vmsav8_64::MakePageEntry(leaf_pa, perms), 3);
  f.Clean(t[3]);
  for (int level = 2; level >= 0; --level) {
    f.Write(t[level], 0, vmsav8_64::MakeTableEntry(t[level + 1]), level);
    f.Clean(t[level]);
  }
  f.EnableContext(t[0]);
  return t;
}

// A table is unlinked and freed with no TLB invalidation while no leaf was
// ever cached under it. Only a walk cache (not a leaf-only TLB) can notice.
TEST(SmmuModelDetectorTest, TableFreedWhileInWalkCache) {
  {
    DetectorFixture f;
    const paddr_t root = f.AllocTable();
    const paddr_t l1 = f.AllocTable();
    f.Write(root, 0, vmsav8_64::MakeTableEntry(l1), 0);
    f.Clean(root);
    f.EnableContext(root);
    EXPECT_TRUE(f.HasNoHazard());

    f.Write(root, 0, 0, 0);
    f.Clean(root);
    f.Free(l1);
    EXPECT_TRUE(f.TakeHazard("Premature page-table free"));

    // The correct sequence is accepted.
    const paddr_t l1b = f.AllocTable();
    f.Write(root, 1, vmsav8_64::MakeTableEntry(l1b), 0);
    f.Clean(root);
    f.Write(root, 1, 0, 0);
    f.Clean(root);
    f.TlbiAsid();
    f.Sync();
    f.Free(l1b);
    EXPECT_TRUE(f.HasNoHazard());
  }
  EXPECT_EQ(0u, PmmMock::Get().GetAllocatedPageCount());
}

// A table descriptor that is valid in PoC DRAM points at memory which is not
// an allocated page.
TEST(SmmuModelDetectorTest, WalkReachesUnallocatedTable) {
  DetectorFixture f;
  const paddr_t root = f.AllocTable();
  f.EnableContext(root);
  f.Write(root, 0, vmsav8_64::MakeTableEntry(0x0000'7000'dead'0000), 0);
  f.Clean(root);
  EXPECT_TRUE(f.TakeHazard("unallocated table page"));
}

// A table descriptor is replaced through an invalid state, but with no TLB
// invalidation in between, so the old table is still in the walk cache when
// the new one becomes loadable.
TEST(SmmuModelDetectorTest, TableReplacedWithoutTlbi) {
  DetectorFixture f;
  const paddr_t root = f.AllocTable();
  const paddr_t l1a = f.AllocTable();
  const paddr_t l1b = f.AllocTable();
  f.Write(root, 0, vmsav8_64::MakeTableEntry(l1a), 0);
  f.Clean(root);
  f.EnableContext(root);
  f.Write(root, 0, 0, 0);
  f.Clean(root);
  EXPECT_TRUE(f.HasNoHazard());
  f.Write(root, 0, vmsav8_64::MakeTableEntry(l1b), 0);
  EXPECT_TRUE(f.TakeHazard("un-invalidated cached"));
}

// A leaf is unmapped and cleaned but never invalidated; a DMA access still
// hits the cached leaf.
TEST(SmmuModelDetectorTest, StaleLeafHit) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  EXPECT_TRUE(f.HasNoHazard());
  f.Write(t[3], 0, 0, 3);
  f.Clean(t[3]);
  EXPECT_EQ(DmaFaultType::kStaleTlbHit, f.Translate(0x40).fault);
  EXPECT_TRUE(f.TakeHazard("Stale PWC/IOTLB hit"));

  // After invalidate + sync the same access is an ordinary translation fault.
  f.TlbiVa(0);
  f.Sync();
  EXPECT_EQ(DmaFaultType::kTranslation, f.Translate(0x40).fault);
  EXPECT_TRUE(f.HasNoHazard());
}

// A leaf is written and then cleared again before it was ever cleaned to the
// PoC, and the clear is not invalidated. The CPU cache may have written the
// valid value back in between, so hardware may have cached it.
TEST(SmmuModelDetectorTest, WrittenThenClearedWithoutTlbi) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 1, vmsav8_64::MakePageEntry(0x9000'0000, IOMMU_FLAG_PERM_READ), 3);
  f.Write(t[3], 1, 0, 3);
  f.Clean(t[3]);
  EXPECT_TRUE(f.HasNoHazard());
  EXPECT_EQ(DmaFaultType::kStaleTlbHit, f.Translate(0x1040).fault);
  EXPECT_TRUE(f.TakeHazard("Stale PWC/IOTLB hit"));

  f.TlbiVa(0x1000);
  f.Sync();
  EXPECT_EQ(DmaFaultType::kTranslation, f.Translate(0x1040).fault);
  EXPECT_TRUE(f.HasNoHazard());
}

// The stream's privilege override decides which permission rules apply. An
// execute-only mapping (AP[2:1] = 0b10) is unreadable by an unprivileged access
// but readable by a privileged one.
TEST(SmmuModelDetectorTest, PrivilegeOverrideIsModelled) {
  DetectorFixture f;
  BuildOneMapping(f, 0x8000'0000, IOMMU_FLAG_PERM_EXECUTE);

  // Forced unprivileged (what the driver programs): read is denied, execute is allowed.
  EXPECT_EQ(DmaFaultType::kPermission, f.Translate(0x40, IOMMU_FLAG_PERM_READ).fault);
  EXPECT_TRUE(f.Translate(0x40, IOMMU_FLAG_PERM_EXECUTE).is_ok());
  // Left "as presented": the device may be privileged, so the read is not reliably denied.
  f.RouteStream(0);
  EXPECT_TRUE(f.Translate(0x40, IOMMU_FLAG_PERM_READ).is_ok());
  // Forced privileged: read is allowed.
  f.RouteStream(3);
  EXPECT_TRUE(f.Translate(0x40, IOMMU_FLAG_PERM_READ).is_ok());
  EXPECT_TRUE(f.HasNoHazard());
}

// A stream is routed to a context bank whose CBAR.TYPE is not a stage 1 type.
TEST(SmmuModelDetectorTest, ContextNotStage1) {
  DetectorFixture f;
  BuildOneMapping(f);
  EXPECT_TRUE(f.Translate(0x40).is_ok());
  f.WriteCbar(0);  // CBAR.TYPE = stage 2 context
  EXPECT_FALSE(f.Translate(0x40).is_ok());
  EXPECT_TRUE(f.TakeHazard("not a stage 1 context"));
}

// A context is translating with EPD1 == 0, with hardware table updates enabled,
// or with table descriptors that carry hierarchical attributes.
TEST(SmmuModelDetectorTest, UnsupportedContextConfig) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  EXPECT_TRUE(f.hw().VerifyQuiescent());

  f.WriteTcr(16);  // T0SZ = 16, EPD1 = 0
  EXPECT_FALSE(f.hw().VerifyQuiescent());
  EXPECT_TRUE(f.TakeHazard("EPD1 == 0"));
  f.WriteTcr((1u << 23) | 16u);

  f.WriteTcr2(1u << 10);  // HA
  f.hw().ClearHazards();
  EXPECT_FALSE(f.hw().VerifyQuiescent());
  EXPECT_TRUE(f.TakeHazard("hardware translation table updates enabled"));
  f.WriteTcr2(0);

  const paddr_t l1 = f.AllocTable();
  f.Write(t[0], 1, vmsav8_64::MakeTableEntry(l1) | (1ULL << 61), 0);
  EXPECT_TRUE(f.TakeHazard("hierarchical attribute"));
  f.Write(t[0], 1, 0, 0);
  f.Clean(t[0]);
  f.TlbiAsid();
  f.Sync();
  f.hw().ClearHazards();
}

// With output address size enforcement switched on, a page whose output
// address does not fit the configured size is an address size fault.
TEST(SmmuModelDetectorTest, OutputAddressSize) {
  SimSmmuHardwareModel::Config cfg;
  cfg.enforce_tcr2_fields = true;
  DetectorFixture f(cfg);
  BuildOneMapping(f, 0x1'0000'0000);
  f.WriteTcr2(0);  // PASize = 32 bits
  EXPECT_EQ(DmaFaultType::kAddressSize, f.Translate(0x40).fault);
  f.WriteTcr2(5);  // PASize = 48 bits
  EXPECT_TRUE(f.Translate(0x40).is_ok());
}

// TCR.A1 selects the ASID from TTBR1. A driver which leaves it set while
// giving every context the same TTBR1 ASID makes all contexts share one ASID,
// and its invalidations (which name the TTBR0 ASID) remove nothing.
TEST(SmmuModelDetectorTest, AsidSelectedFromTtbr1) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  // Move the context onto the TTBR1 ASID. Nothing was invalidated, so the
  // entries loaded under the old ASID are now stale.
  f.WriteTtbr1(uint64_t{0xffff} << 48);
  f.WriteTcr((1u << 23) | (1u << 22) | 16u);  // EPD1, A1, T0SZ
  EXPECT_FALSE(f.hw().VerifyQuiescent());
  EXPECT_TRUE(f.TakeHazard("Stale cached descriptor"));

  // An unmap which invalidates by the TTBR0 ASID does not reach the entry
  // tagged 0xffff.
  f.Write(t[3], 0, 0, 3);
  f.Clean(t[3]);
  f.TlbiVa(0);
  f.Sync();
  EXPECT_EQ(DmaFaultType::kStaleTlbHit, f.Translate(0x40).fault);
  EXPECT_TRUE(f.TakeHazard("Stale PWC/IOTLB hit"));
}

// With TCR2 enforcement on and TCR2.AS == 0, ASIDs are 8 bits wide: an
// invalidation which names a 16-bit ASID is malformed.
TEST(SmmuModelDetectorTest, AsidWidth) {
  SimSmmuHardwareModel::Config cfg;
  cfg.enforce_tcr2_fields = true;
  DetectorFixture f(cfg);
  const paddr_t root = f.AllocTable();
  f.EnableContext(root);
  f.WriteTcr2(5);  // PASize = 48 bits, AS = 0
  f.TlbiAsid();
  f.Sync();
  EXPECT_TRUE(f.TakeHazard("upper 8 bits must be written as 0"));
  f.WriteTcr2((1u << 4) | 5u);  // AS = 1
  f.TlbiAsid();
  f.Sync();
  EXPECT_TRUE(f.HasNoHazard());
}

// Routing states which exist only between two register writes, or which let a
// stream bypass translation in enforced mode.
TEST(SmmuModelDetectorTest, TransientMisrouting) {
  DetectorFixture f;
  BuildOneMapping(f);
  EXPECT_TRUE(f.HasNoHazard());

  // A second match register is made valid while its S2CR still holds a bypass type.
  f.WriteGr0(0xc04, 1u << 16);           // S2CR1: TYPE = bypass (stale)
  f.WriteGr0(0x804, (1u << 31) | 0x20);  // SMR1: valid, stream 0x20
  EXPECT_TRUE(f.TakeHazard("bypass-type S2CR"));
  f.WriteGr0(0x804, 0);
  f.hw().ClearHazards();

  // Two valid match registers match the same stream.
  f.WriteGr0(0xc04, (2u << 24) | DetectorFixture::kCb);
  f.WriteGr0(0x804, (1u << 31) | DetectorFixture::kStreamId);
  EXPECT_TRUE(f.TakeHazard("match a common stream ID"));
  f.WriteGr0(0x804, 0);
  f.hw().ClearHazards();

  // The context's MMU is switched off while a stream is still routed to it.
  f.WriteSctlr(0);
  EXPECT_TRUE(f.TakeHazard("SCTLR.M is 0"));
  f.WriteSctlr(1);
  f.hw().ClearHazards();

  // Unmatched streams are allowed to bypass.
  f.WriteGr0(0x000, 0);
  EXPECT_TRUE(f.TakeHazard("USFCFG == 0"));
  f.WriteGr0(0x000, 1u << 10);
  f.hw().ClearHazards();
}

// The driver writes a register the model does not implement, or writes half of
// a 64-bit register.
TEST(SmmuModelDetectorTest, UnmodelledRegisterAccess) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.WriteCbRaw32(0x008, 1);  // RESUME: not modelled
  EXPECT_TRUE(f.TakeHazard("unmodelled context bank register"));
  f.WriteCbRaw32(0x020, static_cast<uint32_t>(t[0]));  // low half of TTBR0
  EXPECT_TRUE(f.TakeHazard("32-bit write to 64-bit"));
  f.WriteGr0(0x064, 0);  // TLBIVMID: not modelled
  EXPECT_TRUE(f.TakeHazard("unmodelled GR0 register"));
  f.WriteGr0(0x800 + 4 * 100, 1u << 31);  // SMR100, beyond NUMSMRG
  EXPECT_TRUE(f.TakeHazard("beyond NUMSMRG"));
}

// A table is unlinked after a "last level only" invalidation. That discards
// the leaf entry but not the walk-cache entry which points at the table, so
// freeing the table is premature.
TEST(SmmuModelDetectorTest, LeafOnlyInvalidationKeepsWalkCache) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 0, 0, 3);
  f.Clean(t[3]);
  f.Write(t[2], 0, 0, 2);
  f.Clean(t[2]);
  f.TlbiVaLeaf(0);
  f.Sync();
  // The walk still goes through the cached level 2 descriptor.
  EXPECT_EQ(DmaFaultType::kStaleTlbHit, f.Translate(0x40).fault);
  EXPECT_TRUE(f.TakeHazard("Stale PWC/IOTLB hit"));
  f.Free(t[3]);
  EXPECT_TRUE(f.TakeHazard("Premature page-table free"));
}

// The root table of a translating context is returned to the PMM.
TEST(SmmuModelDetectorTest, FreeOfActiveRoot) {
  DetectorFixture f;
  const paddr_t root = f.AllocTable();
  f.EnableContext(root);
  f.Free(root);
  EXPECT_TRUE(f.TakeHazard("still set as active TTBR0"));
}

// A stream is routed to a context bank which still has a posted, unsynced
// invalidation.
TEST(SmmuModelDetectorTest, StreamEnabledWithUnsyncedTlbi) {
  DetectorFixture f;
  BuildOneMapping(f);
  f.TlbiAsid();
  f.WriteGr0(0xc04, (2u << 24) | DetectorFixture::kCb);  // S2CR1: translation, same context
  f.WriteGr0(0x804, (1u << 31) | 0x20);                  // SMR1: valid, stream 0x20
  EXPECT_TRUE(f.TakeHazard("unsynchronized TLBI"));
  f.WriteGr0(0x804, 0);
  f.Sync();
}

// Descriptors the driver must never write: a hardware-writable leaf (DBM), a
// non-zero invalid descriptor, a block at an intermediate level, and a level 3
// descriptor without the page bit.
TEST(SmmuModelDetectorTest, MalformedDescriptors) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 1, vmsav8_64::MakePageEntry(0x9000'0000, IOMMU_FLAG_PERM_READ) | (1ULL << 51), 3);
  EXPECT_TRUE(f.TakeHazard("Hardware-writable L3 PTE hazard"));
  f.Write(t[3], 1, 0, 3);
  f.hw().ClearHazards();

  f.Write(t[3], 2, 0x9000'0000, 3);
  EXPECT_TRUE(f.TakeHazard("non-zero invalid descriptor"));
  f.Write(t[3], 2, 0, 3);

  f.Write(t[1], 1, 0x4000'0000 | (1u << 10) | 1u, 1);  // valid, bit 1 clear: a block
  EXPECT_TRUE(f.TakeHazard("non-table valid descriptor"));
  f.Write(t[1], 1, 0, 1);
  f.hw().ClearHazards();

  f.Write(t[3], 3, 0x9000'0000 | (1u << 10) | 1u, 3);  // valid, bit 1 clear at level 3
  EXPECT_TRUE(f.TakeHazard("invalid L3 page descriptor"));
  f.Write(t[3], 3, 0, 3);
  f.Clean(t[1]);
  f.Clean(t[3]);
  f.TlbiAsid();
  f.Sync();
  f.hw().ClearHazards();
}

// A register access beyond the end of the implemented context banks.
TEST(SmmuModelDetectorTest, OutOfRangeMmio) {
  DetectorFixture f;
  const uint64_t beyond = uint64_t{4096} * (16 + 64);  // context bank 64; only 8 are implemented
  f.hw().Write32(beyond, 1);
  EXPECT_TRUE(f.TakeHazard("out-of-range MMIO"));
  (void)f.hw().Read32(beyond);
  EXPECT_TRUE(f.TakeHazard("out-of-range MMIO"));
}

// A context fault is delivered only when SCTLR.CFIE is set and the vector is
// unmasked, it is delivered later if it was recorded while disabled, and a
// handler which leaves it asserted is reported.
TEST(SmmuModelDetectorTest, InterruptDelivery) {
  static int handler_runs = 0;
  handler_runs = 0;
  InterruptMock::Get().Reset();
  DetectorFixture f;
  BuildOneMapping(f);
  const uint32_t vector = f.hw().config().base_context_irq_num + DetectorFixture::kCb;
  // A fault handler which does nothing about the fault.
  ASSERT_OK(register_int_handler(vector, interrupt_handler_t(+[](void*) { ++handler_runs; })));

  // A translation fault with the interrupt disabled and masked: recorded, not delivered.
  SimSmmuHardwareModel::ServicedInterrupts serviced;
  EXPECT_EQ(DmaFaultType::kTranslation, f.hw()
                                            .DeviceAccess(DetectorFixture::kStreamId, 0x10'0000,
                                                          IOMMU_FLAG_PERM_READ, &serviced)
                                            .fault);
  EXPECT_TRUE(serviced.contexts.empty());
  EXPECT_EQ(1u << 1, f.hw().cb_fsr(DetectorFixture::kCb));
  EXPECT_EQ(0, handler_runs);

  // Enabled but still masked: not delivered.
  f.WriteSctlr((1u << 6) | 1u);
  EXPECT_TRUE(f.hw().ServicePendingInterrupts().contexts.empty());

  // Unmasked: the fault recorded earlier is delivered now. The handler does
  // not clear it, so it would fire again at once.
  ASSERT_OK(unmask_interrupt(vector));
  EXPECT_TRUE(f.hw().ServicePendingInterrupts().ran_context(DetectorFixture::kCb));
  EXPECT_EQ(1, handler_runs);
  EXPECT_TRUE(f.TakeHazard("still asserted"));

  // A second fault while one is recorded sets MULTI and keeps the first syndrome.
  f.hw().DeviceAccess(DetectorFixture::kStreamId, 0x20'0000, IOMMU_FLAG_PERM_READ);
  EXPECT_EQ((1u << 31) | (1u << 1), f.hw().cb_fsr(DetectorFixture::kCb));
  f.hw().ClearHazards();

  // Clearing the status (write one to clear) deasserts it.
  f.WriteCbRaw32(0x058, ~0u);
  EXPECT_TRUE(f.hw().ServicePendingInterrupts().contexts.empty());
  ASSERT_OK(mask_interrupt(vector));
  EXPECT_TRUE(f.HasNoHazard());
  InterruptMock::Get().Reset();
}

// The abstract specification and the hardware model disagree about a mapping.
TEST(SmmuModelDetectorTest, SpecMismatchIsDetected) {
  DetectorFixture f;
  BuildOneMapping(f);
  AbstractIommuSpec spec(ArmSmmuMode::kEnforced);
  spec.AddBti(DetectorFixture::kStreamId);
  spec.AddMapping(DetectorFixture::kStreamId, 0, {0x8000'0000}, IOMMU_FLAG_PERM_READ);
  EXPECT_TRUE(spec.VerifyAllMappings(f.hw()));

  // The specification expects a second page which the hardware does not map.
  spec.AddMapping(DetectorFixture::kStreamId, 0x1000, {0x9000'0000}, IOMMU_FLAG_PERM_READ);
  EXPECT_FALSE(spec.VerifyAllMappings(f.hw()));
  spec.RemoveMapping(DetectorFixture::kStreamId, 0x1000, 1);

  // The specification expects different memory behind the mapped page.
  spec.RemoveMapping(DetectorFixture::kStreamId, 0, 1);
  spec.AddMapping(DetectorFixture::kStreamId, 0, {0xa000'0000}, IOMMU_FLAG_PERM_READ);
  EXPECT_FALSE(spec.VerifyAllMappings(f.hw()));
}

// A table is zero-filled in bulk rather than descriptor by descriptor. The
// model sees one clear for each descriptor which was not already zero.
TEST(SmmuModelDetectorTest, BulkZeroIsObserved) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 7, vmsav8_64::MakePageEntry(0x9000'0000, IOMMU_FLAG_PERM_READ), 3);
  f.Write(t[3], 9, vmsav8_64::MakePageEntry(0xa000'0000, IOMMU_FLAG_PERM_READ), 3);
  const size_t before = f.hw().descriptor_write_count();
  vmsav8_64::ZeroTablePage(paddr_to_physmap(t[3]));
  // Three descriptors were valid: index 0 (from the fixture), 7 and 9.
  EXPECT_EQ(before + 3, f.hw().descriptor_write_count());
  EXPECT_TRUE(f.HasNoHazard());
  f.Clean(t[3]);
  f.TlbiAsid();
  f.Sync();
  EXPECT_TRUE(f.hw().VerifyQuiescent());
}

// The driver passes the wrong level for the table it is writing. The model
// derives the level from how the table is linked and reports the disagreement.
TEST(SmmuModelDetectorTest, WrongLevelClaimed) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 5, vmsav8_64::MakePageEntry(0x9000'0000, IOMMU_FLAG_PERM_READ), 2);
  EXPECT_TRUE(f.TakeHazard("claims level 2"));
  f.Write(t[3], 5, 0, 3);
  f.Clean(t[3]);
  f.TlbiVa(0x5000);
  f.Sync();
  EXPECT_TRUE(f.hw().VerifyQuiescent());
}

// An invalidation is posted in one context bank and synced in a different
// one. A context bank sync covers only its own bank (IHI 0062D.c section
// 5.4.2), so the entry must survive. A global sync covers every bank.
TEST(SmmuModelDetectorTest, SyncOnWrongContextBank) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 0, 0, 3);
  f.Clean(t[3]);
  f.TlbiVa(0);
  f.SyncOn(DetectorFixture::kCb + 1);
  EXPECT_EQ(DmaFaultType::kStaleTlbHit, f.Translate(0x40).fault);
  EXPECT_TRUE(f.TakeHazard("Stale PWC/IOTLB hit"));

  f.GlobalSync();
  EXPECT_EQ(DmaFaultType::kTranslation, f.Translate(0x40).fault);
  EXPECT_TRUE(f.hw().VerifyQuiescent());
}

// A sync is requested but its completion is never awaited before the
// operation continues.
TEST(SmmuModelDetectorTest, SyncNotAwaited) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  const paddr_t spare = f.AllocTable();
  f.Write(t[3], 0, 0, 3);
  f.Clean(t[3]);
  f.TlbiVa(0);
  f.SyncWithoutWaiting();

  // The entry is still cached, a free is reported, and the operation is not quiescent.
  EXPECT_EQ(DmaFaultType::kStaleTlbHit, f.Translate(0x40).fault);
  EXPECT_TRUE(f.TakeHazard("Stale PWC/IOTLB hit"));
  f.Free(spare);
  EXPECT_TRUE(f.TakeHazard("sync is still in progress"));
  EXPECT_FALSE(f.hw().VerifyQuiescent());
  EXPECT_TRUE(f.TakeHazard("not synced to completion"));

  // Polling the status register to idle completes it.
  f.PollSync();
  EXPECT_TRUE(f.hw().VerifyQuiescent());
}

// A leaf is unmapped and cleaned, and the operation "returns" with the
// invalidation missing, or posted but not synced. Nothing ever touches that
// address again.
TEST(SmmuModelDetectorTest, StaleEntryAtQuiescence) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  EXPECT_TRUE(f.hw().VerifyQuiescent());
  f.Write(t[3], 0, 0, 3);
  f.Clean(t[3]);
  EXPECT_FALSE(f.hw().VerifyQuiescent());
  EXPECT_TRUE(f.TakeHazard("Stale cached descriptor"));

  f.TlbiVa(0);
  EXPECT_FALSE(f.hw().VerifyQuiescent());
  EXPECT_TRUE(f.TakeHazard("not synced to completion"));

  f.Sync();
  EXPECT_TRUE(f.hw().VerifyQuiescent());
}

// A leaf is cleared in the CPU view and invalidated, but the clear is never
// cleaned to the PoC, so the sync reloads the old descriptor.
TEST(SmmuModelDetectorTest, MissingCleanBeforeSync) {
  DetectorFixture f;
  const std::array<paddr_t, 4> t = BuildOneMapping(f);
  f.Write(t[3], 0, 0, 3);
  // No clean of t[3] here.
  f.TlbiVa(0);
  f.Sync();
  EXPECT_TRUE(f.Translate(0x40).is_ok());
  // The driver believes the page is unmapped; hardware still translates it.
  // Remapping the slot exposes the conflict.
  f.Write(t[3], 0, vmsav8_64::MakePageEntry(0x9000'0000, IOMMU_FLAG_PERM_READ), 3);
  EXPECT_TRUE(f.TakeHazard("Make-Before-Break"));
}

}  // namespace
}  // namespace testing
}  // namespace arm_smmu
