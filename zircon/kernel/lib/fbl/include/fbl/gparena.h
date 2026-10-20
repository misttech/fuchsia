// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_FBL_INCLUDE_FBL_GPARENA_H_
#define ZIRCON_KERNEL_LIB_FBL_INCLUDE_FBL_GPARENA_H_

#include <align.h>
#include <lib/fit/defer.h>
#include <lib/page/size.h>

#include <fbl/confine_array_index.h>
#include <kernel/mutex.h>
#include <ktl/atomic.h>
#include <vm/vm_address_region.h>
#include <vm/vm_aspace.h>
#include <vm/vm_object_paged.h>

namespace fbl {

// A simple lock-free list for use by GPArena.
//
// Rather than by address, the list refers to nodes by their 1-based index in an array of
// |SlotSize|-byte slots that starts at the address passed to Init, with index 0 meaning "no node".
// This makes the head of the list, which pairs the index of the first node with a generation
// count, small enough to fit in 64 bits, so that it can be updated with 8-byte atomic operations.
// Unlike 16-byte atomic operations, these are lock-free on all supported architectures.
//
// |Node| must have a naturally aligned uint32_t |next| member, which holds the index of the next
// node in the list while the node is in the list, and which the list accesses atomically.
template <typename Node, size_t SlotSize>
class LockFreeList {
 public:
  // The head of the list holds the index of the first node in its low kIndexBits bits, and the
  // generation count in its high kGenerationBits bits. See |head_| for why kGenerationBits is
  // enough.
  static constexpr uint32_t kGenerationBits = 40;
  static constexpr uint32_t kIndexBits = 64 - kGenerationBits;

  // The maximum number of slots, and therefore nodes, that the list can refer to.
  static constexpr size_t kMaxNodes = (uint64_t{1} << kIndexBits) - 1;

  // Set the address of the first slot. Must be called before any node is pushed.
  void Init(uintptr_t base) { base_ = base; }

  // Push |node| onto the list.
  void Push(Node* node) {
    const uint64_t index = ToIndex(node);
    DEBUG_ASSERT(index - 1 < kMaxNodes && ToNode(index) == node);
    // Take a local copy/snapshot of the current head.
    uint64_t head = head_.load(ktl::memory_order_relaxed);
    uint64_t next_head;
    do {
      // Every time the compare_exchange below fails head becomes the current value and so
      // we need to reset our intended next index every iteration.
      Next(node).store(static_cast<uint32_t>(HeadIndex(head)), ktl::memory_order_relaxed);
      // Build our candidate next head.
      next_head = NextGeneration(head) | index;
      // Use release semantics so that any writes to the Persist area, and our write to
      // node->next, are visible before the node can be seen in the free list and reused.
    } while (!head_.compare_exchange_strong(head, next_head, ktl::memory_order_release,
                                            ktl::memory_order_relaxed));
  }

  // Pop a node from the list.  Returns nullptr if empty.
  Node* Pop() {
    // Take a local copy/snapshot of the current head.
    // Use an acquire to match with the release in Push.
    uint64_t head = head_.load(ktl::memory_order_acquire);
    while (HeadIndex(head) != 0) {
      Node* const node = ToNode(HeadIndex(head));
      // This read is speculative: if another thread pops |node| before we do, it may then
      // overwrite node->next, so this may read garbage. If so, the generation count will have
      // changed, so the compare_exchange below will fail and the garbage will be discarded. Mask
      // the value regardless, so that garbage can never alter the generation count in next_head,
      // which must only ever advance by one.
      //
      // WARNING: If another thread popped |node|, this read formally races with its new owner's
      // plain stores, and no store or fence before handing out nodes can order the two. This is
      // benign: the arena's memory stays mapped for the life of the arena, so the read can't
      // fault, and as described above, whatever it returns is masked and then discarded.
      const uint64_t next = Next(node).load(ktl::memory_order_relaxed);
      const uint64_t next_head = NextGeneration(head) | (next & kIndexMask);
      if (head_.compare_exchange_strong(head, next_head, ktl::memory_order_acquire,
                                        ktl::memory_order_acquire)) {
        return node;
      }
      // There is no pause here as we don't need to wait for anyone before trying again,
      // rather the sooner we retry the *more* likely we are to succeed given that we just
      // received the most up to date copy of head.
    }
    return nullptr;
  }

