// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <string.h>

#include <arch/arm64/periphmap.h>
#include <hwreg/mmio.h>
#include <vm/pmm.h>

zx_status_t PmmMock::AllocPage(uint32_t alloc_flags, vm_page_t** p) {
  {
    std::lock_guard<std::mutex> guard(lock_);
    if (alloc_limit_) {
      if (*alloc_limit_ == 0) {
        return ZX_ERR_NO_MEMORY;
      }
      --(*alloc_limit_);
    }
  }

  std::unique_ptr<vm_page_t> page = std::make_unique<vm_page_t>();

  // Allocate separate 4k buffers for CPU cache view and PoC DRAM view.
  uint8_t* page_mem = new (std::align_val_t(kPageSize)) uint8_t[kPageSize];
  if (!page_mem) {
    return ZX_ERR_NO_MEMORY;
  }
  memset(page_mem, 0xa5, kPageSize);
  page->paddr_priv.reset(page_mem);

  uint8_t* poc_mem = new (std::align_val_t(kPageSize)) uint8_t[kPageSize];
  if (!poc_mem) {
    return ZX_ERR_NO_MEMORY;
  }
  memset(poc_mem, 0xa5, kPageSize);
  page->poc_dram_priv.reset(poc_mem);

  std::lock_guard<std::mutex> guard(lock_);
  *p = page.get();
  paddr_t pa = (*p)->paddr();
  allocated_pages_[pa] = std::move(page);
  return ZX_OK;
}

void PmmMock::Free(VmPageDoublyLinkedList* list) {
  while (!list->is_empty()) {
    vm_page_t* p = list->pop_front();
    const paddr_t pa = p->paddr();

    PageFreeObserver on_free;
    {
      std::lock_guard<std::mutex> guard(lock_);
      if (on_page_free_) {
        on_free = [this](paddr_t page_pa) { on_page_free_(page_pa); };
      }
    }
    if (on_free) {
      on_free(pa);
    }

    uint8_t* ptr = p->paddr_priv.get();
    uint8_t* poc_ptr = p->poc_dram_priv.get();
    for (size_t i = 0; i < kPageSize; ++i) {
      ZX_ASSERT_MSG(ptr[i] == 0,
                    "Page at paddr 0x%lx not zeroed in CPU cache view at offset 0x%zx (val=0x%x)",
                    pa, i, ptr[i]);
      ZX_ASSERT_MSG(poc_ptr[i] == 0,
                    "Page at paddr 0x%lx not cleaned/zeroed in PoC DRAM view at offset 0x%zx "
                    "(val=0x%x) before PMM free",
                    pa, i, poc_ptr[i]);
    }

    std::lock_guard<std::mutex> guard(lock_);
    allocated_pages_.erase(pa);
  }
}

size_t PmmMock::GetAllocatedPageCount() {
  std::lock_guard<std::mutex> guard(lock_);
  return allocated_pages_.size();
}

vm_page_t* PmmMock::PaddrToVmPage(paddr_t pa) {
  std::lock_guard<std::mutex> guard(lock_);
  auto it = allocated_pages_.find(pa);
  if (it != allocated_pages_.end()) {
    return it->second.get();
  }
  return nullptr;
}

void PmmMock::SetAllocationLimit(uint64_t limit) {
  std::lock_guard<std::mutex> guard(lock_);
  alloc_limit_ = limit;
}

void PmmMock::ClearAllocationLimit() {
  std::lock_guard<std::mutex> guard(lock_);
  alloc_limit_ = std::nullopt;
}

void PmmMock::CleanDataCacheRange(vaddr_t addr, size_t size) {
  if (size == 0) {
    return;
  }
  const paddr_t page_pa = addr & ~paddr_t{kPageSize - 1};
  const size_t offset = addr & (kPageSize - 1);
  ZX_ASSERT_MSG(offset + size <= kPageSize,
                "CleanDataCacheRange(0x%lx, 0x%zx) crosses page boundary", addr, size);

  CacheCleanObserver on_clean;
  {
    std::lock_guard<std::mutex> guard(lock_);
    auto it = allocated_pages_.find(page_pa);
    ZX_ASSERT_MSG(it != allocated_pages_.end(),
                  "CleanDataCacheRange on unallocated page paddr 0x%lx", page_pa);
    vm_page_t* page = it->second.get();
    memcpy(page->poc_dram_priv.get() + offset, page->paddr_priv.get() + offset, size);
    if (on_cache_clean_) {
      on_clean = [this](paddr_t pa, size_t off, size_t sz) { on_cache_clean_(pa, off, sz); };
    }
  }
  if (on_clean) {
    on_clean(page_pa, offset, size);
  }
}

