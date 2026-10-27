// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_OWNED_WAIT_QUEUE_POOL_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_OWNED_WAIT_QUEUE_POOL_H_

#include <zircon/types.h>

#include <fbl/macros.h>
#include <kernel/ffi.h>
#include <kernel/lazy_owned_wait_queue.h>
#include <kernel/mutex.h>

class OwnedWaitQueuePool;

// The implementation of OwnedWaitQueuePool lives in Rust
// (zircon/kernel/kernel/owned_wait_queue_pool.rs).
extern "C" {
void rust_owned_wait_queue_pool_new(OwnedWaitQueuePool* pool);
void rust_owned_wait_queue_pool_destroy(OwnedWaitQueuePool* pool);
OwnedWaitQueuePool* rust_owned_wait_queue_pool_get();
zx_status_t rust_owned_wait_queue_pool_grow(OwnedWaitQueuePool* pool);
void rust_owned_wait_queue_pool_shrink(OwnedWaitQueuePool* pool);
void rust_owned_wait_queue_pool_recycle_locked(OwnedWaitQueuePool* pool, OwnedWaitQueue* queue);
OwnedWaitQueue* rust_owned_wait_queue_pool_borrow_locked(OwnedWaitQueuePool* pool);
int64_t rust_owned_wait_queue_pool_get_excess_capacity_for_test(const OwnedWaitQueuePool* pool);
}  // extern "C"

// Global pool for OwnedWaitQueue objects.
class OwnedWaitQueuePool {
 public:
  OwnedWaitQueuePool() {
    static_assert(sizeof(OwnedWaitQueuePool) == kStorageSize);
    static_assert(alignof(OwnedWaitQueuePool) == kStorageAlign);
    rust_owned_wait_queue_pool_new(this);
  }
  ~OwnedWaitQueuePool() { rust_owned_wait_queue_pool_destroy(this); }

  DISALLOW_COPY_ASSIGN_AND_MOVE(OwnedWaitQueuePool);

  // Returns the global singleton instance.
  static OwnedWaitQueuePool& Get() { return *rust_owned_wait_queue_pool_get(); }

  // Increases the pool size by one if the number of active threads exceeds the current pool
  // capacity.
  //
  // May block the caller.
  //
  // Must be called during Thread construction to ensure pool invariants are upheld.
  //
  // Returns ZX_ERR_NO_MEMORY if allocation fails.
  zx_status_t Grow() { return rust_owned_wait_queue_pool_grow(this); }

  // Decreases the active thread count.
  //
  // Must be called during Thread destruction or resource cleanup.
  //
  // Note that this does not currently shrink the size of the pool, it shrinks the number of
  // active threads tracked by the pool. This allows new threads in the future to reuse already
  // allocated OwnedWaitQueue's.
  void Shrink() { rust_owned_wait_queue_pool_shrink(this); }

  // Recycle a queue back to the pool.
  //
  // The queue must be in a clean state (no owner, no waiters).
  void RecycleLocked(OwnedWaitQueue* queue) TA_REQ(OwnedWaitQueuePoolLock::Get()) {
    rust_owned_wait_queue_pool_recycle_locked(this, queue);
  }

  // Borrow a queue from the pool.
  OwnedWaitQueue* BorrowLocked() TA_REQ(OwnedWaitQueuePoolLock::Get()) {
    return rust_owned_wait_queue_pool_borrow_locked(this);
  }

  // Returns the value of the excess capacity OWQ kcounter.
  int64_t GetExcessCapacityForTest() const {
    return rust_owned_wait_queue_pool_get_excess_capacity_for_test(this);
  }

 private:
  // sizeof(ObjectCache<OwnedWaitQueue>) (48 + sizeof(Lock<Mutex>)) + 16 (free_list + counters).
  static constexpr size_t kStorageSize = 64 + sizeof(Lock<Mutex>);
  static constexpr size_t kStorageAlign = 8;

  [[maybe_unused]] OpaqueStorage<kStorageSize, kStorageAlign> storage_{};
};

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_OWNED_WAIT_QUEUE_POOL_H_
