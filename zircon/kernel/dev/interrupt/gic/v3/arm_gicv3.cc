// Copyright 2017 The Fuchsia Authors
// Copyright (c) 2017, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <align.h>
#include <assert.h>
#include <bits.h>
#include <inttypes.h>
#include <lib/arch/intrin.h>
#include <lib/ktrace.h>
#include <lib/page/size.h>
#include <lib/pow2_range_allocator.h>
#include <lib/root_resource_filter.h>
#include <lib/zbi-format/driver-config.h>
#include <lib/zx/result.h>
#include <pow2.h>
#include <string.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/arm64/hypervisor/gic/gicv3.h>
#include <arch/ops.h>
#include <arch/regs.h>
#include <dev/interrupt.h>
#include <dev/interrupt/arm_gic_common.h>
#include <dev/interrupt/arm_gic_hw_interface.h>
#include <dev/interrupt/arm_gicv3_init.h>
#include <kernel/cpu.h>
#include <kernel/spinlock.h>
#include <kernel/stats.h>
#include <kernel/thread.h>
#include <ktl/algorithm.h>
#include <lk/init.h>
#include <pdev/interrupt.h>
#include <vm/physmap.h>
#include <vm/pmm.h>
#include <vm/vm.h>
#include <vm/vm_aspace.h>

#include "arm_gicv3_regs.h"

#define LOCAL_TRACE 0

#include <arch/arm64.h>
#define IFRAME_PC(frame) ((frame)->elr)

// Base and stride of the mapped apertures, exported to other files in this driver.
vaddr_t arm_gicv3_gicd_base = 0;
vaddr_t arm_gicv3_gicr_base = 0;
vaddr_t arm_gicv3_gits_base = 0;
uint64_t arm_gicv3_gicr_stride = 0;