void PmmMock::OnPteWrite(const uint64_t* slot, uint64_t new_val, uint32_t level) {
  const paddr_t slot_pa = reinterpret_cast<paddr_t>(slot);
  const paddr_t table_pa = slot_pa & ~paddr_t{kPageSize - 1};
  const uint32_t index = static_cast<uint32_t>((slot_pa & (kPageSize - 1)) / sizeof(uint64_t));
  const uint64_t old_val = *slot;

  PteWriteObserver on_write;
  {
    std::lock_guard<std::mutex> guard(lock_);
    auto it = allocated_pages_.find(table_pa);
    ZX_ASSERT_MSG(it != allocated_pages_.end(), "PTE write on unallocated page table paddr 0x%lx",
                  table_pa);
    if (on_pte_write_) {
      on_write = [this](paddr_t pa, uint32_t idx, uint64_t old_v, uint64_t new_v, uint32_t lvl) {
        on_pte_write_(pa, idx, old_v, new_v, lvl);
      };
    }
  }
  if (on_write) {
    on_write(table_pa, index, old_val, new_val, level);
  }
}

void PmmMock::OnTableZero(vaddr_t addr, size_t size) {
  const paddr_t table_pa = addr & ~paddr_t{kPageSize - 1};
  const size_t offset = addr & (kPageSize - 1);
  ZX_ASSERT_MSG((offset % sizeof(uint64_t)) == 0 && (size % sizeof(uint64_t)) == 0 &&
                    offset + size <= kPageSize,
                "OnTableZero(0x%lx, 0x%zx) is not a run of descriptors within one page", addr,
                size);
  PteWriteObserver on_write;
  {
    std::lock_guard<std::mutex> guard(lock_);
    ZX_ASSERT_MSG(allocated_pages_.find(table_pa) != allocated_pages_.end(),
                  "Zero-fill of unallocated page table paddr 0x%lx", table_pa);
    if (on_pte_write_) {
      on_write = [this](paddr_t pa, uint32_t idx, uint64_t old_v, uint64_t new_v, uint32_t lvl) {
        on_pte_write_(pa, idx, old_v, new_v, lvl);
      };
    }
  }
  if (!on_write) {
    return;
  }
  const uint64_t* slots = reinterpret_cast<const uint64_t*>(addr);
  for (size_t i = 0; i < size / sizeof(uint64_t); ++i) {
    if (slots[i] != 0) {
      on_write(table_pa, static_cast<uint32_t>(offset / sizeof(uint64_t) + i), slots[i], 0,
               kPteLevelUnknown);
    }
  }
}

std::optional<uint64_t> PmmMock::ReadPocDram64(paddr_t pa) {
  std::lock_guard<std::mutex> guard(lock_);
  const paddr_t page_pa = pa & ~paddr_t{kPageSize - 1};
  const size_t offset = pa & (kPageSize - 1);
  if ((offset & (sizeof(uint64_t) - 1)) != 0 || offset + sizeof(uint64_t) > kPageSize) {
    return std::nullopt;
  }
  auto it = allocated_pages_.find(page_pa);
  if (it == allocated_pages_.end()) {
    return std::nullopt;
  }
  uint64_t val = 0;
  memcpy(&val, it->second->poc_dram_priv.get() + offset, sizeof(uint64_t));
  return val;
}

std::optional<uint64_t> PmmMock::ReadCpuCache64(paddr_t pa) {
  std::lock_guard<std::mutex> guard(lock_);
  const paddr_t page_pa = pa & ~paddr_t{kPageSize - 1};
  const size_t offset = pa & (kPageSize - 1);
  if ((offset & (sizeof(uint64_t) - 1)) != 0 || offset + sizeof(uint64_t) > kPageSize) {
    return std::nullopt;
  }
  auto it = allocated_pages_.find(page_pa);
  if (it == allocated_pages_.end()) {
    return std::nullopt;
  }
  uint64_t val = 0;
  memcpy(&val, it->second->paddr_priv.get() + offset, sizeof(uint64_t));
  return val;
}

void PmmMock::SetObservers(PteWriteObserver on_pte_write, CacheCleanObserver on_cache_clean,
                           PageFreeObserver on_page_free) {
  std::lock_guard<std::mutex> guard(lock_);
  on_pte_write_ = std::move(on_pte_write);
  on_cache_clean_ = std::move(on_cache_clean);
  on_page_free_ = std::move(on_page_free);
}

void PmmMock::ClearObservers() {
  std::lock_guard<std::mutex> guard(lock_);
  on_pte_write_ = nullptr;
  on_cache_clean_ = nullptr;
  on_page_free_ = nullptr;
}

zx_status_t pmm_alloc_page(uint32_t alloc_flags, vm_page_t** p) {
  return PmmMock::Get().AllocPage(alloc_flags, p);
}

zx_status_t pmm_alloc_page(uint32_t alloc_flags, vm_page_t** p, paddr_t* pa) {
  zx_status_t status = pmm_alloc_page(alloc_flags, p);
  if (status == ZX_OK) {
    *pa = (*p)->paddr();
  }
  return status;
}

void pmm_free(VmPageDoublyLinkedList* list) { PmmMock::Get().Free(list); }

vm_page_t* paddr_to_vm_page(paddr_t pa) { return PmmMock::Get().PaddrToVmPage(pa); }

vaddr_t periph_paddr_to_vaddr(paddr_t paddr) {
  return FakeSmmuMmioRouter::Get().PaddrToVaddr(paddr);
}

paddr_t periph_vaddr_to_paddr(vaddr_t vaddr) {
  return FakeSmmuMmioRouter::Get().VaddrToPaddr(vaddr);
}
