// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_VIRTUAL_ALLOC_INCLUDE_LIB_VIRTUAL_ALLOC_H_
#define ZIRCON_KERNEL_LIB_VIRTUAL_ALLOC_INCLUDE_LIB_VIRTUAL_ALLOC_H_

#include <inttypes.h>
#include <lib/zx/result.h>
#include <zircon/errors.h>

#include <fbl/macros.h>
#include <kernel/ffi.h>
#include <vm/page.h>

class VirtualAlloc;

extern "C" {
void rust_virtual_alloc_new(VirtualAlloc* alloc, vm_page_state allocated_page_state);
void rust_virtual_alloc_drop(VirtualAlloc* alloc);
zx_status_t rust_virtual_alloc_init(VirtualAlloc* alloc, vaddr_t base, size_t size,
                                    size_t alloc_guard, size_t align_log2);
zx_status_t rust_virtual_alloc_alloc_pages(VirtualAlloc* alloc, size_t pages, vaddr_t* out_vaddr);
void rust_virtual_alloc_free_pages(VirtualAlloc* alloc, vaddr_t vaddr, size_t pages);
bool rust_virtual_alloc_can_attempt_contiguous_mappings(const VirtualAlloc* alloc);
size_t rust_virtual_alloc_debug_bitmap_pages(const VirtualAlloc* alloc);
void rust_virtual_alloc_debug_free_all_allocations(VirtualAlloc* alloc);
void rust_virtual_alloc_debug_allocate_vaddr_range(VirtualAlloc* alloc, vaddr_t vaddr,
                                                   size_t pages);
}

// VirtualAlloc is a page granule allocator that manages a given virtual region and provides
// virtually contiguous allocations inside that region. This allocator explicitly has no dependency
// on the heap and retrieves all its backing memory directly from the pmm. To achieve this it maps
// pages directly into the hardware page tables via the arch aspace, and consequently assumes that
// these operations will only depend on the pmm and not the heap for allocating any intermediate
// page tables.
//
// This class is thread-unsafe.
//
// The implementation lives in Rust (`//zircon/kernel/lib/virtual_alloc/virtual_alloc.rs`).
class VirtualAlloc {
 public:
  // Allocator needs to be told what state to set any allocated pages to. This allows for using the
  // allocator as a heap or generic allocator for an object.
  explicit VirtualAlloc(vm_page_state allocated_page_state) {
    rust_virtual_alloc_new(this, allocated_page_state);
  }
  ~VirtualAlloc() { rust_virtual_alloc_drop(this); }

  DISALLOW_COPY_ASSIGN_AND_MOVE(VirtualAlloc);

  // Initialize the allocator and make it ready for use. Takes a a virtual address range as a base
  // and size in bytes. These both must be page aligned and there must be no pages mapped into the
  // hardware page tables for this range.
  // |alloc_guard| is the minimum number of virtual pages to place between two allocations and
  // serve as guard pages. Any reads or writes that over or under run an allocation into the padding
  // will trigger an immediate hardware page fault.
  zx_status_t Init(vaddr_t base, size_t size, size_t alloc_guard, size_t align_log2) {
    return rust_virtual_alloc_init(this, base, size, alloc_guard, align_log2);
  }

  // AllocPages can only be called after after a successful Init call, otherwise it will return an
  // error. Number of pages requested must be non zero.
  zx::result<vaddr_t> AllocPages(size_t pages) {
    vaddr_t vaddr = 0;
    zx_status_t status = rust_virtual_alloc_alloc_pages(this, pages, &vaddr);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(vaddr);
  }

  // Returns a non-zero number of pages at the given virtual address. Partial frees are supported
  // such that if 2 pages were allocated at X it is allowed to FreePages(X, 1) and separately
  // FreePages(X + kPageSize, 1).
  void FreePages(vaddr_t vaddr, size_t pages) { rust_virtual_alloc_free_pages(this, vaddr, pages); }

  // Returns whether or not the allocator will attempt to optimize large allocations with
  // contiguous allocations and mappings that can get promoted to large pages. It does not guarantee
  // that it will succeed at doing this though.
  bool CanAttemptContiguousMappings() const {
    return rust_virtual_alloc_can_attempt_contiguous_mappings(this);
  }

  // Returns the number of pages allocated to provide the bitmap for tracking allocations. Exposed
  // as a debug named function as there is no promise that it has to be a bitmap for tracking and
  // this interface is likely to change and should only be used for debugging.
  size_t DebugBitmapPages() const { return rust_virtual_alloc_debug_bitmap_pages(this); }

  // Frees any allocated pages as if FreePages had been called on every outstanding allocation. This
  // is exposed for testing for circumstances where externally tracking every allocations is
  // burdensome and not the goal of the test.
  void DebugFreeAllAllocations() { rust_virtual_alloc_debug_free_all_allocations(this); }

  // Force allocates the given range, overriding any padding or alignment requirements. This is used
  // for testing to more precisely control available allocation regions. Panics if range is invalid
  // or already allocated.
  void DebugAllocateVaddrRange(vaddr_t vaddr, size_t pages) {
    rust_virtual_alloc_debug_allocate_vaddr_range(this, vaddr, pages);
  }

 private:
  [[maybe_unused]] OpaqueStorage<64, 8> storage_{};
};

static_assert(sizeof(VirtualAlloc) == 64);
static_assert(alignof(VirtualAlloc) == 8);

#endif  // ZIRCON_KERNEL_LIB_VIRTUAL_ALLOC_INCLUDE_LIB_VIRTUAL_ALLOC_H_