namespace {

uint64_t gicd_offset = 0;
uint64_t gicr_offset = 0;
uint64_t gic_mmio_phys = 0;
uint32_t ipi_base = 0;
uint32_t gic_max_int;

constexpr uint32_t kLpiBase = 8192;
constexpr uint32_t kMaxLpis = 256;
constexpr uint32_t kMaxItsPciSlots = 32;
constexpr size_t kItsTableSize = 0x10000;  // 64 KiB
constexpr size_t kItsTablePages = kItsTableSize / kPageSize;
constexpr uint8_t kItsTableAlignLog2 = 16;  // 64 KiB alignment

constexpr uint8_t kLpiPropDefaultPrio = 0xa0;
constexpr uint8_t kLpiPropGroup1 = (1u << 1);
constexpr uint8_t kLpiPropEnabled = (1u << 0);

constexpr uint8_t kGitsCmdSync = 0x05;
constexpr uint8_t kGitsCmdMapd = 0x08;
constexpr uint8_t kGitsCmdMapc = 0x09;
constexpr uint8_t kGitsCmdMapti = 0x0a;
constexpr uint8_t kGitsCmdInv = 0x0c;

uint64_t gits_phys = 0;
bool its_enabled = false;
paddr_t lpi_prop_pa = 0;
uint8_t* lpi_prop_va = nullptr;
paddr_t lpi_pend_pa[SMP_MAX_CPUS] = {};
uint8_t* its_cmdq_va = nullptr;
uint64_t its_cmdq_cwriter = 0;
uint64_t its_cpu0_rdbase = 0;
bool lpi_mapped[kMaxLpis] = {};

DECLARE_SINGLETON_SPINLOCK_WITH_TYPE(GicItsLock, MonitoredSpinLock);
Pow2RangeAllocator g_lpi_alloc;
interrupt_handler_t g_lpi_handlers[kMaxLpis] TA_GUARDED(GicItsLock::Get());

bool gic_is_valid_interrupt(interrupt_vector_t vector, uint32_t flags) {
  return (vector < gic_max_int);
}

uint32_t gic_get_base_vector() {
  // ARM Generic Interrupt Controller v3&4 chapter 2.2
  // INTIDs 0-15 are local CPU interrupts
  return 16;
}

uint32_t gic_get_max_vector() { return gic_max_int; }

bool gic_wait_for_mask(uint64_t reg, uint64_t mask, uint64_t expect) {
  int count = 1000000;
  while ((arm_gicv3_read32(reg) & mask) != expect) {
    count -= 1;
    if (!count) {
      LTRACEF("arm_gicv3: wait timeout reg:0x%lx, val:0x%x, mask:0x%lx\n", reg,
              arm_gicv3_read32(reg), mask);
      return false;
    }
  }
  return true;
}

void gic_wait_for_rwp(uint64_t reg) {
  // Maintain the current log message for RWP timeouts.
  if (!gic_wait_for_mask(reg, GICD_CTLR_RWP, 0)) {
    LTRACEF("arm_gicv3: rwp timeout 0x%x\n", arm_gicv3_read32(reg));
  }
}

void gic_set_enable(uint vector, bool enable) {
  uint32_t reg = vector / 32;
  uint32_t mask = static_cast<uint32_t>(1ULL << (vector % 32));
  if (vector < 32) {
    cpu_num_t cpu_id = arch_curr_cpu_num();
    if (enable) {
      arm_gicv3_write32(GICR_ISENABLER0(cpu_id), mask);
    } else {
      arm_gicv3_write32(GICR_ICENABLER0(cpu_id), mask);
    }
    gic_wait_for_rwp(GICR_CTLR(cpu_id));
  } else {
    if (enable) {
      arm_gicv3_write32(GICD_ISENABLER(reg), mask);
    } else {
      arm_gicv3_write32(GICD_ICENABLER(reg), mask);
    }
    gic_wait_for_rwp(GICD_CTLR);
  }
}

// Redistributors for each PE need to woken up before they will
// distribute interrupts.
// https://developer.arm.com/documentation/198123/0302/Configuring-the-Arm-GIC
void gic_redistributor_sleep(bool sleep) {
  cpu_num_t cpu = arch_curr_cpu_num();
  DEBUG_ASSERT(arch_ints_disabled());

  // GICR_WAKER could be RW or RAZ/WI.  When GICD_CTLR.DS is 1, GICR_WAKER is
  // RW.  However, when GICD_CTLR.DS is 0, GICR_WAKER could be RW or RAZ/WI
  // depending on whether the access is Secure/Non-secure and FEAT_RME.
  //
  // Instead of checking those things we're going to take a shortcut.  In the
  // case we're writing a 1 to WAKER_PROCESSOR_SLEEP we'll read back GICR_WAKER
  // to determine if it's RW or RAZ/WI.  If the former, we'll
  // gic_wait_for_mask.  If the latter, we'll bail out.
  uint waker = arm_gicv3_read32(GICR_WAKER(cpu));
  if (sleep) {
    waker |= WAKER_PROCESSOR_SLEEP;
  } else {
    waker &= ~WAKER_PROCESSOR_SLEEP;
  }
  arm_gicv3_write32(GICR_WAKER(cpu), waker);
  if (sleep) {
    const uint read_back = arm_gicv3_read32(GICR_WAKER(cpu));
    if ((read_back & WAKER_PROCESSOR_SLEEP) == 0) {
      // Our write didn't take.  Must be RAZ/WI.  Don't bother waiting.
      return;
    }
  }

  uint64_t val = sleep ? WAKER_CHILDREN_ASLEEP : 0;
  gic_wait_for_mask(GICR_WAKER(cpu), WAKER_CHILDREN_ASLEEP, val);
}

void gic_init_percpu_lpi(cpu_num_t cpu) {
  if (!lpi_prop_va || cpu >= SMP_MAX_CPUS || !lpi_pend_pa[cpu] ||
      !(arm_gicv3_read64(GICR_TYPER(cpu, 0)) & GICR_TYPER_PLPIS)) {
    return;
  }

  // Ensure LPIs are disabled on this redistributor before programming PROPBASER/PENDBASER.
  // Note: GICR_CTLR.EnableLPIs may be WI once set; if it does not clear (e.g. on resume when
  // redistributor state was retained), keep the existing configuration.
  uint32_t gicr_ctlr = arm_gicv3_read32(GICR_CTLR(cpu));
  if (gicr_ctlr & GICR_CTLR_ENABLE_LPIS) {
    arm_gicv3_write32(GICR_CTLR(cpu), gicr_ctlr & ~GICR_CTLR_ENABLE_LPIS);
    gic_wait_for_mask(GICR_CTLR(cpu), GICR_CTLR_RWP, 0);
    if (arm_gicv3_read32(GICR_CTLR(cpu)) & GICR_CTLR_ENABLE_LPIS) {
      return;
    }
  }

  // Ensure the pending table is zeroed in memory before setting GICR_PENDBASER_PTZ.
  void* pend_va = paddr_to_physmap(lpi_pend_pa[cpu]);
  memset(pend_va, 0, kItsTableSize);
  arch_clean_cache_range(reinterpret_cast<vaddr_t>(pend_va), kItsTableSize);

  // Program LPI Configuration Table (IDbits = 15 -> 16-bit INTIDs 0..65535).
  uint64_t propbaser = (lpi_prop_pa & GICR_PROPBASER_ADDR_MASK) | GICR_PROPBASER_SHAREABILITY_ISH |
                       GICR_PROPBASER_INNER_CACHE_WB | 15ULL;
  arm_gicv3_write64(GICR_PROPBASER(cpu), propbaser);
  if ((arm_gicv3_read64(GICR_PROPBASER(cpu)) & GICR_PROPBASER_SHAREABILITY_MASK) == 0) {
    propbaser = (propbaser & ~(GICR_PROPBASER_SHAREABILITY_MASK | GICR_PROPBASER_INNER_CACHE_WB)) |
                GICR_PROPBASER_INNER_CACHE_NC;
    arm_gicv3_write64(GICR_PROPBASER(cpu), propbaser);
  }

  // Program LPI Pending Table with PTZ (Pending Table Zero) set.
  uint64_t pendbaser = (lpi_pend_pa[cpu] & GICR_PENDBASER_ADDR_MASK) |
                       GICR_PENDBASER_SHAREABILITY_ISH | GICR_PENDBASER_INNER_CACHE_WB |
                       GICR_PENDBASER_PTZ;
  arm_gicv3_write64(GICR_PENDBASER(cpu), pendbaser);
  if ((arm_gicv3_read64(GICR_PENDBASER(cpu)) & GICR_PENDBASER_SHAREABILITY_MASK) == 0) {
    pendbaser = (pendbaser & ~(GICR_PENDBASER_SHAREABILITY_MASK | GICR_PENDBASER_INNER_CACHE_WB)) |
                GICR_PENDBASER_INNER_CACHE_NC;
    arm_gicv3_write64(GICR_PENDBASER(cpu), pendbaser);
  }

  // Enable LPIs on this redistributor.
  arm_gicv3_write32(GICR_CTLR(cpu), arm_gicv3_read32(GICR_CTLR(cpu)) | GICR_CTLR_ENABLE_LPIS);
  arch::DeviceMemoryBarrier();
}

void gic_init_percpu_early() {
  cpu_num_t cpu = arch_curr_cpu_num();

  // wake up the redistributor
  gic_redistributor_sleep(false);

  // redistributer config: configure sgi/ppi as non-secure group 1.
  arm_gicv3_write32(GICR_IGROUPR0(cpu), ~0);
  arm_gicv3_write32(GICR_IGRPMOD0(cpu), 0);
  gic_wait_for_rwp(GICR_CTLR(cpu));

  // redistributer config: clear and mask sgi/ppi.
  arm_gicv3_write32(GICR_ICENABLER0(cpu), 0xffffffff);
  arm_gicv3_write32(GICR_ICPENDR0(cpu), ~0);
  gic_wait_for_rwp(GICR_CTLR(cpu));

  gic_init_percpu_lpi(cpu);

  // enable system register interface
  uint32_t sre = gic_read_sre();
  if (!(sre & 0x1)) {
    gic_write_sre(sre | 0x1);
    sre = gic_read_sre();
    DEBUG_ASSERT(sre & 0x1);
  }

  // set priority threshold to max.
  gic_write_pmr(0xff);

  // enable group 1 interrupts.
  gic_write_igrpen(1);

  // Ensure the virtual CPU interface is disabled when running at EL2.
  if (arm64_get_boot_el() >= 2) {
    __arm_wsr64("ich_hcr_el2", 0);
    __isb(ARM_MB_SY);
  }
}

paddr_t gic_alloc_its_table(uint8_t fill = 0) {
  VmPageDoublyLinkedList pages;
  paddr_t pa = 0;
  if (pmm_alloc_contiguous(kItsTablePages, 0, kItsTableAlignLog2, &pa, &pages) != ZX_OK) {
    return 0;
  }
  while (!pages.is_empty()) {
    pages.pop_front();
  }
  void* va = paddr_to_physmap(pa);
  memset(va, fill, kItsTableSize);
  arch_clean_cache_range(reinterpret_cast<vaddr_t>(va), kItsTableSize);
  return pa;
}

bool gic_its_cmd(uint64_t dw0, uint64_t dw1, uint64_t dw2, bool sync = true) {
  if (!its_cmdq_va) {
    return false;
  }
  auto post = [](uint64_t d0, uint64_t d1, uint64_t d2) {
    uint64_t* slot = reinterpret_cast<uint64_t*>(its_cmdq_va + its_cmdq_cwriter);
    slot[0] = d0;
    slot[1] = d1;
    slot[2] = d2;
    slot[3] = 0;
    arch_clean_cache_range(reinterpret_cast<vaddr_t>(slot), 32);
    its_cmdq_cwriter = (its_cmdq_cwriter + 32) % kItsTableSize;
  };
  post(dw0, dw1, dw2);
  if (!sync) {
    return true;
  }
  post(kGitsCmdSync, 0, its_cpu0_rdbase << 16);
  arch::DeviceMemoryBarrier();
  arm_gicv3_write64(GITS_CWRITER, its_cmdq_cwriter);
  for (int count = 1000000; count > 0; --count) {
    uint64_t creadr = arm_gicv3_read64(GITS_CREADR);
    if (creadr & GITS_CREADR_STALLED) {
      break;
    }
    if (creadr == its_cmdq_cwriter) {
      return true;
    }
  }
  dprintf(INFO, "GICv3 ITS: command queue timeout/stall (CWRITER=%#lx, CREADR=%#lx)\n",
          its_cmdq_cwriter, arm_gicv3_read64(GITS_CREADR));
  return false;
}

void gic_lpi_set_enable_locked(uint32_t idx, bool enable) TA_REQ(GicItsLock::Get()) {
  if (!lpi_prop_va || idx >= kMaxLpis) {
    return;
  }
  uint8_t val = kLpiPropDefaultPrio | kLpiPropGroup1 | (enable ? kLpiPropEnabled : 0);
  if (lpi_prop_va[idx] == val) {
    return;
  }
  lpi_prop_va[idx] = val;
  arch_clean_cache_range(reinterpret_cast<vaddr_t>(&lpi_prop_va[idx]), 1);
  arch::DeviceMemoryBarrier();

  if (arm_gicv3_read64(GICR_TYPER(0, 0)) & GICR_TYPER_DIRECT_LPI) {
    arm_gicv3_write64(GICR_INVLPIR(0), kLpiBase + idx);
    gic_wait_for_mask(GICR_SYNCR(0), 1, 0);
  }
  if (its_enabled && lpi_mapped[idx]) {
    gic_its_cmd(kGitsCmdInv, idx, 0);
  }
}

template <typename MapFn>
void gic_init_its(uint32_t gicd_typer, MapFn& map_region) {
  if (!(gicd_typer & GICD_TYPER_LPIS) || gits_phys == 0) {
    return;
  }

  auto result = map_region(gits_phys, GITS_REG_SIZE, "gits");
  if (!result.is_ok()) {
    dprintf(INFO, "GICv3: failed to map ITS registers at %#lx: %d\n", gits_phys,
            result.error_value());
    return;
  }
  arm_gicv3_gits_base = result.value();

  uint64_t gits_typer = arm_gicv3_read64(GITS_TYPER);
  if (!(gits_typer & GITS_TYPER_PLPIS)) {
    dprintf(INFO, "GICv3: ITS at %#lx does not support physical LPIs (TYPER=%#lx)\n", gits_phys,
            gits_typer);
    return;
  }

  // Allocate the shared 64 KiB LPI configuration table and per-CPU LPI pending tables.
  lpi_prop_pa = gic_alloc_its_table(kLpiPropDefaultPrio | kLpiPropGroup1);
  if (!lpi_prop_pa) {
    printf("GICv3: failed to allocate LPI configuration table\n");
    return;
  }
  lpi_prop_va = static_cast<uint8_t*>(paddr_to_physmap(lpi_prop_pa));

  uint max_cpus = ktl::min<uint>(arch_max_num_cpus(), SMP_MAX_CPUS);
  for (uint cpu = 0; cpu < max_cpus; ++cpu) {
    lpi_pend_pa[cpu] = gic_alloc_its_table();
    if (!lpi_pend_pa[cpu]) {
      printf("GICv3: failed to allocate LPI pending table for CPU %u\n", cpu);
      return;
    }
  }
  gic_init_percpu_lpi(0);

  // Disable ITS before programming BASER and CBASER registers.
  uint32_t gits_ctlr = arm_gicv3_read32(GITS_CTLR);
  if (gits_ctlr & GITS_CTLR_ENABLE) {
    arm_gicv3_write32(GITS_CTLR, gits_ctlr & ~GITS_CTLR_ENABLE);
    gic_wait_for_mask(GITS_CTLR, GITS_CTLR_QUIESCENT, GITS_CTLR_QUIESCENT);
  }

  // Allocate and program GITS_BASER(0..7) tables (Device and Interrupt Collection tables).
  for (uint n = 0; n < 8; ++n) {
    uint64_t baser = arm_gicv3_read64(GITS_BASER(n));
    if (((baser >> GITS_BASER_TYPE_SHIFT) & GITS_BASER_TYPE_MASK) == 0) {
      continue;
    }

    paddr_t table_pa = gic_alloc_its_table();
    if (!table_pa) {
      printf("GICv3: failed to allocate GITS_BASER(%u) table\n", n);
      return;
    }

    // Probe supported Page_Size (preferring 64K) and Shareability (WARL fields).
    arm_gicv3_write64(GITS_BASER(n), GITS_BASER_PAGE_SIZE_64K | GITS_BASER_SHAREABILITY_ISH);
    baser = arm_gicv3_read64(GITS_BASER(n));

    uint64_t page_size_field = baser & GITS_BASER_PAGE_SIZE_MASK;
    uint64_t num_pages = 16;
    uint64_t pa_field = table_pa & GITS_BASER_ADDR_MASK_4K_16K;
    if (page_size_field == GITS_BASER_PAGE_SIZE_64K) {
      num_pages = 1;
      pa_field = (table_pa & GITS_BASER_ADDR_MASK_64K) | ((table_pa >> 36) & 0xf000ULL);
    } else if (page_size_field == GITS_BASER_PAGE_SIZE_16K) {
      num_pages = 4;
    }

    uint64_t cache_share = GITS_BASER_INNER_CACHE_NC;
    if (baser & GITS_BASER_SHAREABILITY_MASK) {
      cache_share = GITS_BASER_SHAREABILITY_ISH | GITS_BASER_INNER_CACHE_WB;
    }
    arm_gicv3_write64(GITS_BASER(n), GITS_BASER_VALID | cache_share | page_size_field | pa_field |
                                         (num_pages - 1));
  }

  // Allocate and program the 64 KiB ITS Command Queue (GITS_CBASER).
  paddr_t cmdq_pa = gic_alloc_its_table();
  if (!cmdq_pa) {
    printf("GICv3: failed to allocate GITS command queue\n");
    return;
  }
  its_cmdq_va = static_cast<uint8_t*>(paddr_to_physmap(cmdq_pa));
  its_cmdq_cwriter = 0;

  uint64_t cbaser = GITS_CBASER_VALID | GITS_CBASER_INNER_CACHE_WB | GITS_CBASER_SHAREABILITY_ISH |
                    (cmdq_pa & GITS_CBASER_ADDR_MASK) | (kItsTablePages - 1);
  arm_gicv3_write64(GITS_CBASER, cbaser);
  if ((arm_gicv3_read64(GITS_CBASER) & GITS_CBASER_SHAREABILITY_MASK) == 0) {
    cbaser = (cbaser & ~(GITS_CBASER_SHAREABILITY_MASK | GITS_CBASER_INNER_CACHE_WB)) |
             GITS_CBASER_INNER_CACHE_NC;
    arm_gicv3_write64(GITS_CBASER, cbaser);
  }
  arm_gicv3_write64(GITS_CWRITER, 0);

  // Enable the ITS.
  arm_gicv3_write32(GITS_CTLR, GITS_CTLR_ENABLE);
  arch::DeviceMemoryBarrier();

  its_cpu0_rdbase = (gits_typer & GITS_TYPER_PTA)
                        ? ((gic_mmio_phys + gicr_offset) >> 16)
                        : ((arm_gicv3_read64(GICR_TYPER(0, 0)) >> 8) & 0xffffULL);

  // Map Collection 0 to CPU 0.
  if (!gic_its_cmd(kGitsCmdMapc, 0, (its_cpu0_rdbase << 16) | BIT_64(63))) {
    printf("GICv3: failed to map ITS collection 0\n");
    return;
  }

  // Map PCI bus 0 slots 0..31 (8 event ID bits = 256 events per device).
  size_t itt_entry_size =
      ((gits_typer >> GITS_TYPER_ITT_ENTRY_SIZE_SHIFT) & GITS_TYPER_ITT_ENTRY_SIZE_MASK) + 1;
  size_t itt_size = ROUNDUP(kMaxLpis * itt_entry_size, 256);
  paddr_t itt_pa = 0;
  size_t itt_rem = 0;
  for (uint32_t slot = 0; slot < kMaxItsPciSlots; ++slot) {
    if (itt_rem < itt_size) {
      itt_pa = gic_alloc_its_table();
      if (!itt_pa) {
        printf("GICv3: failed to allocate ITS ITT pool\n");
        return;
      }
      itt_rem = kItsTableSize;
    }
    if (!gic_its_cmd(kGitsCmdMapd | (uint64_t{slot << 3} << 32), 7,
                     (itt_pa & 0x000fffffffffff00ULL) | BIT_64(63), slot + 1 == kMaxItsPciSlots)) {
      printf("GICv3: failed to map ITS device %#x\n", slot << 3);
      return;
    }
    itt_pa += itt_size;
    itt_rem -= itt_size;
  }

  if (g_lpi_alloc.Init(kMaxLpis) != ZX_OK || g_lpi_alloc.AddRange(0, kMaxLpis) != ZX_OK) {
    printf("GICv3: failed to init LPI range allocator\n");
    return;
  }

  its_enabled = true;
  printf("GICv3 ITS enabled at %#lx (TYPER %#lx, %u LPIs)\n", gits_phys, gits_typer, kMaxLpis);
}

zx_status_t gic_init() {
  LTRACE_ENTRY;

  DEBUG_ASSERT(arch_ints_disabled());

  auto map_region = [](uint64_t phys_base, uint64_t size, const char* name) -> zx::result<vaddr_t> {
    void* ptr;
    zx_status_t status = VmAspace::kernel_aspace()->AllocPhysical(
        name, size, &ptr, PAGE_SIZE_SHIFT, phys_base, 0,
        ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_UNCACHED_DEVICE);
    if (status == ZX_OK) {
      return zx::ok(reinterpret_cast<vaddr_t>(ptr));
    }
    return zx::error{status};
  };

  // map in the distributor registers
  auto result = map_region(gic_mmio_phys + gicd_offset, GICD_REG_SIZE, "gicd");
  if (!result.is_ok()) {
    printf("GICv3: failed to map distributor registers");
    return result.error_value();
  }
  arm_gicv3_gicd_base = result.value();
  dprintf(SPEW, "GICv3: distributor mmio region [%#lx, %#lx)\n", arm_gicv3_gicd_base,
          arm_gicv3_gicd_base + GICD_REG_SIZE);

  uint pidr2 = arm_gicv3_read32(GICD_PIDR2);
  uint rev = BITS_SHIFT(pidr2, 7, 4);
  if (rev != GICV3 && rev != GICV4) {
    return ZX_ERR_NOT_FOUND;
  }

  // Compute the stride of the redistributor registers.
  // Override what was passed in from the bootloader, since this is the truth based on the GIC
  // revision.
  // TODO: remove the gicr_stride variable passed in from bootloaders
  if (rev == GICV3) {
    arm_gicv3_gicr_stride = 0x20000;
  } else if (rev == GICV4) {
    arm_gicv3_gicr_stride = 0x40000;
  }

  // map in the redistributor registers
  size_t redistributer_size = arch_max_num_cpus() * arm_gicv3_gicr_stride;
  result = map_region(gic_mmio_phys + gicr_offset, redistributer_size, "gicr");
  if (!result.is_ok()) {
    printf("GICv3: failed to map redistributor registers");
    return result.error_value();
  }
  arm_gicv3_gicr_base = result.value();
  dprintf(SPEW, "GICv3: redistributor mmio region [%#lx, %#lx)\n", arm_gicv3_gicr_base,
          arm_gicv3_gicr_base + redistributer_size);

  uint32_t typer = arm_gicv3_read32(GICD_TYPER);
  gic_max_int = (BITS(typer, 4, 0) + 1) * 32;

  printf("GICv3 detected: rev %u, max interrupts %u, TYPER %#x\n", rev, gic_max_int, typer);

  // disable the distributor
  arm_gicv3_write32(GICD_CTLR, 0);
  gic_wait_for_rwp(GICD_CTLR);
  __isb(ARM_MB_SY);

  // distributor config: mask and clear all spis, set group 1.
  uint i;
  for (i = 32; i < gic_max_int; i += 32) {
    arm_gicv3_write32(GICD_ICENABLER(i / 32), ~0);
    arm_gicv3_write32(GICD_ICPENDR(i / 32), ~0);
    arm_gicv3_write32(GICD_IGROUPR(i / 32), ~0);
    arm_gicv3_write32(GICD_IGRPMODR(i / 32), 0);
  }
  gic_wait_for_rwp(GICD_CTLR);

  // enable distributor with ARE, group 1 enable
  arm_gicv3_write32(GICD_CTLR, CTLR_ENABLE_G0 | CTLR_ENABLE_G1NS | CTLR_ARE_S);
  gic_wait_for_rwp(GICD_CTLR);

  // ensure we're running on cpu 0
  DEBUG_ASSERT(arch_curr_cpu_num() == 0);

  // set spi to target cpu 0 and its associated affinity. must do this after ARE enable
  uint max_cpu = BITS_SHIFT(typer, 7, 5);
  if (max_cpu > 0) {
    for (i = 32; i < gic_max_int; i++) {
      arm_gicv3_write64(GICD_IROUTER(i), arch_cpu_num_to_mpidr(0));
    }
  }

  gic_init_percpu_early();

  gic_init_its(typer, map_region);

  arch::DeviceMemoryBarrier();
  __isb(ARM_MB_SY);

  return ZX_OK;
}

// Extract AFF3, AFF2, and AFF1 field out of a mpidr and format according to the ICC_SGI1R
// register.
constexpr uint64_t mpidr_aff_mask_to_sgir_mask(uint64_t mpidr) {
  uint64_t mask = ((mpidr & MPIDR_AFF3_MASK) >> MPIDR_AFF3_SHIFT) << 48;
  mask |= ((mpidr & MPIDR_AFF2_MASK) >> MPIDR_AFF2_SHIFT) << 32;
  mask |= ((mpidr & MPIDR_AFF1_MASK) >> MPIDR_AFF1_SHIFT) << 16;
  return mask;
}

// Send a pending IPI for the AFF3-1 cluster we've been accumulating a mask for.
void send_sgi_for_cluster(unsigned int irq, uint64_t aff321, uint64_t aff0_mask) {
  if (aff0_mask) {
    DEBUG_ASSERT((aff0_mask & 0xffff) == aff0_mask);
    const uint64_t sgi1r = ((irq & 0xf) << 24) | mpidr_aff_mask_to_sgir_mask(aff321) | aff0_mask;
    gic_write_sgi1r(sgi1r);
  }
}

zx_status_t arm_gic_sgi(const unsigned int irq, const unsigned int flags, unsigned int cpu_mask) {
  LTRACEF("irq %u, flags %u, cpu_mask %#x\n", irq, flags, cpu_mask);

  if (flags != ARM_GIC_SGI_FLAG_NS) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (irq >= 16) {
    return ZX_ERR_INVALID_ARGS;
  }

  arch::ThreadMemoryBarrier();

  uint64_t curr_aff321 = 0;  // Current AFF3-1 we're dealing with.
  uint64_t aff0_mask = 0;    // 16 bit mask of the AFF0 we're accumulating.

  for (cpu_num_t cpu = 0; cpu_mask && cpu < arch_max_num_cpus(); cpu++) {
    const uint64_t mpidr = arch_cpu_num_to_mpidr(cpu);
    const uint64_t aff321 = mpidr & (MPIDR_AFF3_MASK | MPIDR_AFF2_MASK | MPIDR_AFF1_MASK);
    const uint64_t aff0 = mpidr & (MPIDR_AFF0_MASK);

    // Without the RS field set, we can only deal with the first
    // 16 cpus within a single cluster.
    DEBUG_ASSERT(aff0 < 16);

    if (aff321 != curr_aff321) {
      // AFF3-1 has changed, see if we need to fire a pending IPI
      send_sgi_for_cluster(irq, curr_aff321, aff0_mask);
      curr_aff321 = aff321;
      aff0_mask = 0;
    }

    // This cpu is within the current aff mask we're looking at, accumulate.
    if (cpu_mask & (1u << cpu)) {
      cpu_mask &= ~(1u << cpu);
      aff0_mask |= 1u << aff0;
    }
  }

  // Fire any leftover accumulated mask.
  send_sgi_for_cluster(irq, curr_aff321, aff0_mask);

  return ZX_OK;
}

zx_status_t gic_mask_interrupt(interrupt_vector_t vector) {
  LTRACEF("vector %u\n", vector);

  if (vector >= gic_max_int) {
    return ZX_ERR_INVALID_ARGS;
  }

  gic_set_enable(vector, false);

  return ZX_OK;
}

zx_status_t gic_unmask_interrupt(interrupt_vector_t vector) {
  LTRACEF("vector %u\n", vector);

  if (vector >= gic_max_int) {
    return ZX_ERR_INVALID_ARGS;
  }

  gic_set_enable(vector, true);

  return ZX_OK;
}

zx_status_t gic_deactivate_interrupt(interrupt_vector_t vector) {
  if (vector >= gic_max_int) {
    return ZX_ERR_INVALID_ARGS;
  }

  uint32_t reg = 1 << (vector % 32);
  arm_gicv3_write32(GICD_ICACTIVER(vector / 32), reg);

  return ZX_OK;
}

zx_status_t gic_configure_interrupt(interrupt_vector_t vector, enum interrupt_trigger_mode tm,
                                    enum interrupt_polarity pol) {
  LTRACEF("vector %u, trigger mode %s, polarity %s\n", vector, interrupt_trigger_mode_string(tm),
          interrupt_polarity_string(pol));

