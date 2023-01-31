// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_TASK_RUNTIME_STATS_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_TASK_RUNTIME_STATS_H_

#include <lib/kconcurrent/copy.h>
#include <lib/kconcurrent/seqlock.h>
#include <lib/relaxed_atomic.h>
#include <zircon/syscalls/object.h>
#include <zircon/time.h>

#include <kernel/lockdep.h>
#include <kernel/scheduler_state.h>

//
// Types and utilities for efficiently accumulating and aggregating task runtime stats.
//
// Runtime stats are maintained at three levels: thread, process, and job. Threads maintain and
// update their runtime stats, actively rolling up to their owning process, whenever relevant
// scheduling operations occur. Terminating processes roll up to their owning job, however, running
// processes under a job are aggregated on demand.
//
// Per-thread stats are maintained by ThreadRuntimeStats, which provides a sequence locked snapshot
// of the runtime stats with an affordance to compensate for unaccounted runtime/queuetime when a
// thread is in a runnable state (i.e. ready or running).
//
// Per-process stats are maintained by ProcessRuntimeStats, which provides a sequence locked
// snapshot of the runtime stats. However, a similar compensation affordance is not provided, since
// process stats are the sum of the constituent thread stats and on-demand aggregation can be
// expensive while holding the process dispatcher lock. Consequently, process runtime stats may
// slightly lag the total compensated runtimes when any of the threads are runnable.
//

// Runtime stats of a thread, process, or job.
struct TaskRuntimeStats {
  // The total duration spent running on a CPU.
  zx_duration_t cpu_time = 0;

  // The total duration spent ready to start running.
  zx_duration_t queue_time = 0;

  // The total duration (in ticks) spent handling page faults.
  zx_ticks_t page_fault_ticks = 0;

  // The total duration (in ticks) spent contented on kernel locks.
  zx_ticks_t lock_contention_ticks = 0;

  // Adds another TaskRuntimeStats to this one.
  constexpr TaskRuntimeStats& operator+=(const TaskRuntimeStats& other) {
    cpu_time = zx_duration_add_duration(cpu_time, other.cpu_time);
    queue_time = zx_duration_add_duration(queue_time, other.queue_time);
    page_fault_ticks = zx_ticks_add_ticks(page_fault_ticks, other.page_fault_ticks);
    lock_contention_ticks = zx_ticks_add_ticks(lock_contention_ticks, other.lock_contention_ticks);
    return *this;
  }

  // Conversion to zx_info_task_runtime_t.
  operator zx_info_task_runtime_t() const;
};

// Manages sequence locked updates and access to per-thread runtime stats.
class ThreadRuntimeStats {
  template <typename>
  struct LockOption {};

 public:
  struct ThreadStats {
    thread_state state = thread_state::THREAD_INITIAL;  // Last state.
    zx_time_t state_time = 0;                           // When the thread entered state.
    zx_duration_t cpu_time = 0;                         // Time spent on CPU.
    zx_duration_t queue_time = 0;                       // Time spent ready to start running.
  };

  ThreadRuntimeStats() = default;

  // Returns a coherent snapshot of the ThreadStats state.
  ThreadStats Read() const TA_EXCL(seq_lock_) {
    ThreadStats stats;
    bool success;

    do {
      {
        Guard<SeqLock, SharedNoIrqSave> guard{&seq_lock_, success};
        published_stats_.Read(stats, concurrent::SyncOpt_AcqRelOps);
      }
      if (!success) {
        arch::Yield();
      }
    } while (!success);

    return stats;
  }

  // Update must be locked Exclusive with either IrqSave or NoIrqSave.
  static constexpr LockOption<ExclusiveIrqSave> IrqSave{};
  static constexpr LockOption<ExclusiveNoIrqSave> NoIrqSave{};

  // Updates the ThreadStats state with the given deltas and last thread state.
  template <typename ExclusiveOption>
  void Update(const ThreadStats& delta, LockOption<ExclusiveOption>) TA_EXCL(seq_lock_) {
    Guard<SeqLock, ExclusiveOption> guard{&seq_lock_};

    // Update the accumulators and last state. The unsynchronized reads are protected by the
    // spinlock semantics of SeqLock exclusive acquire.
    ThreadStats stats = published_stats_.unsynchronized_get();
    stats.cpu_time = zx_duration_add_duration(stats.cpu_time, delta.cpu_time);
    stats.queue_time = zx_duration_add_duration(stats.queue_time, delta.queue_time);
    stats.state = delta.state;
    stats.state_time = delta.state_time;

    // Update the published snapshot.
    // TODO(fxbug.dev/121343): Evaluate whether this fence is safe to remove for a small efficiency
    // boost.
    published_stats_.Update(stats, concurrent::SyncOpt_AcqRelOps);
  }

  // Updates the page fault / lock contention ticks with the given deltas. These values do not
  // require relative coherence with other state.
  void AddPageFaultTicks(zx_ticks_t delta) { page_fault_ticks_.fetch_add(delta); }
  void AddLockContentionTicks(zx_ticks_t delta) { lock_contention_ticks_.fetch_add(delta); }

