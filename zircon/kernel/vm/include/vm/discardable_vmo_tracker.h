// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_DISCARDABLE_VMO_TRACKER_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_DISCARDABLE_VMO_TRACKER_H_

#include <assert.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <ktl/utility.h>
#include <object/opaque_storage.h>
#include <vm/vm_cow_pages.h>

class DiscardableVmoTracker;

__BEGIN_CDECLS

void rust_discardable_vmo_tracker_init(DiscardableVmoTracker* tracker);
void rust_discardable_vmo_tracker_destroy(DiscardableVmoTracker* tracker);
void rust_discardable_vmo_tracker_init_cow_pages(DiscardableVmoTracker* tracker, VmCowPages* cow);
VmCowPages* rust_discardable_vmo_tracker_debug_get_cow(const DiscardableVmoTracker* tracker);
void rust_discardable_vmo_tracker_remove_from_discardable_list_locked(
    DiscardableVmoTracker* tracker);
zx_status_t rust_discardable_vmo_tracker_lock_discardable_locked(
    DiscardableVmoTracker* tracker, bool try_lock, bool* was_discarded_out,
    bool* updated_reclaim_candidates_out);
zx_status_t rust_discardable_vmo_tracker_unlock_discardable_locked(
    DiscardableVmoTracker* tracker, bool* updated_reclaim_candidates_out);
bool rust_discardable_vmo_tracker_is_eligible_for_reclamation_locked(
    const DiscardableVmoTracker* tracker);
bool rust_discardable_vmo_tracker_was_discarded_locked(const DiscardableVmoTracker* tracker);
void rust_discardable_vmo_tracker_set_discarded_locked(DiscardableVmoTracker* tracker);
void rust_discardable_vmo_tracker_debug_discardable_page_counts(
    VmCowPages::DiscardablePageCounts* out_counts);
uint8_t rust_discardable_vmo_tracker_discardable_state_locked(const DiscardableVmoTracker* tracker);
uint64_t rust_discardable_vmo_tracker_debug_get_lock_count(const DiscardableVmoTracker* tracker);
bool rust_discardable_vmo_tracker_debug_is_reclaimable(const DiscardableVmoTracker* tracker);
bool rust_discardable_vmo_tracker_debug_is_unreclaimable(const DiscardableVmoTracker* tracker);
bool rust_discardable_vmo_tracker_debug_is_discarded(const DiscardableVmoTracker* tracker);

__END_CDECLS

// Tracks state relevant for discardable VMOs. This class offers separation of the logic
// required for discardable VMO management; the members are still protected by the owning
// VmCowPages' lock.
class DiscardableVmoTracker final {
 public:
  DiscardableVmoTracker() { rust_discardable_vmo_tracker_init(this); }
  ~DiscardableVmoTracker() { rust_discardable_vmo_tracker_destroy(this); }

  DiscardableVmoTracker(const DiscardableVmoTracker&) = delete;
  DiscardableVmoTracker& operator=(const DiscardableVmoTracker&) = delete;
  DiscardableVmoTracker(DiscardableVmoTracker&&) = delete;
  DiscardableVmoTracker& operator=(DiscardableVmoTracker&&) = delete;

  void InitCowPages(VmCowPages* cow) {
    ASSERT(cow);
    rust_discardable_vmo_tracker_init_cow_pages(this, cow);
  }

  // See comment near discardable_state_ for details.
  enum class DiscardableState : uint8_t {
    kUnset = 0,
    kReclaimable = 1,
    kUnreclaimable = 2,
    kDiscarded = 3,
  };
  static_assert(sizeof(DiscardableState) == 1);
  static_assert(static_cast<uint8_t>(DiscardableState::kUnset) == 0);
  static_assert(static_cast<uint8_t>(DiscardableState::kReclaimable) == 1);
  static_assert(static_cast<uint8_t>(DiscardableState::kUnreclaimable) == 2);
  static_assert(static_cast<uint8_t>(DiscardableState::kDiscarded) == 3);

  // The |cow_lock| parameter of the *Locked methods below must be the lock of the VmCowPages that
  // owns this tracker (i.e. |cow_->lock()| in the original C++ implementation). TA_REQ(cow_lock)
  // only proves that the caller holds the lock it passed in, so each method additionally
  // DEBUG_ASSERTs that it is the owning VmCowPages' lock.