  if (vector <= 15 || vector >= gic_max_int) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (pol != interrupt_polarity::HIGH) {
    // TODO: polarity should actually be configure through a GPIO controller
    return ZX_ERR_NOT_SUPPORTED;
  }

  uint reg = vector / 16;
  uint mask = 0x2 << ((vector % 16) * 2);
  uint32_t val = arm_gicv3_read32(GICD_ICFGR(reg));
  if (tm == interrupt_trigger_mode::EDGE) {
    val |= mask;
  } else {
    val &= ~mask;
  }
  arm_gicv3_write32(GICD_ICFGR(reg), val);

  const uint32_t clear_reg = vector / 32;
  const uint32_t clear_mask = 1 << (vector % 32);
  arm_gicv3_write32(GICD_ICPENDR(clear_reg), clear_mask);

  return ZX_OK;
}

zx_status_t gic_get_interrupt_config(interrupt_vector_t vector, interrupt_trigger_mode* tm,
                                     interrupt_polarity* pol) {
  LTRACEF("vector %u\n", vector);

  if (vector >= gic_max_int) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (tm) {
    *tm = interrupt_trigger_mode::EDGE;
  }
  if (pol) {
    *pol = interrupt_polarity::HIGH;
  }

  return ZX_OK;
}

zx_status_t gic_set_affinity(interrupt_vector_t vector, cpu_mask_t mask) {
  LTRACEF("vector %u, mask %#x\n", vector, mask);

  // Don't complain if we are asked to set affinity for the boot CPU, this is
  // what we always do by default.
  if (mask == cpu_num_to_mask(BOOT_CPU_ID)) {
    return ZX_OK;
  }

  return ZX_ERR_NOT_SUPPORTED;
}

interrupt_vector_t gic_remap_interrupt(interrupt_vector_t vector) {
  LTRACEF("vector %u\n", vector);
  return vector;
}

// called from assembly
void gic_handle_irq(iframe_t* frame) {
  // ICC_IAR1_EL1 returns a 24-bit INTID in bits [23:0]:
  //   0..15:                SGIs
  //   16..31:               PPIs
  //   32..1019 (0x3fb):     SPIs
  //   1020..1023 (0x3fc..0x3ff): Special / spurious INTIDs
  //   8192+ (kLpiBase+):    LPIs
  uint32_t iar = gic_read_iar();
  uint32_t vector = iar & 0xffffff;

  LTRACEF_LEVEL(2, "iar %#x, vector %u\n", iar, vector);

  // Ignore architectural special/spurious INTIDs (1020..1023) without masking
  // out LPIs (INTID >= 8192). Special INTIDs must not be written to EOIR.
  if (vector >= 0x3fc && vector <= 0x3ff) {
    return;
  }

  // tracking external hardware irqs in this variable
  if (vector >= 32) {
    CPU_STATS_INC(interrupts);
  }

  ktrace::Scope trace = KTRACE_CPU_BEGIN_SCOPE("kernel:irq", "irq", ("irq #", vector));

  LTRACEF_LEVEL(2, "iar 0x%x cpu %u currthread %p vector %u pc %#" PRIxPTR "\n", iar,
                arch_curr_cpu_num(), Thread::Current::Get(), vector, (uintptr_t)IFRAME_PC(frame));

  // Deliver LPIs (INTID >= 8192) via the ITS LPI handler table, and
  // SGIs/PPIs/SPIs via the standard pdev interrupt table.
  if (vector >= kLpiBase) {
    uint32_t idx = vector - kLpiBase;
    if (idx < kMaxLpis) {
      interrupt_handler_t handler;
      {
        Guard<MonitoredSpinLock, NoIrqSave> guard{GicItsLock::Get(), SOURCE_TAG};
        handler = g_lpi_handlers[idx];
      }
      if (handler) {
        handler.fn(handler.cookie);
      }
    }
  } else {
    pdev_invoke_int_if_present(vector);
  }
  gic_write_eoir(vector);

  LTRACEF_LEVEL(2, "cpu %u exit\n", arch_curr_cpu_num());
}

zx_status_t gic_send_ipi(cpu_mask_t target, mp_ipi ipi) {
  uint gic_ipi_num = static_cast<uint32_t>(ipi) + ipi_base;

  // filter out targets outside of the range of cpus we care about
  target &= static_cast<cpu_mask_t>(((1UL << arch_max_num_cpus()) - 1));
  if (target != 0) {
    LTRACEF("target 0x%x, gic_ipi %u\n", target, gic_ipi_num);
    arm_gic_sgi(gic_ipi_num, ARM_GIC_SGI_FLAG_NS, target);
  }

  return ZX_OK;
}

void arm_ipi_halt_handler() {
  LTRACEF("cpu %u\n", arch_curr_cpu_num());

  arch_disable_ints();
  while (true) {
    __wfi();
  }
}

void gic_init_percpu() {
  mp_set_curr_cpu_online(true);
  unmask_interrupt(static_cast<uint32_t>(mp_ipi::GENERIC) + ipi_base);
  unmask_interrupt(static_cast<uint32_t>(mp_ipi::RESCHEDULE) + ipi_base);
  unmask_interrupt(static_cast<uint32_t>(mp_ipi::INTERRUPT) + ipi_base);
  unmask_interrupt(static_cast<uint32_t>(mp_ipi::HALT) + ipi_base);
}

void gic_shutdown() {
  // Turn off all GIC0 interrupts at the distributor.
  arm_gicv3_write32(GICD_CTLR, 0);
}

// Returns true if any PPIs are enabled on the calling CPU.
[[maybe_unused]] bool is_ppi_enabled() {
  DEBUG_ASSERT(arch_ints_disabled());

  // PPIs are 16-31.
  uint32_t mask = 0xffff0000;

  cpu_num_t cpu_num = arch_curr_cpu_num();
  uint32_t reg = arm_gicv3_read32(GICR_ICENABLER0(cpu_num));

  return (reg & mask);
}

// Returns true if any SPIs are enabled on the calling CPU.
[[maybe_unused]] bool is_spi_enabled() {
  DEBUG_ASSERT(arch_ints_disabled());

  const cpu_num_t cpu_num = arch_curr_cpu_num();
  const uint64_t mpidr = arch_cpu_num_to_mpidr(cpu_num);
  const uint64_t aff_mask = mpidr & ARM64_MPIDR_MASK;

  // Check each SPI to see if it's routed to this CPU and enabled.
  for (uint i = 32u; i < gic_max_int; ++i) {
    if ((arm_gicv3_read64(GICD_IROUTER(i)) & ARM64_MPIDR_MASK) == aff_mask) {
      uint32_t reg_n = i / 32;
      uint32_t bit_n = 1U << (i % 32);
      if (arm_gicv3_read32(GICD_ISENABLER(reg_n)) & bit_n) {
        return true;
      }
    }
  }

  return false;
}

void gic_shutdown_cpu() {
  DEBUG_ASSERT(arch_ints_disabled());

  // If we're running on a secondary CPU there's a good chance this CPU will be powered off
  // shortly (PSCI_CPU_OFF).  Sending an interrupt to a CPU that's been powered off may result in
  // an "erronerous state" (see Power State Coordination Interface (PSCI) System Software on ARM
  // specification, 5.5.2).  So before we shutdown the GIC, make sure we've migrated/disabled any
  // and all peripheral interrupts targeted at this CPU (PPIs and SPIs).
  //
  // Note, we don't perform these checks on the boot CPU because we don't call PSCI_CPU_OFF on the
  // boot CPU, and we likely still have PPIs and SPIs targeting the boot CPU.
  DEBUG_ASSERT(arch_curr_cpu_num() == BOOT_CPU_ID || !is_ppi_enabled());
  DEBUG_ASSERT(arch_curr_cpu_num() == BOOT_CPU_ID || !is_spi_enabled());
  // TODO(maniscalco): If/when we start using LPIs, make sure none are targeted at this CPU.

  // Disable group 1 interrupts at the CPU interface.
  gic_write_igrpen(0);

  // Mark the PE as offline. This will keep the redistributor from routing
  // interrupts and for any interrupts targeting it, trigger a wake-request to
  // the power controller.
  gic_redistributor_sleep(true);
}

zx_status_t gic_suspend_cpu() {
  DEBUG_ASSERT(arch_ints_disabled());

  gic_redistributor_sleep(true);

  return ZX_OK;
}

zx_status_t gic_resume_cpu() {
  gic_init_percpu_early();
  gic_init_percpu();
  return ZX_OK;
}

bool gic_msi_is_supported() { return its_enabled; }

bool gic_msi_supports_masking() { return its_enabled; }

void gic_msi_mask_unmask(const msi_block_t* block, uint msi_id, bool mask) {
  DEBUG_ASSERT(block && block->allocated);
  DEBUG_ASSERT(msi_id < block->num_irq);
  uint32_t lpi_id = block->base_irq_id + msi_id;
  DEBUG_ASSERT(lpi_id >= kLpiBase && lpi_id < kLpiBase + kMaxLpis);
  uint32_t idx = lpi_id - kLpiBase;

  Guard<MonitoredSpinLock, IrqSave> guard{GicItsLock::Get(), SOURCE_TAG};
  gic_lpi_set_enable_locked(idx, !mask);
}

zx_status_t gic_msi_alloc_block(uint requested_irqs, bool can_target_64bit, bool is_msix,
                                msi_block_t* out_block) {
  if (!out_block) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (out_block->allocated) {
    return ZX_ERR_BAD_STATE;
  }
  if (!its_enabled) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  if (!requested_irqs || requested_irqs > MAX_MSI_IRQS) {
    return ZX_ERR_INVALID_ARGS;
  }

  uint64_t doorbell = gits_phys + GITS_TRANSLATER_OFFSET;
  bool is_32bit = (doorbell >> 32) == 0;
  if (!can_target_64bit && !is_32bit) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  uint32_t alloc_size = 1u << log2_uint_ceil(requested_irqs);
  uint32_t alloc_start = 0;
  zx_status_t status = g_lpi_alloc.AllocateRange(alloc_size, &alloc_start);
  if (status != ZX_OK) {
    return status;
  }

  {
    Guard<MonitoredSpinLock, IrqSave> guard{GicItsLock::Get(), SOURCE_TAG};
    for (uint32_t i = 0; i < alloc_size; ++i) {
      uint32_t idx = alloc_start + i;
      if (!lpi_mapped[idx]) {
        for (uint32_t slot = 0; slot < kMaxItsPciSlots; ++slot) {
          gic_its_cmd(kGitsCmdMapti | (uint64_t{slot << 3} << 32),
                      idx | (uint64_t{kLpiBase + idx} << 32), 0, false);
        }
        lpi_mapped[idx] = true;
      }
      gic_lpi_set_enable_locked(idx, true);
    }
  }

  out_block->is_32bit = is_32bit;
  out_block->base_irq_id = kLpiBase + alloc_start;
  out_block->num_irq = alloc_size;
  out_block->tgt_addr = doorbell;
  out_block->tgt_data = alloc_start;
  out_block->allocated = true;
  return ZX_OK;
}

void gic_msi_free_block(msi_block_t* block) {
  DEBUG_ASSERT(block);
  DEBUG_ASSERT(block->allocated);
  DEBUG_ASSERT(block->base_irq_id >= kLpiBase &&
               block->base_irq_id + block->num_irq <= kLpiBase + kMaxLpis);

  uint32_t alloc_start = block->base_irq_id - kLpiBase;
  {
    Guard<MonitoredSpinLock, IrqSave> guard{GicItsLock::Get(), SOURCE_TAG};
    for (uint32_t i = 0; i < block->num_irq; ++i) {
      uint32_t idx = alloc_start + i;
      g_lpi_handlers[idx] = nullptr;
      gic_lpi_set_enable_locked(idx, false);
    }
  }

  g_lpi_alloc.FreeRange(alloc_start, block->num_irq);
  memset(block, 0, sizeof(*block));
}

void gic_msi_register_handler(const msi_block_t* block, uint msi_id, interrupt_handler_t handler) {
  DEBUG_ASSERT(block && block->allocated);
  DEBUG_ASSERT(msi_id < block->num_irq);
  uint32_t lpi_id = block->base_irq_id + msi_id;
  DEBUG_ASSERT(lpi_id >= kLpiBase && lpi_id < kLpiBase + kMaxLpis);
  uint32_t idx = lpi_id - kLpiBase;

  Guard<MonitoredSpinLock, IrqSave> guard{GicItsLock::Get(), SOURCE_TAG};
  g_lpi_handlers[idx] = handler;
  gic_lpi_set_enable_locked(idx, static_cast<bool>(handler));
}

zx_status_t gic_get_status(interrupt_vector_t vector, bool& out_pending, bool& out_enabled) {
  out_pending = out_enabled = false;
  if (vector >= gic_max_int) {
    return ZX_ERR_INVALID_ARGS;
  }

  const uint32_t reg = vector / 32;
  const uint32_t mask = static_cast<uint32_t>(1ULL << (vector % 32));
  if (vector < 32) {
    cpu_num_t cpu_id = arch_curr_cpu_num();
    out_enabled = (arm_gicv3_read32(GICR_ISENABLER0(cpu_id)) & mask) != 0;
    out_pending = (arm_gicv3_read32(GICR_ISPENDR0(cpu_id)) & mask) != 0;
  } else {
    out_enabled = (arm_gicv3_read32(GICD_ISENABLER(reg)) & mask) != 0;
    out_pending = (arm_gicv3_read32(GICD_ISPENDR(reg)) & mask) != 0;
  }

  return ZX_OK;
}

const struct pdev_interrupt_ops gic_ops = {
    .mask = gic_mask_interrupt,
    .unmask = gic_unmask_interrupt,
    .deactivate = gic_deactivate_interrupt,
    .configure = gic_configure_interrupt,
    .get_config = gic_get_interrupt_config,
    .set_affinity = gic_set_affinity,
    .is_valid = gic_is_valid_interrupt,
    .get_base_vector = gic_get_base_vector,
    .get_max_vector = gic_get_max_vector,
    .remap = gic_remap_interrupt,
    .send_ipi = gic_send_ipi,
    .init_percpu_early = gic_init_percpu_early,
    .init_percpu = gic_init_percpu,
    .handle_irq = gic_handle_irq,
    .shutdown = gic_shutdown,
    .shutdown_cpu = gic_shutdown_cpu,
    .suspend_cpu = gic_suspend_cpu,
    .resume_cpu = gic_resume_cpu,
    .msi_is_supported = gic_msi_is_supported,
    .msi_supports_masking = gic_msi_supports_masking,
    .msi_mask_unmask = gic_msi_mask_unmask,
    .msi_alloc_block = gic_msi_alloc_block,
    .msi_free_block = gic_msi_free_block,
    .msi_register_handler = gic_msi_register_handler,
    .get_status = gic_get_status,
};

}  // anonymous namespace

