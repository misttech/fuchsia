// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/vm_cow_pages.h"

#include <lib/arch/intrin.h>
#include <lib/boot-options/boot-options.h>
#include <lib/counters.h>
#include <lib/fit/defer.h>
#include <trace.h>

#include <cstdint>

#include <kernel/range_check.h>
#include <ktl/type_traits.h>
#include <ktl/utility.h>
#include <lk/init.h>
#include <vm/compression.h>
#include <vm/discardable_vmo_tracker.h>
#include <vm/fault.h>
#include <vm/page.h>
#include <vm/physmap.h>
#include <vm/pmm.h>
#include <vm/vm_object.h>
#include <vm/vm_object_paged.h>
#include <vm/vm_page_list.h>

#include "ktl/optional.h"
#include "vm_priv.h"

#include <ktl/enforce.h>

#define LOCAL_TRACE VM_GLOBAL_TRACE(0)

// add expensive code to do a full validation of the VMO at various points.
#define VMO_VALIDATION (0 || (LK_DEBUGLEVEL > 2))

// Assertion that is only enabled if VMO_VALIDATION is enabled.
#define VMO_VALIDATION_ASSERT(x) \
  do {                           \
    if (VMO_VALIDATION) {        \
      ASSERT(x);                 \
    }                            \
  } while (0)

// Add not-as-expensive code to do some extra validation at various points.  This is off in normal
// debug builds because it can add O(n) validation to an O(1) operation, so can still make things
// slower, despite not being as slow as VMO_VALIDATION.
#define VMO_FRUGAL_VALIDATION (0 || (LK_DEBUGLEVEL > 2))

// Assertion that is only enabled if VMO_FRUGAL_VALIDATION is enabled.
#define VMO_FRUGAL_VALIDATION_ASSERT(x) \
  do {                                  \
    if (VMO_FRUGAL_VALIDATION) {        \
      ASSERT(x);                        \
    }                                   \
  } while (0)

namespace {

KCOUNTER(vm_vmo_high_priority, "vm.vmo.high_priority")
KCOUNTER(vm_vmo_dont_need, "vm.vmo.dont_need")
KCOUNTER(vm_vmo_always_need, "vm.vmo.always_need")
KCOUNTER(vm_vmo_compression_zero_slot, "vm.vmo.compression.zero_empty_slot")
KCOUNTER(vm_vmo_compression_marker, "vm.vmo.compression_zero_marker")
KCOUNTER(vm_vmo_range_update_from_parent_skipped, "vm.vmo.range_updated_from_parent.skipped")
KCOUNTER(vm_vmo_range_update_from_parent_performed, "vm.vmo.range_updated_from_parent.performed")

KCOUNTER(vm_reclaim_evict_accessed, "vm.reclaim.evict_accessed")
KCOUNTER(vm_reclaim_compress_accessed, "vm.reclaim.compress_accessed")
KCOUNTER(vm_reclaim_no_reclamation_strategy, "vm.reclaim.no_reclamation_strategy")
KCOUNTER(vm_reclaim_always_need_skipped, "vm.reclaim.always_need_skipped")
KCOUNTER(vm_reclaim_discardable_failed, "vm.reclaim.discardable_failed")
KCOUNTER(vm_reclaim_incorrect_page, "vm.reclaim.incorrect_page")
KCOUNTER(vm_reclaim_high_priority, "vm.reclaim.high_priority")
KCOUNTER(vm_reclaim_pinned, "vm.reclaim.pinned")
KCOUNTER(vm_reclaim_dirty, "vm.reclaim.dirty")
KCOUNTER(vm_reclaim_uncached, "vm.reclaim.uncached")

template <typename T>
uint32_t GetShareCount(T p) {
  DEBUG_ASSERT(p->IsPageOrRef());

  uint32_t share_count = 0;
  if (p->IsPage()) {
    share_count = p->Page()->object.share_count;
  } else if (p->IsReference()) {
    share_count = Pmm::Node().GetPageCompression()->GetMetadata(p->Reference());
  }

  return share_count;
}

void ZeroPage(paddr_t pa) {
  void* ptr = paddr_to_physmap(pa);
  DEBUG_ASSERT(ptr);

  arch_zero_page(ptr);
}

void ZeroPage(vm_page_t* p) {
  paddr_t pa = p->paddr();
  ZeroPage(pa);
}

bool IsZeroPage(vm_page_t* p) {
  uint64_t* base = (uint64_t*)paddr_to_physmap(p->paddr());
  for (size_t i = 0; i < PAGE_SIZE / sizeof(uint64_t); i++) {
    if (base[i] != 0)
      return false;
  }
  return true;
}

void InitializeVmPage(vm_page_t* p) {
  DEBUG_ASSERT(p);
  DEBUG_ASSERT(!list_in_list(&p->queue_node));
  // Page should be in the ALLOC state so we can transition it to the OBJECT state.
  DEBUG_ASSERT(p->state() == vm_page_state::ALLOC);
  p->set_state(vm_page_state::OBJECT);
  p->object.share_count = 0;
  p->object.pin_count = 0;
  p->object.always_need = 0;
  p->object.dirty_state = uint8_t(VmCowPages::DirtyState::Untracked);
  p->object.set_object(nullptr);
  p->object.set_page_offset(0);
}

inline uint64_t CheckedAdd(uint64_t a, uint64_t b) {
  uint64_t result;
  bool overflow = add_overflow(a, b, &result);
  DEBUG_ASSERT(!overflow);
  return result;
}

inline uint64_t CheckedSub(uint64_t a, uint64_t b) {
  DEBUG_ASSERT(b <= a);
  return a - b;
}

inline uint64_t ClampedLimit(uint64_t offset, uint64_t limit, uint64_t max_limit) {
  // Return a clamped `limit` value such that `offset + clamped_limit <= max_limit`.
  // If `offset > max_limit` to begin with, then clamp `limit` to 0 to avoid underflow.
  //
  // This is typically used to update a child node's parent limit when its parent is resized or the
  // child moves to a new parent. This guaranatees that the child cannot see any ancestor content
  // beyond what it could before the resize or move operation.
  uint64_t offset_limit = CheckedAdd(offset, limit);
  return ktl::max(ktl::min(offset_limit, max_limit), offset) - offset;
}

ktl::optional<vm_page_t*> MaybeDecompressReference(VmCompression* compression,
                                                   VmCompression::CompressedRef ref) {
  if (auto maybe_page_and_metadata = compression->MoveReference(ref)) {
    InitializeVmPage(maybe_page_and_metadata->page);
    // Ensure the share count is propagated from the compressed page.
    maybe_page_and_metadata->page->object.share_count = maybe_page_and_metadata->metadata;

    return maybe_page_and_metadata->page;
  }

  return ktl::nullopt;
}

void FreeReference(VmPageOrMarker::ReferenceValue content) {
  VmCompression* compression = Pmm::Node().GetPageCompression();
  DEBUG_ASSERT(compression);
  compression->Free(content);
}

// Helper to allow for accessing the VmCowPages::paged_ref_ without needing to manually assert the
// lock. Declared as a local helper here instead of a method in VmCowPages due to VmCowPages being
// defined prior to VmObjectPaged.
VmObjectPaged* paged_backlink_locked(VmCowPages* cow) TA_REQ(cow->lock())
    TA_ASSERT(paged_backlink_locked(cow)->lock()) {
  return cow->get_paged_backlink_locked();
}

}  // namespace

// static
void VmCowPages::DebugDumpReclaimCounters() {
  printf("Failed reclaim evict_accessed %ld\n", vm_reclaim_evict_accessed.SumAcrossAllCpus());
  printf("Failed reclaim compress_accessed %ld\n", vm_reclaim_compress_accessed.SumAcrossAllCpus());
  printf("Failed reclaim no_strategy %ld\n", vm_reclaim_no_reclamation_strategy.SumAcrossAllCpus());
  printf("Failed reclaim always_need %ld\n", vm_reclaim_always_need_skipped.SumAcrossAllCpus());
  printf("Failed reclaim discardable %ld\n", vm_reclaim_discardable_failed.SumAcrossAllCpus());
  printf("Failed reclaim incorrect_page %ld\n", vm_reclaim_incorrect_page.SumAcrossAllCpus());
  printf("Failed reclaim high_priority %ld\n", vm_reclaim_high_priority.SumAcrossAllCpus());
  printf("Failed reclaim pinned %ld\n", vm_reclaim_pinned.SumAcrossAllCpus());
  printf("Failed reclaim dirty %ld\n", vm_reclaim_dirty.SumAcrossAllCpus());
  printf("Failed reclaim uncached %ld\n", vm_reclaim_uncached.SumAcrossAllCpus());
}

// Helper for walking up a VmCowPages hierarchy where the start node is locked, and the immediate
// parent may or may not be locked.
class LockedParentWalker {
 public:
  // Construct the parent walker with a reference to a LockedPtr of any locked parent. The
  // referenced LockedPtr can be empty if the immediate parent is either not locked, or does not
  // exist. It is the callers responsibility to ensure the LockedPtr lives long enough.
  explicit LockedParentWalker(const VmCowPages::LockedPtr& maybe_locked_parent)
      : pre_locked_parent_(maybe_locked_parent) {}

  // Returns a locked reference to the current node in the walk. The TA_ASSERT is deemed correct as
  // all code paths return a `LockedPtr::locked*` method, that themselves have a TA_ASSERT.
  VmCowPages& current(VmCowPages* self) const TA_REQ(self->lock()) TA_ASSERT(current(self).lock()) {
    if (current_is_pre_locked_parent_) {
      return pre_locked_parent_.locked();
    }
    return current_.locked_or(self);
  }
  const VmCowPages& current(const VmCowPages* self) const TA_REQ(self->lock())
      TA_ASSERT(current(self).lock()) {
    if (current_is_pre_locked_parent_) {
      return pre_locked_parent_.locked();
    }
    return current_.locked_or(self);
  }

  // Resets the walker to its initial state, allowing for a new walk.
  void reset() {
    current_.release();
    current_is_pre_locked_parent_ = false;
  }

  // Walk up the hierarchy, changing the current node to the current nodes parent. It is an error to
  // call this if current has no parent.
  void WalkUp(const VmCowPages* self) TA_REQ(self->lock()) {
    VmCowPages* next = current(self).parent_.get();
    DEBUG_ASSERT(next);
    // If the next node in the chain matches the pre locked parent, then use that, otherwise move
    // current_ up and acquire the lock.
    if (next == pre_locked_parent_.get()) {
      // Double check that the pre_locked_parent_ is actually the immediate parent.
      DEBUG_ASSERT(self->parent_.get() == next);
      current_is_pre_locked_parent_ = true;
    } else {
      current_is_pre_locked_parent_ = false;
      current_ = VmCowPages::LockedPtr(next, next->lock_order());
    }
  }

 private:
  // Tracks whether a call to |current| should return the |pre_locked_locked_parent_|, or the normal
  // |current_| tracker.
  bool current_is_pre_locked_parent_ = false;
  const VmCowPages::LockedPtr& pre_locked_parent_;
  VmCowPages::LockedPtr current_;
};

// Helper class for collecting pages to performed batched Removes from the page queue to not incur
// its spinlock overhead for every single page. Pages that it removes from the page queue get placed
// into a provided list. Note that pages are not moved into the list until *after* Flush has been
// called and Flush must be called prior to object destruction.
//
// This class has a large internal array and should be marked uninitialized.
class BatchPQRemove {
 public:
  explicit BatchPQRemove(ScopedPageFreedList& freed_list) : freed_list_(freed_list) {}
  ~BatchPQRemove() { DEBUG_ASSERT(count_ == 0); }
  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(BatchPQRemove);

  // Add a page to the batch set. Automatically calls |Flush| if the limit is reached.
  void Push(vm_page_t* page) {
    DEBUG_ASSERT(page);
    ASSERT(page->object.pin_count == 0);
    DEBUG_ASSERT(count_ < kMaxPages);
    if (count_ != 0 && page->is_loaned() != is_loaned_) {
      Flush();
    }
    if (count_ == 0) {
      is_loaned_ = page->is_loaned();
    }

    pages_[count_] = page;
    count_++;
    if (count_ == kMaxPages) {
      Flush();
    }
  }

  // Removes any content from the supplied |page_or_marker| and either calls |Push| or otherwise
  // frees it. Always leaves the |page_or_marker| in the empty state.
  // Automatically calls |Flush| if the limit on pages is reached.
  void PushContent(VmPageOrMarker* page_or_marker) {
    if (page_or_marker->IsPage()) {
      Push(page_or_marker->ReleasePage());
    } else if (page_or_marker->IsReference()) {
      // TODO(https://fxbug.dev/42138396): Consider whether it is worth batching these.
      FreeReference(page_or_marker->ReleaseReference());
    } else {
      *page_or_marker = VmPageOrMarker::Empty();
    }
  }

  // Performs |Remove| on any pending pages. This allows you to know that all pages are in the
  // original list so that you can do operations on the list.
  void Flush() {
    if (count_ > 0) {
      if (is_loaned_) {
        Pmm::Node().BeginFreeLoanedArray(
            pages_, count_,
            [](vm_page_t** pages, size_t count, list_node_t* free_list) {
              pmm_page_queues()->RemoveArrayIntoList(pages, count, free_list);
            },
            freed_list_.Flph());
      } else {
        pmm_page_queues()->RemoveArrayIntoList(pages_, count_, freed_list_.List());
        freed_count_ += count_;
      }
      count_ = 0;
    }
  }

  // Returns the number of pages that were added to |freed_list_| by calls to Flush(). The
  // |freed_count_| counter keeps a running count of freed pages as they are removed and added to
  // |freed_list_|, avoiding having to walk |freed_list_| to compute its length.
  size_t freed_count() const { return freed_count_; }

  // Produces a callback suitable for passing to VmPageList::RemovePages that will |PushContent| all
  // items.
  auto RemovePagesCallback() {
    return [this](VmPageOrMarker* p, uint64_t off) {
      PushContent(p);
      return ZX_ERR_NEXT;
    };
  }

 private:
  // The value of 64 was chosen as there is minimal performance gains originally measured by using
  // higher values. There is an incentive on this being as small as possible due to this typically
  // being created on the stack, and our stack space is limited.
  static constexpr size_t kMaxPages = 64;

  size_t count_ = 0;
  size_t freed_count_ = 0;
  vm_page_t* pages_[kMaxPages];
  ScopedPageFreedList& freed_list_;
  bool is_loaned_ = false;
};

// Helper class for collecting pages to perform batched calls of |ChangeObjectOffset| on the page
// queue in order to avoid incurring its spinlock overhead for every single page. Note that pages
// are not modified until *after* Flush has been called and Flush must be called prior to object
// destruction.
//
// This class has a large internal array and should be marked uninitialized.
class BatchPQUpdateBacklink {
 public:
  explicit BatchPQUpdateBacklink(VmCowPages* object) : object_(object) {}
  ~BatchPQUpdateBacklink() { DEBUG_ASSERT(count_ == 0); }
  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(BatchPQUpdateBacklink);

  // Add a page to the batch set. Automatically calls |Flush| if the limit is reached.
  void Push(vm_page_t* page, uint64_t offset) {
    DEBUG_ASSERT(page);
    DEBUG_ASSERT(count_ < kMaxPages);

    pages_[count_] = page;
    offsets_[count_] = offset;
    count_++;

    if (count_ == kMaxPages) {
      Flush();
    }
  }

  // Performs |ChangeObjectOffset| on any pending pages.
  void Flush() {
    if (count_ > 0) {
      pmm_page_queues()->ChangeObjectOffsetArray(pages_, object_, offsets_, count_);
      count_ = 0;
    }
  }

 private:
  // Align the batch size here with the overall PageQueues batch size.
  // We measured no performance gains from using larger values and this value should be as small as
  // is reasonable due to this object being stack allocated.
  static constexpr size_t kMaxPages = PageQueues::kMaxBatchSize;

  VmCowPages* object_ = nullptr;

  size_t count_ = 0;
  vm_page_t* pages_[kMaxPages];
  uint64_t offsets_[kMaxPages];
};

// Helper class for iterating over a subtree while respecting the child->parent lock ordering
// requirement.
// Cursor is constructed with a root, i.e. the starting point, and will iterate over at least
// every node that existed at the point of construction. Nodes that are racily created mid
// iteration may or may not be visited. Utilizes the cursor lists in the VmCowPages to coordinate
// with any destruction.
// A cursor is logically at a 'current' location, which is initially the root the cursor was
// constructed at. As the current location is always held locked, the cursor can be assumed to be
// initially valid, and is valid as long as any iteration request (NextChild / NextSibling) returns
// true. The cursor explicitly performs a pre-order walk, allowing subtrees of a given node to be
// skipped during the iteration.
class VmCowPages::TreeWalkCursor
    : public fbl::ContainableBaseClasses<
          fbl::TaggedDoublyLinkedListable<TreeWalkCursor*, VmCowPages::RootListTag>,
          fbl::TaggedDoublyLinkedListable<TreeWalkCursor*, VmCowPages::CurListTag>> {
 public:
  explicit TreeWalkCursor(LockedPtr root)
      : root_(root.get()), cur_(root.get()), cur_locked_(ktl::move(root)) {
    DEBUG_ASSERT(cur_locked_.locked().life_cycle_ == LifeCycle::Alive);
    cur_locked_.locked().root_cursor_list_.push_back(this);
    cur_locked_.locked().cur_cursor_list_.push_back(this);
  }
  ~TreeWalkCursor() {
    if (root_) {
      reset();
    }
  }
  // These static methods exist to simplify the call sites in VmCowPages in such a way that the lock
  // annotations are preserved. A generic 'perform arbitrary lambda on all cursors' helper would
  // reduce the code duplication here, but it would lose the annotations.
  // See description of the non static methods for these do.

  static void MoveToSibling(fbl::TaggedDoublyLinkedList<TreeWalkCursor*, CurListTag>& cursor_list,
                            VmCowPages* cur, VmCowPages* sibling) TA_REQ(cur->lock())
      TA_REQ(sibling->lock()) {
    while (!cursor_list.is_empty()) {
      cursor_list.front().MoveToSibling(cur, sibling);
    }
  }
  static void MoveToSiblingOfParent(
      fbl::TaggedDoublyLinkedList<TreeWalkCursor*, CurListTag>& cursor_list, VmCowPages* cur,
      VmCowPages* parent) TA_REQ(cur->lock()) TA_REQ(parent->lock()) {
    while (!cursor_list.is_empty()) {
      cursor_list.front().MoveToSiblingOfParent(cur, parent);
    }
  }
  static void Erase(fbl::TaggedDoublyLinkedList<TreeWalkCursor*, RootListTag>& cursor_list,
                    VmCowPages* leaf) TA_REQ(leaf->lock()) {
    while (!cursor_list.is_empty()) {
      cursor_list.front().Erase(leaf);
    }
  }
  static void MergeToChild(fbl::TaggedDoublyLinkedList<TreeWalkCursor*, CurListTag>& cur_list,
                           fbl::TaggedDoublyLinkedList<TreeWalkCursor*, RootListTag>& root_list,
                           VmCowPages* cur, VmCowPages* child) TA_REQ(cur->lock())
      TA_REQ(child->lock()) {
    while (!root_list.is_empty()) {
      root_list.front().MergeRootToChild(cur, child);
    }
    while (!cur_list.is_empty()) {
      cur_list.front().MergeToChild(cur, child);
    }
  }

  // Inform the cursor that its current node is going away, and it should re-home to its sibling.
  void MoveToSibling(VmCowPages* cur, VmCowPages* sibling) TA_REQ(cur->lock())
      TA_REQ(sibling->lock()) {
    Guard<CriticalMutex> guard{&lock_};
    DEBUG_ASSERT(cur->parent_ && cur->parent_ == sibling->parent_);
    // If current was the root, then do not move to the sibling, as that would be outside our
    // iteration tree, erase instead.
    if (cur == root_) {
      EraseLocked(cur, cur);
      return;
    }
    MoveCurLocked(
        cur, sibling,
        CheckedSub(cumulative_parent_offset_, cur->parent_offset_) + sibling->parent_offset_);
  }

  // Inform the cursor that the root node is going away. Since a node can only be removed if it has
  // no children, this implies that the cursor is still at the root, and so the entire cursor should
  // be removed.
  void Erase(VmCowPages* root) TA_REQ(root->lock()) {
    DEBUG_ASSERT(root->children_list_len_ == 0);
    Guard<CriticalMutex> guard{&lock_};
    EraseLocked(root, root);
  }

  // Inform the cursor that the root node is being merged into the child, and the cursor should be
  // moved.
  void MergeRootToChild(VmCowPages* root, VmCowPages* child) TA_REQ(root->lock())
      TA_REQ(child->lock()) {
    Guard<CriticalMutex> guard{&lock_};
    DEBUG_ASSERT(root == root_);
    DEBUG_ASSERT(child->parent_.get() == root);
    // If the cursor was still pointing at the root then also move it. Although this would get
    // updated by a separate call to MergeToChild anyway, it's preferable to maintain the invariant.
    if (cur_ == root) {
      MoveCurLocked(root, child, cumulative_parent_offset_ + child->parent_offset_);
    }
    root->root_cursor_list_.erase(*this);
    child->root_cursor_list_.push_back(this);
    root_ = child;
  }

  // Inform the cursor that the current node is merging with its child.
  void MergeToChild(VmCowPages* cur, VmCowPages* child) TA_REQ(cur->lock()) TA_REQ(child->lock()) {
    Guard<CriticalMutex> guard{&lock_};
    DEBUG_ASSERT(child->parent_.get() == cur);
    MoveCurLocked(cur, child, cumulative_parent_offset_ + child->parent_offset_);
  }

  // Inform the cursor that both the current node and its parent are going away and the cursor
  // should be moved to the next available sibling of the parent, assuming that is still within the
  // subtree to be walked.
  // This method will logically end up at the same final node as just MoveToNextSibling, and it is
  // specialized not for performance, but rather for the scenario where the lock of |parent| is
  // already held, and hence directly using MoveToNextSibling would cause a double lock acquisition.
  void MoveToSiblingOfParent(VmCowPages* cur, VmCowPages* parent) TA_REQ(cur->lock())
      TA_REQ(parent->lock()) {
    DEBUG_ASSERT(cur->parent_.get() == parent);
    // Not trying to be efficient, as this method is only used for cleaning up when racing
    // deletion with a cursor traversal, so just move the cursor to the parent, then move to the
    // sibling.
    {
      Guard<CriticalMutex> guard{&lock_};
      if (cur == root_) {
        EraseLocked(cur, cur);
        return;
      }
      if (parent == root_) {
        EraseLocked(cur, parent);
        return;
      }
      MoveCurLocked(cur, parent, CheckedSub(cumulative_parent_offset_, cur->parent_offset_));
    }
    MoveToNextSibling(parent);
  }

  // Move the cursor to the next un-visited child, or if no children the next sibling. Returns false
  // if iteration has completed and the cursor is now invalid. This may not be called on an invalid
  // cursor.
  bool NextChild() {
    DEBUG_ASSERT(cur_locked_);
    do {
      // If no child then find a sibling instead.
      if (cur_locked_.locked().children_list_len_ == 0) {
        return NextSibling();
      }

      // To acquire the child lock we need to release the current lock, so first take a refptr to
      // the child.
      fbl::RefPtr<VmCowPages> child_ref = fbl::MakeRefPtrUpgradeFromRaw(
          &cur_locked_.locked().children_list_.front(), cur_locked_.locked().lock());
      cur_locked_.release();

      {
        LockedPtr child(child_ref.get());
        // While the locks were dropped things could have changed, so check that the child still has
        // a parent before attempting to acquire the parents lock.
        if (child.locked().parent_) {
          LockedPtr parent(child.locked().parent_.get());
          Guard<CriticalMutex> guard{&lock_};
          // If nothing raced then the parent of child should still be cur_.
          if (parent.get() == cur_) {
            // Both cur_ and child must be in the alive state, otherwise cur_ would have been
            // updated on a dead transition. The fact that a dead transition has not occurred, and
            // that child lock must be acquired to perform said transition, is why it is safe for us
            // to drop child_ref and store a raw LockedPtr of child.
            DEBUG_ASSERT(parent.locked().life_cycle_ == LifeCycle::Alive &&
                         child.locked().life_cycle_ == LifeCycle::Alive);
            MoveCurLocked(&parent.locked(), &child.locked(),
                          cumulative_parent_offset_ + child.locked().parent_offset_);
            cur_locked_ = ktl::move(child);
            // cur_ is updated and cur_locked_ holds a lock acquired with the correct order so we
            // can directly return and do not need to use UpdateCurLocked to reacquire.
            return true;
          }
        }
      }
      // We raced with a modification to the tree. This modification will have set the new value of
      // cur_ (possibly to nullptr if the cursor has been deleted), and we call UpdateCurLocked to
      // retrieve this and then go around the loop and check again for a child.
    } while (UpdateCurLocked());
    // Only reach here if UpdateCurLocked returns false, which only happens if the cursor was
    // deleted, in which case we definitely have no child.
    return false;
  }

  // Move the cursor to the next un-visited sibling, skipping any children of the current node.
  // Returns false if iteration has completed and the cursor is now invalid. This may not be called
  // on an invalid cursor.
  bool NextSibling() {
    DEBUG_ASSERT(cur_locked_);
    {
      LockedPtr cur = ktl::move(cur_locked_);
      // Due to the way the sibling lock gets acquired we always need to re-acquire it as a first
      // acquisition with its normal lock order. For this reason there is no point in attempting to
      // retain the lock of the updated cur_, and so we use a common helper and then re-read (and
      // re-lock) cur_.
      MoveToNextSibling(&cur.locked());
    }
    return UpdateCurLocked();
  }

  // Retrieves the offset that projects an offset from the starting node into an offset in the
  // current node. This does not imply that the current node can 'see' the content at that offset,
  // just that if it could that is the offset that would do it.
  // May only be called while the cursor is valid.
  uint64_t GetCurrentOffset() const {
    // As long as we hold cur_locked_ then no one can be altering cur_ and so we own the offset.
    DEBUG_ASSERT(cur_locked_);
    return cumulative_parent_offset_;
  }

  // Retrieve a reference to the current node.
  const LockedPtr& GetCur() const { return cur_locked_; }

 private:
  // Helper for moving cur_ to the next sibling. The |start| location, which must be equal to cur_
  // and held locked externally, must be passed in. This allows |cur_locked_| to be set by this
  // method without having to release its lock.
  // Walking the next sibling involves walking both 'up' and 'right' until we either find a node or
  // we encounter root_ and terminate.
  void MoveToNextSibling(VmCowPages* start) TA_REQ(start->lock()) {
    DEBUG_ASSERT(!cur_locked_);
    uint64_t offset;
    {
      Guard<CriticalMutex> guard{&lock_};
      DEBUG_ASSERT(start == cur_);
      // The later loop wants to assume that we have a parent (in order to be finding a sibling),
      // which could be false if we are presently at the root_ and there is otherwise no parent.
      if (start == root_) {
        EraseLocked(start, start);
        return;
      }
      // As we hold the lock to cur_, the offset cannot change, so we can cache it outside the lock.
      offset = cumulative_parent_offset_;
    }
    LockedPtr cur;
    while (true) {
      // If we aren't at the root then, by definition, we are in a subtree and must have a parent.
      DEBUG_ASSERT(cur.locked_or(start).parent_.get());
      fbl::RefPtr<VmCowPages> sibling_ref;
      {
        // Acquire the parent lock and check for a sibling.
        LockedPtr parent(cur.locked_or(start).parent_.get());
        auto iter = ++parent.locked().children_list_.make_iterator(cur.locked_or(start));
        if (!iter.IsValid()) {
          // If no sibling then walk up to the parent, ensuring we do not walk past the root.
          Guard<CriticalMutex> guard{&lock_};
          // Although we checked this previously, the root can get moved into its child, and so we
          // must re-check.
          if (start == root_) {
            EraseLocked(start, start);
            return;
          }
          if (parent.get() == root_) {
            EraseLocked(start, &parent.locked());
            return;
          }
          offset = CheckedSub(offset, cur.locked_or(start).parent_offset_);
          cur = ktl::move(parent);
          continue;
        }
        // Make a ref to the sibling, we have to drop the parent lock before acquiring the sibling
        // lock.
        sibling_ref = fbl::MakeRefPtrUpgradeFromRaw(&*iter, parent.locked().lock());
      }

      LockedPtr sibling(sibling_ref.get(), cur.locked_or(start).lock_order() + 1);
      // If the sibling is still from the same parent then no race occurred and sibling must still
      // be alive.
      if (sibling.locked().parent_ == cur.locked_or(start).parent_) {
        Guard<CriticalMutex> guard{&lock_};
        DEBUG_ASSERT(start == cur_);
        MoveCurLocked(start, &sibling.locked(),
                      CheckedSub(offset, cur.locked_or(start).parent_offset_) +
                          sibling.locked().parent_offset_);
        return;
      }
      // Raced with a modification, need to go around again and see what the state of the tree is
      // now and try again. The only way our siblings parent could have changed is if it got
      // deleted, and since new siblings will be placed at the head of the list (where as we are
      // iterating towards the tail), the number of times we can race is strictly bounded.
    }
  }

  // Updates cur_locked_ to be what is in cur_. This is used to resolve scenarios where the lock to
  // current needs to be dropped, and hence a racing deletion might move it.
  bool UpdateCurLocked() TA_EXCL(lock_) {
    // We must do this loop as the lock ordering is vmo->cursor and so in between dropping the
    // cursor lock to acquire cur_locked_, cur_ could move again.
    Guard<CriticalMutex> guard{&lock_};
    fbl::RefPtr<VmCowPages> cur;
    // Use a local cur_locked while we are looping and only update cur_locked_ at the end once we
    // are certain we have the correct lock.
    LockedPtr cur_locked = ktl::move(cur_locked_);
    do {
      // Clear any previous lock.
      cur_locked.release();
      // Cursor was deleted.
      if (!cur_) {
        return false;
      }
      cur = fbl::MakeRefPtrUpgradeFromRaw(cur_, lock_);
      guard.CallUnlocked([&cur, &cur_locked]() { cur_locked = LockedPtr(cur.get()); });
    } while (cur_locked.get() != cur_);
    // We have the lock to cur_ and so we safely drop the RefPtr, knowing that the object cannot be
    // destroyed without our backlink being updated, which would require someone else to acquire the
    // lock first. All this is only true if the object is presently in the Alive state.
    DEBUG_ASSERT(cur_locked.locked().life_cycle_ == LifeCycle::Alive);
    cur_locked_ = ktl::move(cur_locked);
    return true;
  }

  // Erase the cursor, removing all the backlinks.
  void EraseLocked(VmCowPages* cur, VmCowPages* root) TA_REQ(cur->lock()) TA_REQ(root->lock())
      TA_REQ(lock_) {
    DEBUG_ASSERT(cur == cur_);
    DEBUG_ASSERT(root == root_);
    cur->cur_cursor_list_.erase(*this);
    root->root_cursor_list_.erase(*this);
    cur_ = root_ = nullptr;
  }

  // Helper to update the current location of the cursor.
  void MoveCurLocked(VmCowPages* old_cur, VmCowPages* new_cur, uint64_t new_offset) TA_REQ(lock_)
      TA_REQ(old_cur->lock()) TA_REQ(new_cur->lock()) {
    DEBUG_ASSERT(old_cur == cur_);
    DEBUG_ASSERT(new_cur != root_);
    // Validate there is no cur_locked_, and so we can update this without racing with any readers
    // as hold the lock of cur_.
    DEBUG_ASSERT(!cur_locked_);
    cumulative_parent_offset_ = new_offset;
    old_cur->cur_cursor_list_.erase(*this);
    new_cur->cur_cursor_list_.push_back(this);
    cur_ = new_cur;
  }

  // Reset and invalidate the cursor.
  void reset() {
    LockedPtr cur = ktl::move(cur_locked_);
    Guard<CriticalMutex> guard{&lock_};
    LockedPtr root_locked;
    fbl::RefPtr<VmCowPages> root;
    // We must do this loop as the lock ordering is vmo->cursor and so in between dropping the
    // cursor lock to acquire root_locked, root_ could move again.
    do {
      root_locked.release();
      if (!root_) {
        return;
      }
      if (root_ == cur_) {
        EraseLocked(&cur.locked(), &cur.locked());
        return;
      }
      root = fbl::MakeRefPtrUpgradeFromRaw(root_, lock_);
      guard.CallUnlocked([&]() { root_locked = LockedPtr(root.get()); });
    } while (root_locked.get() != root_);
    EraseLocked(&cur.locked(), &root_locked.locked());
  }

  // Modifying any item, such as root_ or cur_, requires holding the lock of the respective object,
  // but to support being able to non-racily read the current value we define an additional lock_.
  // Reading any value can be performed by holding either lock_, or the respective object lock_, but
  // both must be held to modify.
  DECLARE_CRITICAL_MUTEX(TreeWalkCursor) lock_;
  // Tracks the offset that projects offsets from the original root, to the current node. This is
  // logically locked by cur_->lock(), but this annotation cannot be properly expressed. Although we
  // can say TA_REQ(cur_->lock()), there are times when we want to read this value know that
  // cur_locked_ is valid when we do not hold lock_, hence we cannot even write
  // AssertHeld(cur_->lock()), as we do not hold lock_ to dereference cur_, and hence cannot explain
  // to the static analysis that cur_locked_ is an alias of cur_.
  uint64_t cumulative_parent_offset_ = 0;
  // The invariant that we maintain is that if root_ or cur_ is not null, then the object they point
  // to must be in the Alive state, and this cursor must be in the respective cursor_list_.
  // Modifying these can only be done when holding the respective object lock, as well as lock_.
  // Attempting to annotate these with something like TA_GUARDED(cur_->lock()) is not useful since
  // the static analysis cannot resolve the pointer aliasing, and since these are pointers that can
  // change, using AssertHeld is dangerous as it can provide a false sense of correctness.
  VmCowPages* root_ TA_GUARDED(lock_) = nullptr;
  VmCowPages* cur_ TA_GUARDED(lock_) = nullptr;

  // Whenever the cursor is valid, then cur_locked_ is a LockedPtr to cur_. This lock is only
  // dropped internally when walking between nodes. Storing this internally, instead of returning it
  // to the user on successful calls to NextChild or NextSibling is merely to ensure that they do
  // not release the lock at all, allowing us to make assumptions when resuming iteration.
  LockedPtr cur_locked_;
};

bool VmCowRange::IsBoundedBy(uint64_t max) const { return InRange(offset, len, max); }

// Allocates a new page and populates it with the data at |parent_paddr|.
zx_status_t VmCowPages::AllocateCopyPage(paddr_t parent_paddr, list_node_t* alloc_list,
                                         AnonymousPageRequest* request, vm_page_t** clone) {
  DEBUG_ASSERT(request || !(pmm_alloc_flags_ & PMM_ALLOC_FLAG_CAN_WAIT));
  DEBUG_ASSERT(!is_source_supplying_specific_physical_pages());

  vm_page_t* p_clone = nullptr;

  if (request->has_page()) {
    p_clone = request->take_page();
  } else if (alloc_list) {
    p_clone = list_remove_head_type(alloc_list, vm_page, queue_node);
  }

  if (p_clone) {
    InitializeVmPage(p_clone);
  } else {
    zx_status_t status = AllocPage(&p_clone, request);
    if (status != ZX_OK) {
      return status;
    }
    DEBUG_ASSERT(p_clone);
  }

  void* dst = paddr_to_physmap(p_clone->paddr());
  DEBUG_ASSERT(dst);

  if (parent_paddr != vm_get_zero_page_paddr()) {
    // do a direct copy of the two pages
    const void* src = paddr_to_physmap(parent_paddr);
    DEBUG_ASSERT(src);
    memcpy(dst, src, PAGE_SIZE);
  } else {
    // avoid pointless fetches by directly zeroing dst
    arch_zero_page(dst);
  }

  *clone = p_clone;

  return ZX_OK;
}

zx_status_t VmCowPages::AllocUninitializedPage(vm_page_t** page, AnonymousPageRequest* request) {
  paddr_t paddr = 0;
  DEBUG_ASSERT(!is_source_supplying_specific_physical_pages());
  // Another layer has already allocated a page for us.
  if (request->has_page()) {
    *page = request->take_page();
    return ZX_OK;
  }

  zx_status_t status = CacheAllocPage(pmm_alloc_flags_, page, &paddr);
  if (status == ZX_ERR_SHOULD_WAIT) {
    request->MakeActive();
  }
  return status;
}

zx_status_t VmCowPages::AllocPage(vm_page_t** page, AnonymousPageRequest* request) {
  zx_status_t status = AllocUninitializedPage(page, request);
  if (status == ZX_OK) {
    InitializeVmPage(*page);
  }
  return status;
}

template <typename F>
zx::result<vm_page_t*> VmCowPages::AllocLoanedPage(F allocated) {
  DEBUG_ASSERT(!is_source_supplying_specific_physical_pages());
  return Pmm::Node().AllocLoanedPage([allocated](vm_page_t* page) {
    InitializeVmPage(page);
    allocated(page);
  });
}

void VmCowPages::RemovePageLocked(vm_page_t* page, DeferredOps& ops) {
  if (page->is_loaned()) {
    Pmm::Node().BeginFreeLoanedPage(
        page, [](vm_page_t* page) { pmm_page_queues()->Remove(page); }, ops.FreedList(this).Flph());
  } else {
    pmm_page_queues()->Remove(page);
    list_add_tail(ops.FreedList(this).List(), &page->queue_node);
  }
}

zx_status_t VmCowPages::CacheAllocPage(uint alloc_flags, vm_page_t** p, paddr_t* pa) {
  if (!page_cache_) {
    return pmm_alloc_page(alloc_flags, p, pa);
  }

  zx::result result = page_cache_.Allocate(1, alloc_flags);
  if (result.is_error()) {
    return result.error_value();
  }

  vm_page_t* page = list_remove_head_type(&result->page_list, vm_page_t, queue_node);
  DEBUG_ASSERT(page != nullptr);
  DEBUG_ASSERT(result->page_list.is_empty());

  *p = page;
  *pa = page->paddr();
  return ZX_OK;
}

void VmCowPages::CacheFree(list_node_t* list) {
  if (!page_cache_) {
    pmm_free(list);
    return;
  }

  page_cache_.Free(ktl::move(*list));
}

void VmCowPages::CacheFree(vm_page_t* p) {
  if (!page_cache_) {
    pmm_free_page(p);
    return;
  }

  page_cache::PageCache::PageList list;
  list_add_tail(&list, &p->queue_node);

  page_cache_.Free(ktl::move(list));
}

zx_status_t VmCowPages::MakePageFromReference(VmPageOrMarkerRef page_or_mark,
                                              AnonymousPageRequest* page_request) {
  DEBUG_ASSERT(page_or_mark->IsReference());
  VmCompression* compression = Pmm::Node().GetPageCompression();
  DEBUG_ASSERT(compression);

  vm_page_t* p;
  zx_status_t status = AllocPage(&p, page_request);
  if (status != ZX_OK) {
    return status;
  }

  const auto ref = page_or_mark.SwapReferenceForPage(p);
  void* page_data = paddr_to_physmap(p->paddr());
  uint32_t page_metadata;
  compression->Decompress(ref, page_data, &page_metadata);
  // Ensure the share count is propagated from the compressed page.
  p->object.share_count = page_metadata;

  return ZX_OK;
}

zx_status_t VmCowPages::ReplaceReferenceWithPageLocked(VmPageOrMarkerRef page_or_mark,
                                                       uint64_t offset,
                                                       AnonymousPageRequest* page_request) {
  // First replace the ref with a page.
  zx_status_t status = MakePageFromReference(page_or_mark, page_request);
  if (status != ZX_OK) {
    return status;
  }
  // Add the new page to the page queues for tracking. References are by definition not pinned, so
  // we know this is not wired.
  SetNotPinnedLocked(page_or_mark->Page(), offset);
  return ZX_OK;
}

VmCowPages::VmCowPages(VmCowPagesOptions options, uint32_t pmm_alloc_flags, uint64_t size,
                       fbl::RefPtr<PageSource> page_source,
                       ktl::unique_ptr<DiscardableVmoTracker> discardable_tracker,
                       uint64_t lock_order)
    : pmm_alloc_flags_(pmm_alloc_flags),
      options_(options),
// If both local and shared locks are defined then there is still only one true lock, the shared
// one, with the local lock existing to increase the tracking ability of lockdep. The local lock
// therefore needs to be pointed at the shared lock to forward the actual locking actions.
#if VMO_USE_LOCAL_LOCK && VMO_USE_SHARED_LOCK
      lock_(hierarchy_state_ptr_->lock()->lock()),
#endif
#if (LOCK_DEP_ENABLED_FEATURE_LEVEL > 0)
      lock_order_(lock_order),
#endif
      size_(size),
      page_source_(ktl::move(page_source)),
      discardable_tracker_(ktl::move(discardable_tracker)) {
  DEBUG_ASSERT(IS_PAGE_ROUNDED(size));
  // If we are tracking correct lock orders then add some asserts that nodes are created with lock
  // orders that at least vaguely make sense.
#if (LOCK_DEP_ENABLED_FEATURE_LEVEL > 0)
  // Nodes with a page source must always be the root, and have the respective lock order.
  DEBUG_ASSERT(!page_source_ || lock_order_ == kLockOrderRoot);
  // Hidden nodes must always have a lock order above the anonymous numbering area.
  DEBUG_ASSERT(!is_hidden() || lock_order_ > kLockOrderFirstAnon);
  // First anonymous nodes (i.e. not hidden and not with a direct page source) should fall into the
  // anonymous numbering area.
  DEBUG_ASSERT(page_source_ || is_hidden() || lock_order_ <= kLockOrderFirstAnon);
#endif
}

void VmCowPages::TransitionToAliveLocked() {
  ASSERT(life_cycle_ == LifeCycle::Init);
  life_cycle_ = LifeCycle::Alive;
}

fbl::RefPtr<VmCowPages> VmCowPages::MaybeDeadTransition() {
  // We perform a dead transition if |should_dead_transition_locked| is true, but in order to do the
  // transition we require holding multiple locks. Due to races with either other attempts at dead
  // transitions, or other creation and deletions modifying the tree, we may need to attempt the
  // lock acquisitions multiple times until we can get a stable snapshot.
  // The purpose of acquiring all the locks here is to ensure that once we begin a dead transition
  // we can continuously hold all the locks that lead to that decision (namely our own), otherwise
  // we would need to reason about our state potentially changing mid way through after dropping our
  // lock.
  // The locks we need to be holding to do a dead transition are: our own, our parent (if we have
  // one) and our sibling (if we have one). The sibling is a bit nuanced as we generally only want
  // the right sibling (i.e. next in parents child list), and if no right sibling can skip. The
  // exception being when our parent is hidden and has exactly two children, in which case the left
  // sibling is required to perform the hidden parent merge step.
  while (true) {
    fbl::RefPtr<VmCowPages> sibling_ref;
    VmCowPages* parent_raw;
    // Use a subscope as we potentially need to drop and then reacquire the locks.
    {
      Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};
      // With the lock now held check if we even need to do a dead transition.
      if (!should_dead_transition_locked()) {
        return nullptr;
      }
      // If no parent, then there can be no sibling, so can just do the transition.
      if (!parent_) {
        return DeadTransitionLocked(LockedPtr(), LockedPtr());
      }
      LockedPtr parent(parent_.get());
      // If we are the only child, then no need to check for siblings.
      if (parent.locked().children_list_len_ == 1) {
        return DeadTransitionLocked(ktl::move(parent), LockedPtr());
      }
      // First check if there is a sibling to our right.
      auto sibling_iter = ++parent.locked().children_list_.make_iterator(*this);
      if (sibling_iter.IsValid()) {
        // We found a sibling to our right, and so we can acquire its lock without dropping our own.
        // However, we do need to drop the parent lock to do so. To do this we take a RefPtr to the
        // sibling to ensure it stays alive, before dropping the parent lock, acquiring the sibling
        // lock and reacquiring the parent lock. A new LockedPtr is used for the parent acquisition
        // simply to allow the default destruction order to correctly release the locks in order.
        sibling_ref = fbl::MakeRefPtrUpgradeFromRaw(&*sibling_iter, parent.locked().lock());
        parent.release();
        LockedPtr sibling = LockedPtr(sibling_ref.get(), lock_order() + 1);
        LockedPtr parent2(parent_.get());
        // We have continuously held our lock, so we know that parent_ is unchanged for us, but
        // check if this is still our sibling or not by recalculating and comparing.
        sibling_iter = ++parent2.locked().children_list_.make_iterator(*this);
        if (!sibling_iter.IsValid() || sibling.get() != &*sibling_iter) {
          // We raced and this sibling has gone away. For simplicity we just try again from the top.
          continue;
        }
        return DeadTransitionLocked(parent2, sibling);
      }
      // There is no right sibling, so check if we need to get the left sibling. The left sibling is
      // needed only if the parent is hidden and we are one of exactly two children.
      if (!parent->is_hidden() || parent.locked().children_list_len_ != 2) {
        return DeadTransitionLocked(parent, LockedPtr());
      }
      // Create a RefPtr to hold the sibling alive and stash the current raw value of parent_ (so we
      // can detect any races later) then drop all the locks.
      sibling_ref = fbl::MakeRefPtrUpgradeFromRaw(&parent.locked().children_list_.front(),
                                                  parent.locked().lock());
      DEBUG_ASSERT(sibling_ref.get() != this);
      parent_raw = parent_.get();
    }

    // Reacquire the locks, sibling first as it is to the 'left' in list order.
    LockedPtr sibling = LockedPtr(sibling_ref.get());
    // We could have the same lock order as our sibling, so we use the gap in the lock orders to
    // acquire.
    Guard<CriticalMutex> guard{AssertOrderedLock, lock(), sibling_ref->lock_order() + 1};
    // With our lock reacquired, check that this still needs a dead transition, as it could already
    // have been done by someone else.
    if (!should_dead_transition_locked()) {
      return nullptr;
    }

    // With both us and our sibling locked check that they are indeed still our sibling by ensuring
    // we both have the same original parent. This check failing would imply that our sibling got
    // dead transitioned and we merged with the parent. We might still need a dead transition, but
    // the locks we need are now all different so we just retry from the top.
    if (parent_.get() != parent_raw || sibling.locked().parent_.get() != parent_raw) {
      continue;
    }
    LockedPtr parent(parent_.get());
    // Even if parent didn't change it could have gained new children and we might be needing to
    // acquire a right sibling instead. For simplicity just retry.
    if (parent.locked().children_list_len_ != 2) {
      continue;
    }

    return DeadTransitionLocked(parent, sibling);
  }
}

fbl::RefPtr<VmCowPages> VmCowPages::DeadTransitionLocked(const LockedPtr& parent,
                                                         const LockedPtr& sibling) {
  canary_.Assert();
  DEBUG_ASSERT(life_cycle_ == LifeCycle::Alive);
  // Change our life cycle to the dying state so that if we need to drop the lock no other attempts
  // are made at performing a DeadTransition.
  life_cycle_ = LifeCycle::Dying;

  // Close any PageSource. It does not matter if we do this before or after removing the pages, as
  // we hold the lock continuously, but it makes more sense (and is slightly more efficient for the
  // PhysicalPageProvider) to notify the close before.
  if (page_source_) {
    page_source_->Close();
  }

  // To prevent races with a hidden parent creation or merging, it is necessary to hold the lock
  // over the is_hidden and parent_ check and into the subsequent removal call.

  // At the point of destruction we should no longer have any mappings or children still
  // referencing us, and by extension our priority count must therefore be back to zero.
  DEBUG_ASSERT(high_priority_count_ == 0);
  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
  fbl::RefPtr<VmCowPages> deferred;

  // If we're not a hidden vmo then we need to remove ourselves from our parent and free any pages
  // that we own.
  if (!is_hidden()) {
    // Clear out all content that we can see. This means dropping references to any pages in our
    // parents, as well as removing any pages in our own page list.
    __UNINITIALIZED ScopedPageFreedList freed_list;
    ReleaseOwnedPagesLocked(0, parent, freed_list);
    freed_list.FreePages(this);

    DEBUG_ASSERT(parent.get() == parent_.get());
    if (parent_) {
      parent.locked().RemoveChildLocked(this, sibling);

      // We removed a child from the parent, and so it may also need to be cleaned.
      // Avoid recursing destructors and dead transitions when we delete our parent by using the
      // deferred deletion method, i.e. return the parent_ and have the caller call dead transition
      // on it.
      deferred = ktl::move(parent_);
    } else {
      // If we had a parent then RemoveChildLocked would have cleaned up any cursors, but otherwise
      // we must erase from any lists. As we have no parent and cannot have children the root and
      // current cursor list must be equivalent, and so only need to process one.
      TreeWalkCursor::Erase(root_cursor_list_, this);
    }
  } else {
    // Most of the hidden vmo's state should have already been cleaned up when it merged
    // itself into its child in ::RemoveChildLocked.
    DEBUG_ASSERT(children_list_len_ == 0);
    DEBUG_ASSERT(page_list_.HasNoPageOrRef());
    DEBUG_ASSERT(!parent_);
  }

  DEBUG_ASSERT(page_list_.IsEmpty());
  DEBUG_ASSERT(root_cursor_list_.is_empty());
  DEBUG_ASSERT(cur_cursor_list_.is_empty());

  // Due to the potential lock dropping earlier double check our life_cycle_ is what we expect.
  DEBUG_ASSERT(life_cycle_ == LifeCycle::Dying);
  life_cycle_ = LifeCycle::Dead;
  return deferred;
}

VmCowPages::~VmCowPages() {
  // Most of the explicit cleanup happens in DeadTransition() with asserts and some remaining
  // cleanup happening here in the destructor.
  canary_.Assert();
  DEBUG_ASSERT(page_list_.HasNoPageOrRef());
  // A cow pages can only be destructed if it is either still in the Init state, suggesting
  // something when wrong with completing construction, or if it is fully in the Dead state, nothing
  // in between.
  DEBUG_ASSERT(life_cycle_ == LifeCycle::Init || life_cycle_ == LifeCycle::Dead);
  // The discardable tracker is unlinked explicitly in the destructor to ensure that no RefPtrs can
  // be constructed to the VmCowPages from here. See comment in
  // DiscardableVmoTracker::DebugDiscardablePageCounts that depends upon this being here instead of
  // during the dead transition.
  if (discardable_tracker_) {
    Guard<CriticalMutex> guard{lock()};
    discardable_tracker_->assert_cow_pages_locked();
    discardable_tracker_->RemoveFromDiscardableListLocked();
  }
}

template <typename T>
zx_status_t VmCowPages::ForEveryOwnedHierarchyPageInRangeLocked(T func, uint64_t offset,
                                                                uint64_t size,
                                                                const LockedPtr& parent) const {
  return ForEveryOwnedHierarchyPageInRange<const VmPageOrMarker*>(const_cast<VmCowPages*>(this),
                                                                  func, offset, size, parent);
}

template <typename T>
zx_status_t VmCowPages::ForEveryOwnedMutableHierarchyPageInRangeLocked(T func, uint64_t offset,
                                                                       uint64_t size,
                                                                       const LockedPtr& parent) {
  return ForEveryOwnedHierarchyPageInRange<VmPageOrMarkerRef>(this, func, offset, size, parent);
}

template <typename T>
zx_status_t VmCowPages::RemoveOwnedHierarchyPagesInRangeLocked(T func, uint64_t offset,
                                                               uint64_t size,
                                                               const LockedPtr& parent) {
  return ForEveryOwnedHierarchyPageInRange<VmPageOrMarker*>(this, func, offset, size, parent);
}

template <typename P, typename S, typename T>
zx_status_t VmCowPages::ForEveryOwnedHierarchyPageInRange(S* self, T func, uint64_t offset,
                                                          uint64_t size, const LockedPtr& parent) {
  DEBUG_ASSERT(IS_PAGE_ROUNDED(offset));
  DEBUG_ASSERT(IS_PAGE_ROUNDED(size));

  uint64_t start_in_self = offset;
  uint64_t end_in_self = CheckedAdd(offset, size);
  uint64_t start_in_cur = start_in_self;
  uint64_t end_in_cur = end_in_self;

  LockedParentWalker walker(parent);

  while (start_in_self < end_in_self) {
    // We attempt to always inline these lambdas, as its a huge performance benefit and has minimal
    // impact on code size.
    bool stopped_early = false;
    uint64_t parent_content_start = UINT64_MAX;
    uint64_t parent_content_end = 0;
    auto page_callback = [&](auto p, uint64_t page_offset) __ALWAYS_INLINE {
      AssertHeld(self->lock_ref());
      uint64_t cur_to_self = start_in_cur - start_in_self;
      // If we had started tracking a run of contiguous parent content then we must walk up once it
      // stops, either due to a gap or a switch to some other entry type.
      if (parent_content_end != 0 && (page_offset != parent_content_end || !p->IsParentContent())) {
        return ZX_ERR_STOP;
      }
      if (p->IsParentContent()) {
        // ParentContent markers can exist spuriously (see explanation on
        // tree_has_parent_content_markers) and so only consider walking up if within the
        // parent_limit_.
        if (page_offset < walker.current(self).parent_limit_) {
          // Either adding to or starting a new contiguous parent content run.
          parent_content_start = ktl::min(parent_content_start, page_offset);
          parent_content_end = page_offset + PAGE_SIZE;
        }
        return ZX_ERR_NEXT;
      }
      zx_status_t status = func(p, &walker.current(self), page_offset - cur_to_self, page_offset);
      if (status == ZX_ERR_STOP) {
        stopped_early = true;
      }
      return status;
    };
    auto gap_callback = [&](uint64_t gap_start_offset, uint64_t gap_end_offset) __ALWAYS_INLINE {
      // The gap is empty, so walk up if the parent is accessible from any part of it.
      // Mark the range immediately preceding the gap as processed.
      AssertHeld(self->lock_ref());
      // Gaps will never be considered on nodes that have parent content markers, so should never be
      // in the middle of calculating a parent content run.
      DEBUG_ASSERT(parent_content_end == 0);
      if (gap_start_offset < walker.current(self).parent_limit_) {
        parent_content_start = gap_start_offset;
        parent_content_end = gap_end_offset;
        return ZX_ERR_STOP;
      }
      return ZX_ERR_NEXT;
    };

    zx_status_t status = ZX_OK;
    if (walker.current(self).is_parent_hidden_locked() &&
        start_in_cur < walker.current(self).parent_limit_ &&
        !walker.current(self).node_has_parent_content_markers()) {
      // We can see into a hidden parent, and cannot use content markers to optimize the walk up, so
      // need to consider any gaps.
      if constexpr (ktl::is_same_v<P, VmPageOrMarker*>) {
        status = walker.current(self).page_list_.RemovePagesAndIterateGaps(
            page_callback, gap_callback, start_in_cur, end_in_cur);
      } else if constexpr (ktl::is_same_v<P, VmPageOrMarkerRef>) {
        status = walker.current(self).page_list_.ForEveryPageAndGapInRangeMutable(
            page_callback, gap_callback, start_in_cur, end_in_cur);
      } else {
        status = walker.current(self).page_list_.ForEveryPageAndGapInRange(
            page_callback, gap_callback, start_in_cur, end_in_cur);
      }
    } else {
      // Either we cannot see into a hidden parent, or we are able to utilize parent content
      // markers, and so do not need to consider gaps and can just directly process the pages.
      if constexpr (ktl::is_same_v<P, VmPageOrMarker*>) {
        status =
            walker.current(self).page_list_.RemovePages(page_callback, start_in_cur, end_in_cur);
      } else if constexpr (ktl::is_same_v<P, VmPageOrMarkerRef>) {
        status = walker.current(self).page_list_.ForEveryPageInRangeMutable(
            page_callback, start_in_cur, end_in_cur);
      } else {
        status = walker.current(self).page_list_.ForEveryPageInRange(page_callback, start_in_cur,
                                                                     end_in_cur);
      }
    }
    if (status != ZX_OK) {
      return status;
    }

    // If the page callback wanted to stop early, then do so.
    if (stopped_early) {
      return ZX_OK;
    }

    if (parent_content_end != 0) {
      // If we found a run of parent content, either via parent content markers or from a gap, then
      // need to walk up and look for it.
      start_in_self += parent_content_start - start_in_cur;
      start_in_cur = parent_content_start + walker.current(self).parent_offset_;
      end_in_cur = ktl::min(parent_content_end, walker.current(self).parent_limit_) +
                   walker.current(self).parent_offset_;
      walker.WalkUp(self);
    } else {
      // If not walk up, then mark the entire range as processed and begin another walk up from
      // `self`.
      start_in_self += end_in_cur - start_in_cur;
      start_in_cur = start_in_self;
      end_in_cur = end_in_self;
      walker.reset();
    }
  }

  return ZX_OK;
}

bool VmCowPages::DedupZeroPage(vm_page_t* page, uint64_t offset) {
  canary_.Assert();

  __UNINITIALIZED DeferredOps deferred(this);
  Guard<CriticalMutex> guard{lock()};

  // Forbid zero page deduping if this is high priority.
  if (high_priority_count_ != 0) {
    return false;
  }

  // The VmObjectPaged could have been destroyed, or this could be a hidden node. Check if the
  // paged_ref_ is valid first.
  if (paged_ref_) {
    if (!paged_backlink_locked(this)->CanDedupZeroPagesLocked()) {
      return false;
    }
  }

  // Check this page is still a part of this VMO. object.page_offset could be wrong, but there's no
  // harm in looking up a random slot as we'll then notice it's the wrong page.
  // Also ignore any references since we cannot efficiently scan them, and they should presumably
  // already be deduped.
  // Pinned pages cannot be decommited and so also must not be committed. We must also not decommit
  // pages from kernel VMOs, as the kernel cannot fault them back in, but all kernel pages will be
  // pinned.
  VmPageOrMarkerRef page_or_marker = page_list_.LookupMutable(offset);
  if (!page_or_marker || !page_or_marker->IsPage() || page_or_marker->Page() != page ||
      page->object.pin_count > 0 || (is_page_dirty_tracked(page) && !is_page_clean(page))) {
    return false;
  }

  // We expect most pages to not be zero, as such we will first do a 'racy' zero page check where
  // we leave write permissions on the page. If the page isn't zero, which is our hope, then we
  // haven't paid the price of modifying page tables.
  if (!IsZeroPage(page_or_marker->Page())) {
    return false;
  }

  RangeChangeUpdateLocked(VmCowRange(offset, PAGE_SIZE), RangeChangeOp::RemoveWrite, nullptr);
  // No range change needs to be processed for the children since children, by virtue of being
  // copy-on-write, cannot have a writable mapping.

  if (IsZeroPage(page_or_marker->Page())) {
    VmPageOrMarker old_page;

    if (node_has_parent_content_markers()) {
      // If using parent content markers then we do not need to, and are not permitted to, insert a
      // regular marker. Instead just clear the slot, which indicates zero content regardless of any
      // parents above us.
      RangeChangeUpdateLocked(VmCowRange(offset, PAGE_SIZE), RangeChangeOp::Unmap, &deferred);
      old_page = page_list_.RemoveContent(offset);
    } else {
      // Replace the slot with a marker.
      __UNINITIALIZED auto result =
          BeginAddPageWithSlotLocked(offset, page_or_marker, CanOverwriteContent::NonZero);
      DEBUG_ASSERT(result.is_ok());
      old_page = CompleteAddPageLocked(*result, VmPageOrMarker::Marker(), &deferred);
    }
    DEBUG_ASSERT(old_page.IsPage());

    // Free the old page.
    vm_page_t* released_page = old_page.ReleasePage();
    RemovePageLocked(released_page, deferred);

    reclamation_event_count_++;
    VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
    VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
    return true;
  }
  return false;
}

zx_status_t VmCowPages::Create(VmCowPagesOptions options, uint32_t pmm_alloc_flags, uint64_t size,
                               ktl::unique_ptr<DiscardableVmoTracker> discardable_tracker,
                               fbl::RefPtr<VmCowPages>* cow_pages) {
  DEBUG_ASSERT(!(options & VmCowPagesOptions::kInternalOnlyMask));
  fbl::AllocChecker ac;
  auto cow = fbl::AdoptRef<VmCowPages>(new (&ac) VmCowPages(options, pmm_alloc_flags, size, nullptr,
                                                            ktl::move(discardable_tracker),
                                                            kLockOrderFirstAnon));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  if (cow->discardable_tracker_) {
    cow->discardable_tracker_->InitCowPages(cow.get());
  }

  *cow_pages = ktl::move(cow);
  return ZX_OK;
}

zx_status_t VmCowPages::CreateExternal(fbl::RefPtr<PageSource> src, VmCowPagesOptions options,
                                       uint64_t size, fbl::RefPtr<VmCowPages>* cow_pages) {
  DEBUG_ASSERT(!(options & VmCowPagesOptions::kInternalOnlyMask));
  fbl::AllocChecker ac;
  auto cow = fbl::AdoptRef<VmCowPages>(new (&ac) VmCowPages(
      options, PMM_ALLOC_FLAG_CAN_WAIT, size, ktl::move(src), nullptr, kLockOrderRoot));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  *cow_pages = ktl::move(cow);
  return ZX_OK;
}

void VmCowPages::ReplaceChildLocked(VmCowPages* old, VmCowPages* new_child) {
  canary_.Assert();

  [[maybe_unused]] VmCowPages* replaced = children_list_.replace(*old, new_child);
  DEBUG_ASSERT(replaced == old);
}

void VmCowPages::DropChildLocked(VmCowPages* child) {
  canary_.Assert();

  [[maybe_unused]] VmCowPages* erased = children_list_.erase(*child);
  DEBUG_ASSERT(erased == child);
  DEBUG_ASSERT(children_list_len_ > 0);
  --children_list_len_;
}

void VmCowPages::AddChildLocked(VmCowPages* child, uint64_t offset, uint64_t parent_limit) {
  canary_.Assert();

  // This function must succeed, as failure here requires the caller to roll back allocations.

  // The child should definitely stop seeing into the parent at the limit of its size.
  DEBUG_ASSERT(parent_limit <= child->size_);
  // The child's offsets must not overflow when projected onto the root.
  // Callers should validate this externally and report errors as appropriate.
  const uint64_t root_parent_offset = CheckedAdd(offset, root_parent_offset_);
  CheckedAdd(root_parent_offset, child->size_);

  // Write in the parent view values.
  child->root_parent_offset_ = root_parent_offset;
  child->parent_offset_ = offset;
  child->parent_limit_ = parent_limit;

  // The child's page list should skew by the child's offset relative to the parent. This allows
  // fast copies of page list entries when merging the lists later (entire blocks of entries can be
  // copied at once).
  child->page_list_.InitializeSkew(page_list_.GetSkew(), offset);

  // If the child has a non-zero high priority count, then it is counting as an incoming edge to our
  // count.
  if (child->high_priority_count_ > 0) {
    ChangeSingleHighPriorityCountLocked(1);
  }

  child->parent_ = fbl::RefPtr(this);
  children_list_.push_front(child);
  children_list_len_++;
}

VmCowPages::ParentAndRange VmCowPages::FindParentAndRangeForCloneLocked(
    uint64_t offset, uint64_t size, bool parent_must_be_hidden) {
  DEBUG_ASSERT(!is_hidden());

  // The clone's parent limit starts out equal to its size, but it can't exceed the parent's size.
  // This ensures that any clone pages beyond the parent's range get initialized from zeroes.
  uint64_t parent_limit = ClampedLimit(offset, size, size_);

  LockedPtr parent;
  LockedPtr grandparent;

  // Walk up the hierarchy until we find the last node which can correctly be the clone's parent.
  while (VmCowPages* next_parent = parent.locked_or(this).parent_.get()) {
    grandparent = LockedPtr(next_parent);

    // `parent` will always satisfy `parent_must_be_hidden` at this point.
    //
    // If `next_parent` doesn't satisfy `parent_must_be_hidden` then we must use `parent` as the
    // clone's parent, even if it doesn't have any pages for the clone to snapshot.
    if (parent_must_be_hidden && !next_parent->is_hidden()) {
      break;
    }

    // If `parent` owns any pages in the clone's range then we muse use it as the clone's parent.
    // If we continued iterating, the clone couldn't snapshot all ancestor pages that it would be
    // able to if `this` had been the parent.
    // This will specifically walk through any parent content markers, since they indicate the
    // presence of content *above* this node, not held specifically by this node.
    if (parent_limit > 0 && parent.locked_or(this).page_list_.AnyOwnedPagesOrIntervalsInRange(
                                offset, offset + parent_limit)) {
      break;
    }

    // Before the loop the caller validated that the clone's offsets cannot overflow when projected
    // onto the root. Verify this will remain true.
    //
    // Each iteration of this loop must leave the clone's ultimate `root_parent_offset_` unchanged.
    // We will increase the clone's `offset` by the current parent's `parent_offset_` but the new
    // parent's `root_parent_offset_` is smaller by the same amount.
    DEBUG_ASSERT(CheckedAdd(grandparent.locked().root_parent_offset_,
                            parent.locked_or(this).parent_offset_) ==
                 parent.locked_or(this).root_parent_offset_);

    // To move to `next_parent` we need to translate the clone's window to be relative to it.
    //
    // The clone's last visible offset into `next_parent` cannot exceed `parent`'s parent limit, as
    // it shouldn't be able to see more pages than it could see if `parent` had been the parent.
    parent_limit = ClampedLimit(offset, parent_limit, parent.locked_or(this).parent_limit_);
    offset = CheckedAdd(parent.locked_or(this).parent_offset_, offset);

    parent = ktl::move(grandparent);
  }

  return ParentAndRange{ktl::move(parent), ktl::move(grandparent), offset, parent_limit, size};
}

zx::result<VmCowPages::LockedRefPtr> VmCowPages::CloneNewHiddenParentLocked(
    uint64_t offset, uint64_t limit, uint64_t size, VmPageList&& initial_page_list,
    const LockedPtr& parent) {
  canary_.Assert();

  const VmCowPagesOptions options = inheritable_options();

  fbl::AllocChecker ac;
  LockedRefPtr cow_clone;
  // Use a sub-scope to limit visibility of cow_clone_ref as it's just a temporary.
  {
    auto cow_clone_ref = fbl::AdoptRef<VmCowPages>(new (&ac) VmCowPages(
        options, pmm_alloc_flags_, size, nullptr, nullptr, kLockOrderFirstAnon));
    if (!ac.check()) {
      return zx::error(ZX_ERR_NO_MEMORY);
    }
    // As this node was just constructed we know the lock is free, use one of the lock order gap
    // values to acquire without a lockdep violation. If we have a parent, and hence hold its lock,
    // then we must set the lock order after it.
    DEBUG_ASSERT(parent_.get() == parent.get());
    const uint64_t order = (parent ? parent->lock_order() : lock_order()) + 1;
    cow_clone = LockedRefPtr(ktl::move(cow_clone_ref), order);
  }

  DEBUG_ASSERT(!is_hidden());
  // If `parent` is to be the new child's parent then it must become hidden first.
  // That requires creating a new hidden node and rotating `parent` to be its child.
  DEBUG_ASSERT(life_cycle_ == LifeCycle::Alive);
  DEBUG_ASSERT(children_list_len_ == 0);

  // Invalidate everything, both the pages the clone will and will not be able to see. As hidden
  // nodes are immutable, even for pages that the clone cannot see we want the parent_clone to
  // move them back out before modifying them.
  // Note: We could eagerly move these pages into the parent_clone instead.
  // Bi-directional clones may not themselves already have children, so we are able to assume an
  // absence here when performing the range update.
  RangeChangeUpdateLocked(VmCowRange(0, size_), RangeChangeOp::RemoveWrite, nullptr);

  LockedRefPtr hidden_parent;
  // Use a sub-scope to limit visibility of hidden_parent_ref as it's just a temporary.
  {
    // Lock order for a new hidden parent is either derived from its parent, or if no parent
    // starts kLockOrderRoot. Cow creation rules state that our parent is either hidden, or a page
    // root node ensuring that our derived lock order will still be in the hidden range.
    DEBUG_ASSERT(!parent_ || parent_->is_hidden() || parent_->page_source_);
    const uint64_t hidden_lock_order =
        parent_ ? parent_->lock_order() - kLockOrderDelta : kLockOrderRoot;
    auto hidden_parent_ref = fbl::AdoptRef<VmCowPages>(
        new (&ac) VmCowPages(options | VmCowPagesOptions::kHidden, pmm_alloc_flags_, size_, nullptr,
                             nullptr, hidden_lock_order));
    if (!ac.check()) {
      return zx::error(ZX_ERR_NO_MEMORY);
    }
    // If we have a parent (which will become the parent of the new hidden node) then since its
    // lock is already acquired we cannot acquire the new hidden parent using its normal lock
    // order. As we just created this node we know that no one else can be acquiring it, so we use
    // the gap in the regular lock orders, taking into account that the new leaf node was already
    // acquired into the same gap.
    const uint64_t order = parent ? parent->lock_order() + 2 : hidden_parent_ref->lock_order();
    hidden_parent = LockedRefPtr(ktl::move(hidden_parent_ref), order);
  }

  // Create a temporary page list collect the parent content markers we might need to make. This
  // will eventually become our page_list_, but not until we've updated the backlinks and moved it
  // into the hidden parent.
  VmPageList temp_list;
  temp_list.InitializeSkew(page_list_.GetSkew(), 0);

  VmCompression* compression = Pmm::Node().GetPageCompression();
  zx_status_t status = ZX_OK;

  {
    __UNINITIALIZED BatchPQUpdateBacklink page_backlink_updater(&hidden_parent.locked());
    status = page_list_.RemovePages(
        [&](VmPageOrMarker* p, uint64_t offset) {
          if (tree_has_parent_content_markers()) {
            // If a tree is uses parent content markers then, since we are a leaf node, we know that
            // there can be no markers and no intervals, hence this is either content, or a parent
            // marker. In either case we need to retain a ParentContent marker in |this|, and since
            // the page list being iterated will be moved into |hidden_parent|, add a slot to the
            // |temp_list|.
            DEBUG_ASSERT(node_has_parent_content_markers());
            DEBUG_ASSERT(p->IsParentContent() || p->IsPageOrRef());
            auto [slot, _] =
                temp_list.LookupOrAllocate(offset, VmPageList::IntervalHandling::NoIntervals);
            if (!slot) {
              return ZX_ERR_NO_MEMORY;
            }
            *slot = VmPageOrMarker::ParentContent();
            if (p->IsParentContent()) {
              // Hidden nodes do not themselves have parent content markers, as we have effectively
              // moved this to ourselves can clear this slot and continue.
              *p = VmPageOrMarker::Empty();
              return ZX_ERR_NEXT;
            }
          }
          if (p->IsReference()) {
            // A regular reference we can move, a temporary reference we need to turn back into
            // its page so we can move it. To determine if we have a temporary reference we can
            // just attempt to move it, and if it was a temporary reference we will get a page
            // returned.
            if (auto maybe_page = MaybeDecompressReference(compression, p->Reference())) {
              // For simplicity, since this is a very uncommon edge case, just update the page in
              // place in this page list, then move it as a regular page.
              AssertHeld(lock_ref());
              SetNotPinnedLocked(*maybe_page, offset);
              VmPageOrMarker::ReferenceValue ref = p->SwapReferenceForPage(*maybe_page);
              ASSERT(compression->IsTempReference(ref));
            }
          }
          // Not an else-if to intentionally perform this if the previous block turned a reference
          // into a page.
          if (p->IsPage()) {
            page_backlink_updater.Push(p->Page(), offset);
          }
          return ZX_ERR_NEXT;
        },
        0, size_);

    page_backlink_updater.Flush();
  }

  // On error we need to roll-back any partial modifications.
  if (status != ZX_OK) {
    DEBUG_ASSERT_MSG(status == ZX_ERR_NO_MEMORY, "status: %d", status);
    // Re-set all the backlinks back to |this|. Any backlinks that hadn't yet been moved will get a
    // harmless no-op.
    __UNINITIALIZED BatchPQUpdateBacklink page_backlink_updater(this);
    page_list_.ForEveryPage([&](const VmPageOrMarker* p, uint64_t offset) {
      if (p->IsPage()) {
        page_backlink_updater.Push(p->Page(), offset);
      }
      return ZX_ERR_NEXT;
    });
    // Need to put back any ParentContent markers we had deleted.
    temp_list.MergeRangeOntoAndClear(
        [](VmPageOrMarker* src, VmPageOrMarker* dst, uint64_t) {
          // The only items in temp_list are parent content markers we just put in.
          DEBUG_ASSERT(src->IsParentContent());
          // If dst is empty then it use to hold a ParentContent marker, but we deleted it, so put
          // it back. A non-empty dst we leave alone, as that indicates where we created a
          // ParentContent marker for content that we did not modify, and hence do not need to roll
          // back.
          if (dst->IsEmpty()) {
            *dst = ktl::move(*src);
          }
        },
        page_list_, 0, size_);
    // temp_list just contains ParentContent markers, which can be safely dropped.
    return zx::error(status);
  }

  // Move our pagelist before adding ourselves as its child, because we cannot be added as a child
  // unless we have no pages.
  hidden_parent.locked().page_list_ = ktl::move(page_list_);

  hidden_parent.locked().TransitionToAliveLocked();

  // If the current object is not the root of the tree, then we need to replace ourselves in our
  // parent's child list with the new hidden node before we can becomes its child.
  if (parent_) {
    DEBUG_ASSERT(parent && parent.get() == parent_.get());
    // Copy the offsets and limits from the current node to the newly created parent.
    // This logic is similar to AddChildLocked, except that we don't need to recompute these
    // values.
    hidden_parent.locked().root_parent_offset_ = root_parent_offset_;
    hidden_parent.locked().parent_offset_ = parent_offset_;
    hidden_parent.locked().parent_limit_ = parent_limit_;

    // We do not need to set high_priority_count_ because the called to AddChildLocked below
    // will initialize high_priority_count_ for hidden_parent.

    parent.locked().ReplaceChildLocked(this, hidden_parent.get());
    hidden_parent.locked().parent_ = ktl::move(parent_);

    // We have lost our parent, which means we could now be violating the invariant that
    // parent_limit_ being non-zoer implies we have a parent. In practice this assignment
    // shouldn't matter because we are about to add ourselves as a child of `hidden_parent`.
    parent_offset_ = parent_limit_ = 0;
  }

  // Add the children and then populate their initial page lists.
  hidden_parent.locked().AddChildLocked(this, 0, size_);
  hidden_parent.locked().AddChildLocked(&cow_clone.locked(), offset, limit);
  DEBUG_ASSERT(temp_list.GetSkew() == page_list_.GetSkew());
  page_list_ = ktl::move(temp_list);
  DEBUG_ASSERT(cow_clone.locked().page_list_.GetSkew() == initial_page_list.GetSkew());
  cow_clone.locked().page_list_ = ktl::move(initial_page_list);

  // Checking this node's hierarchy will also check the parent's hierarchy.
  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());

  return zx::ok(ktl::move(cow_clone));
}

zx::result<VmCowPages::LockedRefPtr> VmCowPages::CloneChildLocked(uint64_t offset, uint64_t limit,
                                                                  uint64_t size,
                                                                  VmPageList&& initial_page_list,
                                                                  const LockedPtr& parent) {
  canary_.Assert();

  VmCowPagesOptions options = inheritable_options();

  LockedRefPtr cow_clone;
  // Use a sub-scope to limit visibility of cow_clone_ref as it's just a temporary.
  {
    fbl::AllocChecker ac;
    // We are either constructing the first visible anonymous node in a chain, which gets
    // kLockOrderFirstAnon, or this is part of a unidirectional clone chain and takes a lock order
    // derived from ourselves. In full these possibilities are:
    //  * This is userpager root (we have no parent and are not hidden), we are creating first
    //    visible anonymous node
    //  * This is a hidden node, we are creating first visible anonymous node
    //  * Unidirectional clone chain (we have parent and are not hidden), creating derived visible
    //    anonymous node.
    // See comment above lock_order_ definition for more details.
    const uint64_t clone_order =
        (parent_ && !is_hidden()) ? lock_order() - kLockOrderDelta : kLockOrderFirstAnon;
    auto cow_clone_ref = fbl::AdoptRef<VmCowPages>(
        new (&ac) VmCowPages(options, pmm_alloc_flags_, size, nullptr, nullptr, clone_order));
    if (!ac.check()) {
      return zx::error(ZX_ERR_NO_MEMORY);
    }
    // As this node was just constructed we know the lock is free, use one of the lock order gap
    // values to acquire without a lockdep violation. If we have a parent, and hence hold its lock,
    // then we must set the lock order after it.
    DEBUG_ASSERT(parent_.get() == parent.get());
    cow_clone =
        LockedRefPtr(ktl::move(cow_clone_ref), (parent ? parent->lock_order() : lock_order()) + 1);
  }

  AddChildLocked(&cow_clone.locked(), offset, limit);
  // If given a non-empty initial_page_list then place it in the clone.
  if (!initial_page_list.IsEmpty()) {
    DEBUG_ASSERT(cow_clone.locked().page_list_.GetSkew() == initial_page_list.GetSkew());
    cow_clone.locked().page_list_ = ktl::move(initial_page_list);
  }

  // Checking this node's hierarchy will also check the parent's hierarchy.
  // It will not check the child's page sharing however, so check that independently.
  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
  VMO_VALIDATION_ASSERT(cow_clone.locked().DebugValidatePageSharingLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(cow_clone.locked().DebugValidateVmoPageBorrowingLocked());

  return zx::ok(ktl::move(cow_clone));
}

zx::result<VmCowPages::LockedRefPtr> VmCowPages::CreateCloneLocked(SnapshotType type,
                                                                   bool require_unidirectional,
                                                                   VmCowRange range,
                                                                   DeferredOps& ops) {
  canary_.Assert();

  // When creating a clone the DeferredOps is not used beyond acting to serialize operations on
  // pager backed hierarchies via the page_source_lock that it holds. For why this is important see
  // the comments in ::Resize.
  DEBUG_ASSERT(ops.self_ == this);

  LTRACEF("vmo %p offset %#" PRIx64 " size %#" PRIx64 "\n", this, range.offset, range.len);

  DEBUG_ASSERT(range.is_page_aligned());
  DEBUG_ASSERT(!is_hidden());
  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());

  // A full snapshot is not compatible with there being a root page source. More specifically a
  // full snapshot requires that there be no unidirectional clones in the tree, and this invariant
  // is maintained by limiting unidirectional clones to only existing if there *is* a root page
  // source. Any unidirectional clones in the tree would be able to introduce / modify content,
  // which is not compatible with the notion of a full snapshot.
  if (type == SnapshotType::Full && can_root_source_evict()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  // Determine whether the snapshot type is requiring a bidirectional clone or not.
  const bool require_bidirectional = [&]() TA_REQ(lock()) {
    switch (type) {
      case SnapshotType::Full:
        // As per the above check, a full snapshot is incompatible with unidirectional clones, and
        // so this type insists on bidirectional.
        return true;
      case SnapshotType::Modified:
        // If there is a parent then a bidirectional clone is required in order to produce a
        // snapshot of any of the pages we have modified with respect to our parent. In the absence
        // of a parent there is no restriction.
        return !!parent_;
      case SnapshotType::OnWrite:
        // Any kind of clone implements on copy-on-write, so no restriction.
        return false;
    }
    return false;
  }();

  // Offsets within the new clone must not overflow when projected onto the root.
  {
    uint64_t child_root_parent_offset;
    bool overflow;
    overflow = add_overflow(root_parent_offset_, range.offset, &child_root_parent_offset);
    if (overflow) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    uint64_t child_root_parent_end;
    overflow = add_overflow(child_root_parent_offset, range.len, &child_root_parent_end);
    if (overflow) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
  }

  if (require_bidirectional && require_unidirectional) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  const bool unidirectional = !require_bidirectional && can_unidirectional_clone_locked();

  // Only contiguous VMOs have a source that handles free, and those may not have cow clones made of
  // them. Once there is a cow hierarchy tracking exactly what node a page was from to free it is
  // not performed, and it is assumed that therefore that we do not need to free owned pages to
  // their 'correct' object.
  ASSERT(!is_source_handling_free());

  if (unidirectional) {
    ParentAndRange child_range = FindParentAndRangeForCloneLocked(range.offset, range.len, false);

    return child_range.parent.locked_or(this).CloneChildLocked(
        child_range.parent_offset, child_range.parent_limit, child_range.size, VmPageList(),
        child_range.grandparent);
  }

  if (require_unidirectional) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  // If this is non-zero, that means that there are pages which hardware can
  // touch, so the vmo can't be safely cloned.
  // TODO: consider immediately forking these pages.
  if (pinned_page_count_locked()) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  VmCompression* compression = Pmm::Node().GetPageCompression();

  // For any content that we have part or full ownership of in the range to be cloned, then the
  // child, regardless of what actual node it ends up hanging of, will gain part ownership of said
  // content. Therefore we first want to find all such content, incrementing the share counts, and
  // populating a new page list with parent content markers if needed.
  // We explicitly need to do this *before* walking up because, if using parent content markers, the
  // content we are able to see is possibly determined by content markers in *this* node, even if we
  // will be able to mechanically hang the new node higher up.
  VmPageList page_list;
  page_list.InitializeSkew(page_list_.GetSkew(), range.offset);

  // To account for any errors that result in needing to roll back we remember the range we have
  // processed the share counts for.
  uint64_t shared_end = range.offset;
  auto rollback = fit::defer([this, &range, &shared_end, compression]() {
    AssertHeld(lock_ref());

    // Decrement the share count on all pages. As every page we can see is also owned by this, and
    // we have continuously held our lock, no page should need to be freed as a result.
    zx_status_t status = RemoveOwnedHierarchyPagesInRangeLocked(
        [&](VmPageOrMarker* p, const VmCowPages* owner, uint64_t this_offset,
            uint64_t owner_offset) {
          if (p->IsPage()) {
            vm_page_t* page = p->Page();
            DEBUG_ASSERT(page->object.share_count > 0);
            page->object.share_count--;
          } else if (p->IsReference()) {
            const uint32_t share_count = compression->GetMetadata(p->Reference());
            DEBUG_ASSERT(share_count > 0);
            compression->SetMetadata(p->Reference(), share_count - 1);
          }
          return ZX_ERR_NEXT;
        },
        range.offset, shared_end - range.offset, LockedPtr());
    DEBUG_ASSERT(status == ZX_OK);
  });

  // Update any share counts for content the clone will be able to see, and populate a temporary
  // page list with any parent content markers if needed.
  zx_status_t status = ForEveryOwnedMutableHierarchyPageInRangeLocked(
      [&](VmPageOrMarkerRef p, VmCowPages* owner, uint64_t cow_clone_offset,
          uint64_t owner_offset) {
        if (tree_has_parent_content_markers() && p->IsPageOrRef()) {
          const uint64_t off = cow_clone_offset - range.offset;
          auto [slot, _] =
              page_list.LookupOrAllocate(off, VmPageList::IntervalHandling::NoIntervals);
          if (!slot) {
            return ZX_ERR_NO_MEMORY;
          }
          *slot = VmPageOrMarker::ParentContent();
        }
        if (p->IsPage()) {
          p->Page()->object.share_count++;
        } else if (p->IsReference()) {
          VmPageOrMarker::ReferenceValue ref = p->Reference();
          compression->SetMetadata(ref, compression->GetMetadata(ref) + 1);
        }
        shared_end = owner_offset + PAGE_SIZE;

        return ZX_ERR_NEXT;
      },
      range.offset, range.len, LockedPtr());

  if (status != ZX_OK) {
    // However far we got is recorded in |shared_end|, and |rollback| will clean it up.
    return zx::error(status);
  }

  ParentAndRange child_range = FindParentAndRangeForCloneLocked(range.offset, range.len, true);

  // The bidirectional clone check requires looking at the parent of where we want to hang the
  // node, which is represented by |child_range.grandparent|.
  if (!can_bidirectional_clone_locked(child_range.grandparent)) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  // If we found a hidden node to be our parent, then we can just hang a new node under that,
  // otherwise we need to also create a new hidden node to place this and the new child under.
  auto result = child_range.parent.locked_or(this).is_hidden()
                    ? child_range.parent.locked().CloneChildLocked(
                          child_range.parent_offset, child_range.parent_limit, child_range.size,
                          ktl::move(page_list), child_range.grandparent)
                    : child_range.parent.locked_or(this).CloneNewHiddenParentLocked(
                          child_range.parent_offset, child_range.parent_limit, child_range.size,
                          ktl::move(page_list), child_range.grandparent);
  // If everything went well then we can finally cancel the rollback and let the clone own the
  // content we added the share counts for.
  if (result.is_ok()) {
    rollback.cancel();
  }
  return result;
}

void VmCowPages::RemoveChildLocked(VmCowPages* removed, const LockedPtr& sibling) {
  canary_.Assert();

  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());

  // If we have a sibling to the right of the removed node then update any cursors to point there,
  // otherwise find the next valid sibling starting from our parent, which we already hold the lock
  // for.
  const bool removed_left = removed == &children_list_.front();
  if (removed_left && sibling) {
    TreeWalkCursor::MoveToSibling(removed->cur_cursor_list_, removed, &sibling.locked());
  } else {
    TreeWalkCursor::MoveToSiblingOfParent(removed->cur_cursor_list_, removed, this);
  }
  // Moving the cursors should have implicitly cleared any root references since cursors can never
  // be positioned outside their subtree.
  DEBUG_ASSERT(removed->root_cursor_list_.is_empty());

  if (!is_hidden() || children_list_len_ > 2) {
    DropChildLocked(removed);
    // Things should be consistent after dropping the child.
    VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
    VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
    return;
  }

  // Hidden vmos have 0, 2 or more children. If we had more we would have already returned, and we
  // cannot be here with 0 children, therefore we must have 2, including the one we are removing.
  DEBUG_ASSERT(children_list_len_ == 2);

  // Merge any cursors into the remaining child.
  TreeWalkCursor::MergeToChild(cur_cursor_list_, root_cursor_list_, this, &sibling.locked());

  DropChildLocked(removed);
  MergeContentWithChildLocked();

  DEBUG_ASSERT(sibling.get() == &children_list_.front());

  // The child which removed itself and led to the invocation should have a reference
  // to us, in addition to child.parent_ which we are about to clear.
  DEBUG_ASSERT(ref_count_debug() >= 2);

  // We can have a priority count of at most 1, and only if the remaining child is the one
  // contributing to it.
  DEBUG_ASSERT(high_priority_count_ == 0 ||
               (high_priority_count_ == 1 && sibling.locked().high_priority_count_ > 0));
  // Similarly if we have a priority count, and we have a parent, then our parent must have a
  // non-zero count.
  LockedPtr locked_parent;
  if (parent_) {
    locked_parent = LockedPtr(parent_.get());
  }
  if (locked_parent) {
    DEBUG_ASSERT(high_priority_count_ == 0 || locked_parent.locked().high_priority_count_ != 0);
  }
  // If our child has a non-zero count, then it is propagating a +1 count to us, and we in turn are
  // propagating a +1 count to our parent. In the final arrangement after ReplaceChildLocked then
  // the +1 count child was giving to us needs to go to parent, but as we were already giving a +1
  // count to parent, everything is correct.
  // Although the final hierarchy has correct counts, there is still an assertion in our destructor
  // that our count is zero, so subtract of any count that we might have.
  ChangeSingleHighPriorityCountLocked(-high_priority_count_);

  // Drop the child from our list, but don't recurse back into this function. Then
  // remove ourselves from the clone tree and dead transition ourselves.
  DropChildLocked(&sibling.locked());
  if (locked_parent) {
    locked_parent.locked().ReplaceChildLocked(this, &sibling.locked());
  }
  sibling.locked().parent_ = ktl::move(parent_);
  // We just removed our parent, and so we have no parent and no sibling. Performing this dead
  // transition here ensures that we are not in an alive state, despite being detached from the
  // rest of the tree.
  fbl::RefPtr<VmCowPages> deferred = DeadTransitionLocked(LockedPtr(), LockedPtr());
  ASSERT(!deferred);

  // Things should be consistent after dropping one child and merging with the other.
  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_VALIDATION_ASSERT(sibling.locked().DebugValidateHierarchyLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(sibling.locked().DebugValidateVmoPageBorrowingLocked());
}

void VmCowPages::MergeContentWithChildLocked() {
  canary_.Assert();

  DEBUG_ASSERT(is_hidden());
  // There's no technical reason why this merging code cannot be run if there is a page source,
  // however a bi-directional clone will never have a page source and so in case there are any
  // consequence that have no been considered, ensure we are not in this case.
  DEBUG_ASSERT(!is_source_preserving_page_content());
  DEBUG_ASSERT(children_list_len_ == 1);

  VmCowPages& child = children_list_.front();
  AssertHeld(child.lock_ref());
  // We don't check the hierarchy because it is inconsistent at this point.
  // It will be made consistent by the caller and checked then.
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(child.DebugValidateVmoPageBorrowingLocked());

  const uint64_t merge_start_offset = child.parent_offset_;
  const uint64_t merge_end_offset = child.parent_offset_ + child.parent_limit_;
  VmCompression* compression = Pmm::Node().GetPageCompression();

  __UNINITIALIZED BatchPQUpdateBacklink page_backlink_updater(&child);
  page_list_.MergeRangeOntoAndClear(
      [&](VmPageOrMarker* src, VmPageOrMarker* dst, uint64_t off) __ALWAYS_INLINE {
        // Never overwrite any actual content in the destination.
        if (dst->IsPageOrRef()) {
          return;
        }
        // If using parent content markers then any marker we are moving from src can become an
        // empty slot in the destination. We already know that dst does not have any page or ref so
        // clearing dst is guaranteed to not delete content.
        if (src->IsMarker() && child.node_has_parent_content_markers()) {
          DEBUG_ASSERT(dst->IsEmpty() || dst->IsParentContent());
          *dst = VmPageOrMarker::Empty();
          return;
        }

        // Either moving some content that the child was referring to in the parent from the parent
        // into the child, or both parent and child ended up with a marker, in which case the move
        // is a safe no-op.
        DEBUG_ASSERT(dst->IsEmpty() || dst->IsParentContent() ||
                     (dst->IsMarker() && src->IsMarker()));
        if (src->IsReference()) {
          // A regular reference we can move, a temporary reference we need to turn back into its
          // page so we can move it. To determine if we have a temporary reference we can just
          // attempt to move it, and if it was a temporary reference we will get a page returned.
          if (auto maybe_page = MaybeDecompressReference(compression, src->Reference())) {
            // For simplicity, since this is a very uncommon edge case, just update the page in
            // place in this page list, then move it as a regular page.
            AssertHeld(lock_ref());
            SetNotPinnedLocked(*maybe_page, off);
            VmPageOrMarker::ReferenceValue ref = src->SwapReferenceForPage(*maybe_page);
            ASSERT(compression->IsTempReference(ref));
          }
        }
        // Not an else-if to intentionally perform this if the previous block turned a reference
        // into a page.
        if (src->IsPage()) {
          page_backlink_updater.Push(src->Page(), off);
        }
        *dst = ktl::move(*src);
      },
      child.page_list_, merge_start_offset, merge_end_offset);

  page_backlink_updater.Flush();

  // MergeRangeOntoAndClear clears out the page_list_ for us.
  DEBUG_ASSERT(page_list_.IsEmpty());

  // Adjust the child's offset and limit so it will still see the correct range after it replaces
  // this node. The limit must be adjusted before the offset.
  child.parent_limit_ = ClampedLimit(child.parent_offset_, child.parent_limit_, parent_limit_);
  child.parent_offset_ = CheckedAdd(parent_offset_, child.parent_offset_);

  // The child's last visible offset into this node's parent must be no larger than this node's last
  // visible offset, unless the child can't see anything in this node's parent - in which case its
  // limit will be 0.
  DEBUG_ASSERT(child.parent_limit_ == 0 ||
               (parent_offset_ + parent_limit_ >= child.parent_offset_ + child.parent_limit_));

  // We don't check the hierarchy because it is inconsistent at this point.
  // It will be made consistent by the caller and checked then.
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(child.DebugValidateVmoPageBorrowingLocked());
}

void VmCowPages::DumpLocked(uint depth, bool verbose) const {
  canary_.Assert();

  size_t page_count = 0;
  size_t compressed_count = 0;
  page_list_.ForEveryPage([&page_count, &compressed_count](const auto* p, uint64_t) {
    if (p->IsPage()) {
      page_count++;
    } else if (p->IsReference()) {
      compressed_count++;
    }
    return ZX_ERR_NEXT;
  });

  const char* node_type = "";
  if (is_hidden()) {
    node_type = "(hidden) ";
  }

  for (uint i = 0; i < depth; ++i) {
    printf("  ");
  }
  printf("cow_pages %p %ssize %#" PRIx64 " offset %#" PRIx64 " limit %#" PRIx64
         " content pages %zu compressed pages %zu ref %d parent %p\n",
         this, node_type, size_, parent_offset_, parent_limit_, page_count, compressed_count,
         ref_count_debug(), parent_.get());

  if (page_source_) {
    for (uint i = 0; i < depth + 1; ++i) {
      printf("  ");
    }
    printf("page_source preserves content %d\n", is_source_preserving_page_content());
    page_source_->Dump(depth + 1, UINT32_MAX);
  }

  if (verbose) {
    auto f = [depth](const auto* p, uint64_t offset) {
      for (uint i = 0; i < depth + 1; ++i) {
        printf("  ");
      }
      if (p->IsMarker()) {
        printf("offset %#" PRIx64 " zero page marker\n", offset);
      } else if (p->IsPage()) {
        vm_page_t* page = p->Page();
        printf("offset %#" PRIx64 " page %p paddr %#" PRIxPTR " share %" PRIu32 "(%c)\n", offset,
               page, page->paddr(), page->object.share_count, page->object.always_need ? 'A' : '.');
      } else if (p->IsReference()) {
        const uint64_t cookie = p->Reference().value();
        printf("offset %#" PRIx64 " reference %#" PRIx64 " share %" PRIu32 "\n", offset, cookie,
               Pmm::Node().GetPageCompression()->GetMetadata(p->Reference()));
      } else if (p->IsIntervalStart()) {
        printf("offset %#" PRIx64 " page interval start\n", offset);
      } else if (p->IsIntervalEnd()) {
        printf("offset %#" PRIx64 " page interval end\n", offset);
      } else if (p->IsIntervalSlot()) {
        printf("offset %#" PRIx64 " single page interval slot\n", offset);
      }
      return ZX_ERR_NEXT;
    };
    page_list_.ForEveryPage(f);
  }
}

uint32_t VmCowPages::DebugLookupDepthLocked() const {
  canary_.Assert();

  // Count the number of parents we need to traverse to find the root, and call this our lookup
  // depth.
  uint32_t depth = 0;
  LockedPtr ptr;
  while (VmCowPages* parent = ptr.locked_or(this).parent_.get()) {
    depth++;
    ptr = LockedPtr(parent);
  }
  return depth;
}

VmCowPages::AttributionCounts VmCowPages::GetAttributedMemoryInRangeLocked(VmCowRange range) const {
  canary_.Assert();

  // Due to the need to manipulate fields in AttributionCounts that only exist based on the #define
  // we cannot use the normal if constexpr guard and instead need a preprocessor guard.
  DEBUG_ASSERT(!is_hidden());

  VmCompression* compression = Pmm::Node().GetPageCompression();

  // Accumulate bytes for all pages and references this node has ownership over.
  AttributionCounts counts;
  zx_status_t status = ForEveryOwnedHierarchyPageInRangeLocked(
      [&](const VmPageOrMarker* p, const VmCowPages* owner, uint64_t this_offset,
          uint64_t owner_offset) {
        auto do_attribution = [&](auto get_share_count, auto& bytes, auto& private_bytes,
                                  auto& scaled_bytes) {
          // The short-circuit condition of (owner == this) greatly improves performance by removing
          // the need to dereference 'random' vm_page_ts/references in the common case, greatly
          // reducing memory stalls. For this reason the get_share_count is a callback, and not a
          // value.
          const uint32_t share_count = (owner == this) ? 0 : get_share_count();
          if (share_count == 0) {
            bytes += PAGE_SIZE;
            private_bytes += PAGE_SIZE;
            scaled_bytes += PAGE_SIZE;
          } else {
            // An unshared (i.e. private) page has a share count of 0, add 1 to get the number of
            // owners and scale the full page by this.
            const vm::FractionalBytes scaled_contribution =
                vm::FractionalBytes(PAGE_SIZE, share_count + 1);
            bytes += PAGE_SIZE;
            scaled_bytes += scaled_contribution;
          }
        };
        if (p->IsPage()) {
          do_attribution([&]() { return p->Page()->object.share_count; }, counts.uncompressed_bytes,
                         counts.private_uncompressed_bytes, counts.scaled_uncompressed_bytes);
        } else if (p->IsReference()) {
          do_attribution([&]() { return compression->GetMetadata(p->Reference()); },
                         counts.compressed_bytes, counts.private_compressed_bytes,
                         counts.scaled_compressed_bytes);
        }
        return ZX_ERR_NEXT;
      },
      range.offset, range.len, LockedPtr());
  DEBUG_ASSERT(status == ZX_OK);

  return counts;
}

VmPageOrMarker VmCowPages::AddPageTransaction::Complete(VmPageOrMarker p) {
  VmPageOrMarker ret = slot_.SwapContent(ktl::move(p));
  slot_ = VmPageOrMarkerRef();
  return ret;
}

void VmCowPages::AddPageTransaction::Cancel(VmPageList& pl) {
  DEBUG_ASSERT(slot_);
  if (slot_->IsEmpty()) {
    pl.ReturnEmptySlot(offset_);
  }
  slot_ = VmPageOrMarkerRef();
}

zx::result<VmCowPages::AddPageTransaction> VmCowPages::BeginAddPageWithSlotLocked(
    uint64_t offset, VmPageOrMarkerRef slot, CanOverwriteContent overwrite) {
  canary_.Assert();
  zx_status_t status = CheckOverwriteConditionsLocked(offset, slot, overwrite);
  if (unlikely(status != ZX_OK)) {
    return zx::error(status);
  }
  // Do additinoal checks. The IsOffsetInZeroInterval check is expensive, but the assumption is that
  // this method is not used when is_source_preserving_page_content is true, so the assertion should
  // short circuit.
  DEBUG_ASSERT(!is_source_preserving_page_content() || !slot->IsEmpty() ||
               !page_list_.IsOffsetInZeroInterval(offset));
  return zx::ok(AddPageTransaction(slot, offset, overwrite));
}

zx::result<VmCowPages::AddPageTransaction> VmCowPages::BeginAddPageLocked(
    uint64_t offset, CanOverwriteContent overwrite) {
  canary_.Assert();
  auto interval_handling = VmPageList::IntervalHandling::NoIntervals;
  // If we're backed by a page source that preserves content (user pager), we cannot directly update
  // empty slots in the page list. An empty slot might lie in a sparse zero interval, which would
  // require splitting the interval around the required offset before it can be manipulated.
  if (is_source_preserving_page_content()) {
    // We can overwrite zero intervals if we're allowed to overwrite zeros (or non-zeros).
    interval_handling = overwrite != CanOverwriteContent::None
                            ? VmPageList::IntervalHandling::SplitInterval
                            : VmPageList::IntervalHandling::CheckForInterval;
  }
  auto [slot, is_in_interval] = page_list_.LookupOrAllocate(offset, interval_handling);
  if (is_in_interval) {
    // We should not have found an interval if we were not expecting any.
    DEBUG_ASSERT(interval_handling != VmPageList::IntervalHandling::NoIntervals);
    // Return error if the offset lies in an interval but we cannot overwrite intervals.
    if (interval_handling != VmPageList::IntervalHandling::SplitInterval) {
      // The lookup should not have returned a slot for us to manipulate if it was in an interval
      // that cannot be overwritten, even if that slot was already populated (by an interval
      // sentinel).
      DEBUG_ASSERT(!slot);
      return zx::error(ZX_ERR_ALREADY_EXISTS);
    }
    // If offset was in an interval, we should have an interval slot to overwrite at this point.
    DEBUG_ASSERT(slot && slot->IsIntervalSlot());
  }

  if (unlikely(!slot)) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  zx_status_t status = CheckOverwriteConditionsLocked(offset, VmPageOrMarkerRef(slot), overwrite);
  if (unlikely(status != ZX_OK)) {
    if (slot->IsEmpty()) {
      page_list_.ReturnEmptySlot(offset);
    }
    return zx::error(status);
  }

  return zx::ok(AddPageTransaction(VmPageOrMarkerRef(slot), offset, overwrite));
}

zx_status_t VmCowPages::CheckOverwriteConditionsLocked(uint64_t offset, VmPageOrMarkerRef slot,
                                                       CanOverwriteContent overwrite) {
  // Pages can be added as part of Init, but not once we transition to dead.
  DEBUG_ASSERT(life_cycle_ != LifeCycle::Dead);

  if (offset >= size_) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // We cannot overwrite any kind of content.
  if (overwrite == CanOverwriteContent::None) {
    // An anonymous VMO starts off with all its content set to zero, i.e. at no point can it have
    // absence of content.
    if (!page_source_) {
      return ZX_ERR_ALREADY_EXISTS;
    }
    // This VMO is backed by a page source, so empty slots represent absence of content. Fail if the
    // slot is not empty.
    if (!slot->IsEmpty()) {
      return ZX_ERR_ALREADY_EXISTS;
    }
  }

  // We're only permitted to overwrite zero content. This has different meanings based on the
  // whether the VMO is anonymous or is backed by a pager.
  //
  //  * For anonymous VMOs, the initial content for the entire VMO is implicitly all zeroes at the
  //  time of creation. So both zero page markers and empty slots represent zero content. Therefore
  //  the only content type that cannot be overwritten in this case is an actual page.
  //
  //  * For pager backed VMOs, content is either explicitly supplied by the user pager, or
  //  implicitly supplied as zeros by the kernel. Zero content is represented by either zero page
  //  markers (supplied by the user pager), or by sparse zero intervals (supplied by the kernel).
  //  Therefore the only content type that cannot be overwritten in this case as well is an actual
  //  page.
  if (overwrite == CanOverwriteContent::Zero && slot->IsPageOrRef()) {
    // If we have a page source, the page source should be able to validate the page.
    // Note that having a page source implies that any content must be an actual page and so
    // although we return an error for any kind of content, the debug check only gets run for page
    // sources where it will be a real page.
    DEBUG_ASSERT(!page_source_ || page_source_->DebugIsPageOk(slot->Page(), offset));
    return ZX_ERR_ALREADY_EXISTS;
  }
  // If the old entry and actual content then we should be permitted to overwrite any kind of
  // content (zero or non-zero).
  DEBUG_ASSERT(overwrite == CanOverwriteContent::NonZero || !slot->IsPageOrRef());
  return ZX_OK;
}

VmPageOrMarker VmCowPages::CompleteAddPageLocked(AddPageTransaction& transaction,
                                                 VmPageOrMarker&& p, DeferredOps* deferred) {
  if (p.IsPage()) {
    LTRACEF("vmo %p, offset %#" PRIx64 ", page %p (%#" PRIxPTR ")\n", this, transaction.offset(),
            p.Page(), p.Page()->paddr());
  } else if (p.IsReference()) {
    [[maybe_unused]] const uint64_t cookie = p.Reference().value();
    LTRACEF("vmo %p, offset %#" PRIx64 ", reference %#" PRIx64 "\n", this, transaction.offset(),
            cookie);
  } else {
    DEBUG_ASSERT(p.IsMarker());
    LTRACEF("vmo %p, offset %#" PRIx64 ", marker\n", this, transaction.offset());
  }

  // If the new page is an actual page and we have a page source, the page source should be able to
  // validate the page.
  // Note that having a page source implies that any content must be an actual page and so
  // although we return an error for any kind of content, the debug check only gets run for page
  // sources where it will be a real page.
  DEBUG_ASSERT(!p.IsPageOrRef() || !page_source_ ||
               page_source_->DebugIsPageOk(p.Page(), transaction.offset()));

  // Markers should never be placed in a node that uses parent content markers, since doing so is
  // completely redundant and any attempt to do so represents a logic bug somewhere.
  DEBUG_ASSERT(!p.IsMarker() || !node_has_parent_content_markers());

  // If this is actually a real page, we need to place it into the appropriate queue.
  if (p.IsPage()) {
    vm_page_t* low_level_page = p.Page();
    DEBUG_ASSERT(low_level_page->state() == vm_page_state::OBJECT);
    DEBUG_ASSERT(low_level_page->object.pin_count == 0);
    SetNotPinnedLocked(low_level_page, transaction.offset());
  }
  VmPageOrMarker old = transaction.Complete(ktl::move(p));

  if (deferred) {
    // If the old entry is a reference then we know that there can be no mappings to it, since a
    // reference cannot be mapped in, and we can skip the range update.
    if (!old.IsReference()) {
      if (old.IsEmpty() && is_source_preserving_page_content()) {
        // An empty slot where the page source is preserving content cannot have any mappings,
        // either in self or the children, since the content is unknown (i.e. not the zero page),
        // and so we do not need to perform any range change update.
        // However, as we are modifying the contents we still must synchronize with any other
        // modification to this hierarchy, which we know is true because a non-null |deferred| was
        // passed in.
      } else {
        // other mappings may have covered this offset into the vmo, so unmap those ranges
        const RangeChangeOp op = transaction.overwrite() == CanOverwriteContent::NonZero
                                     ? RangeChangeOp::Unmap
                                     : RangeChangeOp::UnmapZeroPage;
        RangeChangeUpdateLocked(VmCowRange(transaction.offset(), PAGE_SIZE), op, deferred);
      }
    }
  }

  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
  VMO_VALIDATION_ASSERT(DebugValidateZeroIntervalsLocked());
  return old;
}

void VmCowPages::CancelAddPageLocked(AddPageTransaction& transaction) {
  transaction.Cancel(page_list_);
}

zx::result<VmPageOrMarker> VmCowPages::AddPageLocked(uint64_t offset, VmPageOrMarker&& p,
                                                     CanOverwriteContent overwrite,
                                                     DeferredOps* deferred) {
  __UNINITIALIZED auto result = BeginAddPageLocked(offset, overwrite);
  if (unlikely(result.is_error())) {
    if (p.IsPage()) {
      FreePage(p.ReleasePage());
    } else if (p.IsReference()) {
      FreeReference(p.ReleaseReference());
    }
    return result.take_error();
  }
  return zx::ok(CompleteAddPageLocked(*result, ktl::move(p), deferred));
}

zx_status_t VmCowPages::AddNewPageLocked(uint64_t offset, vm_page_t* page,
                                         CanOverwriteContent overwrite,
                                         VmPageOrMarker* released_page, bool zero,
                                         DeferredOps* deferred) {
  canary_.Assert();

  __UNINITIALIZED auto result = BeginAddPageLocked(offset, overwrite);
  if (result.is_error()) {
    return result.status_value();
  }
  VmPageOrMarker old = CompleteAddNewPageLocked(*result, page, zero, deferred);
  if (released_page) {
    *released_page = ktl::move(old);
  } else {
    DEBUG_ASSERT(!old.IsPageOrRef());
  }
  return ZX_OK;
}

VmPageOrMarker VmCowPages::CompleteAddNewPageLocked(AddPageTransaction& transaction,
                                                    vm_page_t* page, bool zero,
                                                    DeferredOps* deferred) {
  DEBUG_ASSERT(IS_PAGE_ROUNDED(transaction.offset()));

  InitializeVmPage(page);
  if (zero) {
    ZeroPage(page);
  }

  // Pages being added to pager backed VMOs should have a valid dirty_state before being added to
  // the page list, so that they can be inserted in the correct page queue. New pages start off
  // clean.
  if (is_source_preserving_page_content()) {
    // Only zero pages can be added as new pages to pager backed VMOs.
    DEBUG_ASSERT(zero || IsZeroPage(page));
    UpdateDirtyStateLocked(page, transaction.offset(), DirtyState::Clean, /*is_pending_add=*/true);
  }
  return CompleteAddPageLocked(transaction, VmPageOrMarker::Page(page), deferred);
}

zx_status_t VmCowPages::AddNewPagesLocked(uint64_t start_offset, list_node_t* pages,
                                          CanOverwriteContent overwrite, bool zero,
                                          DeferredOps* deferred) {
  ASSERT(overwrite != CanOverwriteContent::NonZero);
  canary_.Assert();

  DEBUG_ASSERT(IS_PAGE_ROUNDED(start_offset));

  uint64_t offset = start_offset;
  while (vm_page_t* p = list_remove_head_type(pages, vm_page_t, queue_node)) {
    // Defer the range change update by passing false as we will do it in bulk at the end if needed.
    zx_status_t status = AddNewPageLocked(offset, p, overwrite, nullptr, zero, nullptr);
    if (status != ZX_OK) {
      // Put the page back on the list so that someone owns it and it'll get free'd.
      list_add_head(pages, &p->queue_node);
      // Remove any pages we already placed.
      if (offset > start_offset) {
        __UNINITIALIZED ScopedPageFreedList freed_list;
        __UNINITIALIZED BatchPQRemove page_remover(freed_list);

        page_list_.RemovePages(page_remover.RemovePagesCallback(), start_offset, offset);
        page_remover.Flush();
        freed_list.FreePages(this);
      }

      // Free all the pages back as we had ownership of them.
      FreePages(pages);
      return status;
    }
    offset += PAGE_SIZE;
  }

  if (deferred) {
    // other mappings may have covered this offset into the vmo, so unmap those ranges
    RangeChangeUpdateLocked(VmCowRange(start_offset, offset - start_offset), RangeChangeOp::Unmap,
                            deferred);
  }

  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
  return ZX_OK;
}

zx_status_t VmCowPages::CloneCowPageLocked(uint64_t offset, list_node_t* alloc_list,
                                           VmCowPages* page_owner, vm_page_t* page,
                                           uint64_t owner_offset, DeferredOps& deferred,
                                           AnonymousPageRequest* page_request,
                                           vm_page_t** out_page) {
  DEBUG_ASSERT(page != vm_get_zero_page());
  DEBUG_ASSERT(parent_);
  DEBUG_ASSERT(page_request);
  // We only clone pages from hidden to visible nodes.
  DEBUG_ASSERT(page_owner->is_hidden());
  DEBUG_ASSERT(!is_hidden());
  // We don't want to handle intervals here. They should only be present when this node is backed by
  // a user pager, and such nodes don't have parents so cannot be the target of a forked page.
  DEBUG_ASSERT(!is_source_preserving_page_content());

  // Ensure this node is ready to accept a newly-allocated page. If a subsequent step fails (such as
  // allocating the page itself), cancelling the `page_transaction` will handle any rollback logic.
  //
  // By the time this function returns, the transaction will be either completed or canceled.
  __UNINITIALIZED auto page_transaction = BeginAddPageLocked(offset, CanOverwriteContent::Zero);
  auto cancel_transaction = fit::defer([this, out_page, &page_transaction] {
    AssertHeld(lock_ref());

    if (!page_transaction.is_error()) {
      CancelAddPageLocked(*page_transaction);
    }
    *out_page = nullptr;  // Ensure the `out_page` is initialized if we fail at any point.
  });
  if (page_transaction.is_error()) {
    return page_transaction.status_value();
  }

  // If the page is shared we must fork it, otherwise we can migrate it.
  if (page->object.share_count > 0) {
    // Create a fork of the page. This may fail due to inability to allocate a new page.
    // The page is not writable so there is no need to unmap or protect it before reading it for the
    // fork.
    vm_page_t* forked_page = nullptr;
    zx_status_t status = AllocateCopyPage(page->paddr(), alloc_list, page_request, &forked_page);
    if (unlikely(status != ZX_OK)) {
      return status;
    }

    // The page is now shared one less time.
    page->object.share_count--;

    *out_page = forked_page;
  } else {
    // Remove the page from the owner.
    VmPageOrMarker removed = page_owner->page_list_.RemoveContent(owner_offset);
    vm_page* removed_page = removed.ReleasePage();
    DEBUG_ASSERT(removed_page == page);
    // TODO: This could be optimized to a ChangeObjectOffset instead of doing a Remove here and an
    // insert in CompleteAddPageLocked.
    pmm_page_queues()->Remove(removed_page);

    *out_page = removed_page;
  }

  // Now that we can no longer fail to insert the new page into this node, complete the add page
  // transaction.
  //
  // If the new page is different from the original page, then we must remove the original page
  // from any mappings that reference this node or its descendants.
  const bool do_range_update = (*out_page != page);
  [[maybe_unused]] VmPageOrMarker prev_content = CompleteAddPageLocked(
      *page_transaction, VmPageOrMarker::Page(*out_page), do_range_update ? &deferred : nullptr);
  // We should not have been trying to fork at this offset if something already existed.
  DEBUG_ASSERT(prev_content.IsEmpty() || prev_content.IsParentContent());
  // Transaction completed successfully, so it should no longer be cancelled.
  cancel_transaction.cancel();

  return ZX_OK;
}

void VmCowPages::DecrementCowContentShareCount(VmPageOrMarkerRef content, uint64_t offset,
                                               ScopedPageFreedList& list,
                                               VmCompression* compression) {
  // Only hidden nodes have content with a non-zero share count.
  DEBUG_ASSERT(is_hidden());

  // Release the reference we held to the forked page.
  if (content->IsPage()) {
    vm_page_t* page = content->Page();
    if (page->object.share_count > 0) {
      // The page is now shared one less time.
      page->object.share_count--;
    } else {
      // Remove the page from the owner.
      VmPageOrMarker removed = page_list_.RemoveContent(offset);
      vm_page* removed_page = removed.ReleasePage();
      DEBUG_ASSERT(removed_page == page);
      Pmm::Node().GetPageQueues()->Remove(removed_page);
      DEBUG_ASSERT(!page->is_loaned());

      list_add_tail(list.List(), &page->queue_node);
    }
  } else {
    DEBUG_ASSERT(content->IsReference());
    uint32_t prev = compression->GetMetadata(content->Reference());
    if (prev > 0) {
      compression->SetMetadata(content->Reference(), prev - 1);
    } else {
      VmPageOrMarker removed = page_list_.RemoveContent(offset);
      compression->Free(removed.ReleaseReference());
    }
  }
}

zx_status_t VmCowPages::CloneCowContentAsZeroLocked(uint64_t offset, ScopedPageFreedList& list,
                                                    VmCowPages* content_owner,
                                                    VmPageOrMarkerRef owner_content,
                                                    uint64_t owner_offset) {
  DEBUG_ASSERT(parent_);
  // We only clone pages from hidden to visible nodes.
  DEBUG_ASSERT(content_owner->is_hidden());
  DEBUG_ASSERT(!is_hidden());
  // We don't want to handle intervals here. They should only be present when this node is backed by
  // a user pager, and such nodes don't have parents so cannot be the target of a forked page.
  DEBUG_ASSERT(!is_source_preserving_page_content());

  if (owner_content->IsMarker()) {
    // Markers do not have ref counts so nothing else to do, this will already see this as zero.
    return ZX_OK;
  }
  // Only other valid items should be pages or references.
  DEBUG_ASSERT(owner_content->IsPageOrRef());
  // Performing a cow zero of a parent content marker would require clearing a slot in |this| page
  // list, which is a problem for our caller who might be iterating that some page list. As such
  // this method may not be used if there might be parent content markers.
  DEBUG_ASSERT(!node_has_parent_content_markers());

  // Go ahead and insert the new zero marker into the target. We don't have anything to rollback
  // if this fails so we can just bail immediately.
  //
  // We expect the caller to update any mappings as it can more efficiently do this in bulk.
  zx::result<VmPageOrMarker> prev_content =
      AddPageLocked(offset, VmPageOrMarker::Marker(), CanOverwriteContent::Zero, nullptr);
  if (prev_content.is_error()) {
    return prev_content.status_value();
  }
  DEBUG_ASSERT(prev_content->IsEmpty());
  content_owner->DecrementCowContentShareCount(owner_content, owner_offset, list,
                                               Pmm::Node().GetPageCompression());

  return ZX_OK;
}

void VmCowPages::ReleaseOwnedPagesRangeLocked(uint64_t offset, uint64_t len,
                                              const LockedPtr& parent,
                                              ScopedPageFreedList& freed_list) {
  DEBUG_ASSERT(!is_hidden());
  DEBUG_ASSERT(offset <= size_);
  DEBUG_ASSERT(offset + len <= size_);

  __UNINITIALIZED BatchPQRemove page_remover(freed_list);

  // If we know that the only pages in this range that need to be freed are from our own page list,
  // and we no longer need to consider our parent, then just remove them.
  if (!is_parent_hidden_locked() || offset >= parent_limit_) {
    if (offset == 0 && len == size_) {
      page_list_.RemoveAllContent(
          [&page_remover](VmPageOrMarker&& p) { page_remover.PushContent(&p); });
    } else {
      page_list_.RemovePages(page_remover.RemovePagesCallback(), offset, offset + len);
    }
    page_remover.Flush();
    // Potentially trim the parent limit to reflect the range that has been freed.
    if (offset + len >= parent_limit_) {
      parent_limit_ = ktl::min(parent_limit_, offset);
    }
    return;
  }

  VmCompression* compression = Pmm::Node().GetPageCompression();

  // Decrement the share count on all pages, both directly owned by us and shared via our parents,
  // that this node can see, and free any pages with a zero ref count.
  zx_status_t status = RemoveOwnedHierarchyPagesInRangeLocked(
      [&](VmPageOrMarker* p, const VmCowPages* owner, uint64_t this_offset, uint64_t owner_offset) {
        // Explicitly handle this case separately since although we would naturally find these to
        // have a share_count of 0 and free them, we would always like to free any markers, however
        // we can only free markers that are precisely in 'this' since markers have no refcount.
        if (this == owner) {
          page_remover.PushContent(p);
          return ZX_ERR_NEXT;
        }

        if (p->IsPage()) {
          vm_page_t* page = p->Page();
          if (page->object.share_count == 0) {
            page_remover.PushContent(p);
          } else {
            page->object.share_count--;
          }
        } else if (p->IsReference()) {
          const uint32_t share_count = compression->GetMetadata(p->Reference());
          if (share_count == 0) {
            page_remover.PushContent(p);
          } else {
            compression->SetMetadata(p->Reference(), share_count - 1);
          }
        }
        return ZX_ERR_NEXT;
      },
      offset, len, parent);
  DEBUG_ASSERT(status == ZX_OK);

  if (node_has_parent_content_markers()) {
    // Any parent content markers for the pages that we removed the reference counts for need to be
    // separately removed.
    page_list_.RemovePages(
        [&](VmPageOrMarker* slot, uint64_t offset) {
          DEBUG_ASSERT(slot->IsParentContent());
          *slot = VmPageOrMarker::Empty();
          return ZX_ERR_NEXT;
        },
        offset, offset + len);
  }

  // This node can no longer see into its parent in the range we just released.
  DEBUG_ASSERT(offset < parent_limit_);
  if (offset + len >= parent_limit_) {
    parent_limit_ = offset;
  }

  page_remover.Flush();
}

void VmCowPages::FindPageContentLocked(uint64_t offset, uint64_t max_owner_length,
                                       PageLookup* out) {
  const uint64_t this_offset = offset;

  // Search up the clone chain for any committed pages. cur_offset is the offset
  // into cur we care about. The loop terminates either when that offset contains
  // a committed page or when that offset can't reach into the parent.
  LockedPtr cur;
  while (offset < cur.locked_or(this).parent_limit_) {
    VmCowPages* parent = cur.locked_or(this).parent_.get();
    DEBUG_ASSERT(parent);

    __UNINITIALIZED VMPLCursor cursor =
        cur.locked_or(this).page_list_.LookupNearestMutableCursor(offset);
    VmPageOrMarkerRef p = cursor.current();
    const bool cursor_correct_offset =
        p && cursor.offset(cur.locked_or(this).page_list_.GetSkew()) == offset;
    // If this slot has any actual content, then can immediately return it.
    if (cursor_correct_offset && !p->IsEmpty() && !p->IsParentContent()) {
      *out = {cursor, ktl::move(cur), offset, max_owner_length + this_offset};
      return;
    }
    // If using parent content markers then unless there is a marker we can skip walking up, as we
    // know there is no content above us.
    if (cur.locked_or(this).node_has_parent_content_markers() &&
        (!cursor_correct_offset || !p->IsParentContent())) {
      *out = {VMPLCursor(), ktl::move(cur), offset, max_owner_length + this_offset};
      return;
    }

    // Need to walk up, see if we need to trim the owner length.
    if (max_owner_length > PAGE_SIZE) {
      // First trim to the parent limit.
      max_owner_length = ktl::min(max_owner_length, cur.locked_or(this).parent_limit_ - offset);
      if (max_owner_length > PAGE_SIZE) {
        // There are three cases to consider for determining the range of the parent that we can
        // actually see. The cases are considered in order, with each case also assuming the
        // negation of the condition of all cases above it.
        //  1. Leaf node using parent content markers - Here the current cursor must be valid and be
        //     a ParentContent marker, otherwise we would have already returned with content and
        //     would not be walking up. In this case the visible length is the number of contiguous
        //     parentContent markers.
        //  2. The cursor is valid - We know that the current offset does not have content, but
        //     there is some content later on, and see we must find its offset to determine if it
        //     limits the visible range or not.
        //  3. The cursor is invalid - There is no content from here till the end of the page list,
        //     in which case the visible length extends to the parent limit (i.e. what was just
        //     calculated in max_owner_length), and we know there is no content to look for to trim
        //     this length.
        if (cur.locked_or(this).node_has_parent_content_markers()) {
          uint64_t new_owner_length = 0;
          cursor.ForEveryContiguous([&new_owner_length, max_owner_length](VmPageOrMarkerRef p) {
            if (p->IsParentContent() && new_owner_length < max_owner_length) {
              new_owner_length += PAGE_SIZE;
              return ZX_ERR_NEXT;
            }
            return ZX_ERR_STOP;
          });
          // The first slot in the cursor was parent content, so should always have incremented at
          // least once.
          DEBUG_ASSERT(new_owner_length > 0);
          max_owner_length = ktl::min(new_owner_length, max_owner_length);
        } else if (p) {
          cur.locked_or(this).page_list_.ForEveryPageInCursorRange(
              [&offset, &max_owner_length](const VmPageOrMarker* slot, uint64_t slot_offset) {
                DEBUG_ASSERT(!slot->IsEmpty() && slot_offset >= offset);
                const uint64_t new_owner_length = slot_offset - offset;
                DEBUG_ASSERT(new_owner_length > 0 && new_owner_length <= max_owner_length);
                max_owner_length = new_owner_length;
                return ZX_ERR_STOP;
              },
              cursor, offset + max_owner_length);
        }
      }
    }

    offset += cur.locked_or(this).parent_offset_;
    cur = LockedPtr(parent);
  }
  *out = {cur.locked_or(this).page_list_.LookupMutableCursor(offset), ktl::move(cur), offset,
          max_owner_length + this_offset};
}

void VmCowPages::FindInitialPageContentLocked(uint64_t offset, PageLookup* out) {
  if (parent_ && offset < parent_limit_) {
    LockedPtr parent = LockedPtr(parent_.get());
    parent.locked().FindPageContentLocked(offset + parent_offset_, PAGE_SIZE, out);
    if (!out->owner) {
      out->owner = ktl::move(parent);
    }
  } else {
    *out = {VMPLCursor(), LockedPtr(), offset, offset + PAGE_SIZE};
  }
}

void VmCowPages::UpdateDirtyStateLocked(vm_page_t* page, uint64_t offset, DirtyState dirty_state,
                                        bool is_pending_add) {
  ASSERT(page);
  ASSERT(is_source_preserving_page_content());

  // If the page is not pending being added to the page list, it should have valid object info.
  DEBUG_ASSERT(is_pending_add || page->object.get_object() == this);
  DEBUG_ASSERT(is_pending_add || page->object.get_page_offset() == offset);

  // If the page is Dirty or AwaitingClean, it should not be loaned.
  DEBUG_ASSERT(!(is_page_dirty(page) || is_page_awaiting_clean(page)) || !page->is_loaned());

  // Perform state-specific checks. We will finally update the state below.
  bool update_page_queues = false;
  switch (dirty_state) {
    case DirtyState::Clean:
      // If the page is not in the process of being added, we can only see a transition to Clean
      // from AwaitingClean.
      ASSERT(is_pending_add || is_page_awaiting_clean(page));

      // If we are expecting a pending Add[New]PageLocked, we can defer updating the page queue.
      if (!is_pending_add) {
        update_page_queues = true;
      }
      break;
    case DirtyState::Dirty:
      // If the page is not in the process of being added, we can only see a transition to Dirty
      // from Clean or AwaitingClean.
      ASSERT(is_pending_add || (is_page_clean(page) || is_page_awaiting_clean(page)));

      // A loaned page cannot be marked Dirty as loaned pages are reclaimed by eviction; Dirty pages
      // cannot be evicted.
      DEBUG_ASSERT(!page->is_loaned());

      // If we are expecting a pending Add[New]PageLocked, we can defer updating the page queue.
      if (!is_pending_add) {
        update_page_queues = true;
      }
      break;
    case DirtyState::AwaitingClean:
      // A newly added page cannot start off as AwaitingClean.
      ASSERT(!is_pending_add);
      // A pinned page will be kept Dirty as long as it is pinned.
      //
      // Note that there isn't a similar constraint when setting the Clean state as it is possible
      // to pin a page for read after it has been marked AwaitingClean. Since it is a pinned read it
      // does not need to dirty the page. So when the writeback is done it can transition from
      // AwaitingClean -> Clean with a non-zero pin count.
      //
      // It is also possible for us to observe an intermediate pin count for a write-pin that has
      // not fully completed yet, as we will only attempt to dirty pages after pinning them. So it
      // is possible for a thread to be waiting on a DIRTY request on a pinned page, while a racing
      // writeback transitions the page from AwaitingClean -> Clean with a non-zero pin count.
      ASSERT(page->object.pin_count == 0);
      // We can only transition to AwaitingClean from Dirty.
      ASSERT(is_page_dirty(page));
      // A loaned page cannot be marked AwaitingClean as loaned pages are reclaimed by eviction;
      // AwaitingClean pages cannot be evicted.
      DEBUG_ASSERT(!page->is_loaned());
      // No page queue update. Leave the page in the Dirty queue for now as it is not clean yet;
      // it will be moved out on WritebackEnd.
      DEBUG_ASSERT(pmm_page_queues()->DebugPageIsPagerBackedDirty(page));
      break;
    default:
      ASSERT(false);
  }
  page->object.dirty_state = static_cast<uint8_t>(dirty_state) & VM_PAGE_OBJECT_DIRTY_STATES_MASK;
  if (update_page_queues && page->object.pin_count == 0) {
    // Move the page to the appropriate page queue, checking for global state such as high priority
    // count etc.
    //
    // If Clean:
    // Move to evictable pager backed queue to start tracking age information.
    //
    // If Dirty:
    // Move the page to the Dirty queue, which does not track page age. While the page is in the
    // Dirty queue, age information is not required (yet). It will be required when the page
    // becomes Clean (and hence evictable) again, at which point it will get moved to the MRU
    // pager backed queue and will age as normal.
    // TODO(rashaeqbal): We might want age tracking for the Dirty queue in the future when the
    // kernel generates writeback pager requests.
    MoveToNotPinnedLocked(page, offset);
  }
}

zx_status_t VmCowPages::PrepareForWriteLocked(VmCowRange range, LazyPageRequest* page_request,
                                              uint64_t* dirty_len_out) {
  DEBUG_ASSERT(range.is_page_aligned());
  DEBUG_ASSERT(range.IsBoundedBy(size_));

  DEBUG_ASSERT(page_source_);
  DEBUG_ASSERT(is_source_preserving_page_content());

  uint64_t dirty_len = 0;
  const uint64_t start_offset = range.offset;
  const uint64_t end_offset = range.end();

  // If the VMO does not require us to trap dirty transitions, simply mark the pages dirty, and move
  // them to the dirty page queue. Do this only for the first consecutive run of committed pages
  // within the range starting at offset. Any absent pages will need to be provided by the page
  // source, which might fail and terminate the lookup early. Any zero page markers and zero
  // intervals might need to be forked, which can fail too. Only mark those pages dirty that the
  // lookup is guaranteed to return successfully.
  if (!page_source_->ShouldTrapDirtyTransitions()) {
    zx_status_t status = page_list_.ForEveryPageAndGapInRange(
        [this, &dirty_len, start_offset](const VmPageOrMarker* p, uint64_t off) {
          // TODO(johngro): remove this explicit unused-capture warning suppression
          // when https://bugs.llvm.org/show_bug.cgi?id=35450 gets fixed.
          (void)start_offset;  // used only in DEBUG_ASSERT
          if (p->IsMarker() || p->IsIntervalZero()) {
            // Found a marker or zero interval. End the traversal.
            return ZX_ERR_STOP;
          }
          // VMOs with a page source will never have compressed references, so this should be a
          // real page.
          DEBUG_ASSERT(p->IsPage());
          vm_page_t* page = p->Page();
          DEBUG_ASSERT(is_page_dirty_tracked(page));
          DEBUG_ASSERT(page->object.get_object() == this);
          DEBUG_ASSERT(page->object.get_page_offset() == off);

          // End the traversal if we encounter a loaned page. We reclaim loaned pages by evicting
          // them, and dirty pages cannot be evicted.
          if (page->is_loaned()) {
            // If this is a loaned page, it should be clean.
            DEBUG_ASSERT(is_page_clean(page));
            return ZX_ERR_STOP;
          }
          DEBUG_ASSERT(!page->is_loaned());

          // Mark the page dirty.
          if (!is_page_dirty(page)) {
            AssertHeld(lock_ref());
            UpdateDirtyStateLocked(page, off, DirtyState::Dirty);
          }
          // The page was either already dirty, or we just marked it dirty. Proceed to the next one.
          DEBUG_ASSERT(start_offset + dirty_len == off);
          dirty_len += PAGE_SIZE;
          return ZX_ERR_NEXT;
        },
        [](uint64_t start, uint64_t end) {
          // We found a gap. End the traversal.
          return ZX_ERR_STOP;
        },
        start_offset, end_offset);
    // We don't expect a failure from the traversal.
    DEBUG_ASSERT(status == ZX_OK);

    *dirty_len_out = dirty_len;
    VMO_VALIDATION_ASSERT(DebugValidateZeroIntervalsLocked());
    return ZX_OK;
  }

  // Otherwise, generate a DIRTY page request for pages in the range which need to transition to
  // Dirty. Pages that qualify are:
  //  - Any contiguous run of non-Dirty pages (committed pages as well as zero page markers).
  //  For the purpose of generating DIRTY requests, both Clean and AwaitingClean pages are
  //  considered equivalent. This is because pages that are in AwaitingClean will need another
  //  acknowledgment from the user pager before they can be made Dirty (the filesystem might need to
  //  reserve additional space for them etc.).
  //  - Any zero intervals are implicit zero pages, i.e. the kernel supplies zero pages when they
  //  are accessed. Since these pages are not supplied by the user pager via zx_pager_supply_pages,
  //  we will need to wait on a DIRTY request before the sparse range can be replaced by an actual
  //  page for writing (the filesystem might need to reserve additional space).
  uint64_t pages_to_dirty_len = 0;

  // Helper lambda used in the page list traversal below. Try to add pages in the range
  // [dirty_pages_start, dirty_pages_end) to the run of dirty pages being tracked. Return codes are
  // the same as those used by VmPageList::ForEveryPageAndGapInRange to continue or terminate
  // traversal.
  auto accumulate_dirty_pages = [&pages_to_dirty_len, &dirty_len, start_offset](
                                    uint64_t dirty_pages_start,
                                    uint64_t dirty_pages_end) -> zx_status_t {
    // Bail if we were tracking a non-zero run of pages to be dirtied as we cannot extend
    // pages_to_dirty_len anymore.
    if (pages_to_dirty_len > 0) {
      return ZX_ERR_STOP;
    }
    // Append the page to the dirty range being tracked if it immediately follows it.
    if (start_offset + dirty_len == dirty_pages_start) {
      dirty_len += (dirty_pages_end - dirty_pages_start);
      return ZX_ERR_NEXT;
    }
    // Otherwise we cannot accumulate any more contiguous dirty pages.
    return ZX_ERR_STOP;
  };

  // Helper lambda used in the page list traversal below. Try to add pages in the range
  // [to_dirty_start, to_dirty_end) to the run of to-be-dirtied pages being tracked. Return codes
  // are the same as those used by VmPageList::ForEveryPageAndGapInRange to continue or terminate
  // traversal.
  auto accumulate_pages_to_dirty = [&pages_to_dirty_len, &dirty_len, start_offset](
                                       uint64_t to_dirty_start,
                                       uint64_t to_dirty_end) -> zx_status_t {
    // Bail if we were already accumulating a non-zero run of Dirty pages.
    if (dirty_len > 0) {
      return ZX_ERR_STOP;
    }
    // Append the pages to the range being tracked if they immediately follow it.
    if (start_offset + pages_to_dirty_len == to_dirty_start) {
      pages_to_dirty_len += (to_dirty_end - to_dirty_start);
      return ZX_ERR_NEXT;
    }
    // Otherwise we cannot accumulate any more contiguous to-dirty pages.
    return ZX_ERR_STOP;
  };

  // This tracks the beginning of an interval that falls in the specified range. Since we might
  // start partway inside an interval, this is initialized to start_offset so that we only consider
  // the portion of the interval inside the range. If we did not start inside an interval, we will
  // end up reinitializing this when we do find an interval start, before this value is used, so it
  // is safe to initialize to start_offset in all cases.
  uint64_t interval_start_off = start_offset;
  // This tracks whether we saw an interval start sentinel in the traversal, but have not yet
  // encountered a matching interval end sentinel. Should we end the traversal partway in an
  // interval, we will need to handle the portion of the interval between the interval start and the
  // end of the specified range.
  bool unmatched_interval_start = false;
  bool found_page_or_gap = false;
  zx_status_t status = page_list_.ForEveryPageAndGapInRange(
      [&accumulate_dirty_pages, &accumulate_pages_to_dirty, &interval_start_off,
       &unmatched_interval_start, &found_page_or_gap, this](const VmPageOrMarker* p, uint64_t off) {
        found_page_or_gap = true;
        if (p->IsPage()) {
          vm_page_t* page = p->Page();
          DEBUG_ASSERT(is_page_dirty_tracked(page));
          // VMOs that trap dirty transitions should not have loaned pages.
          DEBUG_ASSERT(!page->is_loaned());
          // Page is already dirty. Try to add it to the dirty run.
          if (is_page_dirty(page)) {
            return accumulate_dirty_pages(off, off + PAGE_SIZE);
          }
          // If the page is clean, mark it accessed to grant it some protection from eviction
          // until the pager has a chance to respond to the DIRTY request.
          if (is_page_clean(page)) {
            AssertHeld(lock_ref());
            pmm_page_queues()->MarkAccessed(page);
          }
        } else if (p->IsIntervalZero()) {
          if (p->IsIntervalStart() || p->IsIntervalSlot()) {
            unmatched_interval_start = true;
            interval_start_off = off;
          }
          if (p->IsIntervalEnd() || p->IsIntervalSlot()) {
            unmatched_interval_start = false;
            // We need to commit pages if this is an interval, irrespective of the dirty state.
            return accumulate_pages_to_dirty(interval_start_off, off + PAGE_SIZE);
          }
          return ZX_ERR_NEXT;
        }

        // We don't compress pages in pager-backed VMOs.
        DEBUG_ASSERT(!p->IsReference());
        // Parent content markers do not appear in pager-backed hierarchies.
        DEBUG_ASSERT(!p->IsParentContent());
        // This is a either a zero page marker (which represents a clean zero page) or a committed
        // page which is not already Dirty. Try to add it to the range of pages to be dirtied.
        DEBUG_ASSERT(p->IsMarker() || !is_page_dirty(p->Page()));
        return accumulate_pages_to_dirty(off, off + PAGE_SIZE);
      },
      [&found_page_or_gap](uint64_t start, uint64_t end) {
        found_page_or_gap = true;
        // We found a gap. End the traversal.
        return ZX_ERR_STOP;
      },
      start_offset, end_offset);

  // We don't expect an error from the traversal above. If an incompatible contiguous page or
  // a gap is encountered, we will simply terminate early.
  DEBUG_ASSERT(status == ZX_OK);

  // Process the last remaining interval if there is one.
  if (unmatched_interval_start) {
    accumulate_pages_to_dirty(interval_start_off, end_offset);
  }

  // Account for the case where we started and ended in unpopulated slots inside an interval, i.e we
  // did not find either a page or a gap in the traversal. We would not have accumulated any pages
  // in that case.
  if (!found_page_or_gap) {
    DEBUG_ASSERT(page_list_.IsOffsetInZeroInterval(start_offset));
    DEBUG_ASSERT(page_list_.IsOffsetInZeroInterval(end_offset - PAGE_SIZE));
    DEBUG_ASSERT(dirty_len == 0);
    DEBUG_ASSERT(pages_to_dirty_len == 0);
    // The entire range falls in an interval so it needs a DIRTY request.
    pages_to_dirty_len = end_offset - start_offset;
  }

  // We should either have found dirty pages or pages that need to be dirtied, but not both.
  DEBUG_ASSERT(dirty_len == 0 || pages_to_dirty_len == 0);
  // Check that dirty_len and pages_to_dirty_len both specify valid ranges.
  DEBUG_ASSERT(start_offset + dirty_len <= end_offset);
  DEBUG_ASSERT(pages_to_dirty_len == 0 || start_offset + pages_to_dirty_len <= end_offset);

  *dirty_len_out = dirty_len;

  VMO_VALIDATION_ASSERT(DebugValidateZeroIntervalsLocked());

  // No pages need to transition to Dirty.
  if (pages_to_dirty_len == 0) {
    return ZX_OK;
  }

  // Found a contiguous run of pages that need to transition to Dirty. There might be more such
  // pages later in the range, but we will come into this call again for them via another
  // LookupCursor call after the waiting caller is unblocked for this range.

  VmoDebugInfo vmo_debug_info{};
  // We have a page source so this cannot be a hidden node, but the VmObjectPaged could have been
  // destroyed. We could be looking up a page via a lookup in a child (slice) after the parent
  // VmObjectPaged has gone away, so paged_ref_ could be null. Let the page source handle any
  // failures requesting the dirty transition.
  if (paged_ref_) {
    vmo_debug_info.vmo_id = paged_ref_->user_id();
    paged_ref_->get_name(vmo_debug_info.vmo_name, sizeof(vmo_debug_info.vmo_name));
  }
  status = page_source_->RequestDirtyTransition(page_request->get(), start_offset,
                                                pages_to_dirty_len, vmo_debug_info);
  // The page source will never succeed synchronously.
  DEBUG_ASSERT(status != ZX_OK);
  return status;
}

void VmCowPages::LookupCursor::EstablishCursor() {
  // Check if the cursor needs recalculating.
  if (IsCursorValid()) {
    return;
  }

  // Ensure still in the valid range.
  DEBUG_ASSERT(offset_ < end_offset_);

  target_->FindPageContentLocked(offset_, end_offset_ - offset_, &owner_info_);
  owner_cursor_ = owner_info_.cursor.current();
  is_valid_ = true;
}

inline VmCowPages::LookupCursor::RequireResult VmCowPages::LookupCursor::PageAsResultNoIncrement(
    vm_page_t* page, bool in_target) {
  // The page is writable if it's present in the target (non owned pages are never writable) and it
  // does not need a dirty transition. A page doesn't need a dirty transition if the target isn't
  // preserving page contents, or if the page is just already dirty.
  RequireResult result{page,
                       (in_target && (!target_preserving_page_content_ || is_page_dirty(page)))};
  return result;
}

void VmCowPages::LookupCursor::IncrementOffsetAndInvalidateCursor(uint64_t delta) {
  offset_ += delta;
  InvalidateCursor();
}

bool VmCowPages::LookupCursor::CursorIsContentZero() const {
  // Markers are always zero.
  if (CursorIsMarker()) {
    return true;
  }

  if (owner_info_.owner.locked_or(target_).page_source_) {
    // With a page source emptiness implies needing to request content, however we can have zero
    // intervals which do start as zero content.
    return CursorIsInIntervalZero();
  }
  // Without a page source emptiness is filled with zeros and intervals are only permitted if there
  // is a page source.
  // We consider parent content to be empty since a parent content marker can be spurious, and the
  // only time the cursor would actually point to the parent content marker is if there is no
  // content to be found in the parent.
  return CursorIsEmpty() || CursorIsParentContent();
}

bool VmCowPages::LookupCursor::TargetZeroContentSupplyDirty(bool writing) const {
  if (!TargetDirtyTracked()) {
    return false;
  }
  if (writing) {
    return true;
  }
  // Markers start clean
  if (CursorIsMarker()) {
    return false;
  }
  // The only way this offset can have been zero content and reach here, is if we are in an
  // interval. If this slot were empty then, since we are dirty tracked and hence must have a
  // page source, we would not consider this zero.
  DEBUG_ASSERT(CursorIsInIntervalZero());
  // Zero intervals are considered implicitly dirty and allocating them, even for reading, causes
  // them to be supplied as new dirty pages.
  return true;
}

zx::result<VmCowPages::LookupCursor::RequireResult>
VmCowPages::LookupCursor::TargetAllocateCopyPageAsResult(vm_page_t* source, DirtyState dirty_state,
                                                         VmCowPages::DeferredOps& deferred,
                                                         AnonymousPageRequest* page_request) {
  vm_page_t* out_page = nullptr;
  zx_status_t status =
      target_->AllocateCopyPage(source->paddr(), alloc_list_, page_request, &out_page);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  // The forked page was just allocated, and so cannot be a loaned page.
  DEBUG_ASSERT(!out_page->is_loaned());

  // We could be allocating a page to replace a zero page marker in a pager-backed VMO. If so then
  // set its dirty state to what was requested, AddPageLocked below will then insert the page into
  // the appropriate page queue.
  if (target_preserving_page_content_) {
    // The only page we can be forking here is the zero page.
    DEBUG_ASSERT(source == vm_get_zero_page());
    // The object directly owns the page.
    DEBUG_ASSERT(TargetIsOwner());

    target_->UpdateDirtyStateLocked(out_page, offset_, dirty_state,
                                    /*is_pending_add=*/true);
  }

  // For efficiently we would like to use the slot we already have in our cursor if possible,
  // however that can only be done if all of the following hold:
  //  * TargetIsOwner() - If not true then we do not even have a cursor (and hence slot) for where
  //    the insertion is happening.
  //  * owner_pl_cursor_.current() != nullptr - Must be an actual node and slot already allocated,
  //    it is just Empty()
  //  * !is_source_preserving_page_content() - A source preserving page content may have intervals,
  //    which are zeroes that we could be overwriting here, but the slot itself we have found could
  //    be empty and the interval may need splitting. For simplicity we do not attempt to check for
  //    and handle interval splitting, and just skip reusing our slot in this case.
  const bool can_reuse_slot =
      (TargetIsOwner() && owner_info_.cursor.current() &&
       !owner_info_.owner.locked_or(target_).is_source_preserving_page_content());
  __UNINITIALIZED auto page_transaction =
      can_reuse_slot ? target_->BeginAddPageWithSlotLocked(offset_, owner_info_.cursor.current(),
                                                           CanOverwriteContent::Zero)
                     : target_->BeginAddPageLocked(offset_, CanOverwriteContent::Zero);
  if (page_transaction.is_error()) {
    target_->FreePage(out_page);
    return page_transaction.take_error();
  }

  [[maybe_unused]] VmPageOrMarker old =
      target_->CompleteAddPageLocked(*page_transaction, VmPageOrMarker::Page(out_page), &deferred);
  DEBUG_ASSERT(!old.IsPageOrRef());

  // If asked to explicitly mark zero forks, and this is actually fork of the zero page, move to the
  // correct queue. Discardable pages are not considered zero forks as they are always in the
  // reclaimable page queues.
  if (zero_fork_ && source == vm_get_zero_page() && !target_->is_discardable()) {
    pmm_page_queues()->MoveAnonymousToAnonymousZeroFork(out_page);
  }

  // This is the only path where we can allocate a new page without being a clone (clones are
  // always cached). So we check here if we are not fully cached and if so perform a
  // clean/invalidate to flush our zeroes. After doing this we will not touch the page via the
  // physmap and so we can pretend there isn't an aliased mapping.
  // There are three potential states that may exist
  //  * VMO is cached, paged_ref_ might be null, we might have children -> no cache op needed
  //  * VMO is uncached, paged_ref_ is not null, we have no children -> cache op needed
  //  * VMO is uncached, paged_ref_ is null, we have no children -> cache op not needed /
  //                                                                state cannot happen
  // In the uncached case we know we have no children, since it is by definition not valid to
  // have copy-on-write children of uncached pages. The third case cannot happen, but even if it
  // could with no children and no paged_ref_ the pages cannot actually be referenced so any
  // cache operation is pointless.
  // The paged_ref_ could be null if the VmObjectPaged has been destroyed.
  if (target_->paged_ref_) {
    if (paged_backlink_locked(target_)->GetMappingCachePolicyLocked() != ARCH_MMU_FLAG_CACHED) {
      arch_clean_invalidate_cache_range((vaddr_t)paddr_to_physmap(out_page->paddr()), PAGE_SIZE);
    }
  }

  // Need to increment the cursor, but we have also potentially modified the page lists in the
  // process of inserting the page.
  if (TargetIsOwner()) {
    // In the case of TargetIsOwner() we may have to create a node and need to establish a cursor.
    // However, if we already had a node, i.e. the cursor was valid, then it would have had the page
    // inserted into it.
    if (!owner_info_.cursor.current()) {
      IncrementOffsetAndInvalidateCursor(PAGE_SIZE);
    } else {
      // Cursor should have been updated to the new page
      DEBUG_ASSERT(CursorIsPage());
      DEBUG_ASSERT(owner_cursor_->Page() == out_page);
      IncrementCursor();
    }
  } else {
    // If !TargetIsOwner() then the owner's page list will not have been modified, so safe to just
    // increment.
    IncrementCursor();
  }

  // Return the page. We know it's in the target, since we just put it there, but let PageAsResult
  // determine if that means it is actually writable or not.
  return zx::ok(PageAsResultNoIncrement(out_page, true));
}

zx_status_t VmCowPages::LookupCursor::CursorReferenceToPage(AnonymousPageRequest* page_request) {
  DEBUG_ASSERT(CursorIsReference());

  return owner_info_.owner.locked_or(target_).ReplaceReferenceWithPageLocked(
      owner_cursor_, owner_info_.owner_offset, page_request);
}

zx_status_t VmCowPages::LookupCursor::ReadRequest(uint max_request_pages,
                                                  PageRequest* page_request) {
  // The owner must have a page_source_ to be doing a read request.
  DEBUG_ASSERT(owner_info_.owner.locked_or(target_).page_source_);
  // The cursor should be explicitly empty as read requests are only for complete content absence.
  DEBUG_ASSERT(CursorIsEmpty());
  DEBUG_ASSERT(!CursorIsInIntervalZero());
  // The total range requested should not be beyond the cursors valid range.
  DEBUG_ASSERT(offset_ + PAGE_SIZE * max_request_pages <= end_offset_);
  DEBUG_ASSERT(max_request_pages > 0);

  VmoDebugInfo vmo_debug_info{};
  // The page owner has a page source so it cannot be a hidden node, but the VmObjectPaged
  // could have been destroyed. We could be looking up a page via a lookup in a child after
  // the parent VmObjectPaged has gone away, so paged_ref_ could be null. Let the page source
  // handle any failures requesting the pages.
  if (owner_info_.owner.locked_or(target_).paged_ref_) {
    vmo_debug_info.vmo_id = owner_info_.owner.locked_or(target_).paged_ref_->user_id();
    owner_info_.owner.locked_or(target_).paged_ref_->get_name(vmo_debug_info.vmo_name,
                                                              sizeof(vmo_debug_info.vmo_name));
  }

  // Try and batch more pages up to |max_request_pages|.
  uint64_t request_size = static_cast<uint64_t>(max_request_pages) * PAGE_SIZE;
  if (!TargetIsOwner()) {
    DEBUG_ASSERT(owner_info_.visible_end > offset_);
    // Limit the request by the number of pages that are actually visible from the target_ to
    // owner.
    request_size = ktl::min(request_size, owner_info_.visible_end - offset_);
  }
  // Limit |request_size| to the first page visible in the page owner to avoid requesting pages
  // that are already present. If there is one page present in an otherwise long run of absent pages
  // then it might be preferable to have one big page request, but for now only request absent
  // pages.If already requesting a single page then can avoid the page list operation.
  if (request_size > PAGE_SIZE) {
    owner_info_.owner.locked_or(target_).page_list_.ForEveryPageInRange(
        [&](const VmPageOrMarker* p, uint64_t offset) {
          DEBUG_ASSERT(!p->IsParentContent());
          // Content should have been empty initially, so should not find anything at the start
          // offset.
          DEBUG_ASSERT(offset > owner_info_.owner_offset);
          // If this is an interval sentinel, it can only be a start or slot, since we know we
          // started in a true gap outside of an interval.
          DEBUG_ASSERT(!p->IsInterval() || p->IsIntervalSlot() || p->IsIntervalStart());
          const uint64_t new_size = offset - owner_info_.owner_offset;
          // Due to the limited range of the operation, the only way this callback ever fires is if
          // the range is actually getting trimmed.
          DEBUG_ASSERT(new_size < request_size);
          request_size = new_size;
          return ZX_ERR_STOP;
        },
        owner_info_.owner_offset, owner_info_.owner_offset + request_size);
  }
  DEBUG_ASSERT(request_size >= PAGE_SIZE);

  zx_status_t status = owner_info_.owner.locked_or(target_).page_source_->GetPages(
      owner_info_.owner_offset, request_size, page_request, vmo_debug_info);
  // Pager page sources will never synchronously return a page.
  DEBUG_ASSERT(status != ZX_OK);
  return status;
}

zx_status_t VmCowPages::LookupCursor::DirtyRequest(uint max_request_pages,
                                                   LazyPageRequest* page_request) {
  // Dirty requests, unlike read requests, happen directly against the target, and not the owner.
  // This is because to make something dirty you must own it. Simply checking for TargetIsOwner() is
  // insufficient, since the cursor may have been made invalid (clearing the owner) just prior to
  // generating this dirty request, and we do not otherwise need the cursor here.
  // So we also validate that we have no parent, and that we have a page source.
  DEBUG_ASSERT(TargetIsOwner());
  DEBUG_ASSERT(!target_->parent_);
  DEBUG_ASSERT(target_->page_source_);
  DEBUG_ASSERT(max_request_pages > 0);
  DEBUG_ASSERT(offset_ + PAGE_SIZE * max_request_pages <= end_offset_);

  // As we know target_ is the owner there is no need to trim the requested range to any kind of
  // visible range, so just attempt to dirty the entire range.
  uint64_t dirty_len = 0;
  zx_status_t status = target_->PrepareForWriteLocked(
      VmCowRange(offset_, PAGE_SIZE * max_request_pages), page_request, &dirty_len);
  if (status == ZX_OK) {
    // If success is claimed then it must be the case that at least one page was dirtied, allowing
    // us to make progress.
    DEBUG_ASSERT(dirty_len != 0 && dirty_len <= max_request_pages * PAGE_SIZE);
  } else {
    DEBUG_ASSERT(dirty_len == 0);
  }
  return status;
}

vm_page_t* VmCowPages::LookupCursor::MaybePage(bool will_write) {
  EstablishCursor();

  // If the page is immediately usable, i.e. no dirty transitions etc needed, then we can provide
  // it. Otherwise just increment the cursor and return the nullptr.
  vm_page_t* page = CursorIsUsablePage(will_write) ? owner_cursor_->Page() : nullptr;

  if (page && mark_accessed_) {
    pmm_page_queues()->MarkAccessed(page);
  }

  IncrementCursor();

  return page;
}

uint64_t VmCowPages::LookupCursor::SkipMissingPages() {
  EstablishCursor();

  // Check if the cursor is truly empty
  if (!CursorIsEmpty() || CursorIsInIntervalZero()) {
    return 0;
  }

  uint64_t possibly_empty = owner_info_.visible_end - offset_;
  // Limit possibly_empty by the first page visible in the owner which, since our cursor is empty,
  // would also be the root vmo.
  if (possibly_empty > PAGE_SIZE) {
    owner_info_.owner.locked_or(target_).page_list_.ForEveryPageInRange(
        [&](const VmPageOrMarker* p, uint64_t offset) {
          // Content should have been empty initially, so should not find anything at the start
          // offset.
          DEBUG_ASSERT(offset > owner_info_.owner_offset);
          // If this is an interval sentinel, it can only be a start or slot, since we know we
          // started in a true gap outside of an interval.
          DEBUG_ASSERT(!p->IsInterval() || p->IsIntervalSlot() || p->IsIntervalStart());
          const uint64_t new_size = offset - owner_info_.owner_offset;
          // Due to the limited range of the operation, the only way this callback ever fires is if
          // the range is actually getting trimmed.
          DEBUG_ASSERT(new_size < possibly_empty);
          possibly_empty = new_size;
          return ZX_ERR_STOP;
        },
        owner_info_.owner_offset, owner_info_.owner_offset + possibly_empty);
  }
  // The cursor was empty, so we should have ended up with at least one page.
  DEBUG_ASSERT(possibly_empty >= PAGE_SIZE);
  DEBUG_ASSERT(IS_PAGE_ROUNDED(possibly_empty));
  DEBUG_ASSERT(possibly_empty + offset_ <= end_offset_);
  IncrementOffsetAndInvalidateCursor(possibly_empty);
  return possibly_empty / PAGE_SIZE;
}

uint VmCowPages::LookupCursor::IfExistPages(bool will_write, uint max_pages, paddr_t* paddrs) {
  // Ensure that the requested range is valid.
  DEBUG_ASSERT(offset_ + PAGE_SIZE * max_pages <= end_offset_);
  DEBUG_ASSERT(paddrs);

  EstablishCursor();

  // We only return actual pages that are ready to use right now without any dirty transitions or
  // copy-on-write or needing to mark them accessed.
  if (!CursorIsUsablePage(will_write) || mark_accessed_) {
    return 0;
  }

  // Trim max pages to the visible length of the current owner. This only has an effect when
  // target_ is not the owner as otherwise the visible_end is the same as end_offset_ and we already
  // validated that we are within that range.
  if (!TargetIsOwner()) {
    max_pages =
        ktl::min(max_pages, static_cast<uint>((owner_info_.visible_end - offset_) / PAGE_SIZE));
  }
  DEBUG_ASSERT(max_pages > 0);

  // Take up to the max_pages as long as they exist contiguously.
  uint pages = 0;
  owner_info_.cursor.ForEveryContiguous([&](VmPageOrMarkerRef page) {
    if (page->IsPage()) {
      paddrs[pages] = page->PageAsPaddr();
      pages++;
      return pages == max_pages ? ZX_ERR_STOP : ZX_ERR_NEXT;
    }
    return ZX_ERR_STOP;
  });
  // Update the cursor to reflect the number of pages we found and are returning.
  // We could check if cursor is still valid, but it's more efficient to just invalidate it and let
  // any potential next page request recalculate it.
  IncrementOffsetAndInvalidateCursor(pages * PAGE_SIZE);
  return pages;
}

zx::result<VmCowPages::LookupCursor::RequireResult> VmCowPages::LookupCursor::RequireOwnedPage(
    bool will_write, uint max_request_pages, DeferredOps& deferred,
    MultiPageRequest* page_request) {
  DEBUG_ASSERT(page_request);

  // Make sure the cursor is valid.
  EstablishCursor();

  // Convert any references to pages.
  if (CursorIsReference()) {
    // Decompress in place.
    zx_status_t status = CursorReferenceToPage(page_request->GetAnonymous());
    if (status != ZX_OK) {
      return zx::error(status);
    }
  }

  // If page exists in the target, i.e. the owner is the target, then we handle this case separately
  // as it's the only scenario where we might be dirtying an existing committed page.
  if (TargetIsOwner() && CursorIsPage()) {
    // If we're writing to a root VMO backed by a user pager, i.e. a VMO whose page source preserves
    // page contents, we might need to mark pages Dirty so that they can be written back later. This
    // is the only path that can result in a write to such a page; if the page was not present, we
    // would have already blocked on a read request the first time, and ended up here when
    // unblocked, at which point the page would be present.
    if (will_write && target_preserving_page_content_) {
      // If this page was loaned, it should be replaced with a non-loaned page, so that we can make
      // progress with marking pages dirty. PrepareForWriteLocked terminates its page walk when it
      // encounters a loaned page; loaned pages are reclaimed by evicting them and we cannot evict
      // dirty pages.
      if (owner_cursor_->Page()->is_loaned()) {
        vm_page_t* res_page = nullptr;
        DEBUG_ASSERT(is_page_clean(owner_cursor_->Page()));
        zx_status_t status =
            target_->ReplacePageLocked(owner_cursor_->Page(), offset_, /*with_loaned=*/false,
                                       &res_page, deferred, page_request->GetAnonymous());
        if (status != ZX_OK) {
          return zx::error(status);
        }
        // Cursor should remain valid and have been replaced with the page.
        DEBUG_ASSERT(CursorIsPage());
        DEBUG_ASSERT(owner_cursor_->Page() == res_page);
        DEBUG_ASSERT(!owner_cursor_->Page()->is_loaned());
      }
      // If the page is not already dirty, then generate a dirty request. The dirty request code can
      // handle the page already being dirty, this is just a short circuit optimization.
      if (!is_page_dirty(owner_cursor_->Page())) {
        zx_status_t status = DirtyRequest(max_request_pages, page_request->GetLazyDirtyRequest());
        if (status != ZX_OK) {
          if (status == ZX_ERR_SHOULD_WAIT) {
            page_request->MadeDirtyRequest();
          }
          return zx::error(status);
        }
      }
    }
    // Return the page.
    return zx::ok(CursorAsResult());
  }

  // Should there be page, but it not be owned by the target, then we are performing copy on write
  // into the target. As the target cannot have a page source do not need to worry about writes or
  // dirtying.
  if (CursorIsPage()) {
    DEBUG_ASSERT(!TargetIsOwner());
    vm_page_t* res_page = nullptr;
    // Although we are not returning the page, the act of forking counts as an access, and this is
    // an access regardless of whether the final returned page should be considered accessed, so
    // ignore the mark_accessed_ check here.
    pmm_page_queues()->MarkAccessed(owner_cursor_->Page());
    if (!owner_info_.owner.locked_or(target_).is_hidden()) {
      // Directly copying the page from the owner into the target.
      return TargetAllocateCopyPageAsResult(owner_cursor_->Page(), DirtyState::Untracked, deferred,
                                            page_request->GetAnonymous());
    }
    zx_status_t result = target_->CloneCowPageLocked(
        offset_, alloc_list_, &owner_info_.owner.locked_or(target_), owner_cursor_->Page(),
        owner_info_.owner_offset, deferred, page_request->GetAnonymous(), &res_page);
    if (result != ZX_OK) {
      return zx::error(result);
    }
    // Cloning the cow page may have impacted our cursor due to a page being moved so invalidate the
    // cursor to perform a fresh lookup on the next page requested.
    IncrementOffsetAndInvalidateCursor(PAGE_SIZE);
    // This page as just allocated so no need to worry about update access times, can just return.
    return zx::ok(RequireResult{res_page, true});
  }

  // Zero content is the most complicated cases where, even if reading, dirty requests might need to
  // be performed and the resulting committed pages may / may not be dirty.
  if (CursorIsContentZero()) {
    // If the page source is preserving content (is a PagerProxy), and is configured to trap dirty
    // transitions, we first need to generate a DIRTY request *before* the zero page can be forked
    // and marked dirty. If dirty transitions are not trapped, we will fall through to allocate the
    // page and then mark it dirty below.
    //
    // Note that the check for ShouldTrapDirtyTransitions() is an optimization here.
    // PrepareForWriteLocked() would do the right thing depending on ShouldTrapDirtyTransitions(),
    // however we choose to avoid the extra work only to have it be a no-op if dirty transitions
    // should not be trapped.
    const bool target_page_dirty = TargetZeroContentSupplyDirty(will_write);
    if (target_page_dirty && target_->page_source_->ShouldTrapDirtyTransitions()) {
      zx_status_t status = DirtyRequest(max_request_pages, page_request->GetLazyDirtyRequest());
      // Since we know we have a page source that traps, and page sources will never succeed
      // synchronously, our dirty request must have 'failed'.
      DEBUG_ASSERT(status != ZX_OK);
      if (status == ZX_ERR_SHOULD_WAIT) {
        page_request->MadeDirtyRequest();
      }
      return zx::error(status);
    }
    // Allocate the page and mark it dirty or clean as previously determined.
    return TargetAllocateCopyPageAsResult(vm_get_zero_page(),
                                          target_page_dirty ? DirtyState::Dirty : DirtyState::Clean,
                                          deferred, page_request->GetAnonymous());
  }
  DEBUG_ASSERT(CursorIsEmpty());

  // Generate a read request to populate the content in the owner. Even if this is a write, we still
  // populate content first, then perform any dirty transitions / requests.
  return zx::error(ReadRequest(max_request_pages, page_request->GetReadRequest()));
}

zx::result<VmCowPages::LookupCursor::RequireResult> VmCowPages::LookupCursor::RequireReadPage(
    uint max_request_pages, DeferredOps& deferred, MultiPageRequest* page_request) {
  DEBUG_ASSERT(page_request);

  // Make sure the cursor is valid.
  EstablishCursor();

  // If there's a page or reference, return it.
  if (CursorIsPage() || CursorIsReference()) {
    if (CursorIsReference()) {
      zx_status_t status = CursorReferenceToPage(page_request->GetAnonymous());
      if (status != ZX_OK) {
        return zx::error(status);
      }
      DEBUG_ASSERT(CursorIsPage());
    }
    return zx::ok(CursorAsResult());
  }

  // Check for zero page options.
  if (CursorIsContentZero()) {
    IncrementCursor();
    return zx::ok(RequireResult{vm_get_zero_page(), false});
  }

  // No available content, need to fetch it from the page source. ReadRequest performs all the
  // requisite asserts to ensure we are not doing this mistakenly.
  return zx::error(ReadRequest(max_request_pages, page_request->GetReadRequest()));
}

zx::result<VmCowPages::LookupCursor> VmCowPages::GetLookupCursorLocked(VmCowRange range) {
  canary_.Assert();
  DEBUG_ASSERT(!is_hidden());
  DEBUG_ASSERT(!range.is_empty());
  DEBUG_ASSERT(range.is_page_aligned());
  DEBUG_ASSERT(life_cycle_ == LifeCycle::Alive);
  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());

  if (unlikely(range.offset >= size_ || !range.IsBoundedBy(size_))) {
    return zx::error{ZX_ERR_OUT_OF_RANGE};
  }

  if (discardable_tracker_) {
    discardable_tracker_->assert_cow_pages_locked();
    // This vmo was discarded and has not been locked yet after the discard. Do not return any
    // pages.
    if (discardable_tracker_->WasDiscardedLocked()) {
      return zx::error{ZX_ERR_NOT_FOUND};
    }
  }

  return zx::ok(LookupCursor(this, range));
}

zx_status_t VmCowPages::CommitRangeLocked(VmCowRange range, DeferredOps& deferred,
                                          uint64_t* committed_len, MultiPageRequest* page_request) {
  canary_.Assert();
  LTRACEF("offset %#" PRIx64 ", len %#" PRIx64 "\n", range.offset, range.len);

  DEBUG_ASSERT(range.is_page_aligned());
  DEBUG_ASSERT(range.IsBoundedBy(size_));
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());

  // If this vmo has a direct page source, then the source will provide the backing memory. For
  // children that eventually depend on a page source, we skip preallocating memory to avoid
  // potentially overallocating pages if something else touches the vmo while we're blocked on the
  // request. Otherwise we optimize things by preallocating all the pages.
  list_node page_list;
  list_initialize(&page_list);
  if (!root_has_page_source()) {
    // make a pass through the list to find out how many pages we need to allocate
    size_t count = range.len / PAGE_SIZE;
    page_list_.ForEveryPageInRange(
        [&count](const auto* p, auto off) {
          if (p->IsPage()) {
            count--;
          }
          return ZX_ERR_NEXT;
        },
        range.offset, range.end());

    if (count == 0) {
      *committed_len = range.len;
      return ZX_OK;
    }

    zx_status_t status = pmm_alloc_pages(count, pmm_alloc_flags_, &page_list);
    // Ignore ZX_ERR_SHOULD_WAIT since the loop below will fall back to a page by page allocation,
    // allowing us to wait for single pages should we need to.
    if (status != ZX_OK && status != ZX_ERR_SHOULD_WAIT) {
      return status;
    }
  }

  auto list_cleanup = fit::defer([&page_list, this]() {
    if (!list_is_empty(&page_list)) {
      FreePages(&page_list);
    }
  });

  const uint64_t start_offset = range.offset;
  const uint64_t end = range.end();
  __UNINITIALIZED auto cursor = GetLookupCursorLocked(range);
  if (cursor.is_error()) {
    return cursor.error_value();
  }
  AssertHeld(cursor->lock_ref());
  // Commit represents an explicit desire to have pages and should not be deduped back to the zero
  // page.
  cursor->DisableZeroFork();
  cursor->GiveAllocList(&page_list);

  zx_status_t status = ZX_OK;
  uint64_t offset = start_offset;
  while (offset < end) {
    __UNINITIALIZED zx::result<VmCowPages::LookupCursor::RequireResult> result =
        cursor->RequireOwnedPage(false, static_cast<uint>((end - offset) / PAGE_SIZE), deferred,
                                 page_request);

    if (result.is_error()) {
      status = result.error_value();
      break;
    }
    offset += PAGE_SIZE;
  }
  // Record how much we were able to process.
  *committed_len = offset - start_offset;

  // Clear the alloc list from the cursor and let list_cleanup free any remaining pages.
  cursor->ClearAllocList();

  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
  return status;
}

zx_status_t VmCowPages::PinRangeLocked(VmCowRange range) {
  canary_.Assert();
  LTRACEF("offset %#" PRIx64 ", len %#" PRIx64 "\n", range.offset, range.len);

  DEBUG_ASSERT(range.is_page_aligned());
  DEBUG_ASSERT(range.IsBoundedBy(size_));

  ever_pinned_ = true;

  // Tracks our expected page offset when iterating to ensure all pages are present.
  uint64_t next_offset = range.offset;

  // Should any errors occur we need to unpin everything.
  auto pin_cleanup = fit::defer([this, offset = range.offset, &next_offset]() {
    if (next_offset > offset) {
      AssertHeld(*lock());
      UnpinLocked(VmCowRange(offset, next_offset - offset), nullptr);
    }
  });

  zx_status_t status = page_list_.ForEveryPageInRange(
      [this, &next_offset](const VmPageOrMarker* p, uint64_t page_offset) {
        AssertHeld(lock_ref());
        if (page_offset != next_offset || !p->IsPage()) {
          return ZX_ERR_BAD_STATE;
        }
        vm_page_t* page = p->Page();
        DEBUG_ASSERT(page->state() == vm_page_state::OBJECT);
        DEBUG_ASSERT(!page->is_loaned());

        if (page->object.pin_count == VM_PAGE_OBJECT_MAX_PIN_COUNT) {
          return ZX_ERR_UNAVAILABLE;
        }

        page->object.pin_count++;
        if (page->object.pin_count == 1) {
          MoveToPinnedLocked(page, page_offset);
        }

        // Pinning every page in the largest vmo possible as many times as possible can't overflow
        static_assert(VmPageList::MAX_SIZE / PAGE_SIZE < UINT64_MAX / VM_PAGE_OBJECT_MAX_PIN_COUNT);
        next_offset += PAGE_SIZE;
        return ZX_ERR_NEXT;
      },
      range.offset, range.end());

  const uint64_t actual = (next_offset - range.offset) / PAGE_SIZE;
  // Count whatever pages we pinned, in the failure scenario this will get decremented on the unpin.
  pinned_page_count_ += actual;

  if (status == ZX_OK) {
    // If the missing pages were at the end of the range (or the range was empty) then our iteration
    // will have just returned ZX_OK. Perform one final check that we actually pinned the number of
    // pages we expected to.
    const uint64_t expected = range.len / PAGE_SIZE;
    if (actual != expected) {
      status = ZX_ERR_BAD_STATE;
    } else {
      pin_cleanup.cancel();
    }
  }
  return status;
}

zx_status_t VmCowPages::DecommitRange(VmCowRange range) {
  canary_.Assert();

  __UNINITIALIZED DeferredOps deferred(this);
  Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};
  // Validate the size and perform our zero-length hot-path check before we recurse
  // up to our top-level ancestor.  Size bounding needs to take place relative
  // to the child the operation was originally targeted against.
  if (!range.IsBoundedBy(size_)) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // was in range, just zero length
  if (range.is_empty()) {
    return ZX_OK;
  }

  // Currently, we can't decommit if the absence of a page doesn't imply zeroes.
  if (parent_ || is_source_preserving_page_content()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  // VmObjectPaged::DecommitRange() rejects is_contiguous() VMOs (for now).
  DEBUG_ASSERT(can_decommit());

  // Demand offset and length be correctly aligned to not give surprising user semantics.
  if (!range.is_page_aligned()) {
    return ZX_ERR_INVALID_ARGS;
  }

  return UnmapAndFreePagesLocked(range.offset, range.len, deferred).status_value();
}

zx::result<uint64_t> VmCowPages::UnmapAndFreePagesLocked(uint64_t offset, uint64_t len,
                                                         DeferredOps& deferred) {
  canary_.Assert();

  if (AnyPagesPinnedLocked(offset, len)) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  LTRACEF("start offset %#" PRIx64 ", end %#" PRIx64 "\n", offset, offset + len);

  // We've already trimmed the range in DecommitRange().
  DEBUG_ASSERT(InRange(offset, len, size_));

  // Verify page alignment.
  DEBUG_ASSERT(IS_PAGE_ROUNDED(offset));
  DEBUG_ASSERT(IS_PAGE_ROUNDED(len) || (offset + len == size_));

  // DecommitRange() will call this function only on a VMO with no parent.
  DEBUG_ASSERT(!parent_);

  // unmap all of the pages in this range on all the mapping regions
  RangeChangeUpdateLocked(VmCowRange(offset, len), RangeChangeOp::Unmap, &deferred);

  __UNINITIALIZED BatchPQRemove page_remover(deferred.FreedList(this));

  page_list_.RemovePages(page_remover.RemovePagesCallback(), offset, offset + len);
  page_remover.Flush();

  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
  return zx::ok(page_remover.freed_count());
}

bool VmCowPages::PageWouldReadZeroLocked(uint64_t page_offset) {
  canary_.Assert();

  DEBUG_ASSERT(IS_PAGE_ROUNDED(page_offset));
  DEBUG_ASSERT(page_offset < size_);
  const VmPageOrMarker* slot = page_list_.Lookup(page_offset);
  if (node_has_parent_content_markers()) {
    if (slot && slot->IsPageOrRef()) {
      return false;
    }
    if (!slot || !slot->IsParentContent()) {
      return true;
    }
  }

  if (slot && slot->IsMarker()) {
    // This is already considered zero as there's a marker.
    return true;
  }
  if (is_source_preserving_page_content() &&
      ((slot && slot->IsIntervalZero()) || page_list_.IsOffsetInZeroInterval(page_offset))) {
    // Pages in zero intervals are supplied as zero by the kernel.
    return true;
  }
  // If we don't have a page or reference here we need to check our parent.
  if (!slot || !slot->IsPageOrRef()) {
    PageLookup content;
    FindInitialPageContentLocked(page_offset, &content);
    if (!content.cursor.current()) {
      // Parent doesn't have a page either, so would also read as zero, assuming no page source.
      return !is_root_source_user_pager_backed();
    }
  }
  // Content either locally or in our parent, assume it is non-zero and return false.
  return false;
}

zx_status_t VmCowPages::ZeroPagesPreservingContentLocked(uint64_t page_start_base,
                                                         uint64_t page_end_base, bool dirty_track,
                                                         DeferredOps& deferred,
                                                         MultiPageRequest* page_request,
                                                         uint64_t* processed_len_out) {
  // Validate inputs.
  DEBUG_ASSERT(IS_PAGE_ROUNDED(page_start_base) && IS_PAGE_ROUNDED(page_end_base));
  DEBUG_ASSERT(page_end_base <= size_);
  DEBUG_ASSERT(is_source_preserving_page_content());

  // Give us easier names for our range.
  const uint64_t start = page_start_base;
  const uint64_t end = page_end_base;

  if (start == end) {
    return ZX_OK;
  }

  // If we're not asked to dirty track, we will need to drop pages, because if a page is present it
  // is going to be in one of the dirty tracked states (Clean, Dirty, AwaitingClean). So check for
  // any pinned pages first.
  if (!dirty_track && AnyPagesPinnedLocked(start, end - start)) {
    return ZX_ERR_BAD_STATE;
  }

  // Inserting zero intervals can modify the page list such that new nodes are added and deleted.
  // So we cannot safely insert zero intervals while iterating the page list. The pattern we
  // follow here is:
  // 1. Traverse the page list to find a range that can be represented by a zero interval instead.
  // 2. When such a range is found, break out of the traversal, and insert the zero interval.
  // 3. Advance past the zero interval we inserted and resume the traversal from there, until
  // we've covered the entire range.

  // The start offset at which to start the next traversal loop.
  uint64_t next_start_offset = start;
  // Dirty state for zero intervals we insert.
  const VmPageOrMarker::IntervalDirtyState required_state =
      dirty_track ? VmPageOrMarker::IntervalDirtyState::Dirty
                  : VmPageOrMarker::IntervalDirtyState::Untracked;
  do {
    // Track whether we find ourselves in a zero interval.
    bool in_interval = false;
    // The start of the zero interval if we are in one.
    uint64_t interval_start = next_start_offset;
    const uint64_t prev_start_offset = next_start_offset;
    // State tracking information for inserting a new zero interval.
    struct {
      bool add_zero_interval;
      uint64_t start;
      uint64_t end;
      bool replace_page;
      bool overwrite_interval;
    } state = {.add_zero_interval = false};

    zx_status_t status = page_list_.RemovePagesAndIterateGaps(
        [&](VmPageOrMarker* p, uint64_t off) {
          // We cannot have references in pager-backed VMOs.
          DEBUG_ASSERT(!p->IsReference());

          // If this is a page, see if we can remove it and absorb it into a zero interval.
          if (p->IsPage()) {
            AssertHeld(lock_ref());
            if (p->Page()->object.pin_count > 0) {
              DEBUG_ASSERT(dirty_track);
              // Cannot remove this page if it is pinned. Lookup the page and zero it. Looking up
              // ensures that we request dirty transition if needed by the pager.
              LookupCursor cursor(this, VmCowRange(off, PAGE_SIZE));
              AssertHeld(cursor.lock_ref());
              zx::result<LookupCursor::RequireResult> result =
                  cursor.RequireOwnedPage(true, 1, deferred, page_request);
              if (result.is_error()) {
                return result.error_value();
              }
              DEBUG_ASSERT(result->page == p->Page());
              // Zero the page we looked up.
              ZeroPage(result->page->paddr());
              *processed_len_out += PAGE_SIZE;
              next_start_offset = off + PAGE_SIZE;
              return ZX_ERR_NEXT;
            }
            // Break out of the traversal. We can release the page and add a zero interval
            // instead.
            state = {.add_zero_interval = true,
                     .start = off,
                     .end = off,
                     .replace_page = true,
                     .overwrite_interval = false};
            return ZX_ERR_STOP;
          }

          // Otherwise this is a marker or zero interval, in which case we already have zeroes, but
          // we might need to change the dirty state.
          DEBUG_ASSERT(p->IsMarker() || p->IsIntervalZero());
          if (p->IsIntervalStart()) {
            // Track the interval start so we know how much to add to processed_len_out later.
            interval_start = off;
            in_interval = true;
            if (p->GetZeroIntervalDirtyState() != required_state) {
              // If we find the matching end, we will update state.end with the correct offset.
              // Do not terminate the traversal yet.
              state = {.add_zero_interval = true,
                       .start = interval_start,
                       .end = UINT64_MAX,
                       .replace_page = false,
                       .overwrite_interval = true};
            }
          } else if (p->IsIntervalEnd()) {
            if (p->GetZeroIntervalDirtyState() != required_state) {
              state = {.add_zero_interval = true,
                       .start = in_interval ? interval_start : UINT64_MAX,
                       .end = off,
                       .replace_page = false,
                       .overwrite_interval = true};
              return ZX_ERR_STOP;
            }
            // Add the range from interval start to end.
            *processed_len_out += (off + PAGE_SIZE - interval_start);
            in_interval = false;
          } else {
            // This is either a single interval slot or a marker. Terminate the traversal to
            // overwrite with a zero interval if:
            //  - this is an interval slot with a different dirty state, OR
            //  - this is a marker and we're asked to not dirty track, since a marker is a clean
            //  zero page.
            if (p->IsMarker() && !dirty_track) {
              // Release the marker so that it can be replaced by a gap by the traversal loop first,
              // where the new zero interval will then be added.
              *p = VmPageOrMarker::Empty();
            }
            if (p->IsEmpty() ||
                (p->IsIntervalSlot() && p->GetZeroIntervalDirtyState() != required_state)) {
              state = {.add_zero_interval = true,
                       .start = off,
                       .end = off,
                       .replace_page = false,
                       .overwrite_interval = p->IsIntervalSlot()};
              return ZX_ERR_STOP;
            }
            *processed_len_out += PAGE_SIZE;
          }
          next_start_offset = off + PAGE_SIZE;
          return ZX_ERR_NEXT;
        },
        [&](uint64_t gap_start, uint64_t gap_end) {
          AssertHeld(lock_ref());
          // This gap will be replaced with a zero interval. Invalidate any read requests in this
          // range. Since we have just validated that this is a gap in the page list we can directly
          // call OnPagesSupplied, instead of iterating through the gaps using
          // InvalidateReadRequestsLocked
          page_source_->OnPagesSupplied(gap_start, gap_end - gap_start);
          // We have found a new zero interval to insert. Break out of the traversal.
          state = {.add_zero_interval = true,
                   .start = gap_start,
                   .end = gap_end - PAGE_SIZE,
                   .replace_page = false,
                   .overwrite_interval = false};
          return ZX_ERR_STOP;
        },
        next_start_offset, end);
    // Bubble up any errors from LookupCursor.
    if (status != ZX_OK) {
      return status;
    }

    // Add any new zero interval.
    if (state.add_zero_interval) {
      if (state.replace_page) {
        DEBUG_ASSERT(state.start == state.end);
        vm_page_t* page = page_list_.ReplacePageWithZeroInterval(state.start, required_state);
        DEBUG_ASSERT(page->object.pin_count == 0);
        RemovePageLocked(page, deferred);
      } else if (state.overwrite_interval) {
        uint64_t old_start = state.start;
        uint64_t old_end = state.end;
        if (state.start == UINT64_MAX) {
          state.start = next_start_offset;
        }
        if (state.end == UINT64_MAX) {
          state.end = end - PAGE_SIZE;
        }
        status = page_list_.OverwriteZeroInterval(old_start, old_end, state.start, state.end,
                                                  required_state);
      } else {
        status = page_list_.AddZeroInterval(state.start, state.end + PAGE_SIZE, required_state);
      }
      if (status != ZX_OK) {
        DEBUG_ASSERT(status == ZX_ERR_NO_MEMORY);
        return status;
      }
      *processed_len_out += (state.end - state.start + PAGE_SIZE);
      next_start_offset = state.end + PAGE_SIZE;
    } else {
      // Handle the last partial interval. Or the case where we did not advance next_start_offset at
      // all, which can only happen if the range fell entirely inside an interval.
      if (in_interval || next_start_offset == prev_start_offset) {
        // If the range fell entirely inside an interval, verify that it was indeed a zero interval.
        DEBUG_ASSERT(next_start_offset != prev_start_offset ||
                     page_list_.IsOffsetInZeroInterval(next_start_offset));
        // If entirely inside an interval, we have one of two possibilities:
        //  (1) The interval is already in required_state in which case we don't need to do
        //  anything.
        //  (2) The interval is not in required_state. We do not expect this case in practice, so
        //  instead of splitting up a zero interval in the middle just to change its dirty state,
        //  claim that we processed the range.
        *processed_len_out += (end - interval_start);
        next_start_offset = end;
      }
    }
    // Ensure we're making progress.
    DEBUG_ASSERT(next_start_offset > prev_start_offset);
  } while (next_start_offset < end);

  VMO_VALIDATION_ASSERT(DebugValidateZeroIntervalsLocked());
  return ZX_OK;
}

zx_status_t VmCowPages::ZeroPagesLocked(VmCowRange range, bool dirty_track, DeferredOps& deferred,
                                        MultiPageRequest* page_request, uint64_t* zeroed_len_out) {
  canary_.Assert();

  DEBUG_ASSERT(range.IsBoundedBy(size_));
  DEBUG_ASSERT(range.is_page_aligned());
  ASSERT(zeroed_len_out);

  // This function tries to zero pages as optimally as possible for most cases, so we attempt
  // increasingly expensive actions only if certain preconditions do not allow us to perform the
  // cheaper action. Broadly speaking, the sequence of actions that are attempted are as follows.
  //  1) Try to decommit each page if the VMO allows it and doing so doesn't expose  content in the
  //  parent (if any) that shouldn't be visible.
  //  2) Otherwise, if this is a child VMO and there is no committed page yet, allocate a zero page.
  //  3) Otherwise, look up the page, faulting it in if necessary, and zero the page. If the page
  //  source needs to supply or dirty track the page, a page request is initialized and we return
  //  early with ZX_ERR_SHOULD_WAIT. The caller is expected to wait on the page request, and then
  //  retry. On the retry, we should be able to look up the page successfully and zero it.

  // Unmap any page that is touched by this range in any of our, or our childrens, mapping
  // regions. We do this on the assumption we are going to be able to free pages either completely
  // or by turning them into markers and it's more efficient to unmap once in bulk here.
  RangeChangeUpdateLocked(range, RangeChangeOp::Unmap, &deferred);

  // Give us easier names for our range.
  const uint64_t start = range.offset;
  const uint64_t end = range.end();

  // If the VMO is directly backed by a page source that preserves content, it should be the root
  // VMO of the hierarchy.
  DEBUG_ASSERT(!is_source_preserving_page_content() || !parent_);

  // If the page source preserves content, we can perform efficient zeroing by inserting dirty zero
  // intervals. Handle this case separately.
  if (is_source_preserving_page_content()) {
    return ZeroPagesPreservingContentLocked(start, end, dirty_track, deferred, page_request,
                                            zeroed_len_out);
  }
  // dirty_track has no meaning for VMOs without page sources that preserve content, so ignore it
  // for the remainder of the function.

  // Helper lambda to determine if this VMO can see parent contents at offset, or if a length is
  // specified as well in the range [offset, offset + length).
  auto can_see_parent = [this](uint64_t offset, uint64_t length = PAGE_SIZE) TA_REQ(lock()) {
    if (!parent_) {
      return false;
    }
    return offset < parent_limit_ && offset + length <= parent_limit_;
  };

  // This is a lambda as it only makes sense to talk about parent mutability when we have a parent
  // for the offset being considered.
  auto parent_immutable = [can_see_parent, this](uint64_t offset) TA_REQ(lock()) {
    // TODO(johngro): remove this explicit unused-capture warning suppression
    // when https://bugs.llvm.org/show_bug.cgi?id=35450 gets fixed.
    (void)can_see_parent;  // used only in DEBUG_ASSERT
    DEBUG_ASSERT(can_see_parent(offset));
    return parent_->is_hidden();
  };

  // Finding the initial page content is expensive, but we only need to call it under certain
  // circumstances scattered in the code below. The lambda get_initial_page_content() will lazily
  // fetch and cache the details. This avoids us calling it when we don't need to, or calling it
  // more than once.
  struct InitialPageContent {
    bool inited = false;
    LockedPtr page_owner;
    uint64_t owner_offset;
    uint64_t cached_offset;
    VmPageOrMarkerRef page_or_marker;
  } initial_content_;
  auto get_initial_page_content = [&initial_content_, can_see_parent, this](uint64_t offset)
                                      TA_REQ(lock()) -> const InitialPageContent& {
    // TODO(johngro): remove this explicit unused-capture warning suppression
    // when https://bugs.llvm.org/show_bug.cgi?id=35450 gets fixed.
    (void)can_see_parent;  // used only in DEBUG_ASSERT

    // If there is no cached page content or if we're looking up a different offset from the cached
    // one, perform the lookup.
    if (!initial_content_.inited || offset != initial_content_.cached_offset) {
      DEBUG_ASSERT(can_see_parent(offset));
      PageLookup content;
      initial_content_.page_owner.release();
      FindInitialPageContentLocked(offset, &content);
      initial_content_.page_owner = ktl::move(content.owner);
      initial_content_.owner_offset = content.owner_offset;
      initial_content_.page_or_marker = content.cursor.current();
      // We only care about the parent having a 'true' vm_page for content. If the parent has a
      // marker then it's as if the parent has no content since that's a zero page anyway, which is
      // what we are trying to achieve.
      initial_content_.inited = true;
      initial_content_.cached_offset = offset;
    }
    DEBUG_ASSERT(offset == initial_content_.cached_offset);
    return initial_content_;
  };

  // Helper lambda to determine if parent has content at the specified offset.
  auto parent_has_content = [&](uint64_t offset) TA_REQ(lock()) {
    if (node_has_parent_content_markers()) {
      // Unless there is a parent content marker then we know the parent has no content for us.
      const VmPageOrMarker* slot = page_list_.Lookup(offset);
      if (!slot || !slot->IsParentContent()) {
        return false;
      }
    }
    const VmPageOrMarkerRef& page_or_marker = get_initial_page_content(offset).page_or_marker;
    return page_or_marker && page_or_marker->IsPageOrRef();
  };

  // In the ideal case we can zero by making there be an Empty slot in our page list. This is true
  // when we're not specifically avoiding decommit on zero and there is nothing pinned.
  //
  // Note that this lambda is only checking for pre-conditions in *this* VMO which allow us to
  // represent zeros with an empty slot. We will combine this check with additional checks for
  // contents visible through the parent, if applicable.
  auto can_decommit_slot = [this](const VmPageOrMarker* slot, uint64_t offset) TA_REQ(lock()) {
    if (!can_decommit_zero_pages() ||
        (slot && slot->IsPage() && slot->Page()->object.pin_count > 0)) {
      return false;
    }
    DEBUG_ASSERT(!is_source_preserving_page_content());
    return true;
  };

  // Like can_decommit_slot but for a range.
  auto can_decommit_slots_in_range = [this](uint64_t offset, uint64_t length) TA_REQ(lock()) {
    if (!can_decommit_zero_pages() || AnyPagesPinnedLocked(offset, length)) {
      return false;
    }
    DEBUG_ASSERT(!is_source_preserving_page_content());
    return true;
  };

  // Helper lambda to zero the slot at offset either by inserting a marker or by zeroing the actual
  // page as applicable. The return codes match those expected for VmPageList traversal.
  auto zero_slot = [&](VmPageOrMarker* slot, uint64_t offset) TA_REQ(lock()) {
    // Ideally we will use a marker, but we can only do this if we can point to a committed page
    // to justify the allocation of the marker (i.e. we cannot allocate infinite markers with no
    // committed pages). A committed page in this case exists if the parent has any content.
    // Otherwise, we'll need to zero an actual page.
    if (!can_decommit_slot(slot, offset) || !parent_has_content(offset)) {
      // If we're here because of !parent_has_content() and slot doesn't have a page, we can simply
      // allocate a zero page to replace the empty slot. Otherwise, we'll have to look up the page
      // and zero it.
      //
      // We could technically fall through to GetLookupCursorLocked even for an empty slot and let
      // RequirePage allocate a new page and zero it, but we want to avoid having to redundantly
      // zero a newly forked zero page.
      if (!slot && can_see_parent(offset) && !parent_has_content(offset)) {
        // We could only have ended up here if the parent was mutable or if there is a pager-backed
        // root, otherwise we should have been able to treat an empty slot as zero (decommit a
        // committed page) and return early above.
        DEBUG_ASSERT(!parent_immutable(offset) || is_root_source_user_pager_backed());
        // We will try to insert a new zero page below. Note that at this point we know that this is
        // not a contiguous VMO (which cannot have arbitrary zero pages inserted into it). We
        // checked for can_see_parent just now and contiguous VMOs do not support clones. Besides,
        // if the slot was empty we should have moved on when we found the gap in the page list
        // traversal as the contiguous page source zeroes supplied pages by default.
        DEBUG_ASSERT(!is_source_supplying_specific_physical_pages());

        // Allocate a new page, it will be zeroed in the process.
        vm_page_t* p;
        // Do not pass our freed_list here as this takes an |alloc_list| list to allocate from.
        zx_status_t status =
            AllocateCopyPage(vm_get_zero_page_paddr(), nullptr, page_request->GetAnonymous(), &p);
        if (status != ZX_OK) {
          return status;
        }
        auto result =
            AddPageLocked(offset, VmPageOrMarker::Page(p), CanOverwriteContent::Zero, nullptr);
        // Absent bugs, AddPageLocked() can only return ZX_ERR_NO_MEMORY.
        if (result.is_error()) {
          ASSERT(result.status_value() == ZX_ERR_NO_MEMORY);
        }
        DEBUG_ASSERT(!result->IsPageOrRef());
        return ZX_ERR_NEXT;
      }

      // Lookup the page which will potentially fault it in via the page source. Zeroing is
      // equivalent to a VMO write with zeros, so simulate a write fault.
      zx::result<VmCowPages::LookupCursor> cursor =
          GetLookupCursorLocked(VmCowRange(offset, PAGE_SIZE));
      if (cursor.is_error()) {
        return cursor.error_value();
      }
      AssertHeld(cursor->lock_ref());
      auto result = cursor->RequirePage(true, 1, deferred, page_request);
      if (result.is_error()) {
        return result.error_value();
      }
      ZeroPage(result->page->paddr());
      return ZX_ERR_NEXT;
    }

    DEBUG_ASSERT(parent_ && parent_has_content(offset) && (!slot || !slot->IsParentContent()));
    // Validate we can insert our own pages/content.
    DEBUG_ASSERT(!is_source_supplying_specific_physical_pages());

    // We are able to insert a marker, but if our page content is from a hidden owner we need to
    // perform slightly more complex cow forking.
    const InitialPageContent& content = get_initial_page_content(offset);
    if (!slot && content.page_owner.locked_or(this).is_hidden()) {
      zx_status_t result = CloneCowContentAsZeroLocked(
          offset, deferred.FreedList(this), &content.page_owner.locked_or(this),
          content.page_or_marker, content.owner_offset);
      if (result != ZX_OK) {
        return result;
      }
      return ZX_ERR_NEXT;
    }

    // Remove any page that could be hanging around in the slot and replace it with a marker.
    auto result =
        AddPageLocked(offset, VmPageOrMarker::Marker(), CanOverwriteContent::NonZero, nullptr);
    // Absent bugs, AddPageLocked() can only return ZX_ERR_NO_MEMORY.
    if (result.is_error()) {
      ASSERT(result.status_value() == ZX_ERR_NO_MEMORY);
      return result.status_value();
    }
    VmPageOrMarker& released_page = *result;
    // Free the old page.
    if (released_page.IsPage()) {
      vm_page_t* page = released_page.ReleasePage();
      RemovePageLocked(page, deferred);
    } else if (released_page.IsReference()) {
      FreeReference(released_page.ReleaseReference());
    }
    return ZX_ERR_NEXT;
  };

  *zeroed_len_out = 0;
  // Main page list traversal loop to remove any existing pages / markers, zero existing pages, and
  // also insert any new markers / zero pages in gaps as applicable. We use the VmPageList traversal
  // helper here instead of iterating over each offset in the range so we can efficiently skip over
  // gaps if possible.
  zx_status_t status = page_list_.RemovePagesAndIterateGaps(
      [&](VmPageOrMarker* slot, uint64_t offset) {
        AssertHeld(lock_ref());

        // We don't expect intervals in non pager-backed VMOs.
        DEBUG_ASSERT(!slot->IsInterval());

        // Contiguous VMOs cannot have markers.
        DEBUG_ASSERT(!direct_source_supplies_zero_pages() || !slot->IsMarker());

        // First see if we can simply get done with an empty slot in the page list. This VMO should
        // allow decommitting a page at this offset when zeroing. Additionally, one of the following
        // conditions should hold w.r.t. to the parent:
        //  * This offset does not relate to our parent, or we don't have a parent.
        //  * This offset does relate to our parent, but our parent is immutable, currently
        //  zero at this offset and there is no pager-backed root VMO.
        if (can_decommit_slot(slot, offset) &&
            (!can_see_parent(offset) || (parent_immutable(offset) && !parent_has_content(offset) &&
                                         !is_root_source_user_pager_backed()))) {
          if (slot->IsPage()) {
            vm_page_t* page = slot->ReleasePage();
            RemovePageLocked(page, deferred);
          } else if (slot->IsReference()) {
            FreeReference(slot->ReleaseReference());
          } else {
            // If this is a marker, simply make the slot empty.
            *slot = VmPageOrMarker::Empty();
          }
          // We successfully zeroed this offset. Move on to the next offset.
          *zeroed_len_out += PAGE_SIZE;
          return ZX_ERR_NEXT;
        }
        if (slot->IsParentContent()) {
          // If the slot is a parent content marker then we can zero by clearing the slot, but to do
          // so we must also remove our ref count of said content.
          DEBUG_ASSERT(can_see_parent(offset) && parent_has_content(offset) &&
                       !root_has_page_source());
          const InitialPageContent& content = get_initial_page_content(offset);
          content.page_owner.locked_or(this).DecrementCowContentShareCount(
              content.page_or_marker, content.owner_offset, deferred.FreedList(this),
              Pmm::Node().GetPageCompression());
          *slot = VmPageOrMarker::Empty();
          *zeroed_len_out += PAGE_SIZE;
          return ZX_ERR_NEXT;
        }

        // If there's already a marker then we can avoid any second guessing and leave the marker
        // alone.
        if (slot->IsMarker()) {
          *zeroed_len_out += PAGE_SIZE;
          return ZX_ERR_NEXT;
        }

        // The only time we would reach here and *not* have a parent is if we could not decommit a
        // page at this offset when zeroing.
        DEBUG_ASSERT(!can_decommit_slot(slot, offset) || parent_);

        // Now we know that we need to do something active to make this zero, either through a
        // marker or a page.
        zx_status_t status = zero_slot(slot, offset);
        if (status == ZX_ERR_NEXT) {
          // If we were able to successfully zero this slot, move on to the next offset.
          *zeroed_len_out += PAGE_SIZE;
        }
        return status;
      },
      [&](uint64_t gap_start, uint64_t gap_end) {
        AssertHeld(lock_ref());
        if (node_has_parent_content_markers()) {
          // Gaps are already zero when using parent content markers.
          *zeroed_len_out += (gap_end - gap_start);
          return ZX_ERR_NEXT;
        }
        if (direct_source_supplies_zero_pages()) {
          // Already logically zero - don't commit pages to back the zeroes if they're not already
          // committed.  This is important for contiguous VMOs, as we don't use markers for
          // contiguous VMOs, and allocating a page below to hold zeroes would not be asking the
          // page_source_ for the proper physical page. This prevents allocating an arbitrary
          // physical page to back the zeroes.
          *zeroed_len_out += (gap_end - gap_start);
          return ZX_ERR_NEXT;
        }

        // If empty slots imply zeroes, and the gap does not see parent contents, we already have
        // zeroes.
        if (can_decommit_slots_in_range(gap_start, gap_end - gap_start) &&
            !can_see_parent(gap_start, gap_end - gap_start)) {
          *zeroed_len_out += (gap_end - gap_start);
          return ZX_ERR_NEXT;
        }

        // Otherwise fall back to examining each offset in the gap to determine the action to
        // perform.
        for (uint64_t offset = gap_start; offset < gap_end;
             offset += PAGE_SIZE, *zeroed_len_out += PAGE_SIZE) {
          // First see if we can simply get done with an empty slot in the page list. This VMO
          // should allow decommitting a page at this offset when zeroing. Additionally, one of the
          // following conditions should hold w.r.t. to the parent:
          //  * This offset does not relate to our parent, or we don't have a parent.
          //  * This offset does relate to our parent, but our parent is immutable, currently
          //  zero at this offset and there is no pager-backed root VMO.
          if (can_decommit_slot(nullptr, offset) &&
              (!can_see_parent(offset) ||
               (parent_immutable(offset) && !parent_has_content(offset) &&
                !is_root_source_user_pager_backed()))) {
            continue;
          }

          // The only time we would reach here and *not* have a parent is if we could not decommit a
          // page at this offset when zeroing.
          DEBUG_ASSERT(!can_decommit_slot(nullptr, offset) || parent_);

          // Now we know that we need to do something active to make this zero, either through a
          // marker or a page.
          zx_status_t status = zero_slot(nullptr, offset);
          if (status != ZX_ERR_NEXT) {
            return status;
          }
        }

        return ZX_ERR_NEXT;
      },
      start, end);

  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_VALIDATION_ASSERT(DebugValidateZeroIntervalsLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
  return status;
}

void VmCowPages::MoveToPinnedLocked(vm_page_t* page, uint64_t offset) {
  pmm_page_queues()->MoveToWired(page);
}

void VmCowPages::MoveToNotPinnedLocked(vm_page_t* page, uint64_t offset) {
  PageQueues* pq = pmm_page_queues();
  if (is_source_preserving_page_content()) {
    DEBUG_ASSERT(is_page_dirty_tracked(page));
    // We can only move Clean pages to the pager backed queues as they track age information for
    // eviction; only Clean pages can be evicted. Pages in AwaitingClean and Dirty are protected
    // from eviction in the Dirty queue.
    if (is_page_clean(page)) {
      if (high_priority_count_ != 0) {
        // If this VMO is high priority then do not place in the pager backed queue as that is
        // reclaimable, place in the high priority queue instead.
        pq->MoveToHighPriority(page);
      } else {
        pq->MoveToReclaim(page);
      }
    } else {
      DEBUG_ASSERT(!page->is_loaned());
      pq->MoveToPagerBackedDirty(page);
    }
  } else {
    // Place pages from contiguous VMOs in the wired queue, as they are notionally pinned until the
    // owner explicitly releases them.
    if (can_decommit_zero_pages()) {
      if (high_priority_count_ != 0 && !pq->ReclaimIsOnlyPagerBacked()) {
        // If anonymous pages are reclaimable, and this VMO is high priority, then places our pages
        // in the high priority queue instead of the anonymous one to avoid reclamation.
        pq->MoveToHighPriority(page);
      } else if (is_discardable()) {
        pq->MoveToReclaim(page);
      } else {
        // If the VMO is mapped uncached, it cannot be reclaimed. The reclamation code is tolerant
        // to this and will skip the page anyway, but uncached memory is typically used by drivers
        // and tends to back large buffers, so avoid wasted work.
        pq->MoveToAnonymous(page,
                            /*skip_reclaim=*/paged_ref_ &&
                                (paged_backlink_locked(this)->GetMappingCachePolicyLocked() &
                                 ZX_CACHE_POLICY_MASK) != ZX_CACHE_POLICY_CACHED);
      }
    } else {
      pq->MoveToWired(page);
    }
  }
}

void VmCowPages::SetNotPinnedLocked(vm_page_t* page, uint64_t offset) {
  PageQueues* pq = pmm_page_queues();
  if (is_source_preserving_page_content()) {
    DEBUG_ASSERT(is_page_dirty_tracked(page));
    // We can only move Clean pages to the pager backed queues as they track age information for
    // eviction; only Clean pages can be evicted. Pages in AwaitingClean and Dirty are protected
    // from eviction in the Dirty queue.
    if (is_page_clean(page)) {
      if (high_priority_count_ != 0) {
        // If this VMO is high priority then do not place in the pager backed queue as that is
        // reclaimable, place in the high priority queue instead.
        pq->SetHighPriority(page, this, offset);
      } else {
        pq->SetReclaim(page, this, offset);
      }
    } else {
      DEBUG_ASSERT(!page->is_loaned());
      pq->SetPagerBackedDirty(page, this, offset);
    }
  } else {
    // Place pages from contiguous VMOs in the wired queue, as they are notionally pinned until the
    // owner explicitly releases them.
    if (can_decommit_zero_pages()) {
      if (high_priority_count_ != 0 && !pq->ReclaimIsOnlyPagerBacked()) {
        // If anonymous pages are reclaimable, and this VMO is high priority, then places our pages
        // in the high priority queue instead of the anonymous one to avoid reclamation.
        pq->SetHighPriority(page, this, offset);
      } else if (is_discardable()) {
        pq->SetReclaim(page, this, offset);
      } else {
        // If the VMO is mapped uncached, it cannot be reclaimed. The reclamation code is tolerant
        // to this and will skip the page anyway, but uncached memory is typically used by drivers
        // and tends to back large buffers, so avoid wasted work.
        pq->SetAnonymous(page, this, offset,
                         /*skip_reclaim=*/paged_ref_ &&
                             (paged_backlink_locked(this)->GetMappingCachePolicyLocked() &
                              ZX_CACHE_POLICY_MASK) != ZX_CACHE_POLICY_CACHED);
      }
    } else {
      pq->SetWired(page, this, offset);
    }
  }
}

zx_status_t VmCowPages::PromoteRangeForReclamation(VmCowRange range) {
  canary_.Assert();

  // Hints only apply to pager backed VMOs.
  if (!can_root_source_evict()) {
    return ZX_OK;
  }
  // Zero lengths have no work to do.
  if (range.is_empty()) {
    return ZX_OK;
  }

  Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};
  if (!range.IsBoundedBy(size_)) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  uint64_t start_offset = ROUNDDOWN_PAGE_SIZE(range.offset);
  uint64_t end_offset = ROUNDUP_PAGE_SIZE(range.end());

  __UNINITIALIZED zx::result<VmCowPages::LookupCursor> cursor =
      GetLookupCursorLocked(VmCowRange(start_offset, end_offset - start_offset));
  if (cursor.is_error()) {
    return cursor.status_value();
  }
  // Do not consider pages accessed as the goal is reclaim them, not consider them used.
  cursor->DisableMarkAccessed();
  AssertHeld(cursor->lock_ref());
  while (start_offset < end_offset) {
    // Lookup the page if it exists, but do not let it get allocated or say we are writing to it.
    // On success or failure this causes the cursor to go to the next offset.
    vm_page_t* page = cursor->MaybePage(false);
    if (page) {
      // Check to see if the page is owned by the root VMO. Hints only apply to the root, as that is
      // where the page source is.
      // Don't move a pinned page or a dirty page to the DontNeed queue.
      // Note that this does not unset the always_need bit if it has been previously set. The
      // always_need hint is sticky.
      VmCowPages* owner = reinterpret_cast<VmCowPages*>(page->object.get_object());
      DEBUG_ASSERT(owner);
      if (owner->page_source_ && page->object.pin_count == 0 && is_page_clean(page)) {
        pmm_page_queues()->MoveToReclaimDontNeed(page);
        vm_vmo_dont_need.Add(1);
      }
    }
    // Can't really do anything in case an error is encountered while looking up the page. Simply
    // ignore it and move on to the next page. Hints are best effort anyway.
    start_offset += PAGE_SIZE;
  }
  return ZX_OK;
}

zx_status_t VmCowPages::ProtectRangeFromReclamation(VmCowRange range, bool set_always_need,
                                                    bool ignore_errors) {
  canary_.Assert();

  // Hints only apply to pager backed VMOs.
  if (!can_root_source_evict()) {
    return ZX_OK;
  }

  // Validate that the range is completely in range at the start of the operation. Although we
  // tolerate the VMO shrinking during the operation, the range must be valid at the point we
  // started.
  {
    Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};
    if (!range.IsBoundedBy(size_)) {
      return ZX_ERR_OUT_OF_RANGE;
    }
    // Zero lengths have no work to do.
    if (range.is_empty()) {
      return ZX_OK;
    }
  }

  range = range.ExpandTillPageAligned();

  __UNINITIALIZED MultiPageRequest page_request;
  while (!range.is_empty()) {
    // Any loaned page replacement needs to happen outside the main lock acquisition so if we loaned
    // page is found we use these variables to record its information and process it after dropping
    // the lock.
    fbl::RefPtr<VmCowPages> loaned_page_owner;
    uint64_t loaned_page_offset = 0;
    vm_page_t* loaned_page = nullptr;
    zx_status_t status;
    {
      __UNINITIALIZED DeferredOps deferred(this);
      Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};
      // The size might have changed since we dropped the lock. Adjust the range if required.
      if (range.offset >= size_) {
        // No more pages to hint.
        return ZX_OK;
      }
      // Shrink the range if required. Proceed with hinting on the remaining pages in the range;
      // we've already hinted on the preceding pages, so just go on ahead instead of returning an
      // error. The range was valid at the time we started hinting.
      if (!range.IsBoundedBy(size_)) {
        range = range.WithLength(size_ - range.offset);
      }

      __UNINITIALIZED zx::result<VmCowPages::LookupCursor> cursor =
          GetLookupCursorLocked(VmCowRange(range.offset, range.len));
      if (cursor.is_error()) {
        return cursor.status_value();
      }
      AssertHeld(cursor->lock_ref());
      for (; !range.is_empty(); range = range.TrimedFromStart(PAGE_SIZE)) {
        // Lookup the page, this will fault in the page from the parent if necessary, but will not
        // allocate pages directly in this if it is a child.
        auto result = cursor->RequirePage(false, static_cast<uint>(range.len / PAGE_SIZE), deferred,
                                          &page_request);
        status = result.status_value();
        if (status != ZX_OK) {
          break;
        }
        // If we reached here, we successfully found a page at the current offset.
        vm_page_t* page = result->page;

        // The root might have gone away when the lock was dropped while waiting above. Compute the
        // root again and check if we still have a page source backing it before applying the hint.
        if (!can_root_source_evict()) {
          // Hinting is not applicable anymore. No more pages to hint.
          return ZX_OK;
        }

        // Check to see if the page is owned by the root VMO. Hints only apply to the root, as that
        // is where the page source is. There could equivalently be no owner if this is the zero
        // page, which should also be ignored.
        VmCowPages* owner = reinterpret_cast<VmCowPages*>(page->object.get_object());
        if (!owner || !owner->page_source_) {
          // Hinting is not applicable to this page, but it might apply to following ones.
          continue;
        }

        // If the page is loaned, replace it with a non-loaned page. Loaned pages are reclaimed by
        // eviction, and hinted pages should not be evicted.
        if (page->is_loaned()) {
          DEBUG_ASSERT(is_page_clean(page));
          // The lock of |owner| may or may not be held depending on the current state of the
          // LookupCursor, however we do not need the owner lock in order to take a RefPtr. Since we
          // were able to get a reference to the page, the page cannot be removed or changed in
          // owner without informing us, as we might have a mapping to it. Us holding our lock
          // blocks that and prevents it from completing, meaning that owner must still be a live
          // object. The page could already be removed from owner, but we will deal with that race
          // in the ReplacePage step down below.
          loaned_page_owner = fbl::MakeRefPtrUpgradeFromRaw<VmCowPages>(owner, lock());
          loaned_page = page;
          loaned_page_offset = page->object.get_page_offset();
          break;
        }
        if (status != ZX_OK) {
          break;
        }

        DEBUG_ASSERT(!page->is_loaned());
        if (set_always_need) {
          page->object.always_need = 1;
          vm_vmo_always_need.Add(1);
          // Nothing more to do beyond marking the page always_need true. The lookup must have
          // already marked the page accessed, moving it to the head of the first page queue.
        }
      }
    }
    // Check if we exited to swap a loaned page.
    if (loaned_page) {
      vm_page_t* after;
      status = loaned_page_owner->ReplacePage(loaned_page, loaned_page_offset, false, &after,
                                              page_request.GetAnonymous());
      if (status != ZX_ERR_SHOULD_WAIT) {
        // Between finding the loaned page and attempting to replace it the lock was dropped and so
        // ReplacePage could spuriously fail, hence ignore any other failure and go around the loop
        // and retry.
        status = ZX_OK;
      }
    }
    if (status != ZX_OK) {
      if (status == ZX_ERR_SHOULD_WAIT) {
        status = page_request.Wait();

        // If the wait succeeded, cur_offset will now have a backing page, so we need to try the
        // same offset again with a new cursor.
        if (status == ZX_OK) {
          continue;
        }
      }

      // Should only get here if an error was encountered, check if we should ignore or return it.
      DEBUG_ASSERT(status != ZX_OK);
      if (!ignore_errors) {
        return status;
      }

      // Ignore the error, move to the next offset.
      range = range.TrimedFromStart(PAGE_SIZE);
    }
  }
  return ZX_OK;
}

zx_status_t VmCowPages::DecompressInRange(VmCowRange range) {
  canary_.Assert();

  Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};
  if (!range.IsBoundedBy(size_)) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  if (range.is_empty()) {
    return ZX_OK;
  }

  uint64_t cur_offset = ROUNDDOWN_PAGE_SIZE(range.offset);
  uint64_t end_offset = ROUNDUP_PAGE_SIZE(range.end());

  zx_status_t status;
  do {
    __UNINITIALIZED AnonymousPageRequest page_request;
    status = ForEveryOwnedMutableHierarchyPageInRangeLocked(
        [&cur_offset, &page_request](VmPageOrMarkerRef p, VmCowPages* owner, uint64_t this_offset,
                                     uint64_t owner_offset) {
          if (!p->IsReference()) {
            return ZX_ERR_NEXT;
          }
          AssertHeld(owner->lock_ref());
          zx_status_t status = owner->ReplaceReferenceWithPageLocked(VmPageOrMarkerRef(p),
                                                                     owner_offset, &page_request);
          if (status == ZX_OK) {
            cur_offset = this_offset + PAGE_SIZE;
            return ZX_ERR_NEXT;
          }
          return status;
        },
        cur_offset, end_offset - cur_offset, LockedPtr());
    if (status == ZX_OK) {
      return ZX_OK;
    }
    if (status == ZX_ERR_SHOULD_WAIT) {
      guard.CallUnlocked(
          [&page_request, &status]() { status = page_request.Allocate().status_value(); });
    }
  } while (status == ZX_OK);
  return status;
}

int64_t VmCowPages::ChangeSingleHighPriorityCountLocked(int64_t delta) {
  const bool was_zero = high_priority_count_ == 0;
  high_priority_count_ += delta;
  DEBUG_ASSERT(high_priority_count_ >= 0);
  const bool is_zero = high_priority_count_ == 0;
  // Any change to or from zero means we need to add or remove a count from our parent (if we have
  // one) and potentially move pages in the page queues.
  if (is_zero && !was_zero) {
    delta = -1;
  } else if (was_zero && !is_zero) {
    delta = 1;
  } else {
    delta = 0;
  }
  if (delta != 0) {
    // If we moved to or from zero then update every page into the correct page queue for tracking.
    // MoveToNotPinnedLocked will check the high_priority_count_, which has already been updated, so
    // can just call that on every page.
    page_list_.ForEveryPage([this](const VmPageOrMarker* page_or_marker, uint64_t offset) {
      if (page_or_marker->IsPage()) {
        vm_page_t* page = page_or_marker->Page();
        if (page->object.pin_count == 0) {
          AssertHeld(lock_ref());
          MoveToNotPinnedLocked(page, offset);
        }
      }
      return ZX_ERR_NEXT;
    });
  }
  vm_vmo_high_priority.Add(delta);
  return delta;
}

void VmCowPages::ChangeHighPriorityCountLocked(int64_t delta) {
  canary_.Assert();

  LockedPtr cur;
  // Any change to or from zero requires updating a count in the parent, so we need to walk up the
  // parent chain as long as a transition is happening.
  while (delta != 0) {
    delta = cur.locked_or(this).ChangeSingleHighPriorityCountLocked(delta);
    VmCowPages* parent = cur.locked_or(this).parent_.get();
    if (!parent) {
      break;
    }
    cur = LockedPtr(parent);
  }
}

void VmCowPages::UnpinLocked(VmCowRange range, DeferredOps* deferred) {
  canary_.Assert();

  // verify that the range is within the object
  ASSERT(range.IsBoundedBy(size_));
  // forbid zero length unpins as zero length pins return errors.
  ASSERT(!range.is_empty());

  const uint64_t start_page_offset = ROUNDDOWN_PAGE_SIZE(range.offset);
  const uint64_t end_page_offset = ROUNDUP_PAGE_SIZE(range.end());

#if (DEBUG_ASSERT_IMPLEMENTED)
  // For any pages that have their pin count transition to 0, i.e. become unpinned, we want to
  // perform a range change op. For efficiency track contiguous ranges.
  uint64_t completely_unpin_start = 0;
  uint64_t completely_unpin_len = 0;
#endif

  uint64_t unpin_count = 0;
  zx_status_t status = page_list_.ForEveryPageAndGapInRange(
      [&](const auto* page, uint64_t off) {
        AssertHeld(lock_ref());
        // Only real pages can be pinned.
        ASSERT(page->IsPage());

        vm_page_t* p = page->Page();
        ASSERT(p->object.pin_count > 0);
        p->object.pin_count--;
        if (p->object.pin_count == 0) {
          MoveToNotPinnedLocked(p, range.offset);
#if (DEBUG_ASSERT_IMPLEMENTED)
          // Check if the current range can be extended.
          if (completely_unpin_start + completely_unpin_len == off) {
            completely_unpin_len += PAGE_SIZE;
          } else {
            // Complete any existing range and then start again at this offset.
            if (completely_unpin_len > 0 && deferred) {
              const VmCowRange range_update =
                  VmCowRange(completely_unpin_start, completely_unpin_len);
              RangeChangeUpdateLocked(range_update, RangeChangeOp::DebugUnpin, deferred);
            }
            completely_unpin_start = off;
            completely_unpin_len = PAGE_SIZE;
          }
#endif
        }
        ++unpin_count;
        return ZX_ERR_NEXT;
      },
      [](uint64_t gap_start, uint64_t gap_end) { return ZX_ERR_NOT_FOUND; }, start_page_offset,
      end_page_offset);
  ASSERT_MSG(status == ZX_OK, "Tried to unpin an uncommitted page");

  // Possible that we were entirely inside a spare interval without any committed pages, in which
  // case neither the page nor gap callback would have triggered, and the assert above would
  // succeed. This is still an error though and can catch this, and any other mistakes, by ensuring
  // we found and decremented the pin counts from the exact expected number of pages.
  ASSERT(unpin_count == (end_page_offset - start_page_offset) / PAGE_SIZE);

#if (DEBUG_ASSERT_IMPLEMENTED)
  // Check any leftover range.
  if (completely_unpin_len > 0 && deferred) {
    const VmCowRange range_update = VmCowRange(completely_unpin_start, completely_unpin_len);
    RangeChangeUpdateLocked(range_update, RangeChangeOp::DebugUnpin, deferred);
  }
#endif

  bool overflow = sub_overflow(pinned_page_count_, unpin_count, &pinned_page_count_);
  ASSERT(!overflow);

  return;
}

bool VmCowPages::DebugIsRangePinnedLocked(VmCowRange range) {
  canary_.Assert();
  DEBUG_ASSERT(range.is_page_aligned());

  uint64_t pinned_count = 0;
  page_list_.ForEveryPageInRange(
      [&pinned_count](const auto* p, uint64_t off) {
        if (p->IsPage() && p->Page()->object.pin_count > 0) {
          pinned_count++;
          return ZX_ERR_NEXT;
        }
        return ZX_ERR_STOP;
      },
      range.offset, range.end());
  return pinned_count == range.len / PAGE_SIZE;
}

bool VmCowPages::AnyPagesPinnedLocked(uint64_t offset, size_t len) {
  canary_.Assert();
  DEBUG_ASSERT(lock_ref().lock().IsHeld());
  DEBUG_ASSERT(IS_PAGE_ROUNDED(offset));
  DEBUG_ASSERT(IS_PAGE_ROUNDED(len));

  const uint64_t start_page_offset = offset;
  const uint64_t end_page_offset = offset + len;

  if (pinned_page_count_ == 0) {
    return false;
  }

  bool found_pinned = false;
  page_list_.ForEveryPageInRange(
      [&found_pinned, start_page_offset, end_page_offset](const auto* p, uint64_t off) {
        DEBUG_ASSERT(off >= start_page_offset && off < end_page_offset);
        if (p->IsPage() && p->Page()->object.pin_count > 0) {
          found_pinned = true;
          return ZX_ERR_STOP;
        }
        return ZX_ERR_NEXT;
      },
      start_page_offset, end_page_offset);

  return found_pinned;
}

void VmCowPages::InvalidateReadRequestsLocked(uint64_t offset, uint64_t len) {
  DEBUG_ASSERT(IS_PAGE_ROUNDED(offset));
  DEBUG_ASSERT(IS_PAGE_ROUNDED(len));
  DEBUG_ASSERT(InRange(offset, len, size_));

  DEBUG_ASSERT(page_source_);

  const uint64_t start = offset;
  const uint64_t end = offset + len;

  zx_status_t status = page_list_.ForEveryPageAndGapInRange(
      [](const auto* p, uint64_t off) { return ZX_ERR_NEXT; },
      [this](uint64_t gap_start, uint64_t gap_end) {
        page_source_->OnPagesSupplied(gap_start, gap_end - gap_start);
        return ZX_ERR_NEXT;
      },
      start, end);
  DEBUG_ASSERT(status == ZX_OK);
}

void VmCowPages::InvalidateDirtyRequestsLocked(uint64_t offset, uint64_t len) {
  DEBUG_ASSERT(IS_PAGE_ROUNDED(offset));
  DEBUG_ASSERT(IS_PAGE_ROUNDED(len));
  DEBUG_ASSERT(InRange(offset, len, size_));

  DEBUG_ASSERT(is_source_preserving_page_content());
  DEBUG_ASSERT(page_source_->ShouldTrapDirtyTransitions());

  const uint64_t start = offset;
  const uint64_t end = offset + len;

  zx_status_t status = page_list_.ForEveryPageAndContiguousRunInRange(
      [](const VmPageOrMarker* p, uint64_t off) {
        // A marker is a clean zero page and might have an outstanding DIRTY request.
        if (p->IsMarker()) {
          return true;
        }
        // An interval is an uncommitted zero page and might have an outstanding DIRTY request
        // irrespective of dirty state.
        if (p->IsIntervalZero()) {
          return true;
        }
        // Although a reference is implied to be clean, VMO backed by a page source should never
        // have references.
        DEBUG_ASSERT(!p->IsReference());
        // Not parent content in pager-backed VMOs.
        DEBUG_ASSERT(!p->IsParentContent());

        vm_page_t* page = p->Page();
        DEBUG_ASSERT(is_page_dirty_tracked(page));

        // A page that is not Dirty already might have an outstanding DIRTY request.
        if (!is_page_dirty(page)) {
          return true;
        }
        // Otherwise the page should already be Dirty.
        DEBUG_ASSERT(is_page_dirty(page));
        return false;
      },
      [](const VmPageOrMarker* p, uint64_t off) {
        // Nothing to update for the page as we're not actually marking it Dirty.
        return ZX_ERR_NEXT;
      },
      [this](uint64_t start, uint64_t end, bool unused) {
        // Resolve any DIRTY requests in this contiguous range.
        page_source_->OnPagesDirtied(start, end - start);
        return ZX_ERR_NEXT;
      },
      start, end);
  // We don't expect an error from the traversal.
  DEBUG_ASSERT(status == ZX_OK);

  // Now resolve DIRTY requests for any gaps. After request generation, pages could either
  // have been evicted, or zero intervals written back, leading to gaps. So it is possible for gaps
  // to have outstanding DIRTY requests.
  status = page_list_.ForEveryPageAndGapInRange(
      [](const VmPageOrMarker* p, uint64_t off) {
        // Nothing to do for pages. We already handled them above.
        return ZX_ERR_NEXT;
      },
      [this](uint64_t gap_start, uint64_t gap_end) {
        // Resolve any DIRTY requests in this gap.
        page_source_->OnPagesDirtied(gap_start, gap_end - gap_start);
        return ZX_ERR_NEXT;
      },
      start, end);
  // We don't expect an error from the traversal.
  DEBUG_ASSERT(status == ZX_OK);
}

zx_status_t VmCowPages::Resize(uint64_t s) {
  canary_.Assert();

  LTRACEF("vmcp %p, size %" PRIu64 "\n", this, s);

  __UNINITIALIZED DeferredOps deferred(this);
  // In the case where we are shrinking any child limits may need to be updated, but the locking
  // order requires their locks to be acquired without our lock held, and so we do this after
  // dropping the main lock, but before any pages are freed from the deferred ops. See the comment
  // and checks where this is set to true for details on the correctness.
  bool update_child_limits = false;
  {
    Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};

    // make sure everything is aligned before we get started
    DEBUG_ASSERT(IS_PAGE_ROUNDED(size_));
    DEBUG_ASSERT(IS_PAGE_ROUNDED(s));

    // see if we're shrinking or expanding the vmo
    if (s < size_) {
      // shrinking
      const uint64_t start = s;
      const uint64_t end = size_;
      const uint64_t len = end - start;

      // bail if there are any pinned pages in the range we're trimming
      if (AnyPagesPinnedLocked(start, len)) {
        return ZX_ERR_BAD_STATE;
      }

      // unmap all of the pages in this range on all the mapping regions
      RangeChangeUpdateLocked(VmCowRange(start, len), RangeChangeOp::Unmap, &deferred);

      // Resolve any outstanding page requests tracked by the page source that are now
      // out-of-bounds.
      if (page_source_) {
        // Tell the page source that any non-resident pages that are now out-of-bounds
        // were supplied, to ensure that any reads of those pages get woken up.
        InvalidateReadRequestsLocked(start, len);

        // If DIRTY requests are supported, also tell the page source that any non-Dirty pages that
        // are now out-of-bounds were dirtied (without actually dirtying them), to ensure that any
        // threads blocked on DIRTY requests for those pages get woken up.
        if (is_source_preserving_page_content() && page_source_->ShouldTrapDirtyTransitions()) {
          InvalidateDirtyRequestsLocked(start, len);
        }
      }

      // If pager-backed and the new size falls partway in an interval, we will need to clip the
      // interval.
      if (is_source_preserving_page_content()) {
        // Check if the first populated slot we find in the now-invalid range is an interval end.
        uint64_t interval_end = UINT64_MAX;
        zx_status_t status = page_list_.ForEveryPageInRange(
            [&interval_end](const VmPageOrMarker* p, uint64_t off) {
              if (p->IsIntervalEnd()) {
                interval_end = off;
              }
              // We found the first populated slot. Stop the traversal.
              return ZX_ERR_STOP;
            },
            start, size_);
        DEBUG_ASSERT(status == ZX_OK);

        if (interval_end != UINT64_MAX) {
          status = page_list_.ClipIntervalEnd(interval_end, interval_end - start + PAGE_SIZE);
          if (status != ZX_OK) {
            DEBUG_ASSERT(status == ZX_ERR_NO_MEMORY);
            return status;
          }
        }
      }

      // Clip the parent limit and release any pages, if any, in this node or the parents.
      //
      // It should never exceed this node's size, either the current size (which is `end`) or the
      // new size (which is `start`).
      DEBUG_ASSERT(parent_limit_ <= end);

      ReleaseOwnedPagesLocked(start, LockedPtr(), deferred.FreedList(this));

      // If the tail of a parent disappears, the children shouldn't be able to see that region
      // again, even if the parent is later reenlarged. So update the children's parent limits.
      if (children_list_len_ != 0) {
        // The only scenario where we can have children is if this is a pager backed hierarchy, in
        // which case the DeferredOps constructed at the top of this function holds the pager
        // hierarchy lock, which is held over all resize operations. Due to this lock being held we
        // know that, even once the VMO lock is dropped, no resize operation to reenlarge can occur
        // till after we have completed updating the child limits.
        // In the present state, with our size_ reduced but child parent_limit_ not updated, the
        // children will just walk up to us, see that the offset is beyond our size_, and substitute
        // a zero page. Once the child parent_limit_s are updated they will instead not walk up to
        // us, and substitute a zero page.
        ASSERT(root_has_page_source());
        update_child_limits = true;
      }
    } else if (s > size_) {
      uint64_t temp;
      // Check that this VMOs new size would not cause it to overflow if projected onto the root.
      bool overflow = add_overflow(root_parent_offset_, s, &temp);
      if (overflow) {
        return ZX_ERR_INVALID_ARGS;
      }
      // expanding
      // figure the starting and ending page offset that is affected
      const uint64_t start = size_;
      const uint64_t end = s;
      const uint64_t len = end - start;

      // inform all our children or mapping that there's new bits
      RangeChangeUpdateLocked(VmCowRange(start, len), RangeChangeOp::Unmap, &deferred);

      // If pager-backed, need to insert a dirty zero interval beyond the old size.
      if (is_source_preserving_page_content()) {
        zx_status_t status =
            page_list_.AddZeroInterval(start, end, VmPageOrMarker::IntervalDirtyState::Dirty);
        if (status != ZX_OK) {
          DEBUG_ASSERT(status == ZX_ERR_NO_MEMORY);
          return status;
        }
      }
    }

    // save bytewise size
    size_ = s;

    // We were able to successfully resize. Mark as modified.
    mark_modified_locked();

    VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
    VMO_VALIDATION_ASSERT(DebugValidateZeroIntervalsLocked());
    VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
  }
  // Now that the lock is dropped, check if we need to update the child limits before the
  // DeferredOps get finalized. When iterating over our children it is important that we iterate
  // precisely over *all* of our children and exactly our direct children (i.e. not our children's
  // children). The TreeWalkCursor is able to provide these guarantees in this case since clone
  // creation is serialized with the page_source_lock in the DeferredOps, just like here.
  // Serializing the clone calls with resize ensures that any child we are iterating cannot move
  // down in the tree and gain a new parent, which happens when a hidden node needs to be inserted.
  // The deletion path is not an issue since if the node we are iterating at gets deleted then the
  // cursor will just move to its sibling (or get deleted if no sibling), which is the behavior that
  // we want anyway.
  if (update_child_limits) {
    // Use a TreeWalkCursor to walk all our children.
    // A child's parent limit will also limit that child's descendants' views into this node, so
    // this method only needs to touch the direct children.
    TreeWalkCursor cursor(LockedPtr(this));
    // Go to the first child, if we still have one.
    if (cursor.NextChild()) {
      // Update this child and all its siblings.
      do {
        // Ensure that we are only modifying direct descendants.
        DEBUG_ASSERT(cursor.GetCur().locked().parent_.get() == this);
        cursor.GetCur().locked().parent_limit_ = ClampedLimit(
            cursor.GetCur().locked().parent_offset_, cursor.GetCur().locked().parent_limit_, s);
      } while (cursor.NextSibling());
    }
  }
  return ZX_OK;
}

zx_status_t VmCowPages::LookupLocked(VmCowRange range, VmObject::LookupFunction lookup_fn) {
  canary_.Assert();
  if (unlikely(range.is_empty())) {
    return ZX_ERR_INVALID_ARGS;
  }

  // verify that the range is within the object
  if (unlikely(!range.IsBoundedBy(size_))) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  const uint64_t start_page_offset = ROUNDDOWN_PAGE_SIZE(range.offset);
  const uint64_t end_page_offset = ROUNDUP_PAGE_SIZE(range.end());

  return page_list_.ForEveryPageInRange(
      [&lookup_fn](const auto* p, uint64_t off) {
        if (!p->IsPage()) {
          // Skip non pages.
          return ZX_ERR_NEXT;
        }
        paddr_t pa = p->Page()->paddr();
        return lookup_fn(off, pa);
      },
      start_page_offset, end_page_offset);
}

zx_status_t VmCowPages::LookupReadableLocked(VmCowRange range, LookupReadableFunction lookup_fn) {
  canary_.Assert();
  if (unlikely(range.is_empty())) {
    return ZX_ERR_INVALID_ARGS;
  }

  // verify that the range is within the object
  if (unlikely(!range.IsBoundedBy(size_))) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  uint64_t current_page_offset = ROUNDDOWN_PAGE_SIZE(range.offset);
  const uint64_t end_page_offset = ROUNDUP_PAGE_SIZE(range.end());

  DEBUG_ASSERT(!is_hidden());

  while (current_page_offset != end_page_offset) {
    // Attempt to process any pages we have first. Skip over anything that's not a page since the
    // lookup_fn only applies to actual pages.
    zx_status_t status = page_list_.ForEveryPageInRange(
        [&lookup_fn, &current_page_offset, this](const VmPageOrMarker* page_or_marker,
                                                 uint64_t offset) {
          // The offset can advance ahead if we encounter gaps or sparse intervals.
          if (offset != current_page_offset) {
            if (!page_or_marker->IsIntervalEnd() && !node_has_parent_content_markers()) {
              // There was a gap before this offset and the tree does not use parent content markers
              // so we must walk up to find the content.
              return ZX_ERR_STOP;
            }
            // Otherwise, we can advance our cursor to the interval/gap end.
            offset = current_page_offset;
          }
          // Parent content is like a gap and so we need to exit and find the content.
          if (page_or_marker->IsParentContent()) {
            return ZX_ERR_STOP;
          }
          DEBUG_ASSERT(offset == current_page_offset);
          current_page_offset = offset + PAGE_SIZE;
          if (!page_or_marker->IsPage()) {
            return ZX_ERR_NEXT;
          }
          return lookup_fn(offset, page_or_marker->Page()->paddr());
        },
        current_page_offset, end_page_offset);

    // Check if we've processed the whole range.
    if (current_page_offset == end_page_offset) {
      break;
    }

    // See if any of our parents have the content.
    // Note that page intervals are only supported in root VMOs, so if we ended the page list
    // traversal above partway into an interval, we will be able to continue the traversal over the
    // rest of the interval after this call - since we're the root, we will be the owner and the
    // owner length won't be clipped.
    PageLookup content;
    FindPageContentLocked(current_page_offset, end_page_offset - current_page_offset, &content);

    // This should always get filled out.
    DEBUG_ASSERT(content.visible_end > current_page_offset);
    const uint64_t owner_length = content.visible_end - current_page_offset;

    // Iterate over any potential content.
    status = content.owner.locked_or(this).page_list_.ForEveryPageInRange(
        [&lookup_fn, current_page_offset, &content](const VmPageOrMarker* page_or_marker,
                                                    uint64_t offset) {
          if (!page_or_marker->IsPage()) {
            return ZX_ERR_NEXT;
          }
          return lookup_fn(offset - content.owner_offset + current_page_offset,
                           page_or_marker->Page()->paddr());
        },
        content.owner_offset, content.owner_offset + owner_length);
    if (status != ZX_OK || status != ZX_ERR_NEXT) {
      return status;
    }

    current_page_offset += owner_length;
  }
  return ZX_OK;
}

zx_status_t VmCowPages::TakePages(VmCowRange range, uint64_t splice_offset, VmPageSpliceList* pages,
                                  uint64_t* taken_len, MultiPageRequest* page_request) {
  canary_.Assert();

  DEBUG_ASSERT(range.is_page_aligned());

  __UNINITIALIZED DeferredOps deferred(this);
  Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};

  if (!range.IsBoundedBy(size_)) {
    pages->Finalize();
    return ZX_ERR_OUT_OF_RANGE;
  }

  if (page_source_) {
    pages->Finalize();
    return ZX_ERR_NOT_SUPPORTED;
  }

  if (AnyPagesPinnedLocked(range.offset, range.len)) {
    pages->Finalize();
    return ZX_ERR_BAD_STATE;
  }

  // On the assumption of success, unamp the entire range we are going to process. This ensures that
  // in the unlikely event of a failure mid way through the unmap of the portion that was modified
  // is not lost.
  RangeChangeUpdateLocked(range, RangeChangeOp::Unmap, &deferred);

  VmCompression* compression = Pmm::Node().GetPageCompression();

  // If we do not have a parent, and the page splice list is empty, then we can use TakePages to
  // directly move the page list nodes into the splice list. It is possible to both have not parent
  // and not have an empty splice list if the parent was concurrently closed while performing this
  // operation, in which case as its an infrequent race condition we fall through to the less
  // efficient code below.
  if (!parent_ && pages->IsEmpty() && splice_offset == 0) {
    bool found_page = false;
    page_list_.ForEveryPageInRangeMutable(
        [&compression, &found_page](VmPageOrMarkerRef p, uint64_t off) {
          found_page = true;
          // Splice lists do not support page intervals.
          ASSERT(!p->IsInterval());
          // Have no parent and so should not see parent content.
          DEBUG_ASSERT(!p->IsParentContent());
          if (p->IsPage()) {
            DEBUG_ASSERT(p->Page()->object.pin_count == 0);
            // Cannot be taking pages from a pager backed VMO, hence cannot be taking a loaned page.
            DEBUG_ASSERT(!p->Page()->is_loaned());
            pmm_page_queues()->Remove(p->Page());
          } else if (p->IsReference()) {
            // A regular reference we can move are permitted in the VmPageSpliceList, it is up to
            // the receiver of the pages to reject or otherwise deal with them. A temporary
            // reference we need to turn back into its page so we can move it.
            if (auto maybe_page = MaybeDecompressReference(compression, p->Reference())) {
              // Don't insert the page in the page queues, since we're trying to remove the pages,
              // just update the page list reader for TakePages below.
              VmPageOrMarker::ReferenceValue ref = p.SwapReferenceForPage(*maybe_page);
              ASSERT(compression->IsTempReference(ref));
            }
          }
          return ZX_ERR_NEXT;
        },
        range.offset, range.end());

    // If we did not find any pages, we could either be entirely inside a gap or an interval. Make
    // sure we're not inside an interval; checking a single offset for membership should suffice.
    ASSERT(found_page || !page_list_.IsOffsetInZeroInterval(range.offset));

    zx_status_t status = page_list_.TakePages(range.offset, pages);
    if (status != ZX_OK) {
      DEBUG_ASSERT(status == ZX_ERR_NO_MEMORY);
      return status;
    }
    *taken_len = range.len;
    return ZX_OK;
  }

  // Removing pages is performed in a loop to more easily handle a couple of potential edge cases.
  // If the VMO has a pager backed parent then any gaps in our page list may presently be seen as
  // non zero content, but need to be zero after taking. To achieve this we can perform
  // copy-on-write on the gap, giving us both pages we can take, and page list node slots we can
  // insert markers into. However, we need to perform a new iteration in order re-walk these offsets
  // that are now committed. Having a loop allows to therefore populate the gap, and then restart
  // the operation, potentially repeatedly if there are multiple gaps in the overall range.
  uint64_t processed = 0;
  do {
    // Helper method that is compatible with being given to VmPageList::RemovePages that moves
    // contents into the splice list, and replaces it with zero content. On error sets *taken_len
    // with the current offset.
    auto remove_page_callback = [&](VmPageOrMarker* slot, uint64_t offset) {
      if (slot->IsMarker()) {
        // Already zero. Can leave a gap, which is implied zero, in the splice list.
        return ZX_ERR_NEXT;
      }
      if (slot->IsParentContent()) {
        AssertHeld(lock_ref());
        auto cursor = GetLookupCursorLocked(VmCowRange(offset, PAGE_SIZE));
        if (cursor.is_error()) {
          *taken_len = offset - range.offset;
          return cursor.error_value();
        }
        AssertHeld(cursor->lock_ref());
        auto result = cursor->RequireOwnedPage(true, 1, deferred, page_request);
        if (result.is_error()) {
          *taken_len = offset - range.offset;
          return result.error_value();
        }
        DEBUG_ASSERT(slot->Page() == result->page);
      }
      if (slot->IsReference()) {
        // A regular reference we can move are permitted in the VmPageSpliceList, it is up to
        // the receiver of the pages to reject or otherwise deal with them. A temporary
        // reference we need to turn back into its page so we can move it.
        if (auto maybe_page = MaybeDecompressReference(compression, slot->Reference())) {
          // Don't insert the page in the page queues, since we're trying to remove the pages,
          // just update the page list reader for TakePages below.
          VmPageOrMarker::ReferenceValue ref = slot->SwapReferenceForPage(*maybe_page);
          ASSERT(compression->IsTempReference(ref));
        }
      } else if (slot->IsPage()) {
        DEBUG_ASSERT(slot->Page()->object.pin_count == 0);
        // Cannot be taking pages from a pager backed VMO, hence cannot be taking a loaned page.
        DEBUG_ASSERT(!slot->Page()->is_loaned());
        Pmm::Node().GetPageQueues()->Remove(slot->Page());
      }
      // Take the content and place it in the splice list.
      DEBUG_ASSERT(slot->IsPageOrRef());
      zx_status_t status = pages->Insert(offset - range.offset + splice_offset, ktl::move(*slot));
      if (status != ZX_OK) {
        ASSERT(status == ZX_ERR_NO_MEMORY);
        *taken_len = offset - range.offset;
        return status;
      }
      // Check if we need to insert a marker to zero the current location.
      auto parent_has_content = [this](uint64_t offset) {
        PageLookup content;
        AssertHeld(lock_ref());
        FindInitialPageContentLocked(offset, &content);
        return !!content.cursor.current();
      };
      if (!node_has_parent_content_markers() &&
          (root_has_page_source() || parent_has_content(offset))) {
        *slot = VmPageOrMarker::Marker();
      }
      return ZX_ERR_NEXT;
    };

    // Process any pages and record any gap we find that needs processing. Start with the assumption
    // that there is a gap at the end of the range to process in order to simplify termination logic
    // later on.
    uint64_t found_gap_start = range.end();
    uint64_t found_gap_end = found_gap_start;
    zx_status_t status = page_list_.RemovePagesAndIterateGaps(
        remove_page_callback,
        [&](uint64_t gap_start, uint64_t gap_end) {
          if (node_has_parent_content_markers()) {
            // Gaps imply zero content so we can just leave a gap in the splice list and continue.
            return ZX_ERR_NEXT;
          }
          found_gap_start = gap_start;
          found_gap_end = gap_end;
          return ZX_ERR_STOP;
        },
        range.offset + processed, range.end());
    if (status != ZX_OK) {
      return status;
    }

    if (found_gap_start < found_gap_end) {
      // Most likely this range is pager backed, since TakePages is impossible to be called on a
      // hidden node, and if we do not have parent content markers, then we are pager backed. As
      // such this entire gap has non-zero content as determined by the page source or one of
      // our intermediate parents and so just perform copy-on-write on the whole range so we can
      // then take those pages.
      const uint64_t gap_len = found_gap_end - found_gap_start;
      AssertHeld(lock_ref());
      auto cursor = GetLookupCursorLocked(VmCowRange(found_gap_start, gap_len));
      if (cursor.is_error()) {
        *taken_len = found_gap_start - range.offset;
        return cursor.error_value();
      }
      AssertHeld(cursor->lock_ref());
      for (uint64_t offset = 0; offset < gap_len; offset += PAGE_SIZE) {
        auto result = cursor->RequireOwnedPage(
            true, static_cast<uint>((gap_len - offset) / PAGE_SIZE), deferred, page_request);
        // In the case of an error we want to take any pages we may have successfully committed in
        // this loop in order to ensure forward progress.
        if (result.is_error()) {
          *taken_len = found_gap_start + offset - range.offset;
          // The only error we need to handle forward progress for is ZX_ERR_SHOULD_WAIT, anything
          // else doesn't matter as it will not retry.
          if (result.error_value() != ZX_ERR_SHOULD_WAIT || offset == 0) {
            return result.error_value();
          }
          status = page_list_.RemovePages(remove_page_callback, found_gap_start,
                                          found_gap_start + offset);
          if (status == ZX_OK) {
            // If RemovePages completed successfully then we can return our original error.
            status = ZX_ERR_SHOULD_WAIT;
          } else if (status == ZX_ERR_SHOULD_WAIT) {
            // The remove_page_callback will have updated `taken_len` to reflect exactly how much
            // progress it made before encountering an allocation failure, ensuring we make forwards
            // progress.
          } else {
            // We received a different error, most likely ZX_ERR_OUT_OF_MEMORY. This takes
            // precedence over the previous ZX_ERR_SHOULD_WAIT, so we need to cancel any page
            // requests.
            page_request->CancelRequests();
          }
          return status;
        }
      }
    }
    // Set the amount we have processed such that we retry from the start of the gap we potentially
    // just committed. If no gap is found then due to the way found_gap_start was initialized we
    // will terminate the loop.
    processed = found_gap_start - range.offset;
    // Keep going as long as we have something to process.
  } while (processed < range.len);

  pages->Finalize();
  *taken_len = range.len;

  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());

  return ZX_OK;
}

zx_status_t VmCowPages::SupplyPagesLocked(VmCowRange range, VmPageSpliceList* pages,
                                          SupplyOptions options, uint64_t* supplied_len,
                                          DeferredOps& deferred, MultiPageRequest* page_request) {
  canary_.Assert();

  DEBUG_ASSERT(range.is_page_aligned());
  DEBUG_ASSERT(supplied_len);
  ASSERT(options != SupplyOptions::PagerSupply || page_source_);

  if (!range.IsBoundedBy(size_)) {
    *supplied_len = 0;
    return ZX_ERR_OUT_OF_RANGE;
  }

  if (options == SupplyOptions::TransferData) {
    if (page_source_) {
      return ZX_ERR_NOT_SUPPORTED;
    }
    if (AnyPagesPinnedLocked(range.offset, range.len)) {
      return ZX_ERR_BAD_STATE;
    }
  }

  if (page_source_ && page_source_->is_detached()) {
    return ZX_ERR_BAD_STATE;
  }

  // If this VMO has a parent, we need to make sure we take ownership of all of the pages in the
  // input range.
  // TODO(https://fxbug.dev/42076904): This is suboptimal, as we take ownership of a page just to
  // free it immediately when we replace it with the supplied page.
  if (parent_) {
    const uint64_t end = range.end();
    uint64_t position = range.offset;
    auto cursor = GetLookupCursorLocked(range);
    if (cursor.is_error()) {
      return cursor.error_value();
    }
    AssertHeld(cursor->lock_ref());
    while (position < end) {
      auto result = cursor->RequireOwnedPage(true, static_cast<uint>((end - position) / PAGE_SIZE),
                                             deferred, page_request);
      if (result.is_error()) {
        return result.error_value();
      }
      position += PAGE_SIZE;
    }
  }

  const uint64_t start = range.offset;
  const uint64_t end = range.end();

  // [new_pages_start, new_pages_start + new_pages_len) tracks the current run of
  // consecutive new pages added to this vmo.
  uint64_t offset = range.offset;
  uint64_t new_pages_start = offset;
  uint64_t new_pages_len = 0;
  zx_status_t status = ZX_OK;
  [[maybe_unused]] uint64_t initial_list_position = pages->Position();
  while (!pages->IsProcessed()) {
    // With a PageSource only Pages are supported, so convert any refs to real pages.
    // We do this without popping a page from the splice list as `MakePageFromReference` may return
    // ZX_ERR_SHOULD_WAIT. This could lead the caller to wait on the page request and call
    // `SupplyPagesLocked` again, at which point it would expect the operation to continue at the
    // exact same page.
    VmPageOrMarkerRef src_page_ref = pages->PeekReference();
    // The src_page_ref can be null if the head of the page list is not a reference or if the page
    // list is empty.
    if (src_page_ref) {
      DEBUG_ASSERT(src_page_ref->IsReference());
      status = MakePageFromReference(src_page_ref, page_request->GetAnonymous());
      if (status != ZX_OK) {
        break;
      }
    }
    VmPageOrMarker src_page = pages->Pop();
    DEBUG_ASSERT(!src_page.IsReference());

    // The pager API does not allow the source VMO of supply pages to have a page source, so we can
    // assume that any empty pages are zeroes and insert explicit markers here. We need to insert
    // explicit markers to actually resolve the pager fault.
    // If we are using parent content markers then we do not want to insert redundant markers
    // into a node. This would only happen when performing transfer data where this is not
    // actually pager backed and so we do not actually need to insert anything as there is no
    // fault to resolve. We will have to make the slot read as zero though, which is handled later
    // on by clearing the slot.
    if (src_page.IsEmpty() && !node_has_parent_content_markers()) {
      src_page = VmPageOrMarker::Marker();
    }

    // A newly supplied page starts off as Clean.
    if (src_page.IsPage() && is_source_preserving_page_content()) {
      UpdateDirtyStateLocked(src_page.Page(), offset, DirtyState::Clean,
                             /*is_pending_add=*/true);
    }

    VmPageOrMarker old_page;
    // Defer individual range updates so we can do them in blocks.
    const CanOverwriteContent overwrite_policy = options == SupplyOptions::TransferData
                                                     ? CanOverwriteContent::NonZero
                                                     : CanOverwriteContent::None;
    if (src_page.IsEmpty()) {
      DEBUG_ASSERT(node_has_parent_content_markers());
      DEBUG_ASSERT(overwrite_policy == CanOverwriteContent::NonZero);
      // If the src page is empty this implies we want to the zero content, which can be achieved
      // when using parent content markers by just clearing the slot.
      old_page = page_list_.RemoveContent(offset);
      // If we had a parent, and hence could have any parent content markers, then the
      // RequireOwnedPage should have transformed them into actual pages and so we should never see
      // a parent content marker at this point.
      DEBUG_ASSERT(!old_page.IsParentContent());
    } else {
      auto page_transaction = BeginAddPageLocked(offset, overwrite_policy);
      if (page_transaction.is_error()) {
        // Unable to insert anything at this slot, cleanup any existing src_page and handle a
        // completed run.
        if (src_page.IsPageOrRef()) {
          DEBUG_ASSERT(src_page.IsPage());
          vm_page_t* page = src_page.ReleasePage();
          DEBUG_ASSERT(!list_in_list(&page->queue_node));
          list_add_tail(deferred.FreedList(this).List(), &page->queue_node);
        }

        if (likely(page_transaction.status_value() == ZX_ERR_ALREADY_EXISTS)) {
          // We hit the end of a run of absent pages, so notify the page source
          // of any new pages that were added and reset the tracking variables.
          if (new_pages_len) {
            RangeChangeUpdateLocked(VmCowRange(new_pages_start, new_pages_len),
                                    RangeChangeOp::Unmap, &deferred);
            if (page_source_) {
              page_source_->OnPagesSupplied(new_pages_start, new_pages_len);
            }
          }
          new_pages_start = offset + PAGE_SIZE;
          new_pages_len = 0;
          offset += PAGE_SIZE;
          continue;
        } else {
          // Only cause for this should be an out of memory from the kernel heap when attempting to
          // allocate a page list node.
          status = page_transaction.status_value();
          ASSERT(status == ZX_ERR_NO_MEMORY);
          break;
        }
      }
      if (options != SupplyOptions::PhysicalPageProvider && should_borrow_locked() &&
          src_page.IsPage() &&
          PhysicalPageBorrowingConfig::Get().is_borrowing_in_supplypages_enabled()) {
        // Assert some things we implicitly know are true (currently).  We can avoid explicitly
        // checking these in the if condition for now.
        DEBUG_ASSERT(!is_source_supplying_specific_physical_pages());
        DEBUG_ASSERT(!src_page.Page()->is_loaned());
        // Try to replace src_page with a loaned page.  We allocate the loaned page one page at a
        // time to avoid failing the allocation due to asking for more loaned pages than there are
        // free loaned pages.
        auto result =
            AllocLoanedPage([this, &src_page, &page_transaction, &old_page](vm_page_t* page) {
              AssertHeld(lock_ref());
              CopyPageMetadataForReplacementLocked(page, src_page.Page());
              old_page =
                  CompleteAddPageLocked(*page_transaction, VmPageOrMarker::Page(page), nullptr);
            });
        if (result.is_ok()) {
          CopyPageContentsForReplacementLocked(*result, src_page.Page());
          vm_page_t* free_page = src_page.ReleasePage();
          list_add_tail(deferred.FreedList(this).List(), &free_page->queue_node);
        } else {
          old_page = CompleteAddPageLocked(*page_transaction, ktl::move(src_page), nullptr);
        }
      } else if (options == SupplyOptions::PhysicalPageProvider) {
        // When being called from the physical page provider, we need to call InitializeVmPage(),
        // which AddNewPageLocked() will do.
        // We only want to populate offsets that have true absence of content, so do not overwrite
        // anything in the page list.
        old_page = CompleteAddNewPageLocked(*page_transaction, src_page.Page(),
                                            /*zero=*/false, nullptr);
        // The page was successfully added, but we still have a copy in the src_page, so we need to
        // release it, however need to store the result in a temporary as we are required to use the
        // result of ReleasePage.
        [[maybe_unused]] vm_page_t* unused = src_page.ReleasePage();
      } else {
        // When not being called from the physical page provider, we don't need InitializeVmPage(),
        // so we use AddPageLocked().
        // We only want to populate offsets that have true absence of content, so do not overwrite
        // anything in the page list.
        old_page = CompleteAddPageLocked(*page_transaction, ktl::move(src_page), nullptr);
      }
    }
    // If the content overwrite policy was None, the old page should be empty.
    DEBUG_ASSERT(overwrite_policy != CanOverwriteContent::None || old_page.IsEmpty());
    // Clean up the old_page if necessary. The action taken is different depending on the state of
    // old_page:
    // 1. Page: If old_page is backed by an actual page, remove it from the page queues and free
    //          the page.
    // 2. Reference: If old_page is a reference, free the reference.
    // 3. Interval: We should not be overwriting data in a pager-backed VMO, so assert that
    //              old_page is not an interval.
    // 4. Marker: There are no resources to free here, so do nothing.
    if (old_page.IsPage()) {
      vm_page_t* released_page = old_page.ReleasePage();
      // We do not overwrite content in pager backed VMOs, the only place where loaned pages can be,
      // so any old page must never have been loaned.
      DEBUG_ASSERT(!released_page->is_loaned());
      pmm_page_queues()->Remove(released_page);
      DEBUG_ASSERT(!list_in_list(&released_page->queue_node));
      list_add_tail(deferred.FreedList(this).List(), &released_page->queue_node);
    } else if (old_page.IsReference()) {
      FreeReference(old_page.ReleaseReference());
    } else {
      DEBUG_ASSERT(!old_page.IsInterval());
      DEBUG_ASSERT(!old_page.IsParentContent());
    }
    new_pages_len += PAGE_SIZE;
    DEBUG_ASSERT(new_pages_start + new_pages_len <= end);

    offset += PAGE_SIZE;
  }
  // Unless there was an error and we exited the loop early, then there should have been the correct
  // number of pages in the splice list.
  DEBUG_ASSERT(offset == end || status != ZX_OK);
  if (new_pages_len) {
    RangeChangeUpdateLocked(VmCowRange(new_pages_start, new_pages_len), RangeChangeOp::Unmap,
                            &deferred);
    if (page_source_) {
      page_source_->OnPagesSupplied(new_pages_start, new_pages_len);
    }
  }

  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());

  *supplied_len = offset - start;
  // In the case of ZX_OK or ZX_ERR_SHOULD_WAIT we should have supplied exactly as many pages as we
  // processed. In any other case the value is undefined.
  DEBUG_ASSERT(((pages->Position() - initial_list_position) == *supplied_len) ||
               (status != ZX_OK && status != ZX_ERR_SHOULD_WAIT));
  return status;
}

// This is a transient operation used only to fail currently outstanding page requests. It does not
// alter the state of the VMO, or any pages that might have already been populated within the
// specified range.
//
// If certain pages in this range are populated, we must have done so via a previous SupplyPages()
// call that succeeded. So it might be fine for clients to continue accessing them, despite the
// larger range having failed.
//
// TODO(rashaeqbal): If we support a more permanent failure mode in the future, we will need to free
// populated pages in the specified range, and possibly detach the VMO from the page source.
zx_status_t VmCowPages::FailPageRequestsLocked(VmCowRange range, zx_status_t error_status) {
  canary_.Assert();

  DEBUG_ASSERT(range.is_page_aligned());

  ASSERT(page_source_);

  if (!PageSource::IsValidInternalFailureCode(error_status)) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (!range.IsBoundedBy(size_)) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  if (page_source_->is_detached()) {
    return ZX_ERR_BAD_STATE;
  }

  page_source_->OnPagesFailed(range.offset, range.len, error_status);
  return ZX_OK;
}

zx_status_t VmCowPages::DirtyPages(VmCowRange range, list_node_t* alloc_list,
                                   AnonymousPageRequest* page_request) {
  canary_.Assert();

  DEBUG_ASSERT(range.is_page_aligned());

  ASSERT(page_source_);

  if (!page_source_->ShouldTrapDirtyTransitions()) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  DEBUG_ASSERT(is_source_preserving_page_content());

  const uint64_t start_offset = range.offset;
  const uint64_t end_offset = range.end();

  __UNINITIALIZED DeferredOps deferred(this);
  Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};

  if (start_offset > size_locked()) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // Overflow check.
  if (end_offset < start_offset) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // After the above checks, the page source has tried to respond correctly to a range of dirty
  // requests, so the kernel should resolve those outstanding dirty requests, even in the failure
  // case. From a returned error, the page source currently has no ability to detect which ranges
  // caused the error, so the kernel should either completely succeed or fail the request instead of
  // holding onto a partial outstanding request that will block pager progress.
  auto invalidate_requests_on_error = fit::defer([this, len = range.len, start_offset] {
    AssertHeld(lock_ref());
    DEBUG_ASSERT(size_locked() >= start_offset);

    uint64_t invalidate_len = ktl::min(size_locked() - start_offset, len);
    InvalidateDirtyRequestsLocked(start_offset, invalidate_len);
  });

  // The page source may have tried to mark a larger range than necessary as dirty. Invalidate the
  // requests and return an error.
  if (end_offset > size_locked()) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  if (page_source_->is_detached()) {
    return ZX_ERR_BAD_STATE;
  }

  // If any of the pages in the range are zero page markers (Clean zero pages), they need to be
  // forked in order to be dirtied (written to). Find the number of such pages that need to be
  // allocated. We also need to allocate zero pages to replace sparse zero intervals.
  size_t zero_pages_count = 0;
  // This tracks the beginning of an interval that falls in the specified range. Since we might
  // start partway inside an interval, this is initialized to start_offset so that we only consider
  // the portion of the interval inside the range. If we did not start inside an interval, we will
  // end up reinitializing this when we do find an interval start, before this value is used, so it
  // is safe to initialize to start_offset in all cases.
  uint64_t interval_start = start_offset;
  // This tracks whether we saw an interval start sentinel in the traversal, but have not yet
  // encountered a matching interval end sentinel. Should we end the traversal partway in an
  // interval, we will need to handle the portion of the interval between the interval start and the
  // end of the specified range.
  bool unmatched_interval_start = false;
  bool found_page_or_gap = false;
  zx_status_t status = page_list_.ForEveryPageAndGapInRange(
      [&zero_pages_count, &interval_start, &unmatched_interval_start, &found_page_or_gap](
          const VmPageOrMarker* p, uint64_t off) {
        found_page_or_gap = true;
        if (p->IsMarker()) {
          zero_pages_count++;
          return ZX_ERR_NEXT;
        }
        if (p->IsIntervalZero()) {
          if (p->IsIntervalStart()) {
            interval_start = off;
            unmatched_interval_start = true;
          } else if (p->IsIntervalEnd()) {
            zero_pages_count += (off - interval_start + PAGE_SIZE) / PAGE_SIZE;
            unmatched_interval_start = false;
          } else {
            DEBUG_ASSERT(p->IsIntervalSlot());
            zero_pages_count++;
          }
          return ZX_ERR_NEXT;
        }
        // Pager-backed VMOs cannot have compressed references, so the only other type is a page.
        DEBUG_ASSERT(p->IsPage());
        return ZX_ERR_NEXT;
      },
      [&found_page_or_gap](uint64_t start, uint64_t end) {
        found_page_or_gap = true;
        // A gap indicates a page that has not been supplied yet. It will need to be supplied
        // first. Although we will never generate a DIRTY request for absent pages in the first
        // place, it is still possible for a clean page to get evicted after the DIRTY request was
        // generated. It is also possible for a dirty zero interval to have been written back such
        // that we have an old DIRTY request for the interval.
        //
        // Spuriously resolve the DIRTY page request, and let the waiter(s) retry looking up the
        // page, which will generate a READ request first to supply the missing page.
        return ZX_ERR_NOT_FOUND;
      },
      start_offset, end_offset);

  if (status != ZX_OK) {
    return status;
  }

  // Handle the last interval or if we did not enter the traversal callbacks at all.
  if (unmatched_interval_start || !found_page_or_gap) {
    DEBUG_ASSERT(found_page_or_gap || interval_start == start_offset);
    zero_pages_count += (end_offset - interval_start) / PAGE_SIZE;
  }

  // If we have found any zero pages to populate, then we need to allocate and transition them to
  // the dirty state.
  if (zero_pages_count > 0) {
    // Allocate the number of zero pages required upfront, so that we can fail the call early if the
    // page allocation fails. First determine how many pages we still need to allocate, based on the
    // number of existing pages in the list.
    uint64_t alloc_list_len = list_length(alloc_list);
    zero_pages_count = zero_pages_count > alloc_list_len ? zero_pages_count - alloc_list_len : 0;

    // First try to allocate all the pages at once. This is an optimization and avoids repeated
    // calls to the PMM to allocate single pages. If the PMM returns ZX_ERR_SHOULD_WAIT, fall back
    // to allocating one page at a time below, giving reclamation strategies a better chance to
    // catch up with incoming allocation requests.
    status = pmm_alloc_pages(zero_pages_count, pmm_alloc_flags_, alloc_list);
    if (status == ZX_OK) {
      // All requested pages allocated.
      zero_pages_count = 0;
    } else {
      if (status != ZX_ERR_SHOULD_WAIT) {
        return status;
      }

      // Fall back to allocating a single page at a time. We want to do this before we can start
      // inserting pages into the page list, to avoid rolling back any pages we inserted but could
      // not dirty in case we fail partway after having inserted some pages into the page list.
      // Rolling back like this can lead to a livelock where we are constantly allocating some
      // pages, freeing them, waiting on the page_request, and then repeating.
      //
      // If allocations do fail partway here, we will have accumulated the allocated pages in
      // alloc_list, so we will be able to reuse them on a subsequent call to DirtyPagesLocked. This
      // ensures we are making forward progress across successive calls.
      while (zero_pages_count > 0) {
        vm_page_t* new_page;
        // We will initialize this page later when passing it to AddNewPageLocked
        status = AllocUninitializedPage(&new_page, page_request);
        // If single page allocation fails, bubble up the failure.
        if (status != ZX_OK) {
          // If propagating up ZX_ERR_SHOULD_WAIT do not consider this an error that requires
          // invalidating the dirty request as we are going to retry it.
          if (status == ZX_ERR_SHOULD_WAIT) {
            invalidate_requests_on_error.cancel();
          }
          return status;
        }
        list_add_tail(alloc_list, &new_page->queue_node);
        zero_pages_count--;
      }
    }
    DEBUG_ASSERT(zero_pages_count == 0);

    // We have to mark all the requested pages Dirty *atomically*. The user pager might be tracking
    // filesystem space reservations based on the success / failure of this call. So if we fail
    // partway, the user pager might think that no pages in the specified range have been dirtied,
    // which would be incorrect. If there are any conditions that would cause us to fail, evaluate
    // those before actually adding the pages, so that we can return the failure early before
    // starting to mark pages Dirty.
    //
    // Install page slots for all the intervals we'll be adding zero pages in. Page insertion will
    // only proceed once we've allocated all the slots without any errors.
    // Populating slots will alter the page list. So break out of the traversal upon finding an
    // interval, populate slots in it, and then resume the traversal after the interval.
    uint64_t next_start_offset = start_offset;
    do {
      struct {
        bool found_interval;
        uint64_t start;
        uint64_t end;
      } state = {.found_interval = false, .start = 0, .end = 0};
      status = page_list_.ForEveryPageAndContiguousRunInRange(
          [](const VmPageOrMarker* p, uint64_t off) {
            return p->IsIntervalStart() || p->IsIntervalEnd();
          },
          [](const VmPageOrMarker* p, uint64_t off) {
            DEBUG_ASSERT(p->IsIntervalZero());
            return ZX_ERR_NEXT;
          },
          [&state](uint64_t start, uint64_t end, bool is_interval) {
            DEBUG_ASSERT(is_interval);
            state = {.found_interval = true, .start = start, .end = end};
            return ZX_ERR_STOP;
          },
          next_start_offset, end_offset);
      DEBUG_ASSERT(status == ZX_OK);

      // No intervals remain.
      if (!state.found_interval) {
        break;
      }
      // Ensure we're making forward progress.
      DEBUG_ASSERT(state.end - state.start >= PAGE_SIZE);
      zx_status_t st = page_list_.PopulateSlotsInInterval(state.start, state.end);
      if (st != ZX_OK) {
        DEBUG_ASSERT(st == ZX_ERR_NO_MEMORY);
        // Before returning, we need to undo any slots we might have populated in intervals we
        // previously encountered. This is a rare error case and can be inefficient.
        for (uint64_t off = start_offset; off < state.start; off += PAGE_SIZE) {
          auto slot = page_list_.Lookup(off);
          if (slot) {
            // If this is an interval slot, return it. Note that even though we did populate all
            // slots until this point, not all will remain slots in this for-loop. When returning
            // slots, they can merge with intervals both before and after, so it's possible that the
            // next slot we were expecting has already been consumed.
            if (slot->IsIntervalSlot()) {
              page_list_.ReturnIntervalSlot(off);
            }
          }
        }
        return st;
      }
      next_start_offset = state.end;
    } while (next_start_offset < end_offset);

    // All operations from this point on must succeed so we can atomically mark pages dirty.

    // Install newly allocated pages in place of the zero page markers and interval sentinels. Start
    // with clean zero pages even for the intervals, so that the dirty transition logic below can
    // uniformly transition them to dirty along with pager supplied pages.
    status = page_list_.ForEveryPageInRange(
        [this, &alloc_list, &deferred](const VmPageOrMarker* p, uint64_t off) {
          if (p->IsMarker() || p->IsIntervalSlot()) {
            DEBUG_ASSERT(!list_is_empty(alloc_list));
            AssertHeld(lock_ref());

            // AddNewPageLocked will also zero the page and update any mappings.
            //
            // TODO(rashaeqbal): Depending on how often we end up forking zero markers, we might
            // want to pass a nullptr here instead of &deferred and perform a single batch update
            // later.
            zx_status_t status =
                AddNewPageLocked(off, list_remove_head_type(alloc_list, vm_page, queue_node),
                                 CanOverwriteContent::Zero, nullptr, true, &deferred);
            // AddNewPageLocked will not fail with ZX_ERR_ALREADY_EXISTS as we can overwrite
            // markers and interval slots since they are zero, nor with ZX_ERR_NO_MEMORY as we don't
            // need to allocate a new slot in the page list, we're simply replacing its content.
            ASSERT(status == ZX_OK);
          }
          return ZX_ERR_NEXT;
        },
        start_offset, end_offset);

    // We don't expect an error from the traversal.
    DEBUG_ASSERT(status == ZX_OK);
  }

  status = page_list_.ForEveryPageAndContiguousRunInRange(
      [](const VmPageOrMarker* p, uint64_t off) {
        DEBUG_ASSERT(!p->IsReference());
        if (p->IsPage()) {
          vm_page_t* page = p->Page();
          DEBUG_ASSERT(is_page_dirty_tracked(page));
          DEBUG_ASSERT(is_page_clean(page) || !page->is_loaned());
          return !is_page_dirty(page);
        }
        return false;
      },
      [this](const VmPageOrMarker* p, uint64_t off) {
        DEBUG_ASSERT(p->IsPage());
        vm_page_t* page = p->Page();
        DEBUG_ASSERT(is_page_dirty_tracked(page));
        DEBUG_ASSERT(!is_page_dirty(page));
        AssertHeld(lock_ref());
        UpdateDirtyStateLocked(page, off, DirtyState::Dirty);
        return ZX_ERR_NEXT;
      },
      [this](uint64_t start, uint64_t end, bool unused) {
        page_source_->OnPagesDirtied(start, end - start);
        return ZX_ERR_NEXT;
      },
      start_offset, end_offset);
  // We don't expect a failure from the traversal.
  DEBUG_ASSERT(status == ZX_OK);

  // All pages have been dirtied successfully, so cancel the cleanup on error.
  invalidate_requests_on_error.cancel();

  VMO_VALIDATION_ASSERT(DebugValidateZeroIntervalsLocked());
  return status;
}

zx_status_t VmCowPages::EnumerateDirtyRangesLocked(VmCowRange range,
                                                   DirtyRangeEnumerateFunction&& dirty_range_fn) {
  canary_.Assert();

  // Dirty pages are only tracked if the page source preserves content.
  if (!is_source_preserving_page_content()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  if (!range.IsBoundedBy(size_)) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  const uint64_t start_offset = ROUNDDOWN_PAGE_SIZE(range.offset);
  const uint64_t end_offset = ROUNDUP_PAGE_SIZE(range.end());

  zx_status_t status = page_list_.ForEveryPageAndContiguousRunInRange(
      [](const VmPageOrMarker* p, uint64_t off) {
        // Enumerate both AwaitingClean and Dirty pages, i.e. anything that is not Clean.
        // AwaitingClean pages are "dirty" too for the purposes of this enumeration, since their
        // modified contents are still in the process of being written back.
        if (p->IsPage()) {
          vm_page_t* page = p->Page();
          DEBUG_ASSERT(is_page_dirty_tracked(page));
          DEBUG_ASSERT(is_page_clean(page) || !page->is_loaned());
          return !is_page_clean(page);
        }
        // Enumerate any dirty zero intervals.
        if (p->IsIntervalZero()) {
          // For now we do not support clean intervals.
          DEBUG_ASSERT(!p->IsZeroIntervalClean());
          return p->IsZeroIntervalDirty();
        }
        // Pager-backed VMOs cannot have compressed references, so the only other type is a marker.
        DEBUG_ASSERT(p->IsMarker());
        return false;
      },
      [](const VmPageOrMarker* p, uint64_t off) {
        if (p->IsPage()) {
          vm_page_t* page = p->Page();
          DEBUG_ASSERT(is_page_dirty_tracked(page));
          DEBUG_ASSERT(!is_page_clean(page));
          DEBUG_ASSERT(!page->is_loaned());
          DEBUG_ASSERT(page->object.get_page_offset() == off);
        } else if (p->IsIntervalZero()) {
          DEBUG_ASSERT(p->IsZeroIntervalDirty());
        }
        return ZX_ERR_NEXT;
      },
      [&dirty_range_fn](uint64_t start, uint64_t end, bool is_interval) {
        // Zero intervals are enumerated as zero ranges.
        return dirty_range_fn(start, end - start, /*range_is_zero=*/is_interval);
      },
      start_offset, end_offset);

  VMO_VALIDATION_ASSERT(DebugValidateZeroIntervalsLocked());
  return status;
}

zx_status_t VmCowPages::WritebackBeginLocked(VmCowRange range, bool is_zero_range) {
  canary_.Assert();

  DEBUG_ASSERT(range.is_page_aligned());

  ASSERT(page_source_);

  if (!range.IsBoundedBy(size_)) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  if (!is_source_preserving_page_content()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  const uint64_t start_offset = range.offset;
  const uint64_t end_offset = range.end();
  // We only need to consider transitioning committed pages if the caller has specified that this is
  // not a zero range. For a zero range, we cannot start cleaning any pages because the caller has
  // expressed intent to write back zeros in this range; any pages we clean might get evicted and
  // incorrectly supplied again as zero pages, leading to data loss.
  //
  // When querying dirty ranges, zero page intervals are indicated as dirty zero ranges. So it's
  // perfectly reasonable for the user pager to write back these zero ranges efficiently without
  // having to read the actual contents of the range, which would read zeroes anyway. There can
  // exist a race however, where the user pager has just discovered a dirty zero range, and before
  // it starts writing it out, an actual page gets dirtied in that range. Consider the following
  // example that demonstrates the race:
  //  1. The zero interval [5, 10) is indicated as a dirty zero range when the user pager queries
  //  dirty ranges.
  //  2. A write comes in for page 7 and it is marked Dirty. The interval is split up into two: [5,
  //  7) and [8, 10).
  //  3. The user pager prepares to write the range [5, 10) with WritebackBegin.
  //  4. Both the intervals as well as page 7 are marked AwaitingClean.
  //  5. The user pager still thinks that [5, 10) is zero and writes back zeroes for the range.
  //  6. The user pager does a WritebackEnd on [5, 10), and page 7 gets marked Clean.
  //  7. At some point in the future, page 7 gets evicted. The data on page 7 (which was prematurely
  //  marked Clean) is now lost.
  //
  // This race occurred because there was a mismatch between what the user pager and the kernel
  // think the contents of the range being written back are. The user pager intended to mark only
  // zero ranges clean, not actual pages. The is_zero_range flag captures this intent, so that the
  // kernel does not incorrectly clean actual committed pages. Committed dirty pages will be
  // returned as actual dirty pages (not dirty zero ranges) on a subsequent call to query dirty
  // ranges, and can be cleaned then.

  auto interval_start = VmPageOrMarkerRef(nullptr);
  uint64_t interval_start_off;
  zx_status_t status = page_list_.ForEveryPageInRangeMutable(
      [is_zero_range, &interval_start, &interval_start_off, this](VmPageOrMarkerRef p,
                                                                  uint64_t off) {
        // VMOs with a page source should never have references.
        DEBUG_ASSERT(!p->IsReference());
        // If the page is pinned we have to leave it Dirty in case it is still being written to
        // via DMA. The VM system will be unaware of these writes, and so we choose to be
        // conservative here and might end up with pinned pages being left dirty for longer, until
        // a writeback is attempted after the unpin.
        // If the caller indicates that they're only cleaning zero pages, any committed pages need
        // to be left dirty.
        if (p->IsPage() && (p->Page()->object.pin_count > 0 || is_zero_range)) {
          return ZX_ERR_NEXT;
        }
        // Transition pages from Dirty to AwaitingClean.
        if (p->IsPage() && is_page_dirty(p->Page())) {
          AssertHeld(lock_ref());
          UpdateDirtyStateLocked(p->Page(), off, DirtyState::AwaitingClean);
          return ZX_ERR_NEXT;
        }
        // Transition dirty zero intervals to AwaitingClean.
        if (p->IsIntervalZero()) {
          if (!p->IsZeroIntervalDirty()) {
            // The only other state we support is Untracked.
            DEBUG_ASSERT(p->IsZeroIntervalUntracked());
            return ZX_ERR_NEXT;
          }
          if (p->IsIntervalStart() || p->IsIntervalSlot()) {
            // Start tracking a dirty interval. It will only transition once the end is encountered.
            DEBUG_ASSERT(!interval_start);
            interval_start = p;
            interval_start_off = off;
          }
          if (p->IsIntervalEnd() || p->IsIntervalSlot()) {
            // Now that we've encountered the end, the entire interval can be transitioned to
            // AwaitingClean. This is done by setting the AwaitingCleanLength of the start sentinel.
            // TODO: If the writeback began partway into the interval, try to coalesce the start's
            // awaiting clean length with the range being cleaned here if it immediately follows.
            if (interval_start) {
              // Set the new AwaitingClean length to the max of the old value and the new one.
              // See comments in WritebackEndLocked for an explanation.
              const uint64_t old_len = interval_start->GetZeroIntervalAwaitingCleanLength();
              interval_start.SetZeroIntervalAwaitingCleanLength(
                  ktl::max(off - interval_start_off + PAGE_SIZE, old_len));
            }
            // Reset the interval start so we can track a new one later.
            interval_start = VmPageOrMarkerRef(nullptr);
          }
          return ZX_ERR_NEXT;
        }
        // This was either a marker (which is already clean), or a non-Dirty page.
        DEBUG_ASSERT(p->IsMarker() || !is_page_dirty(p->Page()));
        return ZX_ERR_NEXT;
      },
      start_offset, end_offset);
  // We don't expect a failure from the traversal.
  DEBUG_ASSERT(status == ZX_OK);

  // Process the last partial interval.
  if (interval_start) {
    DEBUG_ASSERT(interval_start->IsIntervalStart());
    const uint64_t old_len = interval_start->GetZeroIntervalAwaitingCleanLength();
    interval_start.SetZeroIntervalAwaitingCleanLength(
        ktl::max(end_offset - interval_start_off, old_len));
  }

  // Set any mappings for this range to read-only, so that a permission fault is triggered the next
  // time the page is written to in order for us to track it as dirty. This might cover more pages
  // than the Dirty pages found in the page list traversal above, but we choose to do this once for
  // the entire range instead of per page; pages in the AwaitingClean and Clean states will already
  // have their write permission removed, so this is a no-op for them.
  const VmCowRange range_update = VmCowRange(start_offset, end_offset - start_offset);
  RangeChangeUpdateLocked(range_update, RangeChangeOp::RemoveWrite, nullptr);
  // No range change needs to be processed for the children since children, by virtue of being
  // copy-on-write, cannot have a writable mapping.

  VMO_VALIDATION_ASSERT(DebugValidateZeroIntervalsLocked());
  return ZX_OK;
}

zx_status_t VmCowPages::WritebackEndLocked(VmCowRange range) {
  canary_.Assert();

  DEBUG_ASSERT(range.is_page_aligned());

  ASSERT(page_source_);

  if (!range.IsBoundedBy(size_)) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  if (!is_source_preserving_page_content()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  const uint64_t start_offset = range.offset;
  const uint64_t end_offset = range.end();

  // Mark any AwaitingClean pages Clean. Remove AwaitingClean intervals that can be fully cleaned,
  // otherwise clip the interval start removing the part that has been cleaned. Note that deleting
  // an interval start is delayed until the corresponding end is encountered, and to ensure safe
  // continued traversal, the start should always be released before the end, i.e. in the expected
  // forward traversal order for RemovePages.
  VmPageOrMarker* interval_start = nullptr;
  uint64_t interval_start_off;
  // This tracks the end offset until which all zero intervals can be marked clean. This is a
  // running counter that is maintained across multiple zero intervals. Each time we encounter
  // a new interval start, we take the max of the existing value and the AwaitingCleanLength of the
  // new interval. This is because when zero intervals are truncated at the end or split, their
  // AwaitingCleanLength does not get updated, even if it's larger than the current interval length.
  // This is an optimization to avoid having to potentially walk to another node to find the
  // relevant start to update. The reason it is safe to leave the AwaitingCleanLength unchanged is
  // that it should be possible to apply the AwaitingCleanLength to any new zero intervals that get
  // added later beyond the truncated interval. The user pager has indicated its intent to write a
  // range as zeros, so until the point that it actually completes the writeback, it doesn't matter
  // if zero intervals are removed and re-added, as long as they fall in the range that was
  // initially indicated as being written back as zeros.
  uint64_t interval_awaiting_clean_end = start_offset;
  page_list_.RemovePages(
      [&interval_start, &interval_start_off, &interval_awaiting_clean_end, this](VmPageOrMarker* p,
                                                                                 uint64_t off) {
        // VMOs with a page source should never have references.
        DEBUG_ASSERT(!p->IsReference());
        // Transition pages from AwaitingClean to Clean.
        if (p->IsPage() && is_page_awaiting_clean(p->Page())) {
          AssertHeld(lock_ref());
          UpdateDirtyStateLocked(p->Page(), off, DirtyState::Clean);
          return ZX_ERR_NEXT;
        }
        // Handle zero intervals.
        if (p->IsIntervalZero()) {
          if (!p->IsZeroIntervalDirty()) {
            // The only other state we support is Untracked.
            DEBUG_ASSERT(p->IsZeroIntervalUntracked());
            return ZX_ERR_NEXT;
          }
          if (p->IsIntervalStart() || p->IsIntervalSlot()) {
            DEBUG_ASSERT(!interval_start);
            // Start tracking an interval.
            interval_start = p;
            interval_start_off = off;
            // See if we can advance interval_awaiting_clean_end to include the AwaitingCleanLength
            // of this interval.
            interval_awaiting_clean_end = ktl::max(interval_awaiting_clean_end,
                                                   off + p->GetZeroIntervalAwaitingCleanLength());
          }
          if (p->IsIntervalEnd() || p->IsIntervalSlot()) {
            // Can only transition the end if we saw the corresponding start.
            if (interval_start) {
              AssertHeld(lock_ref());
              if (off < interval_awaiting_clean_end) {
                // The entire interval is clean, so can remove it.
                if (interval_start_off != off) {
                  *interval_start = VmPageOrMarker::Empty();
                  // Return the start slot as it could have come from an earlier page list node.
                  // If the start slot came from the same node, we know that we still have a
                  // non-empty slot in that node (the current interval end we're looking at), and so
                  // the current node cannot be freed up, making it safe to continue traversal. The
                  // interval start should always be released before the end, which is consistent
                  // with forward traversal done by RemovePages.
                  page_list_.ReturnEmptySlot(interval_start_off);
                }
                // This empty slot with be returned by the RemovePages iterator.
                *p = VmPageOrMarker::Empty();
              } else {
                // The entire interval cannot be marked clean. Move forward the start by awaiting
                // clean length, which will also set the AwaitingCleanLength for the resulting
                // interval.
                // Ignore any errors. Cleaning is best effort. If this fails, the interval will
                // remain as is and get retried on another writeback attempt.
                page_list_.ClipIntervalStart(interval_start_off,
                                             interval_awaiting_clean_end - interval_start_off);
              }
              // Either way, the interval start tracking needs to be reset.
              interval_start = nullptr;
            }
          }
          return ZX_ERR_NEXT;
        }
        // This was either a marker (which is already clean), or a non-AwaitingClean page.
        DEBUG_ASSERT(p->IsMarker() || !is_page_awaiting_clean(p->Page()));
        return ZX_ERR_NEXT;
      },
      start_offset, end_offset);

  // Handle the last partial interval.
  if (interval_start) {
    // Ignore any errors. Cleaning is best effort. If this fails, the interval will remain as is and
    // get retried on another writeback attempt.
    page_list_.ClipIntervalStart(
        interval_start_off, ktl::min(interval_awaiting_clean_end, end_offset) - interval_start_off);
  }

  VMO_VALIDATION_ASSERT(DebugValidateZeroIntervalsLocked());
  return ZX_OK;
}

fbl::RefPtr<VmCowPages> VmCowPages::DebugGetParent() {
  canary_.Assert();

  Guard<CriticalMutex> guard{lock()};
  return parent_;
}

void VmCowPages::DetachSource() {
  canary_.Assert();

  __UNINITIALIZED DeferredOps deferred(this);
  Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};

  DEBUG_ASSERT(page_source_);
  page_source_->Detach();

  // We would like to remove all committed pages so that all future page faults on this VMO and its
  // clones can fail in a deterministic manner. However, if the page source is preserving content
  // (is a userpager), we need to hold on to un-Clean (Dirty and AwaitingClean pages) so that they
  // can be written back by the page source. If the page source is not preserving content, its pages
  // will not be dirty tracked to begin with i.e. their dirty state will be Untracked, so we will
  // end up removing all pages.

  // We should only be removing pages from the root VMO.
  DEBUG_ASSERT(!parent_);

  // Even though we might end up removing only a subset of the pages, unmap them all at once as an
  // optimization. Only the userpager is expected to access (dirty) pages beyond this point, in
  // order to write back their contents, where the cost of the writeback is presumably much larger
  // than page faults to update hardware page table mappings for resident pages.
  RangeChangeUpdateLocked(VmCowRange(0, size_), RangeChangeOp::Unmap, &deferred);

  __UNINITIALIZED BatchPQRemove page_remover(deferred.FreedList(this));

  // Remove all clean (or untracked) pages.
  // TODO(rashaeqbal): Pages that linger after this will be written back and marked clean at some
  // point, and will age through the pager-backed queues and eventually get evicted. We could
  // adopt an eager approach instead, and decommit those pages as soon as they get marked clean.
  // If we do that, we could also extend the eager approach to supply_pages, where pages get
  // decommitted on supply, i.e. the supply is a no-op.
  page_list_.RemovePages(
      [&page_remover](VmPageOrMarker* p, uint64_t off) {
        // A marker is a clean zero page. Replace it with an empty slot.
        if (p->IsMarker()) {
          *p = VmPageOrMarker::Empty();
          return ZX_ERR_NEXT;
        }

        // Zero intervals are dirty so they cannot be removed.
        if (p->IsIntervalZero()) {
          // TODO: Remove clean intervals once they are supported.
          DEBUG_ASSERT(!p->IsZeroIntervalClean());
          return ZX_ERR_NEXT;
        }

        // VMOs with a page source cannot have references.
        DEBUG_ASSERT(p->IsPage());

        // We cannot remove the page if it is dirty-tracked but not clean.
        if (is_page_dirty_tracked(p->Page()) && !is_page_clean(p->Page())) {
          DEBUG_ASSERT(!p->Page()->is_loaned());
          return ZX_ERR_NEXT;
        }

        // This is a page that we're going to remove; we don't expect it to be pinned.
        DEBUG_ASSERT(p->Page()->object.pin_count == 0);

        page_remover.Push(p->ReleasePage());
        return ZX_ERR_NEXT;
      },
      0, size_);

  page_remover.Flush();
}

void VmCowPages::RangeChangeUpdateLocked(VmCowRange range, RangeChangeOp op,
                                         DeferredOps* deferred) {
  canary_.Assert();
  // If we have children (or this is a pager backed hierarchy) then potentially need to perform
  // deferred operations.
  if (children_list_len_ != 0 || root_has_page_source()) {
    if (deferred) {
      deferred->AddRange(this, range, op);
    } else {
      // If the operation was RemoveWrite then, since children are copy-on-write and cannot have
      // writable mappings, they do not require a deferred operation. This is still true for pager
      // hierarchies as, since no content is actually changing, there is no need for serialization.
      DEBUG_ASSERT(op == RangeChangeOp::RemoveWrite);
    }
  }
  if (paged_ref_ && !range.is_empty()) {
    paged_backlink_locked(this)->RangeChangeUpdateLocked(range, op);
  }
}

// static
void VmCowPages::RangeChangeUpdateCowChildren(LockedPtr self, VmCowRange range, RangeChangeOp op) {
  self->canary_.Assert();

  // Helper for doing checking and performing a range change on a single candidate node. Although
  // this is used once it is split out here to make the loops that actually walk the tree as easy to
  // read as possible.
  // Returns true if the passed in |candidate| had some overlap with the operation range, and hence
  // its children also need to be walked. If false is returned the children of |candidate| can be
  // skipped. Due to not being able to continuously hold locks while walking the subtree, even
  // though we are therefore racing with concurrent modifications to the tree, it is still correct
  // to skip subtrees. To explain why, first consider the following (impossible) scenario:
  //                       A
  //                       |
  //                     |---|
  //                     B  ...
  //                     |
  //                   |---|
  //                   C   D
  //  1. Thread 1 performs an unmap on a page in A (offset X), that can be seen by B, C and D
  //  2. Thread 1 drops the lock of A to prepare to acquire lock of B
  //  3. Thread 2 inserts a page into B at offset X, and starts its own child range change update/
  //  4. Thread 2 drops the lock of B to prepare to acquire lock of C
  //  5. Thread 1 acquires the lock of B, observes that B cannot see X in A and skips the subtree
  //     of C and D.
  // At this point neither of the threads have performed an unmap on C or D, so how can thread 1
  // guarantee that neither can see page A?
  // The reason this cannot happen, and why this is an impossible scenario, as this would require B
  // to not be a hidden node, i.e. part of a user pager hierarchy. However, user pager hierarchies
  // have an additional lock used to serialize all such operations, and so the operation in thread 2
  // would not actually be able to start until thread 1 completely finished its range update and
  // released this serialization lock.
  auto check_candidate = [range, op](VmCowPages* candidate, uint64_t cur_accumulative_offset)
                             TA_REQ(candidate->lock()) -> bool {
    uint64_t candidate_offset = 0;
    uint64_t candidate_len = 0;
    if (!GetIntersect(cur_accumulative_offset, candidate->size_, range.offset, range.len,
                      &candidate_offset, &candidate_len)) {
      // Not intersection, can skip this node and the subtree.
      return false;
    }
    // if they intersect with us, then by definition the new offset must be >= total parent_offset_
    DEBUG_ASSERT(candidate_offset >= cur_accumulative_offset);

    // subtract our offset
    candidate_offset -= cur_accumulative_offset;

    // verify that it's still within range of us
    DEBUG_ASSERT(candidate_offset + candidate_len <= candidate->size_);

    // Check if there are any gaps in this range where we would actually see the parent.
    uint64_t first_gap_start = UINT64_MAX;
    uint64_t last_gap_end = 0;
    candidate->page_list_.ForEveryPageAndGapInRange(
        [&](auto page, uint64_t offset) {
          // If we have found a parent content marker then we can specifically see the parent at
          // this location, and can consider this like a gap. For anything else we know we do not
          // see the parent for this offset, so regardless of what it is just keep looking for a
          // gap. Additionally any children that we have will see this content instead of our
          // parents, and so we know it is also safe to skip them as well.
          if (page->IsParentContent()) {
            first_gap_start = ktl::min(first_gap_start, offset);
            last_gap_end = ktl::max(last_gap_end, offset + PAGE_SIZE);
          }
          return ZX_ERR_NEXT;
        },
        [&](uint64_t start, uint64_t end) {
          // A gap in the page list indicates a range where the parent can be seen, unless this is a
          // leaf node using parent content markers, in which case a gap indicates a range where we
          // do *not* see the parent.
          if (!candidate->node_has_parent_content_markers()) {
            first_gap_start = ktl::min(first_gap_start, start);
            last_gap_end = ktl::max(last_gap_end, end);
          }
          return ZX_ERR_NEXT;
        },
        candidate_offset, candidate_offset + candidate_len);

    if (first_gap_start >= last_gap_end) {
      vm_vmo_range_update_from_parent_skipped.Add(1);
      return false;
    }
    // Invalidate the new, potentially smaller, range that covers the gaps. Due to the
    // inability to store state we cannot use this smaller range for processing any of our
    // children, as we would not be able to restore the original range when walking back up,
    // but this still limits the range we process here and might have elided this subtree
    // altogether if no gap was found.
    // Construct a new, potentially smaller, range that covers the gaps. This will still
    // result in potentially processing pages that are locally covered, but are limited to a
    // single range here.
    if (candidate->paged_ref_) {
      AssertHeld(candidate->paged_ref_->lock_ref());
      candidate->paged_ref_->RangeChangeUpdateLocked(
          VmCowRange(first_gap_start, last_gap_end - first_gap_start), op);
    }
    vm_vmo_range_update_from_parent_performed.Add(1);
    // We processed this node and may need to walk the subtree.
    return true;
  };

  if (range.is_empty()) {
    return;
  }

  if (self.locked().children_list_len_ == 0) {
    return;
  }
  TreeWalkCursor cursor(ktl::move(self));

  bool candidate = cursor.NextChild();

  while (candidate) {
    if (check_candidate(&cursor.GetCur().locked(), cursor.GetCurrentOffset())) {
      candidate = cursor.NextChild();
    } else {
      candidate = cursor.NextSibling();
    }
  }
}

void VmCowPages::FinishTransitionToUncachedLocked() {
  // No need to perform clean/invalidate if size is zero because there can be no pages.
  if (size_ == 0) {
    return;
  }

  page_list_.ForEveryPage([this](const VmPageOrMarker* p, uint64_t off) {
    if (!p->IsPage()) {
      return ZX_ERR_NEXT;
    }
    vm_page_t* page = p->Page();
    DEBUG_ASSERT(page->object.pin_count == 0);
    // Refreshing the page queue will move the page to an unreclaimable one if applicable.
    AssertHeld(lock_ref());
    MoveToNotPinnedLocked(page, off);
    arch_clean_invalidate_cache_range((vaddr_t)paddr_to_physmap(page->paddr()), PAGE_SIZE);
    return ZX_ERR_NEXT;
  });
}

template <typename T>
bool VmCowPages::CanReclaimPageLocked(vm_page_t* page, T actual) {
  // Check this page is still a part of this VMO. After this any failures should mark the page as
  // accessed to prevent the page from remaining a reclamation candidate.
  if (!actual || !actual->IsPage() || actual->Page() != page) {
    vm_reclaim_incorrect_page.Add(1);
    return false;
  }
  // Pinned pages could be in use by DMA so we cannot safely reclaim them.
  if (page->object.pin_count != 0) {
    // Loaned pages should never end up pinned.
    DEBUG_ASSERT(!page->is_loaned());
    pmm_page_queues()->MarkAccessed(page);
    vm_reclaim_pinned.Add(1);
    return false;
  }
  return true;
}

VmCowPages::ReclaimCounts VmCowPages::ReclaimPageForEviction(vm_page_t* page, uint64_t offset,
                                                             EvictionAction eviction_action) {
  canary_.Assert();
  // Without a page source to bring the page back in we cannot even think about eviction.
  DEBUG_ASSERT(can_evict());

  __UNINITIALIZED DeferredOps deferred(this);
  Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};

  const VmPageOrMarker* page_or_marker = page_list_.Lookup(offset);
  if (!CanReclaimPageLocked(page, page_or_marker)) {
    return ReclaimCounts{};
  }
  // Since CanReclaimPageLocked() succeeded, we know that this page is owned by us at the provided
  // offset. So it should be safe to call MarkAccessed() on the page if reclamation fails, provided
  // we don't drop the lock.

  // Now allowed to reclaim if high priority, unless being required to do so.
  if (high_priority_count_ != 0 && (eviction_action != EvictionAction::Require)) {
    pmm_page_queues()->MarkAccessed(page);
    vm_reclaim_high_priority.Add(1);
    return ReclaimCounts{};
  }
  DEBUG_ASSERT(is_page_dirty_tracked(page));

  // We cannot evict the page unless it is clean. If the page is dirty, it will already have been
  // moved to the dirty page queue.
  if (!is_page_clean(page)) {
    DEBUG_ASSERT(pmm_page_queues()->DebugPageIsPagerBackedDirty(page));
    DEBUG_ASSERT(!page->is_loaned());
    vm_reclaim_dirty.Add(1);
    return ReclaimCounts{};
  }

  // Do not evict if the |always_need| hint is set, unless we are told to ignore the eviction hint.
  if (page->object.always_need == 1 && eviction_action == EvictionAction::FollowHint) {
    DEBUG_ASSERT(!page->is_loaned());
    // We still need to move the page from the tail of the LRU page queue(s) so that the eviction
    // loop can make progress. Since this page is always needed, move it out of the way and into the
    // MRU queue. Do this here while we hold the lock, instead of at the callsite.
    //
    // TODO(rashaeqbal): Since we're essentially simulating an access here, this page may not
    // qualify for eviction if we do decide to override the hint soon after (i.e. if an OOM follows
    // shortly after). Investigate adding a separate queue once we have some more data around hints
    // usage. A possible approach might involve moving to a separate queue when we skip the page for
    // eviction. Pages move out of said queue when accessed, and continue aging as other pages.
    // Pages in the queue are considered for eviction pre-OOM, but ignored otherwise.
    pmm_page_queues()->MarkAccessed(page);
    vm_reclaim_always_need_skipped.Add(1);
    return ReclaimCounts{};
  }

  // Remove any mappings to this page before we remove it.
  uint8_t old_queue = page->object.get_page_queue_ref().load(ktl::memory_order_relaxed);
  RangeChangeUpdateLocked(VmCowRange(offset, PAGE_SIZE), RangeChangeOp::UnmapAndHarvest, &deferred);
  const uint8_t new_queue = page->object.get_page_queue_ref().load(ktl::memory_order_relaxed);
  // If queue has changed, the accessed bit will have been set by the unmap.
  // Page has been accessed, don't evict.
  // TODO(https://fxbug.dev/412464435): don't unmap & return accessed status to avoid checking page
  // queues.
  if ((old_queue != new_queue) && (eviction_action != EvictionAction::Require)) {
    vm_reclaim_evict_accessed.Add(1);
    return ReclaimCounts{};
  }

  char vmo_name[ZX_MAX_NAME_LEN] __UNINITIALIZED = "\0";
  // Lambda so that vmo_name is only filled out if tracing is enabled.
  auto get_vmo_name = [&]() __ALWAYS_INLINE {
    AssertHeld(lock_);
    if (paged_ref_) {
      paged_ref_->get_name(vmo_name, sizeof(vmo_name));
    }
    return vmo_name;
  };
  VM_KTRACE_INSTANT(1, "evict_page", ("vmo_id", paged_ref_ ? paged_ref_->user_id() : 0),
                    ("offset", offset), ("vmo_name", get_vmo_name()));

  // Use RemovePage over just writing to page_or_marker so that the page list has the opportunity
  // to release any now empty intermediate nodes.
  vm_page_t* p = page_list_.RemoveContent(offset).ReleasePage();
  DEBUG_ASSERT(p == page);
  const bool loaned = page->is_loaned();
  RemovePageLocked(page, deferred);

  reclamation_event_count_++;
  VMO_VALIDATION_ASSERT(DebugValidateHierarchyLocked());
  VMO_FRUGAL_VALIDATION_ASSERT(DebugValidateVmoPageBorrowingLocked());
  return ReclaimCounts{
      .evicted_non_loaned = loaned ? 0u : 1u,
      .evicted_loaned = loaned ? 1u : 0u,
  };
}

VmCowPages::ReclaimCounts VmCowPages::ReclaimPageForCompression(vm_page_t* page, uint64_t offset,
                                                                VmCompressor* compressor) {
  DEBUG_ASSERT(compressor);
  DEBUG_ASSERT(!page_source_);
  DEBUG_ASSERT(!discardable_tracker_);
  DEBUG_ASSERT(can_decommit_zero_pages());

  // Track whether we should tell the caller we reclaimed a page or not.
  bool reclaimed = false;
  {
    __UNINITIALIZED DeferredOps deferred(this);
    Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};

    // Use a sub-scope as the page_or_marker will become invalid as we will drop the lock later.
    {
      VmPageOrMarkerRef page_or_marker = page_list_.LookupMutable(offset);
      if (!CanReclaimPageLocked(page, page_or_marker)) {
        return ReclaimCounts{};
      }
      // Since CanReclaimPageLocked() succeeded, we know that this page is owned by us at the
      // provided offset. So it should be safe to call MarkAccessed() on the page if reclamation
      // fails, provided we don't drop the lock.

      // Not allowed to reclaim if uncached.
      if ((paged_ref_ && (paged_backlink_locked(this)->GetMappingCachePolicyLocked() &
                          ZX_CACHE_POLICY_MASK) != ZX_CACHE_POLICY_CACHED)) {
        // To avoid this page remaining in the reclamation list we simulate an access.
        pmm_page_queues()->MarkAccessed(page);
        vm_reclaim_uncached.Add(1);
        return ReclaimCounts{};
      }

      // Not allowed to reclaim if high priority.
      if (high_priority_count_ != 0) {
        pmm_page_queues()->MarkAccessed(page);
        vm_reclaim_high_priority.Add(1);
        return ReclaimCounts{};
      }

      DEBUG_ASSERT(!page->is_loaned());

      // Perform the unmap of the page on our mappings while we hold the lock. This removes all
      // possible writable mappings, although our children could still have read-only mappings.
      // These read-only mappings will be dealt with later, for now the page will at least be
      // immutable.
      uint8_t old_queue = page->object.get_page_queue_ref().load(ktl::memory_order_relaxed);
      RangeChangeUpdateLocked(VmCowRange(offset, PAGE_SIZE), RangeChangeOp::UnmapAndHarvest,
                              &deferred);
      const uint8_t new_queue = page->object.get_page_queue_ref().load(ktl::memory_order_relaxed);
      // If queue has changed, the accessed bit will have been set by the unmap.
      // Page has been accessed, don't compress.
      // TODO(https://fxbug.dev/412464435): don't unmap & return accessed status to avoid checking
      // page queues.
      if (old_queue != new_queue) {
        vm_reclaim_compress_accessed.Add(1);
        return ReclaimCounts{};
      }

      // Start compression of the page by swapping the page list to contain the temporary reference.
      // Ensure the compression system is aware of the page's current share_count so it can track
      // any changes we make to that value while compression is running.
      VmPageOrMarker::ReferenceValue temp_ref = compressor->Start(
          VmCompressor::PageAndMetadata{.page = page, .metadata = page->object.share_count});
      [[maybe_unused]] vm_page_t* compress_page = page_or_marker.SwapPageForReference(temp_ref);
      DEBUG_ASSERT(compress_page == page);
    }
    pmm_page_queues()->Remove(page);

    // We now stack own the page (and guarantee to the compressor that it will not be modified) and
    // the VMO owns the temporary reference. We can safely drop the VMO lock and perform the
    // remaining range updates and the compression step.
  }
  compressor->Compress();

  {
    Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};

    // Retrieve the result of compression now that we hold the VMO lock again.
    VmCompressor::CompressResult compression_result = compressor->TakeCompressionResult();

    // We hold the VMO lock again and need to reclaim the temporary reference. Either the
    // temporary reference is still installed, and since we hold the VMO lock we now own both the
    // temp reference and the place, or the temporary reference got replaced, in which case it no
    // longer exists and is not referring to page and so we own page.
    //
    // Determining what state we are in just requires re-looking up the slot and see if the
    // temporary reference we installed is still there.
    auto [slot, is_in_interval] =
        page_list_.LookupOrAllocate(offset, VmPageList::IntervalHandling::NoIntervals);
    DEBUG_ASSERT(!is_in_interval);
    if (slot && slot->IsReference() && compressor->IsTempReference(slot->Reference())) {
      // Slot still holds the original reference; need to replace it with the result of compression.
      VmPageOrMarker::ReferenceValue old_ref{0};
      if (const VmPageOrMarker::ReferenceValue* ref =
              ktl::get_if<VmPageOrMarker::ReferenceValue>(&compression_result)) {
        // Compression succeeded, put the new reference in.
        // When compression succeeded, the |compressor| internally copied the page's metadata from
        // the temp reference to the new reference so we don't need to manually copy it here.
        old_ref = VmPageOrMarkerRef(slot).SwapReferenceForReference(*ref);
        reclamation_event_count_++;
        reclaimed = true;
      } else if (VmCompressor::FailTag* fail =
                     ktl::get_if<VmCompressor::FailTag>(&compression_result)) {
        // Compression failed, put the page back in the slot.
        // The |compressor| doesn't know how to update the |page| with any changes we made to its
        // metadata while compression was running, so we need to manually copy the metadata over to
        // the page's share_count here.
        DEBUG_ASSERT(page == fail->src_page.page);
        page->object.share_count = fail->src_page.metadata;
        old_ref = VmPageOrMarkerRef(slot).SwapReferenceForPage(page);
        // TODO(https://fxbug.dev/42138396): Placing in a queue and then moving it is inefficient,
        // but avoids needing to reason about whether reclamation could be manually attempted on
        // pages that might otherwise not end up in the reclaimable queues.
        SetNotPinnedLocked(page, offset);
        // TODO(https://fxbug.dev/42138396): Marking this page as failing reclamation will prevent
        // it from ever being tried again. As compression might succeed if the contents changes, we
        // should consider moving the page out of this queue if it is modified.
        pmm_page_queues()->CompressFailed(page);
        // Page stays owned by the VMO.
        page = nullptr;
      } else {
        ASSERT(ktl::holds_alternative<VmCompressor::ZeroTag>(compression_result));
        old_ref = slot->ReleaseReference();
        // Check if we can clear the slot, or if we need to insert a marker. Unlike the full zero
        // pages this simply needs to check if there's any visible content above us, and then if
        // there isn't if the root is immutable or not (i.e. if it has a page source).
        auto parent_has_content = [this](uint64_t offset) TA_REQ(lock()) {
          PageLookup content;
          FindInitialPageContentLocked(offset, &content);
          return !!content.cursor.current();
        };
        if (node_has_parent_content_markers() ||
            (!root_has_page_source() && !parent_has_content(offset))) {
          *slot = VmPageOrMarker::Empty();
          page_list_.ReturnEmptySlot(offset);
          vm_vmo_compression_zero_slot.Add(1);
        } else {
          *slot = VmPageOrMarker::Marker();
          vm_vmo_compression_marker.Add(1);
        }
        reclamation_event_count_++;
        reclaimed = true;
      }
      // Temporary reference has been replaced, can return it to the compressor.
      compressor->ReturnTempReference(old_ref);
    } else {
      // The temporary reference is no longer there. We know nothing else about the state of the VMO
      // at this point and will just free any compression result and exit.
      if (const VmPageOrMarker::ReferenceValue* ref =
              ktl::get_if<VmPageOrMarker::ReferenceValue>(&compression_result)) {
        compressor->Free(*ref);
      }
      // If the slot is allocated, but empty, then make sure we properly return it.
      if (slot && slot->IsEmpty()) {
        page_list_.ReturnEmptySlot(offset);
      }
      // In this case we are still going to free the page, but it doesn't count as a reclamation as
      // there is now something new in the slot we were trying to free.
    }
  }
  // One way or another the temporary reference has been returned, and so we can finalize.
  compressor->Finalize();

  if (page) {
    FreePage(page);
    page = nullptr;
  }

  return VmCowPages::ReclaimCounts{.compressed = reclaimed ? 1u : 0u};
}

VmCowPages::ReclaimCounts VmCowPages::ReclaimPage(vm_page_t* page, uint64_t offset,
                                                  EvictionAction hint_action,
                                                  VmCompressor* compressor) {
  canary_.Assert();

  // See if we can reclaim by eviction.
  if (can_evict()) {
    return ReclaimPageForEviction(page, offset, hint_action);
  }
  if (compressor && !page_source_ && !discardable_tracker_) {
    return ReclaimPageForCompression(page, offset, compressor);
  }
  if (discardable_tracker_) {
    // On any errors touch the page so we stop trying to reclaim it. In particular for discardable
    // reclamation attempts, if the page we are passing is not the first page in the discardable
    // VMO then the discard will fail, so touching it will stop us from continuously trying to
    // trigger a discard with it.
    auto result = ReclaimDiscardable(page, offset);
    if (result.is_ok()) {
      return ReclaimCounts{.discarded = *result};
    }
    vm_reclaim_discardable_failed.Add(1);
    return ReclaimCounts{};
  }

  // Keep a count as having no reclamation strategy is probably a sign of miss-configuration.
  vm_reclaim_no_reclamation_strategy.Add(1);

  // Either no other strategies, or reclamation failed, so to avoid this page remaining in a
  // reclamation list we simulate an access. Do not want to place it in the ReclaimFailed queue
  // since our failure was not based on page contents.
  // Before touching it double check this page is page of this VMO, as otherwise we cannot safely
  // know its state to call MarkAccessed.
  Guard<CriticalMutex> guard{lock()};
  const VmPageOrMarker* page_or_marker = page_list_.Lookup(offset);
  if (!page_or_marker || !page_or_marker->IsPage() || page_or_marker->Page() != page) {
    return ReclaimCounts{};
  }
  pmm_page_queues()->MarkAccessed(page);
  return ReclaimCounts{};
}

zx_status_t VmCowPages::ReplacePagesWithNonLoanedLocked(VmCowRange range, DeferredOps& deferred,
                                                        AnonymousPageRequest* page_request,
                                                        uint64_t* non_loaned_len) {
  canary_.Assert();

  DEBUG_ASSERT(range.is_page_aligned());
  DEBUG_ASSERT(range.IsBoundedBy(size_));
  DEBUG_ASSERT(non_loaned_len);

  *non_loaned_len = 0;
  bool found_page_or_gap = false;
  zx_status_t status = page_list_.ForEveryPageAndGapInRange(
      [page_request, non_loaned_len, &found_page_or_gap, &deferred, this](const VmPageOrMarker* p,
                                                                          uint64_t off) {
        found_page_or_gap = true;
        // We only expect committed pages in the specified range.
        if (!p->IsPage()) {
          return ZX_ERR_BAD_STATE;
        }
        vm_page_t* page = p->Page();
        // If the page is loaned, replace is with a non-loaned page.
        if (page->is_loaned()) {
          AssertHeld(lock_ref());
          // A loaned page could only have been clean.
          DEBUG_ASSERT(!is_page_dirty_tracked(page) || is_page_clean(page));
          DEBUG_ASSERT(page_request);
          zx_status_t status =
              ReplacePageLocked(page, off, /*with_loaned=*/false, &page, deferred, page_request);
          if (status == ZX_ERR_SHOULD_WAIT) {
            return status;
          }
          if (status != ZX_OK) {
            return ZX_ERR_BAD_STATE;
          }
        }
        DEBUG_ASSERT(!page->is_loaned());
        *non_loaned_len += PAGE_SIZE;
        return ZX_ERR_NEXT;
      },
      [&found_page_or_gap](uint64_t start, uint64_t end) {
        found_page_or_gap = true;
        // We only expect committed pages in the specified range.
        return ZX_ERR_BAD_STATE;
      },
      range.offset, range.end());

  if (status != ZX_OK) {
    return status;
  }

  // If we did not find a page or a gap, the entire range fell inside an interval. We only expect
  // committed pages in the range.
  if (!found_page_or_gap) {
    return ZX_ERR_BAD_STATE;
  }

  return ZX_OK;
}

zx_status_t VmCowPages::ReplacePageWithLoaned(vm_page_t* before_page, uint64_t offset) {
  canary_.Assert();

  __UNINITIALIZED DeferredOps deferred(this);
  Guard<CriticalMutex> guard{lock()};
  return ReplacePageLocked(before_page, offset, true, nullptr, deferred, nullptr);
}

zx_status_t VmCowPages::ReplacePage(vm_page_t* before_page, uint64_t offset, bool with_loaned,
                                    vm_page_t** after_page, AnonymousPageRequest* page_request) {
  __UNINITIALIZED DeferredOps deferred(this);
  Guard<CriticalMutex> guard{lock()};
  return ReplacePageLocked(before_page, offset, with_loaned, after_page, deferred, page_request);
}

zx_status_t VmCowPages::ReplacePageLocked(vm_page_t* before_page, uint64_t offset, bool with_loaned,
                                          vm_page_t** after_page, DeferredOps& deferred,
                                          AnonymousPageRequest* page_request) {
  // If not replacing with loaned it is required that a page_request be provided.
  DEBUG_ASSERT(with_loaned || page_request);

  VmPageOrMarkerRef p = page_list_.LookupMutable(offset);
  if (!p) {
    return ZX_ERR_NOT_FOUND;
  }
  if (!p->IsPage()) {
    return ZX_ERR_NOT_FOUND;
  }
  vm_page_t* old_page = p->Page();
  if (old_page != before_page) {
    return ZX_ERR_NOT_FOUND;
  }
  DEBUG_ASSERT(old_page != vm_get_zero_page());
  if (old_page->object.pin_count != 0) {
    DEBUG_ASSERT(!old_page->is_loaned());
    return ZX_ERR_BAD_STATE;
  }
  if (old_page->object.always_need) {
    DEBUG_ASSERT(!old_page->is_loaned());
    return ZX_ERR_BAD_STATE;
  }

  // unmap before removing old page
  RangeChangeUpdateLocked(VmCowRange(offset, PAGE_SIZE), RangeChangeOp::Unmap, &deferred);

  VmPageOrMarker released_page;
  auto replace_page_in_list = [&](vm_page_t* new_page) {
    AssertHeld(lock_ref());
    DEBUG_ASSERT(new_page->state() == vm_page_state::OBJECT);

    CopyPageMetadataForReplacementLocked(new_page, old_page);

    // Add replacement page in place of old page.
    __UNINITIALIZED auto result =
        BeginAddPageWithSlotLocked(offset, p, CanOverwriteContent::NonZero);
    // Absent bugs, BeginAddPageWithSlotLocked() can only return ZX_ERR_NO_MEMORY, but that failure
    // can only occur if page_list_ had to allocate.  Here, page_list_ hasn't yet had a chance to
    // clean up any internal structures, so BeginAddPageWithSlotLocked() didn't need to allocate, so
    // we know that BeginAddPageWithSlotLocked() will succeed.
    DEBUG_ASSERT(result.is_ok());
    released_page = CompleteAddPageLocked(*result, VmPageOrMarker::Page(new_page), nullptr);
  };

  vm_page_t* new_page = nullptr;
  zx_status_t status = ZX_OK;
  if (with_loaned) {
    if (!should_borrow_locked()) {
      return ZX_ERR_NOT_SUPPORTED;
    }
    if (is_page_dirty_tracked(old_page) && !is_page_clean(old_page)) {
      return ZX_ERR_BAD_STATE;
    }
    auto result =
        AllocLoanedPage([&replace_page_in_list](vm_page_t* page) { replace_page_in_list(page); });
    status = result.status_value();
    if (result.is_ok()) {
      new_page = *result;
    }
  } else {
    status = AllocPage(&new_page, page_request);
    if (status == ZX_OK) {
      replace_page_in_list(new_page);
    }
  }

  if (status != ZX_OK) {
    return status;
  }
  CopyPageContentsForReplacementLocked(new_page, old_page);

  // Need to take the page out of |released_page| to avoid a [[nodiscard]] error. Since we just
  // checked that this matches the target page, which is now owned by the caller, this is not
  // leaking.
  [[maybe_unused]] vm_page_t* released = released_page.ReleasePage();
  // The page released was the old page.
  DEBUG_ASSERT(released == old_page);

  RemovePageLocked(old_page, deferred);
  if (after_page) {
    *after_page = new_page;
  }

  return ZX_OK;
}

bool VmCowPages::DebugValidateHierarchyLocked() const TA_REQ(lock()) {
  canary_.Assert();

  const VmCowPages* cur = this;
  AssertHeld(cur->lock_ref());
  const VmCowPages* parent_most = cur;
  do {
    if (!cur->DebugValidatePageSharingLocked()) {
      return false;
    }
    cur = cur->parent_.get();
    if (cur) {
      parent_most = cur;
    }
  } while (cur);
  // Iterate whole hierarchy; the iteration order doesn't matter.  Since there are cases with
  // >2 children, in-order isn't well defined, so we choose pre-order, but post-order would also
  // be fine.
  AssertHeld(parent_most->lock_ref());
  zx_status_t status =
      parent_most->DebugForEachDescendant([this](const VmCowPages* cur, int depth) {
        AssertHeld(cur->lock_ref());
        if (!cur->DebugValidateBacklinksLocked()) {
          dprintf(INFO, "cur: %p this: %p\n", cur, this);
          return ZX_ERR_BAD_STATE;
        }
        return ZX_OK;
      });
  return status == ZX_OK;
}

bool VmCowPages::DebugValidatePageSharingLocked() const {
  canary_.Assert();

  // Visible nodes should never contain shared pages.
  if (!is_hidden()) {
    zx_status_t status =
        page_list_.ForEveryPage([this](const VmPageOrMarker* page, uint64_t offset) {
          if (!page->IsPageOrRef()) {
            return ZX_ERR_NEXT;
          }
          AssertHeld(lock_ref());

          const uint32_t share_count = GetShareCount(page);
          if (share_count != 0) {
            if (page->IsPage()) {
              printf("Found shared page in visible node %p (page %p) (off %#" PRIx64
                     ") (share %" PRIu32 "), but expected it to be private\n",
                     this, page->Page(), offset, share_count);
            } else {
              printf("Found shared reference in visible node %p (off %#" PRIx64 ") (share %" PRIu32
                     "), but expected it to be private\n",
                     this, offset, share_count);
            }
            DumpLocked(1, true);
            return ZX_ERR_BAD_STATE;
          }

          return ZX_ERR_NEXT;
        });

    // Nothing else to check for visible nodes
    return status == ZX_OK;
  }

  // Hidden nodes should share their pages with the correct number of visible nodes.
  DEBUG_ASSERT(is_hidden());
  DEBUG_ASSERT(!children_list_.is_empty());  // Hidden nodes must always have children
  zx_status_t status = page_list_.ForEveryPage([this](const VmPageOrMarker* page, uint64_t offset) {
    if (!page->IsPageOrRef()) {
      return ZX_ERR_NEXT;
    }
    AssertHeld(lock_ref());

    const uint32_t share_count = GetShareCount(page);
    const VmCowPages* cur = &children_list_.front();
    uint64_t offset_in_parent = offset;
    uint32_t found_count = 0;
    // For hidden nodes, check that the share counts on their pages and references are correct.
    // For a page with a share count of N, there should be N + 1 visible nodes that can access the
    // page.
    //
    // Walk the subtree rooted at this node. At each visible node we encounter, search back up to
    // see if it can access `page`.
    //
    // We start with cur being an immediate child of 'this', so we can preform subtree traversal
    // until we end up back in 'this'.
    while (cur != this) {
      AssertHeld(cur->lock_ref());
      DEBUG_ASSERT(cur->is_parent_hidden_locked());

      // Check that we can see this page in the parent. Importantly this first checks if
      // |offset_in_parent < cur->parent_offset_| allowing us to safely perform that subtraction
      // from then on.
      if (offset_in_parent < cur->parent_offset_ ||
          offset_in_parent - cur->parent_offset_ >= cur->parent_limit_) {
        // This blank case is used to capture the scenario where current does not see the target
        // offset in the parent, in which case there is no point traversing into the children.
      } else if (cur->is_hidden()) {
        // The children of a hidden node can only access the page if the hidden node isn't
        // covering it with anything, so only walk down if this offset is empty in the hidden node.
        const VmPageOrMarker* l = cur->page_list_.Lookup(offset_in_parent - cur->parent_offset_);
        if (!l || l->IsEmpty()) {
          // Page not found, we need to recurse down into our children.
          DEBUG_ASSERT(!cur->children_list_.is_empty());
          offset_in_parent -= cur->parent_offset_;
          cur = &cur->children_list_.front();
          continue;
        }
      } else {
        // `cur` is a visible node, so search up and see if it has partial ownership over the page.
        cur->ForEveryOwnedHierarchyPageInRangeLocked(
            [&](const VmPageOrMarker* p, const VmCowPages* owner, uint64_t this_offset,
                uint64_t owner_offset) {
              if (p == page) {
                DEBUG_ASSERT(owner == this);
                DEBUG_ASSERT(owner_offset == offset);
                found_count++;
                return ZX_ERR_STOP;
              }

              return ZX_ERR_NEXT;
            },
            offset_in_parent - cur->parent_offset_, PAGE_SIZE, LockedPtr());
      }

      // Our next node should be the next available child in some `children_list_`. We will walk up
      // until `cur` is not the last child in its parent's `children_list_`.
      do {
        const VmCowPages* parent = cur->parent_.get();
        AssertHeld(parent->lock_ref());

        // Check for next child after `cur`.
        auto children_iter = parent->children_list_.make_iterator(*cur);
        children_iter++;
        if (children_iter.IsValid()) {
          cur = children_iter.CopyPointer();
          // Parent shouldn't have changed, so `offset_in_parent` doesn't need to.
          AssertHeld(cur->lock_ref());
          DEBUG_ASSERT(cur->parent_.get() == parent);
          break;
        }

        // Otherwise keep walking up.
        cur = parent;
        offset_in_parent += parent->parent_offset_;
        if (cur == this) {
          break;
        }
      } while (1);
    }

    // Ensure we found the page the correct number of times in the subtree.
    if (found_count != share_count + 1) {
      if (page->IsPage()) {
        printf("Found shared page in hidden node %p (page %p) (off %#" PRIx64 ") (share %" PRIu32
               "), but accessible by wrong number of visible nodes %" PRIu32 "\n",
               this, page->Page(), offset, share_count, found_count);
      } else {
        printf("Found shared reference in hidden node %p (off %#" PRIx64 ") (share %" PRIu32
               "), but accessible by wrong number of visible nodes %" PRIu32 "\n",
               this, offset, share_count, found_count);
      }
      DumpLocked(1, true);
      return ZX_ERR_BAD_STATE;
    }

    return ZX_ERR_NEXT;
  });

  return status == ZX_OK;
}

bool VmCowPages::DebugValidateBacklinksLocked() const {
  canary_.Assert();
  bool result = true;
  page_list_.ForEveryPage([this, &result](const auto* p, uint64_t offset) {
    // Markers, references, and intervals don't have backlinks.
    if (p->IsReference() || p->IsMarker() || p->IsInterval()) {
      return ZX_ERR_NEXT;
    }
    vm_page_t* page = p->Page();
    vm_page_state state = page->state();
    if (state != vm_page_state::OBJECT) {
      dprintf(INFO, "unexpected page state: %u\n", static_cast<uint32_t>(state));
      result = false;
      return ZX_ERR_STOP;
    }
    const VmCowPages* object = reinterpret_cast<VmCowPages*>(page->object.get_object());
    if (!object) {
      dprintf(INFO, "missing object\n");
      result = false;
      return ZX_ERR_STOP;
    }
    if (object != this) {
      dprintf(INFO, "incorrect object - object: %p this: %p\n", object, this);
      result = false;
      return ZX_ERR_STOP;
    }
    uint64_t page_offset = page->object.get_page_offset();
    if (page_offset != offset) {
      dprintf(INFO, "incorrect offset - page_offset: %" PRIx64 " offset: %" PRIx64 "\n",
              page_offset, offset);
      result = false;
      return ZX_ERR_STOP;
    }
    return ZX_ERR_NEXT;
  });
  return result;
}

bool VmCowPages::DebugValidateVmoPageBorrowingLocked() const {
  canary_.Assert();
  // Skip checking larger VMOs to avoid slowing things down too much, since the things being
  // verified will typically assert from incorrect behavior on smaller VMOs (and we can always
  // remove this filter if we suspect otherwise).
  if (size_ >= 2 * 1024 * 1024) {
    return true;
  }
  bool result = true;
  page_list_.ForEveryPage([this, &result](const auto* p, uint64_t offset) {
    AssertHeld(lock_ref());
    if (!p->IsPage()) {
      // If we don't have a page, this is either a marker or reference, both of which are not
      // allowed with contiguous VMOs.
      DEBUG_ASSERT(!direct_source_supplies_zero_pages());
      return ZX_ERR_NEXT;
    }
    vm_page_t* page = p->Page();
    if (page->is_loaned()) {
      if (!can_borrow_locked()) {
        dprintf(INFO, "!can_borrow_locked() but page is loaned?? - offset: 0x%" PRIx64 "\n",
                offset);
        result = false;
        return ZX_ERR_STOP;
      }
      if (page->object.pin_count) {
        dprintf(INFO, "pinned page is loaned?? - offset: 0x%" PRIx64 "\n", offset);
        result = false;
        return ZX_ERR_STOP;
      }
      if (page->object.always_need) {
        dprintf(INFO, "always_need page is loaned?? - offset: 0x%" PRIx64 "\n", offset);
        result = false;
        return ZX_ERR_STOP;
      }
      if (is_page_dirty_tracked(page) && !is_page_clean(page)) {
        dprintf(INFO, "!clean page is loaned?? - offset: 0x%" PRIx64 "\n", offset);
        result = false;
        return ZX_ERR_STOP;
      }
    }
    return ZX_ERR_NEXT;
  });
  if (!result) {
    dprintf(INFO, "DebugValidateVmoPageBorrowingLocked() failing\n");
  }
  return result;
}

bool VmCowPages::DebugValidateZeroIntervalsLocked() const {
  canary_.Assert();
  bool in_interval = false;
  auto dirty_state = VmPageOrMarker::IntervalDirtyState::Untracked;
  zx_status_t status = page_list_.ForEveryPage(
      [&in_interval, &dirty_state, pager_backed = is_source_preserving_page_content()](
          const VmPageOrMarker* p, uint64_t off) {
        if (!pager_backed) {
          if (p->IsInterval()) {
            dprintf(INFO, "found interval at offset 0x%" PRIx64 " in non pager backed vmo\n", off);
            return ZX_ERR_BAD_STATE;
          }
          return ZX_ERR_NEXT;
        }

        if (p->IsInterval()) {
          DEBUG_ASSERT(p->IsIntervalZero());
          DEBUG_ASSERT(p->IsZeroIntervalDirty() || p->IsZeroIntervalUntracked());
          if (p->IsIntervalStart()) {
            if (in_interval) {
              dprintf(INFO, "interval start at 0x%" PRIx64 " while already in interval\n", off);
              return ZX_ERR_BAD_STATE;
            }
            in_interval = true;
            dirty_state = p->GetZeroIntervalDirtyState();
          } else if (p->IsIntervalEnd()) {
            if (!in_interval) {
              dprintf(INFO, "interval end at 0x%" PRIx64 " while not in interval\n", off);
              return ZX_ERR_BAD_STATE;
            }
            if (p->GetZeroIntervalDirtyState() != dirty_state) {
              dprintf(INFO, "dirty state mismatch - start %lu, end %lu\n", (uint64_t)(dirty_state),
                      (uint64_t)(p->GetZeroIntervalDirtyState()));
              return ZX_ERR_BAD_STATE;
            }
            in_interval = false;
            dirty_state = VmPageOrMarker::IntervalDirtyState::Untracked;
          } else {
            if (in_interval) {
              dprintf(INFO, "interval slot at 0x%" PRIx64 " while already in interval\n", off);
              return ZX_ERR_BAD_STATE;
            }
          }
          return ZX_ERR_NEXT;
        }

        if (p->IsReference()) {
          dprintf(INFO, "found compressed ref at offset 0x%" PRIx64 " in pager backed vmo\n", off);
          return ZX_ERR_BAD_STATE;
        }

        if (p->IsPage() && in_interval) {
          dprintf(INFO, "found page at 0x%" PRIx64 " in interval\n", off);
          return ZX_ERR_BAD_STATE;
        }

        if (p->IsMarker() && in_interval) {
          dprintf(INFO, "found marker at 0x%" PRIx64 " in interval\n", off);
          return ZX_ERR_BAD_STATE;
        }
        return ZX_ERR_NEXT;
      });
  return status == ZX_OK;
}

bool VmCowPages::IsLockRangeValidLocked(VmCowRange range) const {
  return range.offset == 0 && range.len == size_locked();
}

zx_status_t VmCowPages::LockRangeLocked(VmCowRange range, zx_vmo_lock_state_t* lock_state_out) {
  canary_.Assert();
  ASSERT(discardable_tracker_);

  if (!IsLockRangeValidLocked(range)) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  if (!lock_state_out) {
    return ZX_ERR_INVALID_ARGS;
  }
  lock_state_out->offset = range.offset;
  lock_state_out->size = range.len;

  discardable_tracker_->assert_cow_pages_locked();

  bool was_discarded = false;
  zx_status_t status =
      discardable_tracker_->LockDiscardableLocked(/*try_lock=*/false, &was_discarded);
  // Locking must succeed if try_lock was false.
  DEBUG_ASSERT(status == ZX_OK);
  lock_state_out->discarded_offset = 0;
  lock_state_out->discarded_size = was_discarded ? size_locked() : 0;

  return status;
}

zx_status_t VmCowPages::TryLockRangeLocked(VmCowRange range) {
  canary_.Assert();
  ASSERT(discardable_tracker_);

  if (!IsLockRangeValidLocked(range)) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  discardable_tracker_->assert_cow_pages_locked();
  bool unused;
  return discardable_tracker_->LockDiscardableLocked(/*try_lock=*/true, &unused);
}

zx_status_t VmCowPages::UnlockRangeLocked(VmCowRange range) {
  canary_.Assert();
  ASSERT(discardable_tracker_);

  if (!IsLockRangeValidLocked(range)) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  discardable_tracker_->assert_cow_pages_locked();
  zx_status_t status = discardable_tracker_->UnlockDiscardableLocked();
  if (status != ZX_OK) {
    return status;
  }
  if (discardable_tracker_->IsEligibleForReclamationLocked()) {
    // Simulate an access to the first page. We use the first page as the discardable trigger, so by
    // simulating an access we ensure that an unlocked VMO is treated as recently accessed
    // equivalent to all other pages. Touching just the first page, instead of all pages, is an
    // optimization as we can simply ignore any attempts to trigger discard from those other pages.
    page_list_.ForEveryPage([](auto* p, uint64_t offset) {
      // Skip over any markers.
      if (!p->IsPage()) {
        return ZX_ERR_NEXT;
      }
      pmm_page_queues()->MarkAccessed(p->Page());
      return ZX_ERR_STOP;
    });
  }
  return status;
}

uint64_t VmCowPages::DebugGetPageCountLocked() const {
  canary_.Assert();
  uint64_t page_count = 0;
  zx_status_t status = page_list_.ForEveryPage([&page_count](auto* p, uint64_t offset) {
    if (!p->IsPageOrRef()) {
      return ZX_ERR_NEXT;
    }
    ++page_count;
    return ZX_ERR_NEXT;
  });
  // We never stop early in lambda above.
  DEBUG_ASSERT(status == ZX_OK);
  return page_count;
}

bool VmCowPages::DebugIsPage(uint64_t offset) const {
  canary_.Assert();
  DEBUG_ASSERT(IS_PAGE_ROUNDED(offset));
  Guard<CriticalMutex> guard{lock()};
  const VmPageOrMarker* p = page_list_.Lookup(offset);
  return p && p->IsPage();
}

bool VmCowPages::DebugIsMarker(uint64_t offset) const {
  canary_.Assert();
  DEBUG_ASSERT(IS_PAGE_ROUNDED(offset));
  Guard<CriticalMutex> guard{lock()};
  const VmPageOrMarker* p = page_list_.Lookup(offset);
  return p && p->IsMarker();
}

bool VmCowPages::DebugIsEmpty(uint64_t offset) const {
  canary_.Assert();
  DEBUG_ASSERT(IS_PAGE_ROUNDED(offset));
  Guard<CriticalMutex> guard{lock()};
  const VmPageOrMarker* p = page_list_.Lookup(offset);
  return !p || p->IsEmpty();
}

vm_page_t* VmCowPages::DebugGetPage(uint64_t offset) const {
  canary_.Assert();
  Guard<CriticalMutex> guard{lock()};
  return DebugGetPageLocked(offset);
}

vm_page_t* VmCowPages::DebugGetPageLocked(uint64_t offset) const {
  canary_.Assert();
  DEBUG_ASSERT(IS_PAGE_ROUNDED(offset));
  const VmPageOrMarker* p = page_list_.Lookup(offset);
  if (p && p->IsPage()) {
    return p->Page();
  }
  return nullptr;
}

bool VmCowPages::DebugIsHighMemoryPriority() const {
  canary_.Assert();
  Guard<CriticalMutex> guard{lock()};
  return is_high_memory_priority_locked();
}

VmCowPages::DiscardablePageCounts VmCowPages::DebugGetDiscardablePageCounts() const {
  canary_.Assert();
  DiscardablePageCounts counts = {};

  // Not a discardable VMO.
  if (!discardable_tracker_) {
    return counts;
  }

  Guard<CriticalMutex> guard{lock()};

  discardable_tracker_->assert_cow_pages_locked();
  const DiscardableVmoTracker::DiscardableState state =
      discardable_tracker_->discardable_state_locked();
  // This is a discardable VMO but hasn't opted into locking / unlocking yet.
  if (state == DiscardableVmoTracker::DiscardableState::kUnset) {
    return counts;
  }

  uint64_t pages = 0;
  page_list_.ForEveryPage([&pages](const auto* p, uint64_t) {
    // TODO(https://fxbug.dev/42138396) Figure out attribution between pages and references.
    if (p->IsPageOrRef()) {
      ++pages;
    }
    return ZX_ERR_NEXT;
  });

  switch (state) {
    case DiscardableVmoTracker::DiscardableState::kReclaimable:
      counts.unlocked = pages;
      break;
    case DiscardableVmoTracker::DiscardableState::kUnreclaimable:
      counts.locked = pages;
      break;
    case DiscardableVmoTracker::DiscardableState::kDiscarded:
      DEBUG_ASSERT(pages == 0);
      break;
    default:
      break;
  }

  return counts;
}

zx::result<uint64_t> VmCowPages::DiscardPagesLocked(DeferredOps& deferred) {
  // Not a discardable VMO.
  if (!discardable_tracker_) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  discardable_tracker_->assert_cow_pages_locked();
  if (!discardable_tracker_->IsEligibleForReclamationLocked()) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  // Remove all pages.
  zx::result<uint64_t> result = UnmapAndFreePagesLocked(0, size_, deferred);

  if (result.is_ok()) {
    reclamation_event_count_++;

    // Set state to discarded.
    discardable_tracker_->SetDiscardedLocked();
  }
  return result;
}

zx::result<uint64_t> VmCowPages::ReclaimDiscardable(vm_page_t* page, uint64_t offset) {
  DEBUG_ASSERT(discardable_tracker_);

  __UNINITIALIZED DeferredOps deferred(this);
  Guard<CriticalMutex> guard{AssertOrderedLock, lock(), lock_order()};

  const VmPageOrMarker* page_or_marker = page_list_.Lookup(offset);
  if (!CanReclaimPageLocked(page, page_or_marker)) {
    return zx::error(ZX_ERR_BAD_STATE);
  }
  // Since CanReclaimPageLocked() succeeded, we know that this page is owned by us at the provided
  // offset. So it should be safe to call MarkAccessed() on the page if reclamation fails, provided
  // we don't drop the lock.

  // Check if this is the first page.
  bool first = false;
  page_list_.ForEveryPage([&first, &offset, &page](auto* p, uint64_t off) {
    if (!p->IsPage()) {
      return ZX_ERR_NEXT;
    }
    first = (p->Page() == page) && off == offset;
    return ZX_ERR_STOP;
  });
  zx::result<uint64_t> result =
      first ? DiscardPagesLocked(deferred) : zx::error(ZX_ERR_INVALID_ARGS);
  if (result.is_error()) {
    // Mark the page accessed so that it's no longer a reclamation candidate. The other error path
    // above already does this inside the CanReclaimPageLocked() helper.
    pmm_page_queues()->MarkAccessed(page);
  }
  return result;
}

void VmCowPages::CopyPageContentsForReplacementLocked(vm_page_t* dst_page, vm_page_t* src_page) {
  DEBUG_ASSERT(!src_page->object.pin_count);
  void* src = paddr_to_physmap(src_page->paddr());
  DEBUG_ASSERT(src);
  void* dst = paddr_to_physmap(dst_page->paddr());
  DEBUG_ASSERT(dst);
  memcpy(dst, src, PAGE_SIZE);
  if (paged_ref_) {
    if (paged_backlink_locked(this)->GetMappingCachePolicyLocked() != ARCH_MMU_FLAG_CACHED) {
      arch_clean_invalidate_cache_range((vaddr_t)dst, PAGE_SIZE);
    }
  }
}

void VmCowPages::CopyPageMetadataForReplacementLocked(vm_page_t* dst_page, vm_page_t* src_page) {
  dst_page->object.share_count = src_page->object.share_count;
  dst_page->object.always_need = src_page->object.always_need;
  DEBUG_ASSERT(!dst_page->object.always_need || (!dst_page->is_loaned() && !src_page->is_loaned()));
  dst_page->object.dirty_state = src_page->object.dirty_state;
}

VmCowPages::DeferredOps::DeferredOps(VmCowPages* self) : self_(self) {
  // If we are referencing a pager backed object then we must acquire the pager hierarchy lock,
  // which requires walking up to the root to find the page_source_.
  if (self_->root_has_page_source()) {
    fbl::RefPtr<PageSource> source;
    {
      Guard<CriticalMutex> guard{AssertOrderedLock, self_->lock(), self_->lock_order()};
      if (self_->life_cycle_ != LifeCycle::Alive) {
        // Although the C++ object is guaranteed to be valid by the caller, it's possible that VMO
        // has transitioned into a dead state. This race can occur typically due to reclamation
        // having to first acquire a RefPtr, then check acquire the lock, then check if the page is
        // still present in the VMO. If the VMO has transitioned to dead then its pages will have
        // been cleared, and so the operation will get skipped.
        // Unfortunately at this point the main lock acquisition and check has not been performed.
        // This is a problem since when dead transitioning the parent_ reference is cleared, meaning
        // we will find a 'fake' root, that will not consequently not have a valid page_source_.
        // So to avoid failing to find a root page_source_ we make sure to terminate if this object
        // is dead.
        // As it is dead and no longer connected to the tree, there is no rest of the hierarchy to
        // synchronize with and so failing to acquire the lock is safe.
        return;
      }
      LockedPtr current;
      while (current.locked_or(self_).parent_) {
        current = LockedPtr(current.locked_or(self_).parent_.get());
      }
      source = current.locked_or(self_).page_source_;
    }
    DEBUG_ASSERT(source);
    page_source_lock_.emplace(source->paged_vmo_lock(), ktl::move(source));
  }
}

VmCowPages::DeferredOps::~DeferredOps() {
  if (range_op_.has_value()) {
    LockedPtr self(self_);
    VmCowPages::RangeChangeUpdateCowChildren(ktl::move(self), range_op_->range, range_op_->op);
  }
  // The pages must be freed *after* any range update is performed, but *before* dropping the
  // |page_source_lock_|. In the case where the page source is handling free this is still a logical
  // operation involving the cow pages and must remain serialized, as demonstrated by FreePages
  // itself taking a reference to the VmCowPages.
  freed_list_.FreePages(self_);
  if (page_source_lock_.has_value()) {
    // When dropping the page_source_lock as we could be holding the last references to the object
    // the mutex must be released first, prior to potentially destroying the object by releasing the
    // refptr.
    page_source_lock_->first.Release();
    page_source_lock_->second.reset();
    page_source_lock_.reset();
  }
}

void VmCowPages::DeferredOps::AddRange(VmCowPages* self, VmCowRange range, RangeChangeOp op) {
  DEBUG_ASSERT(self == self_);
  if (range_op_.has_value()) {
    if (range_op_->op != op) {
      // Permit an UnmapZeroPage to to be upgraded to an Unmap. If already an Unmap, then ignore any
      // UnmapZeroPage.
      if (range_op_->op == RangeChangeOp::UnmapZeroPage && op == RangeChangeOp::Unmap) {
        range_op_->op = op;
      } else {
        DEBUG_ASSERT(range_op_->op == RangeChangeOp::Unmap && op == RangeChangeOp::UnmapZeroPage);
      }
    }
    range_op_->range = range_op_->range.Cover(range);
  } else {
    range_op_ = DeferredRangeOp{.op = op, .range = range};
  }
}

void VmCowPages::InitializePageCache(uint32_t level) {
  ASSERT(level < LK_INIT_LEVEL_THREADING);

  const size_t reserve_pages = 64;
  zx::result<page_cache::PageCache> result = page_cache::PageCache::Create(reserve_pages);

  ASSERT(result.is_ok());
  page_cache_ = ktl::move(result.value());

  if (gBootOptions->pmm_alloc_random_should_wait) {
    page_cache_.SeedRandomShouldWait();
  }
}

// Initialize the cache after the percpu data structures are initialized.
LK_INIT_HOOK(vm_cow_pages_cache_init, VmCowPages::InitializePageCache, LK_INIT_LEVEL_KERNEL)
