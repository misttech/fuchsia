// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/counters.h>
#include <lib/fit/defer.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <pow2.h>
#include <sys/types.h>
#include <trace.h>

#include <fbl/ref_counted.h>
#include <ktl/bit.h>
#include <ktl/utility.h>
#include <object/msi_dispatcher.h>

#include <ktl/enforce.h>

KCOUNTER(msi_create_count, "msi.create")
KCOUNTER(msi_destroy_count, "msi.destroy")

#define LOCAL_TRACE 0

zx_status_t MsiAllocation::Create(uint32_t irq_cnt, fbl::RefPtr<MsiAllocation>* obj,
                                  MsiAllocFn msi_alloc_fn, MsiFreeFn msi_free_fn,
                                  MsiSupportedFn msi_support_fn) {
  if (!msi_support_fn()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  msi_block_t block = {};
  auto cleanup = fit::defer([&msi_free_fn, &block]() {
    if (block.allocated) {
      msi_free_fn(&block);
    }
  });

  // Ensure the requested IRQs fit within the mask of permitted IRQs in an
  // allocation. MSI allocations must be a power of two.
  // MSI supports up to 32, MSI-X supports up to 2048.
  if (irq_cnt == 0 || irq_cnt > kMsiAllocationCountMax || !ispow2(irq_cnt)) {
    return ZX_ERR_INVALID_ARGS;
  }

  zx_status_t st = msi_alloc_fn(irq_cnt, false /* can_target_64bit */, false /* is_msix */, &block);
  if (st != ZX_OK) {
    return st;
  }
  ZX_DEBUG_ASSERT(block.allocated);

  LTRACEF("MSI Allocation: { tgr_addr = 0x%lx, tgt_data = 0x%08x, base_irq_id = %u }\n",
          block.tgt_addr, block.tgt_data, block.base_irq_id);

  ktl::array<char, ZX_MAX_NAME_LEN> name;
  if (block.num_irq == 1) {
    snprintf(name.data(), name.max_size(), "MSI vector %u", block.base_irq_id);
  } else {
    snprintf(name.data(), name.max_size(), "MSI vectors [%u, %u)", block.base_irq_id,
             block.base_irq_id + block.num_irq - 1);
  }

  fbl::AllocChecker ac;
  auto msi = fbl::AdoptRef<MsiAllocation>(new (&ac) MsiAllocation(block, msi_free_fn));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  kcounter_add(msi_create_count, 1);
  cleanup.cancel();
  *obj = ktl::move(msi);
  return ZX_OK;
}

zx_status_t MsiAllocation::ReserveId(MsiId msi_id) {
  if (msi_id >= block_.num_irq) {
    return ZX_ERR_INVALID_ARGS;
  }

  const IdBitMaskType mask = (1 << msi_id);
  const IdBitMaskType prev_value = ids_in_use_.fetch_or(mask, ktl::memory_order_relaxed);
  if (prev_value & mask) {
    return ZX_ERR_ALREADY_BOUND;
  }
  return ZX_OK;
}

zx_status_t MsiAllocation::ReleaseId(MsiId msi_id) {
  if (msi_id >= block_.num_irq) {
    return ZX_ERR_INVALID_ARGS;
  }

  const IdBitMaskType mask = (1 << msi_id);
  const IdBitMaskType prev_value = ids_in_use_.fetch_and(~mask, ktl::memory_order_relaxed);
  if (!(prev_value & mask)) {
    return ZX_ERR_BAD_STATE;
  }
  return ZX_OK;
}

MsiAllocation::~MsiAllocation() {
  DEBUG_ASSERT(ids_in_use_.load() == 0);

  msi_block_t block = block_;
  if (block.allocated) {
    msi_free_fn_(&block);
  }
  DEBUG_ASSERT(!block.allocated);
  kcounter_add(msi_destroy_count, 1);
}

zx_info_msi MsiAllocation::GetInfo() const TA_EXCL(lock_) {
  return {
      .target_addr = block_.tgt_addr,
      .target_data = block_.tgt_data,
      .base_irq_id = block_.base_irq_id,
      .num_irq = block_.num_irq,
      .interrupt_count = static_cast<uint32_t>(ktl::popcount(ids_in_use_.load())),
  };
}