 private:
  static constexpr uint64_t kIndexMask = (uint64_t{1} << kIndexBits) - 1;
  static_assert(kIndexBits <= 32, "Node::next must be able to hold any index");

  static constexpr uint64_t HeadIndex(uint64_t head) { return head & kIndexMask; }

  // Returns |head| with its index cleared and its generation count incremented. The generation
  // count wraps around to 0 when it overflows.
  static constexpr uint64_t NextGeneration(uint64_t head) { return (head | kIndexMask) + 1; }

  uint64_t ToIndex(const Node* node) const {
    return (reinterpret_cast<uintptr_t>(node) - base_) / SlotSize + 1;
  }

  Node* ToNode(uint64_t index) const {
    return reinterpret_cast<Node*>(base_ + (index - 1) * SlotSize);
  }

  // Returns a reference for atomically accessing |node|'s next index. Pop reads the next index of
  // the head node speculatively, without first taking ownership of the node, which can race with
  // other threads writing to it, such as one pushing the same node. Relaxed accesses suffice, as
  // the release and acquire operations on |head_| order the accesses that matter.
  static ktl::atomic_ref<uint32_t> Next(Node* node) {
    return ktl::atomic_ref<uint32_t>(node->next);
  }

  // Stores the index of the current head node and a generation count. The generation count
  // prevents races where one thread is modifying the list whilst another thread rapidly adds and
  // removes. Every time the head is modified the generation count is incremented to generate a
  // unique value.
  //
  // It is important that the count not wrap past an existing value that is still in use. That
  // could only happen if a thread stalled in Pop, between loading the head and the
  // compare_exchange, while the head was modified an exact multiple of 2^kGenerationBits times,
  // leaving the same node at the head of the list. Every modification is a successful
  // compare_exchange of |head_|, and these are serialized, so even at a sustained rate of 10^8
  // modifications per second, which is faster than any real workload, 2^40 modifications take over
  // 3 hours. No thread could plausibly be stalled for that long (by comparison, 2^32 modifications
  // could take less than a minute).
  ktl::atomic<uint64_t> head_ = 0;
  static_assert(ktl::atomic<uint64_t>::is_always_lock_free, "atomic<uint64_t> not lock free");

  // The address of the slot with index 1.
  uintptr_t base_ = 0;
};

// Growable Persistent Arena (GPArena) is an arena that allows for fast allocation and
// deallocation of a single kind of object. Compared to other arena style allocators it
// additionally guarantees that a portion of the objects memory will be preserved between calls to
// Free+Alloc.
template <size_t PersistSize, size_t ObjectSize>
class __OWNER(void) GPArena {
 public:
  GPArena() = default;
  ~GPArena() {
    DEBUG_ASSERT(count_ == 0);
    if (vmar_ != nullptr) {
      // Unmap all of our memory and free our resources.
      const size_t committed_len = committed_ - start_;
      if (committed_len > 0) {
        vmo_->Unpin(0, committed_len);
      }
      vmar_->Destroy();
      vmar_.reset();
    }
    vmo_.reset();
  }

  // The maximum number of objects that an arena can hold.
  static constexpr size_t MaxCount() { return FreeList::kMaxNodes; }

  // Reserves space for at least |max_count| objects, or more if they fit in the space left over
  // from rounding up to a whole number of pages. Fails with ZX_ERR_OUT_OF_RANGE if that would be
  // more than MaxCount() objects.
  zx_status_t Init(const char* name, size_t max_count) {
    if (!max_count) {
      return ZX_ERR_INVALID_ARGS;
    }

    if (vmar_) {
      // Already initialized.
      return ZX_ERR_BAD_STATE;
    }

    // Check this before using max_count to compute mem_sz, so that the computation cannot overflow.
    if (max_count > MaxCount()) {
      return ZX_ERR_OUT_OF_RANGE;
    }

    // Carve out some memory from the kernel root VMAR
    const size_t mem_sz = RoundUpPageSize(max_count * ObjectSize);

    // Alloc hands out every object that fits in mem_sz, which may be more than max_count, and the
    // free list must be able to refer to all of them.
    if (mem_sz / ObjectSize > MaxCount()) {
      return ZX_ERR_OUT_OF_RANGE;
    }

    fbl::RefPtr<VmObjectPaged> vmo;
    zx_status_t status = VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, 0u, mem_sz, &vmo);
    if (status != ZX_OK) {
      return status;
    }

