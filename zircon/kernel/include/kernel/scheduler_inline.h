// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_SCHEDULER_INLINE_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_SCHEDULER_INLINE_H_

// This file defines a number of inline methods out-of-line with the
// declarations in kernel/scheduler.h to reduce the clutter in that header,
// while guaranteeing that callers see the definitions so that inlining may
// occur.

#include <lib/kconcurrent/chainlock_transaction.h>

#include <arch/interrupt.h>
#include <arch/mp.h>
#include <ffl/fixed_format.h>
#include <kernel/scheduler.h>
#include <kernel/scheduler_tracing.h>
#include <ktl/algorithm.h>

// Scales the given value up by the reciprocal of the CPU performance scale.
template <typename T>
inline T Scheduler::ScaleUp(T value) const {
  return value * processing_rate_reciprocal();
}

// Scales the given value down by the CPU performance scale.
template <typename T>
inline T Scheduler::ScaleDown(T value) const {
  return value * processing_rate();
}

// Returns a new flow id when flow tracing is enabled, zero otherwise.
inline uint64_t Scheduler::NextFlowId() {
  if constexpr (LOCAL_KTRACE_LEVEL_ENABLED(FLOW)) {
    return next_flow_id_.fetch_add(1);
  }
  return 0;
}

// Updates the total estimated runtime estimator with the given delta. The
// exported value is scaled by the relative performance factor of the CPU to
// account for performance differences in the estimate.
inline void Scheduler::UpdateTotalExpectedRuntime(SchedDuration delta_ns) {
  rust_scheduler_update_total_expected_runtime(this, delta_ns);
}

// Updates the total deadline utilization estimator with the given delta.
inline void Scheduler::UpdateTotalDeadlineUtilization(SchedUtilization delta) {
  rust_scheduler_update_total_deadline_utilization(this, delta);
}

inline bool Scheduler::UpdateProcessingRate(zx_instant_boot_ticks_t boot_ticks) {
  return rust_scheduler_update_processing_rate(this, boot_ticks);
}

inline void Scheduler::TraceTotalRunnableThreads() const {
  LOCAL_KTRACE_COUNTER(COUNTER, "Queue Length", this_cpu(), ("CPU", runnable_task_count()));
}

inline void Scheduler::RescheduleMask(cpu_mask_t cpus_to_reschedule_mask) {
  rust_scheduler_reschedule_mask(cpus_to_reschedule_mask);
}

inline void Scheduler::RescheduleCpus(cpu_mask_t cpu_mask) {
  InterruptDisableGuard interrupt_disable;
  // TODO(eieio): See if IPIs to idle CPUs can be elided. This may require
  // refactoring some code that updates scheduler bookkeeping and trace
  // counters, such that the modifying CPU does all of the work so the target
  // CPU can remain idle.
  RescheduleMask(cpu_mask);
}

// Writes a context switch record to the ktrace buffer. This is always enabled
// so that user mode tracing can track which threads are running.
inline void SchedTraceContextSwitch(Thread* current_thread, Thread* next_thread,
                                    cpu_num_t current_cpu)
    TA_REQ(current_thread->get_lock(), next_thread->get_lock()) {
  rust_sched_trace_context_switch(current_thread, next_thread, current_cpu);
}

// Writes a thread wakeup record to the ktrace buffer. This is always enabled
// so that user mode tracing can track which threads are waking.
inline void SchedTraceWakeup(Thread* thread, cpu_num_t target_cpu) TA_REQ(thread->get_lock()) {
  rust_sched_trace_wakeup(thread, target_cpu);
}

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_SCHEDULER_INLINE_H_
