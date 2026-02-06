// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_CONTINUOUS_ATTRIBUTION_TRACKER_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_CONTINUOUS_ATTRIBUTION_TRACKER_H_

#include <assert.h>
#include <stdint.h>

#include <ktl/algorithm.h>
#include <ktl/utility.h>

// Tracks the number of populated slots in the VmCowPages' local page list. If the VmCowPages
// changes a slot to being populated, or vice versa, that should be reported to
// ContinuousAttributionTracker.
class ContinuousAttributionTracker final {
 public:
  ContinuousAttributionTracker() = default;
  ~ContinuousAttributionTracker() = default;

  // Move and move assignment zero the |source|.
  ContinuousAttributionTracker(ContinuousAttributionTracker&& source);
  ContinuousAttributionTracker& operator=(ContinuousAttributionTracker&& source);
  ContinuousAttributionTracker(ContinuousAttributionTracker&) = delete;
  ContinuousAttributionTracker& operator=(ContinuousAttributionTracker&) = delete;

  // Returns the tracked count of populated slots.
  uint32_t FetchCurrent() const;

  // Get the greatest number of populated slots since the statistic was last reset.
  //
  // Resets the high-water mark.
  uint32_t FetchHwmAndReset();

  // Increments the count by |by|. This quantity must be strictly positive.
  void Increment(uint32_t by) {
    DEBUG_ASSERT(by > 0);
    [[maybe_unused]] const bool did_overflow = add_overflow(current_slots_, by, &current_slots_);
    // Overflow would require the page list to be tracking 2 ^ 32 - 1 populated slots, which
    // suggests a system with terabytes of physical memory. This is not currently supported.
    DEBUG_ASSERT(!did_overflow);
    hwm_slots_ = ktl::max(hwm_slots_, current_slots_);
  }

  // Decrements the count by |by|. This quantity must be strictly positive.
  void Decrement(uint32_t by) {
    DEBUG_ASSERT(by > 0);
    [[maybe_unused]] const bool did_overflow = sub_overflow(current_slots_, by, &current_slots_);
    // TODO(ethanws): Assert this did not overflow when all changes to the populated slots count are
    // reported to the ContinuousAttributionTracker.
    ktl::ignore = did_overflow;
    DEBUG_ASSERT(hwm_slots_ >= current_slots_);
  }

 private:
  // The number of populated slots in the local page list.
  uint32_t current_slots_ = 0;

  // The greatest number of current_slots_ since the high-water mark value was last reset.
  uint32_t hwm_slots_ = 0;
};

// ContinuousAttributionTracker uses a 32-bit count to represent the number of populated slots in
// the local page list. Since a ContinuousAttributionTracker is intended to be stored inline in a
// VmCowPages, reducing its size (by not using a 64-bit count) is a substantial memory saving for
// the system.
static_assert(sizeof(ContinuousAttributionTracker) == 8);

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_CONTINUOUS_ATTRIBUTION_TRACKER_H_
