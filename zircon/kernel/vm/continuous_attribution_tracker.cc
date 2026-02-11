// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <stdint.h>

#include <vm/continuous_attribution_tracker.h>

ContinuousAttributionTracker::ContinuousAttributionTracker(ContinuousAttributionTracker&& source)
    : current_slots_(source.current_slots_), hwm_slots_(source.hwm_slots_) {
  source.current_slots_ = 0;
  source.hwm_slots_ = 0;
}

ContinuousAttributionTracker& ContinuousAttributionTracker::operator=(
    ContinuousAttributionTracker&& source) {
  current_slots_ = source.current_slots_;
  hwm_slots_ = source.hwm_slots_;
  source.current_slots_ = 0;
  source.hwm_slots_ = 0;
  return *this;
}

uint32_t ContinuousAttributionTracker::FetchHwmAndReset() {
  // TODO(ethanws): Assert that the high-water mark slots are greater than the current slots when
  // all changes to populated slots are reported to the ContinuousAttributionTracker.
  const uint32_t ret = hwm_slots_;
  hwm_slots_ = current_slots_;  // reset
  return ret;
}

uint32_t ContinuousAttributionTracker::FetchCurrent() const { return current_slots_; }
