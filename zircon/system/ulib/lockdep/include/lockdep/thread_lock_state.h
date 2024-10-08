// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#pragma once

#include <stdint.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>

#include <utility>

#include <fbl/intrusive_double_list.h>
#include <lockdep/common.h>
#include <lockdep/lock_class_state.h>

namespace lockdep {

// Linked list entry that tracks a lock acquired by a thread. Each thread
// maintains a local list of AcquiredLockEntry instances. AcquiredLockEntry is
// intended to be allocated on the stack as a member of a RAII type to manage
// the lifetime of the acquisition. Consequently, this type is move-only to
// permit moving the context to a different stack frame. However, an instance
// must only be manipulated by the thread that created it.
class AcquiredLockEntry : public fbl::DoublyLinkedListable<AcquiredLockEntry*> {
 public:
  AcquiredLockEntry() = default;
  AcquiredLockEntry(void* address, LockClassId id, uintptr_t order)
      : address_{address}, id_{id}, order_{order} {}

  ~AcquiredLockEntry() { ZX_DEBUG_ASSERT(!InContainer()); }

  AcquiredLockEntry(const AcquiredLockEntry&) = delete;
  AcquiredLockEntry& operator=(const AcquiredLockEntry&) = delete;

  AcquiredLockEntry(AcquiredLockEntry&& other) noexcept { *this = std::move(other); }
  AcquiredLockEntry& operator=(AcquiredLockEntry&& other) noexcept {
    if (this != &other) {
      ZX_ASSERT(!InContainer());

      // Fill out these values first.  If we end up calling Replace, it will
      // need to know the lock class ID in order to fetch the proper thread lock
      // state structure from the system layer.
      address_ = other.address_;
      id_ = other.id_;
      order_ = other.order_;

      if (other.InContainer()) {
        Replace(&other);
      }

      other.Clear();
    }
    return *this;
  }

  void* address() const { return address_; }
  LockClassId id() const { return id_; }
  uintptr_t order() const { return order_; }

  void Clear() {
    address_ = nullptr;
    id_ = kInvalidLockClassId;
    order_ = 0;
  }

 private:
  friend class ThreadLockState;

  // Replaces the given entry in the list with this entry.
  void Replace(AcquiredLockEntry* target);

  void* address_{nullptr};
  LockClassId id_{kInvalidLockClassId};
  uintptr_t order_{0};
};

// Tracks the locks held by a thread and updates accounting during acquire and
// release operations.
class ThreadLockState {
 public:
  ThreadLockState() = default;
  ~ThreadLockState() = default;

  ThreadLockState(const ThreadLockState&) = delete;
  ThreadLockState& operator=(const ThreadLockState&) = delete;

  // Returns the ThreadLockState instance for the current thread.
  static ThreadLockState* Get(LockFlags lock_flags) { return SystemGetThreadLockState(lock_flags); }

  // Attempts to add the given lock class to the acquired lock list. Lock
  // ordering and other checks are performed here.
  void Acquire(AcquiredLockEntry* lock_entry) {
    if (ValidatorLockClassState::IsTrackingDisabled(lock_entry->id()))
      return;

    if (ValidatorLockClassState::IsReportingDisabled(lock_entry->id()))
      reporting_disabled_count_++;

    // If reporting is disabled don't modify last_result_.  For example, we
    // might be inside a call to Report that has ended up acquiring some lock
    // (think printf) and don't want that acquire to overwrite the reported
    // value.
    if (!reporting_disabled()) {
      last_result_ = LockResult::Success;
    }

    // Scans the acquired lock list and performs the following operations:
    //  1. Checks that there are no leaf locks in (at the end of) the list.
    //  2. Checks that the given lock class is not already in the list unless
    //     the lock class is multi-acquire, or is nestable and external/address
    //     ordering is correctly applied.
    //  3. Checks that the given lock instance is not already in the list.
    //  4. Checks that the given lock class is not in the dependency set for
    //     any lock class already in the list.
    //  5. Checks that irq-safe locks are not held when acquiring an irq-unsafe
    //     lock.
    //  6. Adds each lock class already in the list to the dependency set of the
    //     given lock class.
    for (AcquiredLockEntry& entry : acquired_locks_) {
      if (ValidatorLockClassState::IsLeaf(entry.id())) {
        Report(lock_entry, &entry, LockResult::AcquireAfterLeaf);
      } else if (entry.id() == lock_entry->id()) {
        if (lock_entry->address() == entry.address()) {
          Report(lock_entry, &entry, LockResult::Reentrance);
        } else if (!ValidatorLockClassState::IsMultiAcquire(lock_entry->id()) &&
                   lock_entry->order() <= entry.order()) {
          if (!ValidatorLockClassState::IsNestable(lock_entry->id()) && lock_entry->order() == 0) {
            Report(lock_entry, &entry, LockResult::AlreadyAcquired);
          } else {
            Report(lock_entry, &entry, LockResult::InvalidNesting);
          }
        }
      } else {
        const LockResult result =
            ValidatorLockClassState::AddLockClass(lock_entry->id(), entry.id());
        if (result == LockResult::Success) {
          // A new edge has been added to the graph, trigger a loop
          // detection pass.
          TriggerLoopDetection();
        } else if (result == LockResult::MaxLockDependencies) {
          // If the dependency set is full report error.
          Report(lock_entry, &entry, result);
        } else /* if (result == LockResult::DependencyExists) */ {
          // Nothing to do when there are no changes to the graph.
        }

        // The following tests only need to be run when a new edge is
        // added for this ordered pair of locks; when the edge already
        // exists these tests have been performed before.
        if (result == LockResult::Success) {
          const bool entry_irqsafe = ValidatorLockClassState::IsIrqSafe(entry.id());
          const bool lock_entry_irqsafe = ValidatorLockClassState::IsIrqSafe(lock_entry->id());
          if (entry_irqsafe && !lock_entry_irqsafe) {
            Report(lock_entry, &entry, LockResult::InvalidIrqSafety);
          }

          if (ValidatorLockClassState::HasLockClass(entry.id(), lock_entry->id())) {
            Report(lock_entry, &entry, LockResult::OutOfOrder);
          }
        }
      }
    }

    if (!ValidatorLockClassState::IsActiveListDisabled(lock_entry->id())) {
      acquired_locks_.push_back(lock_entry);
    }
  }

