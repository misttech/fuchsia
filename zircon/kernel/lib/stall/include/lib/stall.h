// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_LIB_STALL_INCLUDE_LIB_STALL_H_
#define ZIRCON_KERNEL_LIB_STALL_INCLUDE_LIB_STALL_H_

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

#endif  // ZIRCON_KERNEL_LIB_STALL_INCLUDE_LIB_STALL_H_
