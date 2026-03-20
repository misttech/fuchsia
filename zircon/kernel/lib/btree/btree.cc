// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/btree.h>

#include <vm/page_slab_allocator.h>

namespace {

DECLARE_SINGLETON_CRITICAL_MUTEX(SlabLock32);
constinit PageSlabAllocator<32> node_slab32_ TA_GUARDED(SlabLock32::Get());
DECLARE_SINGLETON_CRITICAL_MUTEX(SlabLock64);
constinit PageSlabAllocator<64> node_slab64_ TA_GUARDED(SlabLock64::Get());
DECLARE_SINGLETON_CRITICAL_MUTEX(SlabLock128);
constinit PageSlabAllocator<128> node_slab128_ TA_GUARDED(SlabLock128::Get());
DECLARE_SINGLETON_CRITICAL_MUTEX(SlabLock256);
constinit PageSlabAllocator<256> node_slab256_ TA_GUARDED(SlabLock256::Get());

}  // namespace

namespace btree {

void* GlobalSlabAllocator::allocate(size_t size_align) {
  if (size_align == 32) {
    Guard<CriticalMutex> guard{SlabLock32::Get()};
    return node_slab32_.allocate_bytes();
  }
  if (size_align == 64) {
    Guard<CriticalMutex> guard{SlabLock64::Get()};
    return node_slab64_.allocate_bytes();
  }
  if (size_align == 128) {
    Guard<CriticalMutex> guard{SlabLock128::Get()};
    return node_slab128_.allocate_bytes();
  }
  if (size_align == 256) {
    Guard<CriticalMutex> guard{SlabLock256::Get()};
    return node_slab256_.allocate_bytes();
  }
  ZX_ASSERT(false);
  return nullptr;
}

void GlobalSlabAllocator::deallocate(size_t size_align, void* ptr) {
  if (size_align == 32) {
    Guard<CriticalMutex> guard{SlabLock32::Get()};
    node_slab32_.deallocate_bytes(ptr);
  } else if (size_align == 64) {
    Guard<CriticalMutex> guard{SlabLock64::Get()};
    node_slab64_.deallocate_bytes(ptr);
  } else if (size_align == 128) {
    Guard<CriticalMutex> guard{SlabLock128::Get()};
    node_slab128_.deallocate_bytes(ptr);
  } else if (size_align == 256) {
    Guard<CriticalMutex> guard{SlabLock256::Get()};
    node_slab256_.deallocate_bytes(ptr);
  } else {
    ZX_ASSERT(false);
  }
}

}  // namespace btree