  // Removes the given lock entry from the acquired lock list.
  void Release(AcquiredLockEntry* entry) {
    if (ValidatorLockClassState::IsTrackingDisabled(entry->id()))
      return;

    if (ValidatorLockClassState::IsReportingDisabled(entry->id()))
      reporting_disabled_count_--;

    if (entry->InContainer())
      acquired_locks_.erase(*entry);
  }

  void AssertNoLocksHeld() {
    if (!acquired_locks_.is_empty()) {
      // For simplicity just generate an error for the most recently acquired lock.
      SystemLockValidationFatal(&acquired_locks_.back(), this, __GET_CALLER(0), __GET_FRAME(0),
                                LockResult::ShouldNotHold);
    }
  }

  // Returns result of the last Acquire operation for testing.
  LockResult last_result() const { return last_result_; }

  bool reporting_disabled() const { return reporting_disabled_count_ > 0; }

 private:
  friend ThreadLockState* SystemGetThreadLockState();
  friend void SystemInitThreadLockState(ThreadLockState*);
  friend void AcquiredLockEntry::Replace(AcquiredLockEntry*);

  // Replaces the given original entry with the replacement entry. This permits
  // lock entries to be allocated on the stack and migrate between stack
  // frames if lock guards are moved or returned.
  //
  // The original entry must already be on the acquired locks list and the
  // replacement entry must not be on any list.
  void Replace(AcquiredLockEntry* original, AcquiredLockEntry* replacement) {
    acquired_locks_.replace(*original, replacement);
  }

  // Reports a detected lock violation using the system-defined runtime handler.
  void Report(AcquiredLockEntry* bad_entry, AcquiredLockEntry* conflicting_entry,
              LockResult result) {
    if ((result == LockResult::AlreadyAcquired || result == LockResult::InvalidNesting) &&
        ValidatorLockClassState::IsReAcquireFatal(bad_entry->id())) {
      SystemLockValidationFatal(bad_entry, this, __GET_CALLER(0), __GET_FRAME(0),
                                LockResult::AlreadyAcquired);
    }
    if (result == LockResult::AcquireAfterLeaf) {
      SystemLockValidationFatal(bad_entry, this, __GET_CALLER(0), __GET_FRAME(0),
                                LockResult::AcquireAfterLeaf);
    }

    if (!reporting_disabled()) {
      reporting_disabled_count_++;

      SystemLockValidationError(bad_entry, conflicting_entry, this, __GET_CALLER(0), __GET_FRAME(0),
                                result);

      reporting_disabled_count_--;

      // Update the last result for testing.
      if (last_result_ == LockResult::Success) {
        last_result_ = result;
      }
    }
  }

  // Triggers a loop detection by the system-defined runtime handler.
  void TriggerLoopDetection() {
    if (!reporting_disabled()) {
      reporting_disabled_count_++;

      SystemTriggerLoopDetection();

      reporting_disabled_count_--;
    }
  }

  // Tracks the lock classes acquired by the current thread.
  fbl::DoublyLinkedList<AcquiredLockEntry*> acquired_locks_{};

  // Tracks the number of locks held that have the LockFlagsReportingDisabled
  // flag set. Reporting and loop detection are not triggered when this count
  // is greater than zero. This value is also incremented by one for the
  // duration of a report or loop detection trigger to prevent recursive calls
  // due to locks acquired by the system-defined runtime API.
  uint16_t reporting_disabled_count_{0};

  // Tracks the result of the last Acquire operation for testing.
  LockResult last_result_{LockResult::Success};
};

// Defined after ThreadLockState because of dependency on its methods.
inline void AcquiredLockEntry::Replace(AcquiredLockEntry* target) {
  LockFlags flags = ValidatorLockClassState::Get(id_)->flags();
  ThreadLockState::Get(flags)->Replace(target, this);
}

// Generates a fatal system report if the current thread holds any tracked locks.
inline void AssertNoLocksHeld() {
  if constexpr (kLockValidationEnabled) {
    ThreadLockState::Get(LockFlags::LockFlagsNone)->AssertNoLocksHeld();
  }
}

}  // namespace lockdep