    auto kspace = VmAspace::kernel_aspace();
    DEBUG_ASSERT(kspace != nullptr);
    auto root_vmar = kspace->RootVmar();
    DEBUG_ASSERT(root_vmar != nullptr);

    char vname[32];
    snprintf(vname, sizeof(vname), "gparena:%s", name);
    vmo->set_name(vname, sizeof(vname));

    zx_status_t st = root_vmar->CreateSubVmar(
        0,             // offset (ignored)
        mem_sz, false, /*align_pow2=*/
        VMAR_FLAG_CAN_MAP_READ | VMAR_FLAG_CAN_MAP_WRITE | VMAR_FLAG_CAN_MAP_SPECIFIC, vname,
        &vmar_);
    if (st != ZX_OK || vmar_ == nullptr) {
      return ZX_ERR_NO_MEMORY;
    }
    // The VMAR's parent holds a ref, so it won't be destroyed
    // automatically when we return.
    auto destroy_vmar = fit::defer([this]() {
      vmar_->Destroy();
      vmar_.reset();
    });

    zx::result<VmAddressRegion::MapResult> map_result =
        vmar_->CreateVmMapping(0,  // mapping_offset
                               mem_sz,
                               false,  // align_pow2
                               VMAR_FLAG_SPECIFIC | VMAR_FLAG_DEBUG_DYNAMIC_KERNEL_MAPPING, vmo,
                               0,  // vmo_offset
                               ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE, "gparena");
    if (map_result.is_error()) {
      return map_result.status_value();
    }
    mapping_ = ktl::move(map_result->mapping);

    vmo_ = ktl::move(vmo);
    top_ = committed_ = start_ = map_result->base;
    end_ = start_ + mem_sz;
    free_list_.Init(start_);

    DEBUG_ASSERT(IsPageRounded(start_));
    DEBUG_ASSERT(IsPageRounded(end_));

    destroy_vmar.cancel();

    return ZX_OK;
  }

  // Returns a raw pointer and not a reference to an object of type T so that the memory can be
  // inspected prior to construction taking place.
  void* Alloc() {
    void* const node = free_list_.Pop();
    if (node != nullptr) {
      count_.fetch_add(1, ktl::memory_order_relaxed);
      return node;
    }

    // Nothing in the free list, we need to grow.
    uintptr_t top = top_.load(ktl::memory_order_relaxed);
    uintptr_t next_top;
    do {
      // Every time the compare_exchange below fails top becomes the current value and so
      // we recalculate our potential next_top every iteration from it.
      next_top = top + ObjectSize;
      // See if we need to commit more memory.
      if (next_top > committed_.load(ktl::memory_order_relaxed)) {
        if (!Grow(next_top)) {
          return nullptr;
        }
      }
    } while (!top_.compare_exchange_strong(top, next_top, ktl::memory_order_relaxed,
                                           ktl::memory_order_relaxed));
    count_.fetch_add(1, ktl::memory_order_relaxed);
    return reinterpret_cast<void*>(top);
  }

  // Takes a raw pointer as the destructor is expected to have already been run.
  void Free(void* node) {
    free_list_.Push(reinterpret_cast<FreeNode*>(node));
    count_.fetch_sub(1, ktl::memory_order_relaxed);
  }

  size_t DiagnosticCount() const { return count_.load(ktl::memory_order_relaxed); }

  bool Committed(void* node) const {
    uintptr_t n = reinterpret_cast<uintptr_t>(node);
    return n >= start_ && n < top_.load(ktl::memory_order_relaxed);
  }

  // Return |address| if it is within the valid range of the arena or the base of the arena if
  // not. Hardened against Spectre V1 / bounds check bypass speculation attacks - it always
  // returns a safe value, even under speculation.
  uintptr_t Confine(uintptr_t address) const {
    const size_t size = top_.load(ktl::memory_order_relaxed) - start_;
    uintptr_t offset = address - start_;
    offset = fbl::confine_array_index(offset, /*size=*/size);
    return start_ + offset;
  }

  void* Base() const { return reinterpret_cast<void*>(start_); }

