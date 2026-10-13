// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_LIB_STALL_INCLUDE_LIB_STALL_H_
#define ZIRCON_KERNEL_LIB_STALL_INCLUDE_LIB_STALL_H_

#include <lib/zx/result.h>
#include <zircon/types.h>

#include <kernel/spinlock.h>
#include <kernel/thread.h>

// Maintains per-CPU stall timers in real time.
//
// Its counters are conceptually continuous. In fact, we update the saved state whenever the
// conditions change, so we can always compute the current value by extrapolating.
//
// With respect to stall contributions, threads are always in one of these three states:
//  - Not contributing at all to any accumulator.
//  - Contributing to an accumulator as a progressing thread.
//  - Contributing to an accumulator as a stalling thread.
class StallAccumulator {
 public:
  StallAccumulator();
  StallAccumulator(const StallAccumulator &) = delete;
  StallAccumulator(StallAccumulator &&) = delete;
  StallAccumulator &operator=(const StallAccumulator &) = delete;
  StallAccumulator &operator=(StallAccumulator &&) = delete;

  // Alter contributor counts by the given amount.
  //
  // Only values between -1 and +1 are accepted.
  void Update(int op_contributors_progressing, int op_contributors_stalling);

  // Internally called by the scheduler at every context switch.
  static void ApplyContextSwitch(Thread *current_thread, Thread *next_thread)
      TA_REQ(current_thread->get_lock(), next_thread->get_lock());

 private:
  // Storage mirroring the Rust `StallAccumulator` in ../../src/mod.rs, which owns this object:
  // Rust initializes it, acquires its lock and updates its counters. C++ only reserves the
  // storage - a `struct percpu` holds one by value - and forwards the two calls above over FFI,
  // so none of these fields are ever read here.
  //
  // //zircon/kernel/kernel/percpu.rs asserts that the two layouts agree.
  struct Storage {
    // A Rust `ksync::KMutex<ksync::RawSpinlock>`, which mirrors `Lock<SpinLock>`.
    alignas(Lock<SpinLock>) uint8_t lock[sizeof(Lock<SpinLock>)];

    // Number of progressing threads currently tracked by this structure.
    size_t num_contributors_progressing;

    // Number of stalling threads currently tracked by this structure.
    size_t num_contributors_stalling;

    // Timestamp of the last consolidate() call.
    zx_instant_mono_t last_consolidate_time;

    // Accumulated totals at the time of the last update.
    struct {
      zx_duration_mono_t total_time_stall_some;
      zx_duration_mono_t total_time_stall_full;
      zx_duration_mono_t total_time_active;
    } accumulated_stats;
  };
  // Never read here: the FFI calls above hand Rust the whole object, which begins with it.
  [[maybe_unused]] Storage storage_ = {};
};

extern "C" {

// Moves the stall contributions of the outgoing and incoming threads across a context switch.
void rust_stall_apply_context_switch(Thread *current_thread, Thread *next_thread);

void rust_stall_accumulator_init(StallAccumulator *accumulator);

void rust_stall_accumulator_update(StallAccumulator *accumulator, int op_contributors_progressing,
                                   int op_contributors_stalling);
}

// A stall observer that keeps a circular queue with the last N samples (where N corresponds to the
// number of samples covering the requested time window).
//
// Every time a new sample is pushed into it, it tests if the sum of all the stored samples is
// greater than or equal to the given threshold value and then notifies its callback function
// accordingly.
//
// Instances live in Rust (see ../../src/mod.rs); this is an opaque handle to one, so C++ only ever
// holds pointers. Removing an observer from the StallAggregator is what guarantees the sampling
// thread is not part way through a callback, so always unregister before destroying.
class StallObserver {
 public:
  class EventReceiver {
   public:
    virtual void OnAboveThreshold() = 0;
    virtual void OnBelowThreshold() = 0;
  };

  StallObserver() = delete;

  static zx::result<StallObserver *> Create(zx_duration_mono_t threshold, zx_duration_mono_t window,
                                            EventReceiver *event_receiver);

  static void Destroy(StallObserver *observer);
};

// Maintains system-wide stall stats by periodically aggregating measurements from per-CPU
// `StallAccumulator`s.
//
// The implementation and the singleton instance live in Rust (see ../../src/mod.rs); these are
// forwarders to it.
class StallAggregator {
 public:
  struct Stats {
    // Total monotonic time spent with at least one memory-stalled thread.
    zx_duration_mono_t stalled_time_some = 0;

    // Total monotonic time spent with all threads memory-stalled.
    zx_duration_mono_t stalled_time_full = 0;
  };

  // Returns the values of the aggregated stats.
  static Stats ReadStats();

  static void AddObserverSome(StallObserver *observer);
  static void RemoveObserverSome(StallObserver *observer);

  static void AddObserverFull(StallObserver *observer);
  static void RemoveObserverFull(StallObserver *observer);
};

extern "C" {

zx_status_t rust_stall_observer_create(zx_duration_mono_t threshold, zx_duration_mono_t window,
                                       StallObserver::EventReceiver *event_receiver,
                                       StallObserver **out_observer);

void rust_stall_observer_destroy(StallObserver *observer);

void rust_stall_aggregator_read_stats(StallAggregator::Stats *out_stats);

void rust_stall_aggregator_add_observer_some(StallObserver *observer);
void rust_stall_aggregator_remove_observer_some(StallObserver *observer);

void rust_stall_aggregator_add_observer_full(StallObserver *observer);
void rust_stall_aggregator_remove_observer_full(StallObserver *observer);
}

#endif  // ZIRCON_KERNEL_LIB_STALL_INCLUDE_LIB_STALL_H_