void ArmGicInitEarly(const zbi_dcfg_arm_gic_v3_driver_t& config) {
  ASSERT(config.mmio_phys);

  gic_mmio_phys = config.mmio_phys;
  gicd_offset = config.gicd_offset;
  gicr_offset = config.gicr_offset;
  arm_gicv3_gicr_stride = config.gicr_stride;
  ipi_base = config.ipi_base;
  if (config.its_phys != 0) {
    gits_phys = config.its_phys;
  } else if (config.gicr_offset >= 0xa0000) {
    gits_phys = config.mmio_phys + 0x80000;
  }
}

void ArmGicInitPostVm(const zbi_dcfg_arm_gic_v3_driver_t& config) {
  if (gic_init() != ZX_OK) {
    if (config.optional) {
      // failed to detect gic v3 but it's marked optional. continue
      return;
    }
    printf("GICv3: failed to detect GICv3, interrupts will be broken\n");
    return;
  }

  dprintf(SPEW,
          "GICv3: IPI base %u, MMIO phys %#lx, GICD offset %#lx, "
          "GICR offset/stride %#lx/%#lx\n",
          ipi_base, gic_mmio_phys, gicd_offset, gicr_offset, arm_gicv3_gicr_stride);

  pdev_register_interrupts(&gic_ops);

  zx_status_t status =
      gic_register_sgi_handler(static_cast<uint32_t>(mp_ipi::GENERIC) + ipi_base,
                               interrupt_handler_t{nullptr, [](void*) { mp_mbx_generic_irq(); }});
  DEBUG_ASSERT(status == ZX_OK);
  status = gic_register_sgi_handler(
      static_cast<uint32_t>(mp_ipi::RESCHEDULE) + ipi_base,
      interrupt_handler_t{nullptr, [](void*) { mp_mbx_reschedule_irq(); }});
  DEBUG_ASSERT(status == ZX_OK);
  status =
      gic_register_sgi_handler(static_cast<uint32_t>(mp_ipi::INTERRUPT) + ipi_base,
                               interrupt_handler_t{nullptr, [](void*) { mp_mbx_interrupt_irq(); }});
  DEBUG_ASSERT(status == ZX_OK);
  status =
      gic_register_sgi_handler(static_cast<uint32_t>(mp_ipi::HALT) + ipi_base,
                               interrupt_handler_t{nullptr, [](void*) { arm_ipi_halt_handler(); }});
  DEBUG_ASSERT(status == ZX_OK);

  gicv3_hw_interface_register();
}

void ArmGicInitLate(const zbi_dcfg_arm_gic_v3_driver_t& config) {
  ASSERT(gic_mmio_phys);

  // Place the physical address of the GICv3 registers on the MMIO deny list.
  // Users will not be able to create MMIO resources which permit mapping of the
  // GIC registers, even if they have access to the root resource.
  //
  // Unlike GICv2, only the distributor and re-distributor registers are memory
  // mapped. There is one block of distributor registers for the system, and
  // one block of redistributor registers for each CPU.
  root_resource_filter_add_deny_region(gic_mmio_phys + gicd_offset, GICD_REG_SIZE,
                                       ZX_RSRC_KIND_MMIO);
  for (cpu_num_t i = 0; i < arch_max_num_cpus(); i++) {
    root_resource_filter_add_deny_region(gic_mmio_phys + gicr_offset + (arm_gicv3_gicr_stride * i),
                                         arm_gicv3_gicr_stride, ZX_RSRC_KIND_MMIO);
  }
  if (arm_gicv3_gits_base != 0) {
    root_resource_filter_add_deny_region(gits_phys, GITS_REG_SIZE, ZX_RSRC_KIND_MMIO);
  }
}