  // Remove a discardable object from whichever global discardable list it is in.
  // Called from the VmCowPages destructor. Also resets the cow_ back reference.
  void RemoveFromDiscardableListLocked(Lock<CriticalMutex>& cow_lock) TA_REQ(cow_lock) {
    AssertIsCowLock(cow_lock);
    rust_discardable_vmo_tracker_remove_from_discardable_list_locked(this);
  }

  // Lock and unlock functions. Returns ZX_OK if the operation succeeded or an error code
  // if it failed, along with whether the VMO was moved between the discardable reclaimable
  // and non-reclaimable lists. The intent of this is to inform the caller if they might need
  // to update any book-keeping depending on whether the VMO becomes reclaimable or
  // unreclaimable.
  ktl::pair<zx_status_t, bool> LockDiscardableLocked(Lock<CriticalMutex>& cow_lock, bool try_lock,
                                                     bool* was_discarded_out) TA_REQ(cow_lock) {
    AssertIsCowLock(cow_lock);
    ASSERT(was_discarded_out);
    bool updated_reclaim = false;
    zx_status_t status = rust_discardable_vmo_tracker_lock_discardable_locked(
        this, try_lock, was_discarded_out, &updated_reclaim);
    return ktl::make_pair(status, updated_reclaim);
  }
  ktl::pair<zx_status_t, bool> UnlockDiscardableLocked(Lock<CriticalMutex>& cow_lock)
      TA_REQ(cow_lock) {
    AssertIsCowLock(cow_lock);
    bool updated_reclaim = false;
    zx_status_t status =
        rust_discardable_vmo_tracker_unlock_discardable_locked(this, &updated_reclaim);
    return ktl::make_pair(status, updated_reclaim);
  }

  // Returns whether this object qualifies for reclamation based on whether its state is
  // kReclaimable.
  bool IsEligibleForReclamationLocked(Lock<CriticalMutex>& cow_lock) const TA_REQ(cow_lock) {
    AssertIsCowLock(cow_lock);
    return rust_discardable_vmo_tracker_is_eligible_for_reclamation_locked(this);
  }

  // Whether the VMO has been discarded and not locked again yet.
  bool WasDiscardedLocked(Lock<CriticalMutex>& cow_lock) const TA_REQ(cow_lock) {
    AssertIsCowLock(cow_lock);
    return rust_discardable_vmo_tracker_was_discarded_locked(this);
  }

  // Mark the VMO as discarded.
  void SetDiscardedLocked(Lock<CriticalMutex>& cow_lock) TA_REQ(cow_lock) {
    AssertIsCowLock(cow_lock);
    rust_discardable_vmo_tracker_set_discarded_locked(this);
  }

  // Returns the total number of pages locked and unlocked across all discardable vmos.
  // Note that this might not be exact and we might miss some vmos, because the
  // |DiscardableVmosLock| is dropped after processing each vmo on the global discardable lists.
  // That is fine since these numbers are only used for accounting.
  using DiscardablePageCounts = VmCowPages::DiscardablePageCounts;
  static DiscardablePageCounts DebugDiscardablePageCounts() {
    DiscardablePageCounts counts = {};
    rust_discardable_vmo_tracker_debug_discardable_page_counts(&counts);
    return counts;
  }

  // Accessors for private members.
  DiscardableState discardable_state_locked(Lock<CriticalMutex>& cow_lock) const TA_REQ(cow_lock) {
    AssertIsCowLock(cow_lock);
    return static_cast<DiscardableState>(
        rust_discardable_vmo_tracker_discardable_state_locked(this));
  }

  // Debug functions exposed for testing.
  uint64_t DebugGetLockCount() const {
    return rust_discardable_vmo_tracker_debug_get_lock_count(this);
  }
  bool DebugIsReclaimable() const {
    return rust_discardable_vmo_tracker_debug_is_reclaimable(this);
  }
  bool DebugIsUnreclaimable() const {
    return rust_discardable_vmo_tracker_debug_is_unreclaimable(this);
  }
  bool DebugIsDiscarded() const { return rust_discardable_vmo_tracker_debug_is_discarded(this); }

 private:
  // Asserts that |cow_lock| is the lock of the VmCowPages that owns this tracker.
  void AssertIsCowLock(const Lock<CriticalMutex>& cow_lock) const {
    DEBUG_ASSERT(&cow_lock == rust_discardable_vmo_tracker_debug_get_cow(this)->lock());
  }

  [[maybe_unused]] OpaqueStorage<48, 8> storage_{};
};

static_assert(sizeof(DiscardableVmoTracker) == 48);
static_assert(alignof(DiscardableVmoTracker) == 8);

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_DISCARDABLE_VMO_TRACKER_H_
