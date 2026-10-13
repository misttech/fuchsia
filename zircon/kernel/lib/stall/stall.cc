// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/stall.h"

#include <kernel/ffi.h>
StallAccumulator::StallAccumulator() { rust_stall_accumulator_init(this); }

void StallAccumulator::Update(int op_contributors_progressing, int op_contributors_stalling) {
  rust_stall_accumulator_update(this, op_contributors_progressing, op_contributors_stalling);
}

void StallAccumulator::ApplyContextSwitch(Thread *current_thread, Thread *next_thread) {
  rust_stall_apply_context_switch(current_thread, next_thread);
}

zx::result<StallObserver *> StallObserver::Create(zx_duration_mono_t threshold,
                                                  zx_duration_mono_t window,
                                                  EventReceiver *event_receiver) {
  StallObserver *observer = nullptr;
  zx_status_t status = rust_stall_observer_create(threshold, window, event_receiver, &observer);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(observer);
}

void StallObserver::Destroy(StallObserver *observer) { rust_stall_observer_destroy(observer); }

StallAggregator::Stats StallAggregator::ReadStats() {
  Stats result;
  rust_stall_aggregator_read_stats(&result);
  return result;
}

void StallAggregator::AddObserverSome(StallObserver *observer) {
  rust_stall_aggregator_add_observer_some(observer);
}

void StallAggregator::RemoveObserverSome(StallObserver *observer) {
  rust_stall_aggregator_remove_observer_some(observer);
}

void StallAggregator::AddObserverFull(StallObserver *observer) {
  rust_stall_aggregator_add_observer_full(observer);
}

void StallAggregator::RemoveObserverFull(StallObserver *observer) {
  rust_stall_aggregator_remove_observer_full(observer);
}

extern "C" {

FFI_ALWAYS_INLINE void cpp_stall_observer_event_receiver_on_above_threshold(
    StallObserver::EventReceiver *event) {
  event->OnAboveThreshold();
}

FFI_ALWAYS_INLINE void cpp_stall_observer_event_receiver_on_below_threshold(
    StallObserver::EventReceiver *event) {
  event->OnBelowThreshold();
}
}