  void Dump() {
    // Take the mapping lock so we can safely dump the vmar_ without mappings being done in
    // parallel.
    Guard<CriticalMutex> guard{&mapping_lock_};

    DEBUG_ASSERT(vmar_ != nullptr);
    printf("GPArena<%#zx,%#zx> %s mappings:\n", PersistSize, ObjectSize, vmar_->name());
    {
      Guard<CriticalMutex> region_guard{vmar_->region_lock()};
      Guard<CriticalMutex> vmar_guard{vmar_->lock()};
      vmar_->DumpLocked(/* depth */ 1, /* verbose */ true);
    }

    // |top_|, |committed_|, and |count_| are all atomic variables that can change out from under
    // this method so it's not possible to guarantee a consistent snapshot.  Instead, we aim for
    // providing output that's not "obviously wrong".
    //
    // Load (with sequential consistency) all three values into local variable to minimize the
    // chance of getting inconsistent data.
    const size_t top = top_.load();
    const size_t committed = committed_.load();
    const size_t count = count_.load();

    const size_t nslots = (top - start_) / ObjectSize;
    const size_t np = (committed - start_) / kPageSize;
    const size_t npmax = (end_ - start_) / kPageSize;
    const size_t tslots = static_cast<size_t>(end_ - start_) / ObjectSize;

    // If we didn't get a consistent read, at least make sure we don't underflow.
    const size_t free_list = (nslots > count) ? (nslots - count) : 0;

    printf(" start 0x%#zx\n", start_);
    printf(" top 0x%zx (%zu slots allocated)\n", top, nslots);
    printf(" committed 0x%#zx (%zu/%zu pages)\n", committed, np, npmax);
    printf(" end 0x%#zx (%zu slots total)\n", end_, tslots);
    printf(" free list length %zu\n", free_list);
  }

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(GPArena);

  // Attempts to grow the committed memory range such that next_top is included in the range.
  bool Grow(uintptr_t next_top) {
    // take the mapping lock
    Guard<CriticalMutex> guard{&mapping_lock_};
    // Cache committed_ as only we can change it as we have the lock.
    uintptr_t committed = committed_.load(ktl::memory_order_relaxed);
    // now that we have the lock, double check we need to proceed
    if (next_top > committed) {
      uintptr_t nc = committed + 4 * kPageSize;
      // Clip our commit attempt to the end of our mapping.
      if (nc > end_) {
        nc = end_;
      }
      if (nc == committed) {
        // If we aren't going to commit more than we already haven then this
        // means we have completely filled the arena.
        return false;
      }
      const size_t offset = reinterpret_cast<vaddr_t>(committed) - start_;
      const size_t len = nc - committed;
      zx_status_t st = vmo_->CommitRangePinned(offset, len, true);
      if (st != ZX_OK) {
        return false;
      }
      st = mapping_->MapRange(offset, len, /* commit */ true);
      if (st != ZX_OK) {
        // Unpin the range.
        vmo_->Unpin(offset, len);
        // Try to clean up any committed pages, but don't require
        // that it succeeds.
        mapping_->DecommitRange(offset, len);
        return false;
      }
      committed_.store(nc, ktl::memory_order_relaxed);
    }
    return true;
  }

  fbl::RefPtr<VmAddressRegion> vmar_;
  fbl::RefPtr<VmMapping> mapping_;
  fbl::RefPtr<VmObject> vmo_;

  uintptr_t start_ = 0;
  // top_ is the address of the next object to be allocated from the arena.
  ktl::atomic<uintptr_t> top_ = 0;
  // start_ .. committed_ represents the committed, pinned and mapped portion of the arena.
  ktl::atomic<uintptr_t> committed_ = 0;
  // start_ .. end_ represent the total virtual address reservation for the arena, and committed_
  // may not grow past end_.
  uintptr_t end_ = 0;

  DECLARE_CRITICAL_MUTEX(GPArena) mapping_lock_;

  ktl::atomic<size_t> count_ = 0;

  struct FreeNode {
    char data[PersistSize];
    // While this node is in the free list, |next| holds the index of the next node in the list (see
    // LockFreeList). This struct is explicitly not packed to allow for the next field to be
    // naturally aligned, as LockFreeList accesses it atomically. As a result we *may* preserve
    // more than PersistSize, but that is fine.
    uint32_t next;
  };
  static_assert(sizeof(FreeNode) <= ObjectSize, "Not enough free space in object");
  static_assert((ObjectSize % alignof(FreeNode)) == 0,
                "ObjectSize must be common alignment multiple");

  using FreeList = LockFreeList<FreeNode, ObjectSize>;

  FreeList free_list_;
};

}  // namespace fbl

#endif  // ZIRCON_KERNEL_LIB_FBL_INCLUDE_FBL_GPARENA_H_