  // Returns the instantaneous runtime stats for the thread, compensated for unaccounted time when
  // the thread is runnable up until the given time. This value must not be aggregated into process
  // or job runtime stats members, since the corrections are only partials of the actual values
  // accumulated when the thread changes state. Use ProcessStats for measuring aggregate process
  // runtime, as threads automatically aggregate to their owning process at the appropriate state
  // changes.
  TaskRuntimeStats GetCompensatedTaskRuntimeStats(zx_time_t now) const TA_EXCL(seq_lock_) {
    const ThreadStats stats = Read();
    TaskRuntimeStats task_stats = {
        .cpu_time = stats.cpu_time,
        .queue_time = stats.queue_time,
        .page_fault_ticks = page_fault_ticks_,
        .lock_contention_ticks = lock_contention_ticks_,
    };

    // Adjust for the current time when the thread is runnable (i.e. ready or running).
    const zx_duration_t unaccounted_delta = zx_duration_sub_duration(now, stats.state_time);
    if (stats.state == thread_state::THREAD_RUNNING) {
      task_stats.cpu_time = zx_duration_add_duration(task_stats.cpu_time, unaccounted_delta);
    } else if (stats.state == thread_state::THREAD_READY) {
      task_stats.queue_time = zx_duration_add_duration(task_stats.queue_time, unaccounted_delta);
    }

    return task_stats;
  }

 private:
  mutable DECLARE_SEQLOCK(ThreadRuntimeStats) seq_lock_;
  concurrent::WellDefinedCopyable<ThreadStats> published_stats_ TA_GUARDED(seq_lock_){};
  RelaxedAtomic<zx_ticks_t> page_fault_ticks_{0};
  RelaxedAtomic<zx_ticks_t> lock_contention_ticks_{0};
};

// Manages sequence locked updates and access to aggregate per-process runtime stats.
class ProcessRuntimeStats {
  template <typename>
  struct LockOption {};

 public:
  struct ProcessStats {
    zx_duration_t cpu_time = 0;
    zx_duration_t queue_time = 0;
  };

  ProcessRuntimeStats() = default;

  ProcessRuntimeStats(const ProcessRuntimeStats&) = delete;
  ProcessRuntimeStats& operator=(const ProcessRuntimeStats&) = delete;

  // Returns a coherent snapshot of the ProcessStats state.
  ProcessStats Read() const TA_EXCL(seq_lock_) {
    ProcessStats stats;
    bool success;

    do {
      {
        Guard<SeqLock, SharedNoIrqSave> guard{&seq_lock_, success};
        published_stats_.Read(stats, concurrent::SyncOpt_AcqRelOps);
      }
      if (!success) {
        arch::Yield();
      }
    } while (!success);

    return stats;
  }

  // Update must be locked Exclusive with either IrqSave or NoIrqSave.
  static constexpr LockOption<ExclusiveIrqSave> IrqSave{};
  static constexpr LockOption<ExclusiveNoIrqSave> NoIrqSave{};

  // Updates the ProcessStats state with the given deltas.
  template <typename ExclusiveOption>
  void Update(const ProcessStats& delta, LockOption<ExclusiveOption>) TA_EXCL(seq_lock_) {
    Guard<SeqLock, ExclusiveOption> guard{&seq_lock_};

    // Update the accumulators. The unsynchronized reads are protected by the spinlock semantics of
    // SeqLock exclusive acquire.
    ProcessStats stats = published_stats_.unsynchronized_get();
    stats.cpu_time = zx_duration_add_duration(stats.cpu_time, delta.cpu_time);
    stats.queue_time = zx_duration_add_duration(stats.queue_time, delta.queue_time);

    // Update the published snapshot.
    // TODO(fxbug.dev/121343): Evaluate whether this fence is safe to remove for a small efficiency
    // boost.
    published_stats_.Update(stats, concurrent::SyncOpt_AcqRelOps);
  }

  // Updates the page fault / lock contention ticks with the given deltas. These values do not
  // require relative coherence with other state.
  void AddPageFaultTicks(zx_ticks_t ticks) { page_fault_ticks_.fetch_add(ticks); }
  void AddLockContentionTicks(zx_ticks_t ticks) { lock_contention_ticks_.fetch_add(ticks); }

  // Returns the tracked aggregates as a TaskRuntimeStats instance. This value is appropriate to
  // accumulate into the job runtime stats when a process terminates.
  TaskRuntimeStats GetTaskRuntimeStats() const TA_EXCL(seq_lock_) {
    const ProcessStats stats = Read();
    return {
        .cpu_time = stats.cpu_time,
        .queue_time = stats.queue_time,
        .page_fault_ticks = page_fault_ticks_,
        .lock_contention_ticks = lock_contention_ticks_,
    };
  }

 private:
  mutable DECLARE_SEQLOCK(ProcessRuntimeStats) seq_lock_;
  concurrent::WellDefinedCopyable<ProcessStats> published_stats_ TA_GUARDED(seq_lock_){};
  RelaxedAtomic<zx_ticks_t> page_fault_ticks_{0};
  RelaxedAtomic<zx_ticks_t> lock_contention_ticks_{0};
};

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_TASK_RUNTIME_STATS_H_
