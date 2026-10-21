// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "kernel/scheduler.h"

#include <assert.h>
#include <debug.h>
#include <inttypes.h>
#include <lib/arch/intrin.h>
#include <lib/counters.h>
#include <lib/kconcurrent/chainlock.h>
#include <lib/kconcurrent/chainlock_transaction.h>
#include <lib/ktrace.h>
#include <lib/sched/affine.h>
#include <lib/zircon-internal/macros.h>
#include <platform.h>
#include <stdio.h>
#include <string.h>
#include <zircon/errors.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <arch/interrupt.h>
#include <arch/ops.h>
#include <arch/thread.h>
#include <ffl/string.h>
#include <kernel/auto_preempt_disabler.h>
#include <kernel/cpu.h>
#include <kernel/event_limiter.h>
#include <kernel/ffi.h>
#include <kernel/lockdep.h>
#include <kernel/mp.h>
#include <kernel/percpu.h>
#include <kernel/scheduler_inline.h>
#include <kernel/scheduler_state.h>
#include <kernel/scheduler_tracing.h>
#include <kernel/thread.h>
#include <ktl/algorithm.h>
#include <ktl/forward.h>
#include <ktl/limits.h>
#include <ktl/new.h>
#include <ktl/span.h>
#include <ktl/tuple.h>
#include <ktl/utility.h>
#include <object/process_dispatcher.h>
#include <object/thread_dispatcher.h>
#include <vm/vm.h>

// TODO(https://fxbug.dev/322207536): Stop resetting start and finish times
// when unblocking once we solve races higher in the stack.
#include <lib/boot-options/boot-options.h>

#include <ktl/enforce.h>

using ffl::Round;

// Counts the number of times we set the preemption timer to fire at a point
// that's prior to the time at which the CPU last entered the scheduler.  See
// also the comment where this counter is used.
KCOUNTER(counter_preempt_past, "scheduler.preempt_past")

// Counts the number of times we dequeue a deadline thread whose finish_time is
// earlier than its eligible_time.
KCOUNTER(counter_deadline_past, "scheduler.deadline_past")

// Counts the number of times a thread's effective profile or affinity changed
// while transitioning from READY to RUNNING.
KCOUNTER(counter_update_in_transition, "scheduler.update_in_transition")

// Counts the number of times we had to run FindTargetCpu again because our
// selected Target became in-active after we chose it.
KCOUNTER(counter_find_target_cpu_retries, "scheduler.find_target_cpu.retries")

// Counts the number of times the fair timeline was snapped forward to make a
// fair thread eligible to run.
KCOUNTER(counter_fair_timeline_snap_count, "scheduler.fair-timeline-snap.count")

// Counts the amount of variable time the fair timeline was snapped forward to
// make a fair thread eligible to run.
KCOUNTER(counter_fair_timeline_snap_time, "scheduler.fair-timeline-snap.time")

namespace {

// Reset this CPU's preemption timer to fire at |deadline|.
//
// |current_cpu| is the current CPU.
//
// |now| is the time that was latched when the CPU entered the scheduler.
//
// Must be called with interrupts disabled.
void PreemptReset(cpu_num_t current_cpu, zx_instant_mono_t now, zx_instant_mono_t deadline) {
  ktrace::Scope trace =
      LOCAL_KTRACE_BEGIN_SCOPE(DETAILED, "preempt_reset", ("now", now), ("deadline", deadline));
  // Setting a preemption time that's prior to the point at which the CPU
  // entered the scheduler indicates at worst a bug, or at best a wasted
  // reschedule.
  //
  // What do we mean by wasted reschedule?  When leaving the scheduler, if the
  // preemption timer was set to a point prior to entering the scheduler, it
  // will immediately fire once we've re-enabled interrupts.  When it fires,
  // we'll then re-enter the scheduler (provided that preemption is not
  // disabled) without running the the current ask for any appreciable amount of
  // time.  Instead, we should have set the preemption timer to a point that's
  // after the latched entry time and avoided an unnecessary interrupt and trip
  // through the scheduler.
  //
  // TODO(https://fxbug.dev/42182770): For now, simply count the number of times
  // this happens.  Once we have eliminated all causes of "preemption time in
  // the past" (whether they are bugs or simply missed optimization
  // opportunities) replace this counter with a DEBUG_ASSERT.
  //
  // Aside from simple bugs, how can we end up with a preemption time that's
  // earlier than the time at which we last entered the scheduler?  Consider the
  // case where the current thread, A, is blocking and the run queue contains
  // just one thread, B, that's deadline scheduled and whose finish time has
  // already elapsed.  In other words, B's finish time is earlier than the time
  // at which we last entered the scheduler.  In this case, we'll end up
  // scheduling B and setting the preemption timer to B's finish time.  Longer
  // term, we should consider resetting or reactivating all eligible tasks whose
  // finish times would be earlier than the point at which we entered the
  // scheduler.  See also |counter_deadline_past|.
  if (deadline < now) {
    kcounter_add(counter_preempt_past, 1);
  }

  percpu::Get(current_cpu).timer_queue.PreemptReset(deadline);
}

}  // anonymous namespace

void Scheduler::CountUpdateInTransition() { counter_update_in_transition.Add(1); }

fxt::StringRef<fxt::RefType::kId> Scheduler::ToStringRef(PreemptType preempt_type) {
  using fxt::operator""_intern;
  switch (preempt_type) {
    case PreemptType::Irq:
      return "irq"_intern;
    case PreemptType::FlushPending:
      return "flush-pending"_intern;
    case PreemptType::Reschedule:
      return "reschedule"_intern;
    default:
      return "[unknown]"_intern;
  }
}

// Records details about the threads entering/exiting the run queues for various
// CPUs, as well as which task on each CPU is currently active. These events are
// used for trace analysis to compute statistics about overall utilization,
// taking CPU affinity into account.
inline void Scheduler::TraceThreadQueueEvent(const fxt::InternedString& name,
                                             const Thread* thread) const {
  // Traces marking the end of a queue/dequeue operation have arguments encoded
  // as follows:
  //
  // arg0[ 0..64] : TID
  //
  // arg1[ 0..15] : CPU availability mask.
  // arg1[16..19] : CPU_ID of the affected queue.
  // arg1[20..27] : Number of runnable tasks on this CPU after the queue event.
  // arg1[28..28] : 1 == fair, 0 == deadline
  // arg1[29..29] : 1 == eligible, 0 == ineligible
  // arg1[30..30] : 1 == idle thread, 0 == normal thread
  //
  if constexpr (SCHEDULER_QUEUE_TRACING_ENABLED) {
    const zx_instant_mono_t now = current_mono_time();  // TODO(johngro): plumb this in from above
    const bool fair = IsFairThread(thread);
    const bool eligible = fair || (thread->scheduler_state().start_time_ <= now);
    const size_t cnt =
        rust_state_.fair_run_queue.size() + rust_state_.critical_deadline_run_queue.size() +
        rust_state_.deadline_run_queue.size() +
        ((rust_state_.active_thread && !rust_state_.active_thread->IsIdle()) ? 1 : 0);

    const uint64_t arg0 = thread->tid();
    const uint64_t arg1 =
        (thread->scheduler_state().GetEffectiveCpuMask(PeekActiveMask()) & 0xFFFF) |
        (ktl::clamp<uint64_t>(this_cpu(), 0, 0xF) << 16) |
        (ktl::clamp<uint64_t>(cnt, 0, 0xFF) << 20) | ((fair ? 1 : 0) << 28) |
        ((eligible ? 1 : 0) << 29) | ((thread->IsIdle() ? 1 : 0) << 30);

    KTrace::Probe(KTrace::Context::Cpu, name, arg0, arg1);
  }
}

void Scheduler::Dump(FILE* output_target, bool queue_state_only) {
  rust_kernel_scheduler_dump(this, output_target, queue_state_only);
}

void Scheduler::DumpActiveThread(FILE* output_target) {
  rust_kernel_scheduler_dump_active_thread(this, output_target);
}

SchedWeight Scheduler::GetTotalWeight() const {
  Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
  return rust_state_.weight_total;
}

size_t Scheduler::GetRunnableTasks() const {
  Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
  const int64_t total_runnable_tasks = runnable_task_count();
  return static_cast<size_t>(total_runnable_tasks);
}

// Performs an augmented binary search for the task with the earliest finish
// time that also has a start time equal to or later than the given eligible
// time. An optional predicate may be supplied to filter candidates based on
// additional conditions.
//
// The tree is ordered by start time and is augmented by maintaining an
// additional invariant: each task node in the tree stores the minimum finish
// time of its descendents, including itself, in addition to its own start and
// finish time. The combination of these three values permits traversinng the
// tree along a perfect partition of minimum finish times with eligible start
// times.
//
// See fbl/wavl_tree_best_node_observer.h for an explanation of how the
// augmented invariant is maintained.
Thread* Scheduler::FindEarliestEligibleThread(RunQueue* run_queue, SchedTime eligible_time) {
  Thread* out = rust_kernel_run_queue_find_earliest_eligible_thread(run_queue, eligible_time,
                                                                    nullptr, nullptr);
  if (out != nullptr) {
    AssertInScheduler(*out);
  }
  return out;
}

template <typename Predicate>
Thread* Scheduler::FindEarliestEligibleThread(RunQueue* run_queue, SchedTime eligible_time,
                                              Predicate&& predicate) {
  Thread* out = rust_kernel_run_queue_find_earliest_eligible_thread(
      run_queue, eligible_time,
      [](const Thread* iter, const void* ctx) {
        return (*static_cast<ktl::remove_reference_t<Predicate>*>(const_cast<void*>(ctx)))(iter);
      },
      &predicate);
  if (out != nullptr) {
    AssertInScheduler(*out);
  }
  return out;
}

Scheduler* Scheduler::Get() { return Get(arch_curr_cpu_num()); }

Scheduler* Scheduler::Get(cpu_num_t cpu) {
  AssertLayout();
  return &percpu::Get(cpu).scheduler;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dump_active(const Thread* thread,
                                              FILE* output_target) TA_NO_THREAD_SAFETY_ANALYSIS {
  fprintf(output_target, "thread: pid=%lu tid=%lu\n", thread->pid(), thread->tid());
  const ThreadDispatcher* user_thread = thread->user_thread();
  if (user_thread != nullptr) {
    const ProcessDispatcher* process = user_thread->process();
    char name[ZX_MAX_NAME_LEN]{};
    [[maybe_unused]] zx_status_t status = process->get_name(name);
    DEBUG_ASSERT(status == ZX_OK);
    fprintf(output_target, "process: name=%s\n", name);
  }
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Scheduler* cpp_scheduler_get(cpu_num_t cpu) { return Scheduler::Get(cpu); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_block(Thread* current_thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(current_thread != nullptr);
  Scheduler::Block(current_thread);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_unblock(Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(thread != nullptr);
  Scheduler::Unblock(thread);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_unblock_list(Thread::UnblockList* list)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(list != nullptr);
  Scheduler::Unblock(ktl::move(*list));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_thread_base_profile_changed(Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(thread != nullptr);
  Scheduler::ThreadBaseProfileChanged(*thread);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_upstream_thread_base_profile_changed_thread(
    const Thread* upstream, Thread* target) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(upstream != nullptr);
  DEBUG_ASSERT(target != nullptr);
  Scheduler::UpstreamThreadBaseProfileChanged(*upstream, *target);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_upstream_thread_base_profile_changed_owq(
    const Thread* upstream, OwnedWaitQueue* target) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(upstream != nullptr);
  DEBUG_ASSERT(target != nullptr);
  Scheduler::UpstreamThreadBaseProfileChanged(*upstream, *target);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_join_node_to_pi_graph_thread_thread(
    const Thread* upstream, Thread* target,
    ForceInheritance force_inheritance) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(upstream != nullptr);
  DEBUG_ASSERT(target != nullptr);
  Scheduler::JoinNodeToPiGraph(*upstream, *target, force_inheritance);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_join_node_to_pi_graph_thread_owq(
    const Thread* upstream, OwnedWaitQueue* target,
    ForceInheritance force_inheritance) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(upstream != nullptr);
  DEBUG_ASSERT(target != nullptr);
  Scheduler::JoinNodeToPiGraph(*upstream, *target, force_inheritance);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_join_node_to_pi_graph_owq_thread(
    const OwnedWaitQueue* upstream, Thread* target,
    ForceInheritance force_inheritance) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(upstream != nullptr);
  DEBUG_ASSERT(target != nullptr);
  Scheduler::JoinNodeToPiGraph(*upstream, *target, force_inheritance);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_join_node_to_pi_graph_owq_owq(
    const OwnedWaitQueue* upstream, OwnedWaitQueue* target,
    ForceInheritance force_inheritance) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(upstream != nullptr);
  DEBUG_ASSERT(target != nullptr);
  Scheduler::JoinNodeToPiGraph(*upstream, *target, force_inheritance);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_split_node_from_pi_graph_thread_thread(
    Thread* upstream, Thread* target,
    const SchedulerState::EffectiveProfile* old_target_ep) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(upstream != nullptr);
  DEBUG_ASSERT(target != nullptr);
  Scheduler::SplitNodeFromPiGraph(*upstream, *target, old_target_ep);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_split_node_from_pi_graph_thread_owq(
    Thread* upstream, OwnedWaitQueue* target,
    const SchedulerState::EffectiveProfile* old_target_ep) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(upstream != nullptr);
  DEBUG_ASSERT(target != nullptr);
  Scheduler::SplitNodeFromPiGraph(*upstream, *target, old_target_ep);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_split_node_from_pi_graph_owq_thread(
    OwnedWaitQueue* upstream, Thread* target,
    const SchedulerState::EffectiveProfile* old_target_ep) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(upstream != nullptr);
  DEBUG_ASSERT(target != nullptr);
  Scheduler::SplitNodeFromPiGraph(*upstream, *target, old_target_ep);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_split_node_from_pi_graph_owq_owq(
    OwnedWaitQueue* upstream, OwnedWaitQueue* target,
    const SchedulerState::EffectiveProfile* old_target_ep) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(upstream != nullptr);
  DEBUG_ASSERT(target != nullptr);
  Scheduler::SplitNodeFromPiGraph(*upstream, *target, old_target_ep);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE cpu_stats* cpp_get_cpu_stats(cpu_num_t cpu_num) TA_NO_THREAD_SAFETY_ANALYSIS {
  return &percpu::Get(cpu_num).stats;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_scheduler_power_state_active_power_coefficient_nw(
    const Scheduler* sched) TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.active_power_coefficient_nw();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_scheduler_power_level_control_max_idle_power_coefficient_nw(
    const Scheduler* sched) TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.max_idle_power_coefficient_nw();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_lock_queue(Scheduler* sched) TA_NO_THREAD_SAFETY_ANALYSIS {
  sched->queue_lock_.lock().Acquire(SOURCE_TAG);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_unlock_queue(Scheduler* sched) TA_NO_THREAD_SAFETY_ANALYSIS {
  sched->queue_lock_.lock().Release();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_scheduler_lock_queue_irq_save(Scheduler* sched)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  interrupt_saved_state_t state;
  sched->queue_lock_.lock().AcquireIrqSave(state, SOURCE_TAG);
  return static_cast<uint64_t>(state);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_unlock_queue_irq_restore(Scheduler* sched, uint64_t state)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  sched->queue_lock_.lock().ReleaseIrqRestore(static_cast<interrupt_saved_state_t>(state));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_scheduler_has_pending_power_work(const Scheduler* sched) {
  return static_cast<bool>(percpu::Get(sched->this_cpu()).idle_power_thread.pending_power_work());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_update_runtime_stats(Thread* thread, int32_t state) {
  thread->UpdateRuntimeStats(static_cast<thread_state>(state));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE int32_t cpp_thread_get_disposition(const Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return static_cast<int32_t>(thread->disposition());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_scheduler_get_idle_thread(const Scheduler* sched)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return &percpu::Get(sched->this_cpu()).idle_power_thread.thread();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_scheduler_power_level_control_is_enabled(const Scheduler* sched)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.is_enabled();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_scheduler_power_state_scheduler_control_enabled(const Scheduler* sched)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.scheduler_control_enabled();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_scheduler_power_state_is_serving(const Scheduler* sched)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.is_serving();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_scheduler_power_state_is_valid_active_power_level(
    const Scheduler* sched, uint8_t power_level) TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.IsValidActivePowerLevel(power_level);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE cpu_mask_t cpp_scheduler_power_state_domain_cpus(const Scheduler* sched)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.domain()->cpus();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE SchedProcessingRate cpp_scheduler_power_state_target_processing_rate(
    const Scheduler* sched) TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.target_processing_rate();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE SchedProcessingRate cpp_scheduler_power_state_preceding_target_processing_rate(
    const Scheduler* sched) TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.preceding_target_processing_rate();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_scheduler_power_state_has_preceding_target_power_level(
    const Scheduler* sched) TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.has_preceding_target_power_level();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const power_management::PowerLevel*
cpp_scheduler_power_state_find_active_power_level_for_rate(
    const Scheduler* sched, SchedProcessingRate rate) TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.domain()->model().FindActivePowerLevelForRate(rate);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint8_t
cpp_power_level_get_level(const power_management::PowerLevel* power_level) {
  return power_level->level();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_scheduler_power_state_request_transition(
    Scheduler* sched, cpu_num_t cpu, uint8_t power_level,
    power_management::PowerLevelUpdateRequest* out_req) TA_NO_THREAD_SAFETY_ANALYSIS {
  const ktl::optional<power_management::PowerLevelUpdateRequest> request =
      sched->power_state_.RequestTransition(cpu, power_level);
  *out_req = request.value_or(power_management::PowerLevelUpdateRequest{});
  return request.has_value();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE cpu_mask_t cpp_scheduler_power_state_post_update_request(
    const Scheduler* sched,
    const power_management::PowerLevelUpdateRequest* req) TA_NO_THREAD_SAFETY_ANALYSIS {
  const zx::result<cpu_mask_t> result = sched->power_state_.domain()->controller()->Post(*req);
  if (result.is_ok()) {
    return result.value();
  }
  KERNEL_OOPS("Failed to set OPP: domain_id=%" PRIu32 " control_argument=%" PRIu64 ": %d\n",
              req->domain_id, req->control_argument, result.status_value());
  return 0;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_scheduler_power_state_update_active_power_level(
    Scheduler* sched, uint8_t power_level,
    SchedProcessingRate* out_active_rate) TA_NO_THREAD_SAFETY_ANALYSIS {
  const zx::result result = sched->power_state_.UpdateActivePowerLevel(power_level);
  if (result.is_ok()) {
    *out_active_rate = sched->power_state_.active_processing_rate();
    return ZX_OK;
  }
  return result.status_value();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_timer_queue_preempt_reset(cpu_num_t cpu, zx_instant_mono_t deadline) {
  percpu::Get(cpu).timer_queue.PreemptReset(deadline);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_timer_queue_preempt_armed(cpu_num_t cpu) {
  return percpu::Get(cpu).timer_queue.PreemptArmed();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_call_context_switch_fn_locked(Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  thread->CallContextSwitchFnLocked();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE VmAspace* cpp_thread_get_aspace(Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread->aspace();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vmm_context_switch(VmAspace* old_aspace, VmAspace* new_aspace) {
  vmm_context_switch(old_aspace, new_aspace);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_arch_context_switch(Thread* current_thread,
                                               Thread* next_thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  arch_context_switch(current_thread, next_thread);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_signal_check_restartable_sequence_if_needed(Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  thread->SignalCheckRestartableSequenceIfNeeded();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE SchedulerQueueState* cpp_thread_get_queue_state(Thread* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return &thread->scheduler_queue_state();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_koid_t cpp_thread_get_tid(const Thread* thread) { return thread->tid(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const CpuSearchSet::Entry* cpp_get_cpu_search_set_entries(cpu_num_t cpu) {
  return percpu::Get(cpu).search_set.const_iterator().data();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE size_t cpp_get_cpu_search_set_count(cpu_num_t cpu) {
  return percpu::Get(cpu).search_set.const_iterator().size();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_canary_assert(const Thread* thread) { thread->canary().Assert(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE size_t cpp_scheduler_get_power_domain_set_count(const Scheduler* sched)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.power_domain_set().count();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_scheduler_lookup_power_cost(
    const Scheduler* sched, cpu_num_t cpu, SchedProcessingRate rate) TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.power_domain_set().LookupPowerCost(cpu, rate);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE SchedUtilization cpp_scheduler_lookup_total_normalized_utilization(
    const Scheduler* sched, cpu_num_t cpu) TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.power_domain_set().LookupTotalNormalizedUtilization(cpu);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE SchedUtilization cpp_scheduler_power_level_control_update_normalized_utilization(
    Scheduler* sched, SchedUtilization delta) TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.UpdateUtilization(delta);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_scheduler_power_level_control_domain_id(const Scheduler* sched)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.domain()->id();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE SchedUtilization cpp_scheduler_power_level_control_total_normalized_utilization(
    const Scheduler* sched) TA_NO_THREAD_SAFETY_ANALYSIS {
  return sched->power_state_.total_normalized_utilization();
}

void Scheduler::InitializeThread(Thread* thread, const SchedulerState::BaseProfile& profile) {
  new (&thread->scheduler_state()) SchedulerState{profile};
  thread->scheduler_state().expected_runtime_ns_ =
      profile.IsFair() ? kMinimumFairCapacity : profile.deadline.capacity_ns;
}

// Initialize the first thread to run on the current CPU.  Called from
// thread_construct_first, this method will initialize the thread's scheduler
// state, then mark the thread as being "active" in its cpu's scheduler.
void Scheduler::InitializeFirstThread(Thread* thread) {
  cpu_num_t current_cpu = arch_curr_cpu_num();

  // Construct our scheduler state and assign a "priority"
  InitializeThread(thread, SchedulerState::BaseProfile{HIGHEST_PRIORITY});

  // Fill out other details about the thread, making sure to assign it to the
  // current CPU with hard affinity.
  SchedulerState& state = thread->scheduler_state();
  state.state_ = THREAD_RUNNING;
  state.curr_cpu_ = current_cpu;
  state.last_cpu_ = current_cpu;
  state.hard_affinity_ = cpu_num_to_mask(current_cpu);

  // Finally, make sure that the thread is the active thread for the scheduler,
  // and that the weight_total bookkeeping is accurate.
  {
    Scheduler* scheduler = Get(current_cpu);
    Guard<MonitoredSpinLock, NoIrqSave> queue_guard{&scheduler->queue_lock_, SOURCE_TAG};
    scheduler->AssertInScheduler(*thread);

    SchedulerQueueState& queue_state = thread->scheduler_queue_state();
    queue_state.OnInsert();
    scheduler->rust_state_.active_thread = thread;

    scheduler->rust_state_.weight_total = state.effective_profile_.weight();
    scheduler->rust_state_.runnable_task_count++;
    scheduler->UpdateTotalExpectedRuntime(state.expected_runtime_ns_);
  }
}

// Remove the impact of a CPUs first thread from the scheduler's bookkeeping.
//
// During initial startup, threads are not _really_ being scheduled, yet they
// can still do things like obtain locks and block, resulting in profile
// inheritance.  In order to hold the scheduler's bookkeeping invariants, we
// assign these threads a fair weight, and include it in the total fair weight
// tracked by the scheduler instance.  When the thread either becomes the idle
// thread (as the boot CPU first thread does), or exits (as secondary CPU first
// threads do), it is important that we remove this weight from the total
// bookkeeping.  However, this is not as simple as just changing the thread's
// weight via ChangeWeight, as idle threads are special cases who contribute no
// weight to the total.
//
// So, this small method simply fixes up the bookkeeping before allowing the
// thread to move on to become the idle thread (boot CPU), or simply exiting
// (secondary CPU).
void Scheduler::RemoveFirstThread(Thread* thread) {
  cpu_num_t current_cpu = arch_curr_cpu_num();
  Scheduler* scheduler = Get(current_cpu);
  SchedulerState& state = thread->scheduler_state();

  // Since this is becoming an idle thread, it must have been one of the CPU's
  // first threads.  It should already be bound to this core with hard affinity.
  // Assert this.
  DEBUG_ASSERT(state.last_cpu_ == current_cpu);
  DEBUG_ASSERT(state.curr_cpu_ == current_cpu);
  DEBUG_ASSERT(state.hard_affinity_ == cpu_num_to_mask(current_cpu));

  {
    Guard<MonitoredSpinLock, NoIrqSave> queue_guard{&scheduler->queue_lock_, SOURCE_TAG};
    scheduler->AssertInScheduler(*thread);
    SchedulerQueueState& queue_state = thread->scheduler_queue_state();

    // We are becoming the idle thread.  We should currently be running with a
    // fair (not deadline) profile, and we should not be holding any locks
    // (therefore, we should not be inheriting any profile pressure).
    DEBUG_ASSERT(state.base_profile_.IsFair());
    DEBUG_ASSERT(state.inherited_profile_values_.total_weight == SchedWeight{0});
    DEBUG_ASSERT(state.inherited_profile_values_.uncapped_utilization == SchedUtilization{0});

    // We should also be the currently active thread on this core, but no
    // longer.  We are about to either exit, or "UnblockIdle".
    DEBUG_ASSERT(scheduler->rust_state_.active_thread == thread);
    DEBUG_ASSERT(scheduler->rust_state_.runnable_task_count > 0);
    queue_state.OnRemove(INVALID_CPU);  // Set active to false.
    scheduler->rust_state_.active_thread = nullptr;
    scheduler->rust_state_.weight_total -= state.effective_profile_.weight();
    scheduler->rust_state_.runnable_task_count--;
    scheduler->UpdateTotalExpectedRuntime(-state.expected_runtime_ns_);

    state.base_profile_.fair.weight = SchedulerState::ConvertPriorityToWeight(IDLE_PRIORITY);
    state.effective_profile_.MarkBaseProfileChanged();
    state.RecomputeEffectiveProfile();
  }
}

// Removes the thread at the head of the first eligible run queue. If there is
// an eligible deadline thread, it takes precedence over available fair
// threads. If there is no eligible work, attempt to steal work from other busy
// CPUs.
Scheduler::DequeueResult Scheduler::DequeueThread(
    SchedTime now, Guard<MonitoredSpinLock, NoIrqSave>& queue_guard) {
  if (IsDeadlineThreadEligible(now)) {
    return DequeueDeadlineThread(now);
  }
  if (likely(!rust_state_.fair_run_queue.is_empty())) {
    return DequeueFairThread(now);
  }

  // Release the queue lock while attempting to steal work, leaving IRQs
  // disabled.  Latch our scale up factor to use while determining whether or
  // not we can steal a given thread before we drop our lock.
  // TODO(eieio): Since the processing rate is CPU-local and not accessed across
  // processors, it should be unnecessary to latch the value. Change the
  // annotations to allow this.
  DequeueResult result;
  queue_guard.CallUnlocked([&, processing_rate = power_level_control_.processing_rate()] {
    ChainLockTransaction::AssertActive();
    result = StealWork(now, processing_rate);
  });

  if (result) {
    return result;
  }

  return &percpu::Get(this_cpu()).idle_power_thread.thread();
}

// Attempts to steal work from other busy CPUs and move it to the local run
// queues. Returns a pointer to the stolen thread that is now associated with
// the local Scheduler instance, or nullptr is no work was stolen.
Scheduler::DequeueResult Scheduler::StealWork(SchedTime now, SchedProcessingRate processing_rate) {
  DequeueResult result;
  rust_scheduler_steal_work(this, now, processing_rate, &result);
  return result;
}

// Dequeues the eligible thread with the earliest virtual finish time. The
// caller must ensure that there is at least one thread in the queue.
Scheduler::DequeueResult Scheduler::DequeueFairThread(SchedTime monotonic_eligible_time) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(DETAILED, "dequeue_fair_thread");

  // Convert monotonic eligible time to variable time to compare with fair run
  // queue values.
  SchedTime variable_eligible_time =
      rust_state_.fair_affine_transform.MonotonicToVariable(monotonic_eligible_time);

  // Use MarkHasOwnedThreadAccess to satisfy static annotations for shared
  // access to the thread's COVs and exclusive access to the thread's SOVs. This
  // is acceptable because the queue lock is required by this method and the
  // thread is in a queue protectect by the same lock.
  const auto& earliest_thread = rust_state_.fair_run_queue.front();
  MarkHasOwnedThreadAccess(earliest_thread);

  // If there are no eligible fair threads, snap the variable timeline to the
  // earliest start time.
  const SchedTime earliest_start = earliest_thread.scheduler_state().start_time_;
  if (variable_eligible_time < earliest_start) {
    counter_fair_timeline_snap_count.Add(1);
    counter_fair_timeline_snap_time.Add(Round<int64_t>(earliest_start - variable_eligible_time));

    rust_state_.fair_affine_transform.Snap(monotonic_eligible_time, earliest_start);
    variable_eligible_time = earliest_start;
  }

  Thread* const eligible_thread =
      FindEarliestEligibleThread(&rust_state_.fair_run_queue, variable_eligible_time);

  DEBUG_ASSERT_MSG(
      eligible_thread != nullptr,
      "vearliest_start=%" PRId64 ", veligible_time=%" PRId64 " , vstart_time=%" PRId64
      ", vfinish_time=%" PRId64 ", vmin_finish_time=%" PRId64 "!",
      earliest_start.raw_value(), variable_eligible_time.raw_value(),
      earliest_thread.scheduler_state().start_time_.raw_value(),
      earliest_thread.scheduler_state().finish_time_.raw_value(),
      earliest_thread.scheduler_state().subtree_invariants_.min_finish_time.raw_value());

  // Same argument as before for the use of the no-op assert.  We are holding
  // the queue lock, and we just found this thread in the scheduler.
  MarkHasOwnedThreadAccess(*eligible_thread);
  rust_state_.fair_run_queue.erase(*eligible_thread);
  TraceThreadQueueEvent("tqe_deque_fair"_intern, eligible_thread);
  DEBUG_ASSERT(eligible_thread->disposition() == Disposition::Associated);

  const SchedulerState& state = const_cast<const Thread*>(eligible_thread)->scheduler_state();
  const SchedTime mono_start_time_ns =
      rust_state_.fair_affine_transform.VariableToMonotonic(state.start_time());
  const SchedTime mono_finish_time_ns =
      rust_state_.fair_affine_transform.VariableToMonotonic(state.finish_time());

  trace = KTRACE_END_SCOPE(("var_start_time", state.start_time().raw_value()),
                           ("var_finish_time", state.finish_time().raw_value()),
                           ("mono_start_time", mono_start_time_ns.raw_value()),
                           ("mono_finish_time", mono_finish_time_ns.raw_value()));

  return DequeueResult{eligible_thread, mono_start_time_ns, mono_finish_time_ns};
}

// Dequeues the eligible thread with the earliest deadline. The caller must
// ensure that there is at least one eligible thread in either the critical
// deadline run queue or the deadline run queue.
Scheduler::DequeueResult Scheduler::DequeueDeadlineThread(SchedTime eligible_time) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(DETAILED, "dequeue_deadline_thread");
  DEBUG_ASSERT(!rust_state_.critical_deadline_run_queue.is_empty() ||
               !rust_state_.deadline_run_queue.is_empty());

  const Thread* critical_thread =
      FindEarliestEligibleThread(&rust_state_.critical_deadline_run_queue, eligible_time);
  const bool is_oversubscribed =
      power_level_control_.normalized_deadline_utilization() > kCpuUtilizationLimit;

  const Thread* eligible_thread = nullptr;

  if (is_oversubscribed && critical_thread) {
    eligible_thread = critical_thread;
  } else {
    const Thread* normal_thread =
        FindEarliestEligibleThread(&rust_state_.deadline_run_queue, eligible_time);

    // There must be at least one eligible deadline thread, whether critical or
    // normal.
    DEBUG_ASSERT(critical_thread || normal_thread);

    eligible_thread = EarliestDeadlineThread(critical_thread, normal_thread);
  }

  DEBUG_ASSERT_MSG(eligible_thread != nullptr, "eligible_time=%" PRId64, eligible_time.raw_value());

  // Use MarkHasOwnedThreadAccess to satisfy static annotations for shared
  // access to the thread's COVs and exclusive access to the thread's SOVs. This
  // is acceptable because the queue lock is required by this method and the
  // thread is in a queue protectect by the same lock.
  MarkHasOwnedThreadAccess(*eligible_thread);
  RunQueue* target_queue = IsCriticalDeadlineThread(eligible_thread)
                               ? &rust_state_.critical_deadline_run_queue
                               : &rust_state_.deadline_run_queue;
  target_queue->erase(*const_cast<Thread*>(eligible_thread));
  TraceThreadQueueEvent("tqe_deque_deadline"_intern, eligible_thread);
  DEBUG_ASSERT(eligible_thread->disposition() == Disposition::Associated);

  const SchedulerState& state = eligible_thread->scheduler_state();
  if (state.finish_time() <= eligible_time) {
    kcounter_add(counter_deadline_past, 1);
  }
  trace =
      KTRACE_END_SCOPE(("start time", state.start_time()), ("finish time", state.finish_time()));
  return const_cast<Thread*>(eligible_thread);
}

Thread* Scheduler::FindEarlierFairThread(SchedTime eligible_time, SchedTime finish_time) {
  // Convert monotonic to variable times to compare with fair run queue values.
  const SchedTime variable_eligible_time = MonotonicToVariable(eligible_time);
  const SchedTime variable_finish_time = MonotonicToVariable(finish_time);

  Thread* const eligible_thread =
      FindEarliestEligibleThread(&rust_state_.fair_run_queue, variable_eligible_time);

  if (eligible_thread != nullptr) {
    // Use MarkHasOwnedThreadAccess to satisfy static annotations for shared
    // access to the thread's COVs and exclusive access to the thread's SOVs.
    // This is acceptable because the queue lock is required by this method and
    // the thread is in a queue protectect by the same lock.
    MarkHasOwnedThreadAccess(*eligible_thread);
    if (const_cast<const Thread*>(eligible_thread)->scheduler_state().finish_time() <
        variable_finish_time) {
      return eligible_thread;
    }
  }

  return nullptr;
}

Thread* Scheduler::FindEarlierDeadlineThread(SchedTime eligible_time,
                                             const Thread* reference_thread) {
  return rust_scheduler_find_earlier_deadline_thread(this, eligible_time, reference_thread);
}

SchedTime Scheduler::GetNextEligibleTime() const {
  return rust_scheduler_get_next_eligible_time(this);
}

Scheduler::DequeueResult Scheduler::DequeueEarlierFairThread(SchedTime eligible_time,
                                                             SchedTime finish_time) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(DETAILED, "dequeue_earlier_fair_thread");
  Thread* const eligible_thread = FindEarlierFairThread(eligible_time, finish_time);

  if (eligible_thread != nullptr) {
    // Use MarkHasOwnedThreadAccess to satisfy static annotations for shared
    // access to the thread's COVs and exclusive access to the thread's SOVs.
    // This is acceptable because the queue lock is required by this method and
    // the thread is in a queue protectect by the same lock.
    MarkHasOwnedThreadAccess(*eligible_thread);
    rust_state_.fair_run_queue.erase(*eligible_thread);
    TraceThreadQueueEvent("tqe_deque_earlier_fair"_intern, eligible_thread);
    DEBUG_ASSERT(eligible_thread->disposition() == Disposition::Associated);

    const SchedulerState& state = const_cast<const Thread*>(eligible_thread)->scheduler_state();
    const SchedTime mono_start_time_ns =
        rust_state_.fair_affine_transform.VariableToMonotonic(state.start_time());
    const SchedTime mono_finish_time_ns =
        rust_state_.fair_affine_transform.VariableToMonotonic(state.finish_time());

    return DequeueResult{eligible_thread, mono_start_time_ns, mono_finish_time_ns};
  }

  return nullptr;
}

Scheduler::DequeueResult Scheduler::DequeueEarlierDeadlineThread(SchedTime eligible_time,
                                                                 const Thread* reference_thread) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(DETAILED, "dequeue_earlier_deadline_thread");
  Thread* const eligible_thread = FindEarlierDeadlineThread(eligible_time, reference_thread);

  if (eligible_thread != nullptr) {
    // Use MarkHasOwnedThreadAccess to satisfy static annotations for shared
    // access to the thread's COVs and exclusive access to the thread's SOVs.
    // This is acceptable because the queue lock is required by this method and
    // the thread is in a queue protected by the same lock.
    MarkHasOwnedThreadAccess(*eligible_thread);
    if (IsCriticalDeadlineThread(eligible_thread)) {
      rust_state_.critical_deadline_run_queue.erase(*eligible_thread);
    } else {
      rust_state_.deadline_run_queue.erase(*eligible_thread);
    }
    TraceThreadQueueEvent("tqe_deque_earlier_deadline"_intern, eligible_thread);
    DEBUG_ASSERT(eligible_thread->disposition() == Disposition::Associated);
  }

  return eligible_thread;
}

bool Scheduler::NeedsMigration(Thread* thread) {
  const cpu_num_t current_cpu = arch_curr_cpu_num();
  const cpu_mask_t current_cpu_mask = cpu_num_to_mask(current_cpu);
  const cpu_mask_t active_mask = PeekActiveMask();
  // Threads that are not ready should not be migrated.
  if (thread->state() != THREAD_READY) {
    return false;
  }

  // If we have no active CPUs, then we cannot migrate this thread, so return false.
  if (active_mask == 0) {
    return false;
  }

  // We need to migrate if the set of CPUs this thread can run on does not include the current CPU.
  return (thread->scheduler_state().GetEffectiveCpuMask(active_mask) & current_cpu_mask) == 0;
}

Scheduler::DequeueResult Scheduler::EvaluateNextThread(
    SchedTime now, Thread* current_thread, bool current_is_migrating,
    Guard<MonitoredSpinLock, NoIrqSave>& queue_guard) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(DETAILED, "find_thread");

  // First things first.  If we have pending power work, we choose the idle-power thread.
  {
    percpu& self = percpu::Get(this_cpu());
    if (IdlePowerThread::PendingPowerWorkPredicate pending_work =
            self.idle_power_thread.pending_power_work()) {
      // If the idle power thread is in the (offline, offline) state, CPU hot
      // plugging attempted to block before restoring the idle power thread.
      DEBUG_ASSERT(pending_work.current() != IdlePowerThread::State::Offline ||
                   pending_work.target() != IdlePowerThread::State::Offline);
      return &self.idle_power_thread.thread();
    }
  }

  const bool is_idle = current_thread->IsIdle();
  const bool is_runnable = current_thread->state() == THREAD_READY;
  const bool is_deadline = IsDeadlineThread(current_thread);

  if (is_runnable && !is_idle && !current_is_migrating) {
    // Start a new activation, if necessary, before comparing with other threads.
    UpdateActivation(current_thread, now);

    // Determine eligibility after updating the activation period.
    const bool is_eligible = current_thread->scheduler_state().start_time() <= now;

    if (is_deadline && !is_eligible) {
      // Ineligible deadline threads must wait until they become eligible again.
      return DequeueThread(now, queue_guard);
    }

    if (IsDeadlineThreadEligible(now)) {
      if (is_deadline) {
        // The current thread is deadline and eligible. Select the eligible
        // thread with the earliest finish time, which may still be the current
        // thread.
        if (DequeueResult earlier_thread_result =
                DequeueEarlierDeadlineThread(now, current_thread)) {
          return earlier_thread_result;
        }

        // The current thread is eligible and has the earliest finish time.
        return current_thread;
      }

      // The current thread is fair. An eligible deadline thread should preempt
      // the current thread.
      return DequeueDeadlineThread(now);
    }

    if (!is_deadline) {
      if (is_eligible) {
        if (IsFairThreadEligible(now)) {
          // Select the eligible fair thread with the earliest finish time,
          // which may still be the current thread.
          const SchedTime finish_time = current_thread->scheduler_state().finish_time();
          if (DequeueResult earlier_thread_result = DequeueEarlierFairThread(now, finish_time)) {
            return earlier_thread_result;
          }
        }

        // The current fair thread has the earliest eligible finish time.
        return current_thread;
      }

      if (IsFairThreadEligible(now)) {
        // Select an eligible fair thread.
        return DequeueFairThread(now);
      }

      // No fair threads are currently eligible. Temporarily add the current
      // thread to the run queue, before snapping the variable timeline to the
      // earliest eligible time, to simplify selection in the updated timeline.
      // Use Placement::Adjustment to avoid emitting inconsistent trace events.
      QueueThread(current_thread, Placement::Adjustment);

      // Snap the variable timeline to the earliest eligible time and select the
      // thread with the earliest eligible finish time.
      DequeueResult next_thread = DequeueFairThread(now);
      DEBUG_ASSERT(next_thread);

      // If the current thread is not selected to run again, it must be removed
      // from the run queue to avoid being stolen during lock juggling. It will
      // be returned to the run queue after the lock juggling is completed and
      // before switching to the next thread.
      if (next_thread.thread() != current_thread) {
        MarkHasOwnedThreadAccess(*current_thread);
        EraseFromQueue(current_thread);
      }

      return next_thread;
    }

    // The current deadline thread has remaining time and no eligible contender.
    DEBUG_ASSERT(is_deadline);
    DEBUG_ASSERT(is_eligible);
    return current_thread;
  }

  // The thread is either no longer runnable, migrating, or the idle thread.
  return DequeueThread(now, queue_guard);
}

cpu_num_t Scheduler::FindTargetCpu(Thread* thread, UnblockType unblock_type) {
  return rust_scheduler_find_target_cpu(thread, unblock_type == UnblockType::Synchronous ? 1 : 0);
}

void Scheduler::IncFindTargetCpuRetriesKcounter() { counter_find_target_cpu_retries.Add(1u); }

void Scheduler::ProcessSaveStateList() {
  DEBUG_ASSERT(arch_ints_disabled());

  // Move the save_state_list_ to a local, stack allocated list. This allows us to relinquish the
  // queue_lock_ for the rest of the method.
  Thread::SaveStateList local_save_state_list;
  {
    Guard<MonitoredSpinLock, NoIrqSave> queue_guard{&queue_lock_, SOURCE_TAG};
    if (save_state_list_.is_empty()) {
      return;
    }
    local_save_state_list = ktl::move(save_state_list_);
  }

  ChainLockTransaction& active_clt = ChainLockTransaction::ActiveRef();
  active_clt.Restart(CLT_TAG("Scheduler::ProcessSaveStateList (restart)"));

  cpu_mask_t cpus_to_reschedule_mask = 0;
  while (!local_save_state_list.is_empty()) {
    Thread* to_migrate = local_save_state_list.pop_front();
    // Acquire the thread's lock without backing off using an explicit retry
    // loop because the current thread's lock is held and ChainLock::Acquire
    // asserts that no chain locks are currently held to avoid misuse in the
    // general case. We also currently hold the special "sched token", meaning
    // that we will win arbitration against any other chainlock transaction.
    //
    // This is safe to do here because we can guarantee that no other scheduler
    // holds or is attempting to acquire the locks of any of the threads in this
    // save_state_list_. The only context in which schedulers attempt to acquire
    // the locks of threads in other schedulers is during the `StealWork`
    // routine. However, that routine only looks for threads in the RunQueues
    // (not save_state_lists) and also does not steal threads that have
    // migration functions. Therefore, it is impossible for another scheduler to
    // be fighting over this thread's lock.
    while (!to_migrate->get_lock().TryAcquire(ChainLock::AllowFinalized::No,
                                              ChainLock::RecordBackoff::No)) {
      arch::Yield();
    }

    // We now have the thread's lock, so run the migrate function.
    to_migrate->CallMigrateFnLocked(Thread::MigrateStage::Save);

    // TODO(rudymathu): Threads placed in the save_state_list_ via an Unblock
    // operation already performed a `FindTargetCpu` call. In a future
    // optimization, we should be able to reuse that value here and avoid
    // another CPU search, provided that the CPU is still online and load
    // balancing constraints still hold.
    const cpu_num_t target_cpu =
        FindActiveSchedulerForThread(to_migrate, [](Thread* thread, Scheduler* target) {
          MarkInFindActiveSchedulerForThreadCbk(*thread, *target);
          target->Insert(CurrentTime(), thread, Placement::Migration);
        });
    cpus_to_reschedule_mask |= cpu_num_to_mask(target_cpu);
    to_migrate->get_lock().Release();
  }
  ChainLockTransaction::Finalize();

  // Issue reschedule IPIs to CPUs with migrated threads.
  if (cpus_to_reschedule_mask) {
    mp_reschedule(cpus_to_reschedule_mask, 0);
  }
}

void Scheduler::UpdateEstimatedEnergyConsumption(Thread* current_thread,
                                                 SchedMonoTimeAndBootTicks now,
                                                 SchedDuration actual_runtime_ns) {
  rust_scheduler_update_estimated_energy_consumption(this, current_thread, now.mono_time,
                                                     now.boot_ticks, actual_runtime_ns);
}

void Scheduler::UpdateCpuStatsSegment(SchedTime now, CpuStatsSegmentType stats_segment_type,
                                      SchedProcessingRate segment_processing_rate) const {
  rust_scheduler_update_cpu_stats_segment(
      this, now, stats_segment_type == CpuStatsSegmentType::Idle, segment_processing_rate);
}

cpu_stats Scheduler::GetProjectedCpuStats(cpu_num_t cpu_num) {
  cpu_stats stats;
  rust_kernel_scheduler_get_projected_cpu_stats(cpu_num, &stats);
  return stats;
}

void Scheduler::RescheduleCommon(Thread* const current_thread, EndTraceCallback end_outer_trace) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(DETAILED, "reschedule_common");

  DEBUG_ASSERT(current_thread == Thread::Current::Get());
  const cpu_num_t current_cpu = arch_curr_cpu_num();
  SchedulerState* const current_state = &current_thread->scheduler_state();

  // No spinlocks should be held as we come into a reschedule operation.  Only
  // the current thread's lock should be held.
  DEBUG_ASSERT_MSG(const uint32_t locks_held = arch_num_spinlocks_held();
                   locks_held == 0, "arch_num_spinlocks_held() == %u\n", locks_held);
  DEBUG_ASSERT(!arch_blocking_disallowed());
  DEBUG_ASSERT_MSG(const cpu_num_t tcpu = this_cpu();
                   current_cpu == tcpu, "current_cpu=%u this_cpu=%u", current_cpu, tcpu);
  ChainLockTransaction& active_clt = ChainLockTransaction::ActiveRef();
  active_clt.AssertNumLocksHeld(1);

  CPU_STATS_INC(reschedules);

  // Prepare for rescheduling by backing up the chain lock context and switching
  // to the special "sched token" which we use during reschedule operations in
  // order to guarantee that we will win arbitration with other active
  // transactions which are not reschedule operations.  We will restore this
  // context at the end of the operation (whether we context switched or not).
  const auto saved_transaction_state = ChainLockTransaction::SaveStateAndUseReservedToken();
  current_thread->get_lock().SyncToken();

  // Process all threads in the save_state_list_. We do this before performing any
  // other reschedule operation.
  ProcessSaveStateList();

  Guard<MonitoredSpinLock, NoIrqSave> queue_guard{&queue_lock_, SOURCE_TAG};
  // When calling into reschedule, the current thread is only allowed to be in a
  // limited number of states, depending on where it came from.
  //
  // RUNNING   : Thread is the currently active thread on this CPU.
  // BLOCKED*  : Thread has been inserted into a wait queue and is in the
  //             process of de-activating.
  // SUSPENDED : Thread is in the process of suspending (but is not in any queue).
  // DEAD      : Thread is in the process of dying, this is its final reschedule.
  //
  AssertInScheduler(*current_thread);
  DEBUG_ASSERT(current_thread->state() != THREAD_READY);
  if (current_thread->state() == THREAD_RUNNING) {
    DEBUG_ASSERT(current_thread->disposition() == Disposition::Associated);
    current_thread->set_ready();
  }

  // Sample the current time after acquiring the queue lock to avoid large skews
  // between accounting based on now and trace events that should occur
  // approximately now.
  const SchedMonoTimeAndBootTicks mono_and_boot_now = CurrentMonoTimeAndBootTicks();
  const SchedTime now = mono_and_boot_now.mono_time;

  // TODO(https://fxbug.dev/381899402): Find the root cause of small negative
  // values in the actual runtime calculation.
  const SchedDuration actual_runtime_ns =
      ktl::max<SchedDuration>(now - current_state->last_started_running_, SchedDuration{0});
  current_state->last_started_running_ = now;

  const SchedDuration total_runtime_ns = now - rust_state_.start_of_current_time_slice_ns;

  current_state->runtime_ns_ += actual_runtime_ns;
  current_thread->UpdateRuntimeStats(current_thread->state());

  // CRITICAL: Maintain Coherency of Idle and Busy States
  //
  // Apply the following rule:
  //
  //   1. Initial Update: Always performed under the first lock acquisition to
  //      account for time elapsed since the last reschedule.
  //
  // See UpdateCpuStatsSegment for the full set of coherency rules.
  //
  // It is not yet known whether the CPU is transitioning to/from idle or the
  // processing rate is changing. Simply advance the CPU stats to reflect the
  // elapsed idle time or normalized busy time at the last known processing rate
  // since the last update.
  UpdateCpuStatsSegment(now, ToCpuStatsSegmentType(*current_thread), processing_rate());

  // Update the energy consumption accumulators for the current task and
  // processor.
  UpdateEstimatedEnergyConsumption(current_thread, mono_and_boot_now, actual_runtime_ns);

  // Update the used time slice before evaluating the next task. Scale the
  // actual runtime of a deadline task by the relative performance of the CPU,
  // effectively increasing the capacity of the task in proportion to the
  // performance ratio. The remaining time slice may become negative due to
  // scheduler overhead.
  current_state->time_slice_used_ns_ +=
      IsDeadlineThread(current_thread) ? ScaleDown(actual_runtime_ns) : actual_runtime_ns;

  // Adjust the rate of the current thread when fair bandwidth demand changes.
  if (IsThreadAdjustable(current_thread) &&
      rust_state_.weight_total != rust_state_.scheduled_weight_total &&
      current_state->remaining_time_slice_ns() > 0 && current_state->finish_time() > now) {
    ktrace::Scope trace_adjust_rate =
        LOCAL_KTRACE_BEGIN_SCOPE(DETAILED, "adjust_rate", ("total runtime", total_runtime_ns));
    rust_state_.scheduled_weight_total = rust_state_.weight_total;
    AdjustFairBandwidth(current_thread, now);
    rust_state_.target_preemption_time_ns = ktl::clamp<SchedTime>(
        now + current_state->remaining_time_slice_ns(), now, current_state->finish_time());
  }

  // Use a small epsilon to avoid tripping the consistency check below due to
  // rounding when scaling the time slice.
  const SchedDuration time_slice_epsilon{100};
  const bool timeslice_expired = now >= current_state->finish_time_ ||
                                 current_state->remaining_time_slice_ns() <= time_slice_epsilon;

  // Check the consistency of the target preemption time and the current time
  // slice.
  [[maybe_unused]] const auto& ep = current_state->effective_profile_;
  DEBUG_ASSERT_MSG(
      now < rust_state_.target_preemption_time_ns || timeslice_expired,
      "cpu=%u capacity_ns=%" PRId64 " deadline_ns=%" PRId64
      "\nfinish_time                   =%16" PRId64 "\nnow                           =%16" PRId64
      "\ntarget_preemption_time_ns     =%16" PRId64 "\nstart_of_current_time_slice_ns=%16" PRId64
      "\ntotal_runtime_ns              =%16" PRId64 "\nactual_runtime_ns             =%16" PRId64
      "\ntime_slice_ns                 =%16" PRId64 "\ntime_slice_used_ns            =%16" PRId64
      "\nremaining_time_slice_ns       =%16" PRId64,
      this_cpu(), IsDeadlineThread(current_thread) ? ep.deadline().capacity_ns.raw_value() : 0,
      IsDeadlineThread(current_thread) ? ep.deadline().deadline_ns.raw_value() : 0,
      current_state->finish_time_.raw_value(), now.raw_value(),
      rust_state_.target_preemption_time_ns.raw_value(),
      rust_state_.start_of_current_time_slice_ns.raw_value(), total_runtime_ns.raw_value(),
      actual_runtime_ns.raw_value(), current_state->time_slice_ns_.raw_value(),
      current_state->time_slice_used_ns().raw_value(),
      current_state->remaining_time_slice_ns().raw_value());

  // If the current thread needs to be migrated, remove it for migration.
  const bool current_is_migrating = NeedsMigration(current_thread);
  if (current_is_migrating) {
    Remove(now, current_thread);
  }

  // Select the next thread to run.
  //
  // WARNING: This method temporarily drops the queue lock during work stealing,
  // when there are no threads in the run queue and the current thread is no
  // longer in the running state. Do not split updates to different values
  // across this call that need to be coherently read from another CPU and
  // synchronized using the queue lock. Doing so creates an incomplete critical
  // section that can result in incoherent reads of the values updated in
  // different acquisitions of the queue lock.
  DequeueResult next_thread_result =
      EvaluateNextThread(now, current_thread, current_is_migrating, queue_guard);

  DEBUG_ASSERT(next_thread_result);
  DEBUG_ASSERT(!current_thread->scheduler_queue_state().in_queue());

  // Flush pending preemptions.
  mp_reschedule(current_thread->preemption_state().preempts_pending(), 0);
  current_thread->preemption_state().preempts_pending_clear();

  // If the next_thread is not the same as the current thread, we will need to
  // (briefly) drop the scheduler's queue lock in order to acquire the next
  // thread's lock exclusively.  If we have selected the same thread as before,
  // we can skip this step.
  //
  // If we do need to acquire next_thread's lock, we use the same chain lock
  // token that was used to acquire current_thread's lock.  We know this is safe
  // because:
  //
  // TODO(johngro): THIS IS NOT SAFE - FIX THIS COMMENT
  //
  // This turned out to not be safe after all.  The deadlock potential below is
  // why we need to switch to the special "scheduler" token at the start of the
  // operation.
  //
  // While there cannot be any cycles in this individual operation (trivial to
  // show since there are two different locks here), deadlock is still possible.
  //
  // The flow is:
  // 1) T0 is running on CPU X, and is locked entering a reschedule operation.
  // 2) T1 is selected as the next thread to run, and is being locked
  //    exclusively (after dropping the scheduler's lock).  At the same time:
  // 3) T2 is running on CPU Y, and is blocking on an OWQ which currently has T0
  //    as the owner.  T1 has been selected as the new owner.
  // 4) T2 and the queue have been locked by the BAAO operation.
  // 5) T1 is the start of the new owner chain and is locked.
  // 6) The next step in the BAAO locking is to lock the old owner chain,
  //    starting from T0.
  //
  // So:
  // X has T0 and wants T1.
  // Y has T1 and wants T0.
  //
  // CPU Y is in a sequence which obeys the backoff protocol, so as long as its
  // token value > X's token value, Y will back off and we will be OK.  If not,
  // however, we deadlock.  The reschedule operation cannot backoff and drop
  // T0's lock, so it is stuck trying to get T1 from Y.
  //
  // TODO(johngro): END TODO
  //
  // 1) current_thread is currently running.  Because of this, we know that it
  //    is the target node of it's PI graph.
  // 2) next thread is currently READY, which means it is also the target node
  //    of it's PI graph.
  // 3) PI graphs always have exactly one target node.
  // 4) Therefore, current and next cannot be members of the same graph, and can
  //    both be acquired unconditionally.
  //
  // WARNING: This block temporarily drops the queue lock to acquire the next
  // thread's lock. Do not split updates to different values across this call
  // that need to be coherently read from another CPU and synchronized using the
  // queue lock. Doing so creates an incomplete critical section that can result
  // in incoherent reads of the values updated in different acquisitions of the
  // queue lock.
  if (current_thread != next_thread_result.thread()) {
    // The ChainLockTransaction we were in when we entered RescheduleCommon has
    // already been finalized.  We need to restart it in order to obtain new
    // locks.
    active_clt.Restart(CLT_TAG("Scheduler::RescheduleCommon (restart)"));

    // Release the queue lock, acquire the next thread lock, and re-acquire the
    // queue lock. While the queue lock is unlocked, a PI operation or affinity
    // change can update the next thread's state, since these operations could
    // already hold the next thread's lock while waiting for the queue lock.
    // These operations are obligated to reschedule this CPU if they invalidate
    // the next thread's state, such that a different thread should run or the
    // current CPU is no longer in the affinity mask. However, since the next
    // thread is no longer in the run queue, it cannot be discovered and stolen
    // by another CPU while the queue lock is unlocked.
    queue_guard.CallUnlocked(
        [&next_thread_result]() TA_ACQ(next_thread_result.thread()->get_lock()) {
          ChainLockTransaction::AssertActive();
          // Acquire the lock without backing off using an explicit retry loop
          // because the current thread's lock is held and ChainLock::Acquire
          // asserts that no chain locks are currently held to avoid misuse in
          // the general case.
          while (!next_thread_result.thread()->get_lock().TryAcquire(
              ChainLock::AllowFinalized::No, ChainLock::RecordBackoff::No)) {
            arch::Yield();
          }
        });

    // All of the locks needed to complete the reschedule are held.
    ChainLockTransaction::Finalize();
  }

  // Update the state of the current and next thread.
  Thread* const next_thread = next_thread_result.thread();
  next_thread->get_lock().AssertHeld();
  AssertInScheduler(*next_thread);

  const SchedulerQueueState& current_queue_state = current_thread->scheduler_queue_state();
  SchedulerState* const next_state = &next_thread->scheduler_state();

  // Convert from variable time to monotonic time if necessary.
  next_thread_result.thread()->get_lock().MarkHeld();
  next_thread_result.UpdateThreadTimeline();

  // If the next thread was just stolen from another CPU, finish associating it
  // with this CPU and scheduler.
  if (next_thread->disposition() == Disposition::Stolen) {
    DEBUG_ASSERT(next_thread->state() == THREAD_READY);
    DEBUG_ASSERT(next_state->curr_cpu() != current_cpu);
    DEBUG_ASSERT(next_thread->scheduler_queue_state().stolen_by() == current_cpu);
    DEBUG_ASSERT(!next_thread_result.is_updated_pending());
    Insert(now, next_thread, Placement::Association);
  }
  DEBUG_ASSERT(next_thread->disposition() == Disposition::Associated);

  if (current_thread != next_thread) {
    // Update stall contributions of the current and next thread.
    StallAccumulator::ApplyContextSwitch(current_thread, next_thread);

    // If the current thread is not the idle thread and not migrating, return it
    // to the run queue or remove its bookkeeping, depending on whether it is
    // READY.
    if (!current_thread->IsIdle() && !current_is_migrating) {
      if (current_thread->state() == THREAD_READY) {
        QueueThread(current_thread,
                    timeslice_expired ? Placement::Insertion : Placement::Preemption, now);
      } else {
        RemoveThreadLocked(now, current_thread);
      }
    }
  }

  next_thread->set_running();
  next_state->last_cpu_ = current_cpu;
  DEBUG_ASSERT(next_state->curr_cpu_ == current_cpu);
  rust_state_.active_thread = next_thread;

  // Keep track of the weight of the active thread, since it is not in the run
  // queue contributing to the min_weight subtree invariant.
  rust_state_.active_thread_weight = !next_thread->IsIdle() && IsFairThread(next_thread)
                                         ? next_state->effective_profile().weight()
                                         : SchedWeight::Max();

  // Handle any pending migration work.
  next_thread->CallMigrateFnLocked(Thread::MigrateStage::Restore);

  // Update the expected runtime of the current thread and the per-CPU total.
  // Only update the thread and aggregate values if the current thread is still
  // associated with this CPU or is no longer ready.
  const bool current_is_associated =
      !current_queue_state.active() || current_state->curr_cpu_ == current_cpu;
  if (!current_thread->IsIdle() && current_is_associated &&
      (timeslice_expired || current_thread != next_thread)) {
    ktrace::Scope trace_update_ema = LOCAL_KTRACE_BEGIN_SCOPE(DETAILED, "update_expected_runtime");

    // Adjust the runtime for the relative performance of the CPU to account for
    // different performance levels in the estimate. The relative performance
    // scale is in the range (0.0, 1.0], such that the adjusted runtime is
    // always less than or equal to the monotonic runtime.
    const SchedDuration adjusted_total_runtime_ns = ScaleDown(total_runtime_ns);
    current_state->banked_runtime_ns_ += adjusted_total_runtime_ns;

    if (timeslice_expired || !current_queue_state.active()) {
      const SchedDuration delta_ns = rust_scheduler_peak_decay_delta(
          current_state->expected_runtime_ns_, current_state->banked_runtime_ns_);
      current_state->expected_runtime_ns_ += delta_ns;
      current_state->banked_runtime_ns_ = SchedDuration{0};

      // Adjust the aggregate value by the same amount. The adjustment is only
      // necessary when the thread is still active on this CPU.
      if (current_queue_state.active()) {
        UpdateTotalExpectedRuntime(delta_ns);
      }
    }
  }

  if (power_level_control_.is_enabled()) {
    // After the current thread has been potentially removed from this
    // scheduler, clear or prune the bandwidth demand cache and re-evaluate the
    // current power level.
    const bool clear_cache = next_thread->IsIdle() &&
                             rust_state_.critical_deadline_run_queue.is_empty() &&
                             rust_state_.deadline_run_queue.is_empty();
    const SchedUtilization utilization_to_remove =
        clear_cache ? rust_state_.bandwidth_reservation_cache.Clear()
                    : rust_state_.bandwidth_reservation_cache.Prune(now);
    if (utilization_to_remove > 0) {
      LOCAL_KTRACE_INSTANT(QUEUE, "clear/prune", ("utilization_to_remove", utilization_to_remove));

      UpdateTotalDeadlineUtilization(-utilization_to_remove);
    }

    // Evaluate changing the processing rate when the required utilization
    // moves outside the bounds of the current power level. All of the
    // processors in the same domain must be re-evaluated to determine whether
    // an actual rate change should occur.
    if (power_level_control_.scheduler_control_enabled() &&
        !power_level_control_.has_pending_request() &&
        power_level_control_.processing_rate_should_change()) {
      power_level_control_.ReevaluateCurrentPowerLevel();
    }

    // Send a pending power level request before updating the processing rate.
    //
    // WARNING: This call temporarily drops the queue lock when calling the
    // fast-path power level controller. Do not split updates to different
    // values across this call that need to be coherently read from another CPU
    // and synchronized using the queue lock. Doing so creates an incomplete
    // critical section that can result in incoherent reads of the values
    // updated in different acquisitions of the queue lock.
    if (const cpu_mask_t reschedule_mask =
            power_level_control_.SendPendingPowerLevelRequest(queue_guard)) {
      // Reschedule the other CPUs affected by the power level change. The
      // current CPU is filtered out by mp_reschedule.
      mp_reschedule(reschedule_mask, 0);
    }
  }

  // Update the current processing rate only after any uses in the reschedule
  // path above to ensure the scale is applied consistently over the interval
  // between reschedules (i.e. not earlier than the requested update).
  //
  // Updating the processing rate also results in updating the target preemption
  // time below when the current thread is deadline scheduled.
  //
  // TODO(eieio): Shed load when total utilization is above the processing rate.
  const SchedProcessingRate previous_processing_rate = processing_rate();
  const bool processing_rate_updated = UpdateProcessingRate(mono_and_boot_now.boot_ticks);

  // CRITICAL: Maintain Coherency of Idle and Busy States
  //
  // Apply the following rules:
  //
  //   2. Transition Update: If the CPU's idle state changes, upon re-acquiring
  //      the lock, stats MUST be updated again using FRESH time samples.
  //   3. Processing Rate Update: If the CPU's processing rate changes, upon
  //      re-acquiring the lock, stats MUST be updated again using FRESH time
  //      samples.
  //
  // See UpdateCpuStatsSegment for the full set of coherency rules.
  //
  // The queue lock has been released and re-acquired one or more times, each of
  // which may have been interposed by another CPU sampling the critical values
  // and forward projecting idle or normalized busy time. If there is transition
  // to/from idle or the processing rate changed while non-idle, advance the CPU
  // stats to cover the segment since the initial update.
  SetIdle(next_thread->IsIdle());
  if (next_thread->IsIdle() != current_thread->IsIdle() ||
      (!current_thread->IsIdle() && processing_rate_updated)) {
    UpdateCpuStatsSegment(CurrentTime(), ToCpuStatsSegmentType(*current_thread),
                          previous_processing_rate);
  }

  {
    // Each path below must set the preemption time.
    // TODO(eieio): Debug assert that the value set is >= now.
    SchedTime preemption_time_ns = SchedTime::Min();

    if (next_thread->IsIdle()) {
      ktrace::Scope trace_stop_preemption = LOCAL_KTRACE_BEGIN_SCOPE(DETAILED, "idle");
      next_state->last_started_running_ = now;

      // If there are no tasks to run in the future or there is idle/power work to
      // perform, disable the preemption timer.  Otherwise, set the preemption
      // time to the earliest eligible time.
      preemption_time_ns = rust_state_.target_preemption_time_ns =
          percpu::Get(this_cpu()).idle_power_thread.pending_power_work()
              ? SchedTime(ZX_TIME_INFINITE)
              : GetNextEligibleTime();
    } else if (timeslice_expired || current_thread != next_thread) {
      ktrace::Scope trace_start_preemption = LOCAL_KTRACE_BEGIN_SCOPE(DETAILED, "next_slice");

      // Re-compute the time slice and ideal preemption time for the new thread
      // based on the latest state.
      rust_state_.target_preemption_time_ns = NextThreadTimeslice(next_thread, now);

      // Update the thread's runtime stats to record the amount of time that it spent in the run
      // queue.
      next_thread->UpdateRuntimeStats(next_thread->state());

      next_state->last_started_running_ = now;
      rust_state_.start_of_current_time_slice_ns = now;
      rust_state_.scheduled_weight_total = rust_state_.weight_total;

      // Adjust the preemption time to account for a deadline thread becoming
      // eligible before the current time slice expires.
      preemption_time_ns =
          IsFairThread(next_thread)
              ? ClampToDeadline(rust_state_.target_preemption_time_ns)
              : ClampToEarlierDeadline(rust_state_.target_preemption_time_ns, next_thread);
      DEBUG_ASSERT(preemption_time_ns <= rust_state_.target_preemption_time_ns);

      const SchedTime target_preemption_time_ns = rust_state_.target_preemption_time_ns;
      trace_start_preemption =
          KTRACE_END_SCOPE(("preemption_time", preemption_time_ns),
                           ("target preemption time", target_preemption_time_ns));

      // Emit a flow end event to match the flow begin event emitted when the
      // thread was enqueued. Emitting in this scope ensures that thread just
      // came from the run queue (and is not the idle thread).
      LOCAL_KTRACE_FLOW_END(FLOW, "sched_latency", next_state->flow_id(),
                            ("tid", next_thread->tid()));
    } else {
      ktrace::Scope trace_continue = LOCAL_KTRACE_BEGIN_SCOPE(DETAILED, "continue");
      DEBUG_ASSERT(current_thread == next_thread);

      // Update the target preemption time for consistency with the updated CPU
      // processing rate.
      // TODO(eieio): Eventually update the effective bandwidth for all threads.
      if (processing_rate_updated && IsDeadlineThread(next_thread)) {
        rust_state_.target_preemption_time_ns = NextThreadTimeslice(next_thread, now);
      }

      // The current thread should continue to run. A throttled deadline thread
      // might become eligible before the current time slice expires. Figure out
      // whether to set the preemption time earlier to switch to the newly
      // eligible thread.
      //
      // The preemption time should be set earlier when either:
      //   * Current is a fair thread and a deadline thread will become eligible
      //     before its time slice expires.
      //   * Current is a deadline thread and a deadline thread with an earlier
      //     deadline will become eligible before its time slice expires.
      //
      // Note that the target preemption time remains set to the ideal
      // preemption time for the current task, even if the preemption timer is set
      // earlier. If a task that becomes eligible is stolen before the early
      // preemption is handled, this logic will reset to the original target
      // preemption time.
      preemption_time_ns =
          IsFairThread(next_thread)
              ? ClampToDeadline(rust_state_.target_preemption_time_ns)
              : ClampToEarlierDeadline(rust_state_.target_preemption_time_ns, next_thread);
      DEBUG_ASSERT(preemption_time_ns <= rust_state_.target_preemption_time_ns);

      const SchedTime target_preemption_time_ns = rust_state_.target_preemption_time_ns;
      trace_continue = KTRACE_END_SCOPE(("preemption_time", preemption_time_ns),
                                        ("target preemption time", target_preemption_time_ns));
    }

    // Make sure the bandwidth demand cache will get pruned if there are
    // remaining valid entries.
    if (power_level_control_.is_enabled()) {
      preemption_time_ns =
          rust_state_.bandwidth_reservation_cache.ClampToNextFinishTime(preemption_time_ns);
    }

    DEBUG_ASSERT(preemption_time_ns != SchedTime::Min());
    PreemptReset(current_cpu, now.raw_value(), preemption_time_ns.raw_value());

    // Assert that there is no path beside running the idle thread can leave the
    // preemption timer unarmed. However, the preemption timer may or may not be
    // armed when running the idle thread.
    DEBUG_ASSERT(next_thread->IsIdle() || percpu::Get(current_cpu).timer_queue.PreemptArmed());
  }

  // Almost done, we need to handle the actual context switch (if any).
  if (current_thread != next_thread) {
    LOCAL_KTRACE_INSTANT(
        DETAILED, "switch_threads", ("total threads", runnable_task_count()),
        ("total weight", rust_state_.weight_total.raw_value()),
        ("current thread time slice", current_thread->scheduler_state().remaining_time_slice_ns()),
        ("next thread time slice", next_thread->scheduler_state().remaining_time_slice_ns()));

    TraceThreadQueueEvent("tqe_astart"_intern, next_thread);

    // Release queue lock before context switching.
    queue_guard.Release();

    // Finish the migration of the current thread, if pending.
    if (current_is_migrating) {
      current_thread->CallMigrateFnLocked(Thread::MigrateStage::Save);

      const cpu_num_t target_cpu = FindActiveSchedulerForThread(
          current_thread, [now, this](Thread* thread, Scheduler* target) {
            MarkInFindActiveSchedulerForThreadCbk(*thread, *target);
            DEBUG_ASSERT(target != this);
            target->Insert(now, thread, Placement::Migration);
          });

      mp_reschedule(cpu_num_to_mask(target_cpu), 0);
    }

    // Trace the activation of the next thread before context switching.
    SchedTraceContextSwitch(current_thread, next_thread, current_cpu);

    // We invoke the context switch functions before context switching, so that
    // they have a chance to correctly perform the actions required. Doing so
    // after context switching may lead to an invalid CPU state.
    current_thread->CallContextSwitchFnLocked();
    next_thread->CallContextSwitchFnLocked();

    // Notes about Thread aspace rules:
    //
    // Typically, it is only safe for the current thread to access its aspace
    // member directly, as only a running thread can change its own aspace, and
    // if a thread is running, then its process must also be alive and therefore
    // its aspace must also be alive.
    //
    // Context switching is a bit of an edge case.  The current thread is
    // becoming the next thread.  Both aspaces must still be alive (even if the
    // current thread is in the process of becoming rescheduled for the very
    // last time as it exits), and neither one can change its own aspace right
    // now (they are not really running).
    //
    // Because of this, it should be OK for us to directly access the aspaces
    // during the context switch, without needing to either check the thread's
    // state, or add any references to the VmAstate object.
    [&]() TA_NO_THREAD_SAFETY_ANALYSIS {
      if (current_thread->aspace() != next_thread->aspace()) {
        vmm_context_switch(current_thread->aspace(), next_thread->aspace());
      }
    }();

    CPU_STATS_INC(context_switches);

    // Prevent the scheduler durations from spanning the context switch.
    // Some context switches do not resume within this method on the other
    // thread, which results in unterminated durations. All of the callers
    // with durations tail-call this method, so terminating the duration
    // here should not cause significant inaccuracy of the outer duration.
    trace.End();
    if (end_outer_trace) {
      end_outer_trace();
    }

    if constexpr (SCHEDULER_QUEUE_TRACING_ENABLED) {
      const uint64_t arg0 = 0;
      const uint64_t arg1 = (ktl::clamp<uint64_t>(this_cpu(), 0, 0xF) << 16);
      KTrace::Probe(KTrace::Context::Cpu, "tqe_afinish"_intern, arg0, arg1);
    }

    // Time for the context switch.  Before actually performing the switch,
    // record the current thread as the "previous thread" for the next thread
    // becoming scheduled.  The next thread needs to know which thread preceded
    // it in order to drop its lock before unwinding, and our current stack is
    // not going to be available to it.
    DEBUG_ASSERT(previous_thread() == nullptr);
    set_previous_thread(current_thread);
    arch_context_switch(current_thread, next_thread);

    // We made it to the other side of the switch.  We have switched stacks, so
    // the local variable meanings have become redefined.
    //
    // ++ The value of current_thread after the context switch is the value of
    //    next_thread before the switch.
    // ++ The value of next_thread after the context switch is the thread that
    //    the new current_thread context-switched to the last time it ran
    //    through here.  We don't even know if this thread exists now, so its
    //    value is junk.
    // ++ The value of current_thread before the switch has been stashed in
    //    the new current thread's scheduler_state's previous thread member.

    // If the new current thread has a restartable sequence registered, we
    // need to signal the thread to check for the restartable sequence before
    // it returns to userspace.
    current_thread->SignalCheckRestartableSequenceIfNeeded();

    // We need to drop the old current thread's lock, while continuing to hold
    // the new current thread's lock as we unwind.
    // Scheduler::LockHandoffInternal will take care of dropping the previous
    // current_thread's lock, but at this point, the static analyzer is going to
    // be extremely confused because it does not (and cannot) know about the
    // stack-swap which just happened. It thinks that we are still holding
    // next_thread's lock (and we are; it is now current thread) and that we
    // need to drop it (it is wrong about this, we need to drop the previous
    // current thread's lock).  So, just lie to it. Tell it that we have dropped
    // next_thread's lock using a no-op assert (we don't actually want to
    // examine the state of the lock in any way, since next thread is no longer
    // valid).
    Scheduler::LockHandoffInternal(saved_transaction_state, current_thread);
    next_thread->get_lock().MarkReleased();
  } else {
    // Restore the transaction state and drop the queue guard before unwinding.
    ChainLockTransaction::RestoreState(saved_transaction_state);
    current_thread->get_lock().SyncToken();
    queue_guard.Release();
  }
}

void Scheduler::TrampolineLockHandoff() {
  ChainLockTransaction::AssertActive();

  // Construct the fake chainlock transaction we will be restoring.  It should:
  //
  // 1) Not attempt to register itself with the per-cpu data structure.  There
  //    should already be a registered CLT, and attempting to instantiate a new
  //    one using the typical constructor would trigger an ASSERT.  This should
  //    look like a transaction which had been sitting on a stack, but swapped
  //    out using PrepareForReschedule at some point in the past.
  // 2) Report that there are currently two locks held (because there are).
  //    We have the previously running thread's lock, and the current (new)
  //    thread's lock held (obtained by Scheduler::RescheduleCommon).
  // 3) Report that the transaction has been finalized, as it would have been
  //    had we come in via RescheduleCommon.
  //
  ChainLockTransaction transaction =
      ChainLockTransaction::MakeBareTransaction(CLT_TAG("Trampoline"), 2, true);
  // Assert that the current thread's lock was "acquired" here.  It was
  // actually acquired during the context switch from
  // Scheduler::RescheduleCommon, but we now need to release it before letting
  // the new thread run.  The currently registered CLT should be using the
  // scheduler token that was used to acquire the new thread's lock during
  // reschedule common.
  Thread* const current_thread = Thread::Current::Get();
  current_thread->get_lock().AssertAcquired();

  // Now perform our lock handoff.  This will release our previous
  // thread's lock, and restore the transaction we just instantiated as
  // the active transaction, and replace the current thread's lock's
  // token with the new non-scheduler token.
  Scheduler::LockHandoffInternal(transaction.SaveStateInitial(), current_thread);

  // Finally, we just need to release the current thread's lock and we
  // are finished.
  current_thread->get_lock().Release();
}

void Scheduler::LockHandoffInternal(SavedState saved_state, Thread* const current_thread) {
  DEBUG_ASSERT(arch_ints_disabled());
  Scheduler* scheduler = Scheduler::Get();
  Thread* const previous_thread = scheduler->previous_thread();
  DEBUG_ASSERT(previous_thread != nullptr);

  scheduler->set_previous_thread(nullptr);
  previous_thread->get_lock().AssertAcquired();
  ChainLockTransaction::Active()->AssertNumLocksHeld(2);
  ChainLockTransaction::RestoreState(saved_state);
  current_thread->get_lock().SyncToken();
  previous_thread->get_lock().SyncToken();
  previous_thread->get_lock().Release();
}

SchedDuration Scheduler::CalculateTimeslice(const Thread* thread) {
  return rust_scheduler_calculate_timeslice(this, thread);
}

SchedTime Scheduler::ClampToDeadline(SchedTime completion_time) {
  return rust_scheduler_clamp_to_deadline(this, completion_time);
}

SchedTime Scheduler::ClampToEarlierDeadline(SchedTime completion_time,
                                            const Thread* reference_thread) {
  return rust_scheduler_clamp_to_earlier_deadline(this, completion_time, reference_thread);
}

SchedTime Scheduler::NextThreadTimeslice(Thread* thread, SchedTime now) {
  return rust_scheduler_next_thread_timeslice(this, thread, now);
}

void Scheduler::UpdateActivation(Thread* thread, SchedTime now) {
  rust_scheduler_update_activation(this, thread, now);
}

void Scheduler::QueueThread(Thread* thread, Placement placement, SchedTime now,
                            SchedDuration total_runtime_ns) {
  rust_scheduler_queue_thread(this, thread, static_cast<int32_t>(placement), now);
}

void Scheduler::UpdateFairBandwidthPeriod(SchedTime now) {
  rust_scheduler_update_fair_bandwidth_period(this, now);
}

void Scheduler::AdjustFairBandwidth(Thread* thread, SchedTime now) {
  rust_scheduler_adjust_fair_bandwidth(this, thread, now);
}

void Scheduler::Insert(SchedTime now, Thread* thread, Placement placement) {
  rust_scheduler_insert(this, now, thread, static_cast<int32_t>(placement));
}

void Scheduler::Remove(SchedTime now, Thread* thread, cpu_num_t stolen_by) {
  rust_scheduler_remove(this, now, thread, stolen_by);
}

void Scheduler::RemoveThreadLocked(SchedTime now, Thread* thread) {
  rust_scheduler_remove_thread_locked(this, now, thread);
}

void Scheduler::ValidateInvariantsUnconditional() const {
  rust_scheduler_validate_invariants_unconditional(this);
}

void Scheduler::Block(Thread* const current_thread) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(COMMON, "sched_block");

  DEBUG_ASSERT(current_thread == Thread::Current::Get());
  current_thread->canary().Assert();
  DEBUG_ASSERT(current_thread->state() != THREAD_RUNNING);

  Scheduler::Get()->RescheduleCommon(current_thread, trace.Completer());
}

Scheduler::RescheduleTargets Scheduler::UnblockCommon(Thread* thread, SchedTime now,
                                                      UnblockType unblock_type) {
  thread->canary().Assert();

  cpu_num_t target_cpu = INVALID_CPU;
  cpu_mask_t cpus_to_reschedule = 0;
  while (true) {
    // TODO(rudymathu): This target_cpu should be stashed in the thread prior to
    // adding it to the save_state_list_, as that would allow us to bypass CPU
    // selection when processing the list so long as the CPU is still online.
    target_cpu = FindTargetCpu(thread, unblock_type);
    const cpu_num_t last_cpu = thread->scheduler_state().last_cpu();
    const bool needs_migration = (last_cpu != INVALID_CPU && target_cpu != last_cpu &&
                                  thread->has_migrate_fn() && !thread->migrate_pending());
    if (needs_migration) {
      target_cpu = last_cpu;
    }
    Scheduler* const target = Get(target_cpu);
    Guard<MonitoredSpinLock, NoIrqSave> target_queue_guard{&target->queue_lock_, SOURCE_TAG};

    if (target->IsSchedulerActiveLocked()) {
      SchedTraceWakeup(thread, target_cpu);
      thread->set_ready();
      thread->UpdateRuntimeStats(thread->state());
      if (needs_migration) {
        MarkHasOwnedThreadAccess(*thread);
        target->save_state_list_.push_front(thread);
      } else {
        thread->scheduler_state().curr_cpu_ = target_cpu;
        target->AssertInScheduler(*thread);
        target->Insert(now, thread);

        // If a power level change was requested during insertion, attempt to
        // handle it immediately with the fast path and update any affected
        // CPUs. If the fast path is not supported, the request will be left
        // pending on the target CPU and flushed during its reschedule.
        cpus_to_reschedule =
            target->power_level_control_.SendPendingPowerLevelRequest(target_queue_guard);
      }
      break;
    }

    IncFindTargetCpuRetriesKcounter();
  }
  return {.target_cpu = target_cpu, .cpus_to_reschedule = cpus_to_reschedule};
}

void Scheduler::Unblock(Thread* thread) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(COMMON, "sched_unblock");
  ChainLockTransaction::ActiveRef().AssertFinalized();
  const SchedTime now = CurrentTime();
  RescheduleTargets targets = UnblockCommon(thread, now, UnblockType::Normal);
  thread->get_lock().Release();
  trace.End();

  RescheduleMask(targets.Mask());
}

void Scheduler::Unblock(Thread::UnblockList list) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(COMMON, "sched_unblock_list");
  ChainLockTransaction::ActiveRef().AssertFinalized();
  cpu_mask_t reschedule_mask = 0;
  const SchedTime now = CurrentTime();
  Thread* thread;
  while ((thread = list.pop_back()) != nullptr) {
    DEBUG_ASSERT(!thread->IsIdle());
    thread->get_lock().AssertAcquired();
    RescheduleTargets targets = UnblockCommon(thread, now, UnblockType::Normal);
    reschedule_mask |= targets.Mask();
    thread->get_lock().Release();
  }
  trace.End();

  RescheduleMask(reschedule_mask);
}

void Scheduler::UnblockSynchronous(Thread* thread) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(COMMON, "sched_unblock_sync");
  ChainLockTransaction::ActiveRef().AssertFinalized();
  const SchedTime now = CurrentTime();
  RescheduleTargets targets = UnblockCommon(thread, now, UnblockType::Synchronous);
  thread->get_lock().Release();
  trace.End();

  // If there was a power level change we need to account for, reschedule those CPUs. But do not
  // reschedule the target CPU, if it is the current CPU.
  if (targets.target_cpu == arch_curr_cpu_num()) {
    RescheduleMask(targets.cpus_to_reschedule);
  } else {
    RescheduleMask(targets.Mask());
  }
}

void Scheduler::UnblockIdle(Thread* thread) {
  SchedulerState* const state = &thread->scheduler_state();

  DEBUG_ASSERT(thread->IsIdle());
  DEBUG_ASSERT(ktl::popcount(state->hard_affinity_) == 1);

  const cpu_num_t target_cpu = lowest_cpu_set(state->hard_affinity_);
  Scheduler* const target_scheduler = Get(target_cpu);

  {
    Guard<MonitoredSpinLock, NoIrqSave> queue_guard{&target_scheduler->queue_lock_, SOURCE_TAG};
    thread->set_ready();
    state->curr_cpu_ = target_cpu;
    target_scheduler->AssertInScheduler(*thread);
    thread->scheduler_queue_state().OnInsert();
  }
}

void Scheduler::Yield(Thread* const current_thread) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(COMMON, "sched_yield");
  DEBUG_ASSERT(current_thread == Thread::Current::Get());
  DEBUG_ASSERT(!current_thread->IsIdle());

  if (IsFairThread(current_thread)) {
    Scheduler& current_scheduler = *Get();
    SchedulerState& current_state = current_thread->scheduler_state();

    // If the thread is running in an eligible activation, expire the time slice
    // to push it into the next activation, potentially allowing other eligible
    // threads to run. However, if the start time is in the future, this thread
    // is running because there were no other eligible fair threads to run and
    // the eligible time was snapped forward. In that case, avoid pushing the
    // activation further into the future.
    const SchedTime now = CurrentTime();

    if (current_state.start_time() < now) {
      current_state.time_slice_used_ns_ = current_state.time_slice_ns_;
      current_scheduler.RescheduleCommon(current_thread, trace.Completer());
    }
  }
}

void Scheduler::Preempt(PreemptType preempt_type) {
  Thread* current_thread = Thread::Current::Get();
  SingleChainLockGuard thread_guard{IrqSaveOption, current_thread->get_lock(),
                                    CLT_TAG("Scheduler::Preempt")};
  PreemptLocked(current_thread, preempt_type);
}

void Scheduler::PreemptLocked(Thread* current_thread, PreemptType preempt_type) {
  ktrace::Scope trace =
      LOCAL_KTRACE_BEGIN_SCOPE(COMMON, "sched_preempt", ("type", ToStringRef(preempt_type)));
  const cpu_num_t current_cpu = arch_curr_cpu_num();

  // If any spinlocks are held, we can't immediately reschedule.  Instead, send
  // this CPU a reschedule IPI that will fire once the last spinlock has been
  // released and interrupts have been re-enabled.
  if (arch_num_spinlocks_held() > 0) {
    PreemptionState& preemption_state = current_thread->preemption_state();
    preemption_state.preempts_pending_add(cpu_num_to_mask(current_cpu));
    mp_reschedule_self();
    return;
  }

  SchedulerState& current_state = current_thread->scheduler_state();
  DEBUG_ASSERT(current_state.curr_cpu_ == current_cpu);
  DEBUG_ASSERT(current_state.last_cpu_ == current_state.curr_cpu_);
  DEBUG_ASSERT(current_thread->state() == THREAD_RUNNING);

  Get()->RescheduleCommon(current_thread, trace.Completer());
}

void Scheduler::Reschedule(Thread* const current_thread) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(COMMON, "sched_reschedule");

  DEBUG_ASSERT(current_thread == Thread::Current::Get());
  SchedulerState* const current_state = &current_thread->scheduler_state();
  const cpu_num_t current_cpu = arch_curr_cpu_num();

  const bool preempt_enabled = current_thread->preemption_state().EvaluateTimesliceExtension();

  // Pend the preemption rather than rescheduling if preemption is disabled.
  if (!preempt_enabled) {
    current_thread->preemption_state().preempts_pending_add(cpu_num_to_mask(current_cpu));
    return;
  }

  // If any spinlocks are held, we can't immediately reschedule.  Instead, send
  // this CPU a reschedule IPI that will fire once the last spinlock has been
  // released and interrupts have been re-enabled.
  if (arch_num_spinlocks_held() > 0) {
    current_thread->preemption_state().preempts_pending_add(cpu_num_to_mask(current_cpu));
    mp_reschedule_self();
    return;
  }

  DEBUG_ASSERT(current_state->curr_cpu_ == current_cpu);
  DEBUG_ASSERT(current_state->last_cpu_ == current_state->curr_cpu_);

  Get()->RescheduleCommon(current_thread, trace.Completer());
}

void Scheduler::RescheduleInternal(Thread* const current_thread) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(COMMON, "sched_resched_internal");
  Get()->RescheduleCommon(current_thread, trace.Completer());
}

void Scheduler::MigrateUnpinnedThreads() {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(COMMON, "sched_migrate_unpinned");

  const cpu_num_t current_cpu = arch_curr_cpu_num();
  const cpu_mask_t current_cpu_mask = cpu_num_to_mask(current_cpu);
  cpu_mask_t cpus_to_reschedule_mask = 0;

  // Flag this scheduler as being in the process de-activating.  If a thread
  // with a migration function unblocks and needs to become READY while we are
  // in the middle of MigrateUnpinnedThreads, we will allow it to join this
  // CPU's ready-queue knowing that we are going to immediately call the
  // thread's migration function (on this CPU, as required) before reassigning
  // the thread to a different CPU.
  //
  // Note: Because of the nature of the global thread-lock, this race is not
  // currently possible (Threads who are blocked cannot unblock while we hold
  // the global thread lock, which we currently do).  Once the thread lock has
  // been removed, however, this race will become an important edge case to
  // handle.
  const SchedTime now = CurrentTime();
  Scheduler* const current = Get(current_cpu);
  current->set_cpu_deactivating(true);

  // Flag this CPU as being inactive.  This will prevent new threads from being
  // added to this CPU's scheduler's run queues while we migrate the existing
  // threads off this scheduler.
  //
  // Note that this does _not_ prevent FindTargetCpu from _selecting_ a given
  // scheduler as potential target for thread placement, but it _does_ guarantee
  // that by the time we attempt to actually add the thread to the chosen
  // scheduler's queue, we will have had a chance to check the flag once again,
  // this time safely from with the chosen scheduler's queue lock.
  Scheduler::SetCurrCpuActive(false);

  // Call all migrate functions for threads last run on the current CPU who are
  // not currently READY.  The next time these threads unblock or wake up, the
  // will be assigned to a different CPU, and we will have already called their
  // migrate function for them.  When we are finished with this pass, the only
  // threads who have a migrate function still to be called should only exist in
  // this scheduler's READY queue, because:
  //
  // 1)  New threads or threads who have not recently run on this CPU cannot be
  //     assigned to this CPU (because we cleared our active flag)
  // 2a) Threads who were not ready but needed to have their migration function
  //     called were found and had their migration function called during
  //     CallMigrateFnForCpu  --- OR ---
  // 2b) The non-ready thread with a migration function unblocked and joined the
  //     ready queue of this scheduler while it was being deactivated.  We will
  //     call their migration functions next as we move all of the READY threads
  //     off of this scheduler and over to a different one.
  //
  Thread::CallMigrateFnForCpu(current_cpu);

  // There should no longer be any non-ready threads who need to run a
  // migration function on this CPU, and there should not be any new one until
  // we set this CPU back to being active.  We can clear the transitory
  // "cpu_deactivating_" flag now.
  DEBUG_ASSERT(current->cpu_deactivating());
  current->set_cpu_deactivating(false);

  // Now move any READY threads who can be moved over to a temporary list,
  // flagging them as in the process of currently migrating. Also move all
  // of the threads in the save_state_list into a temporary list, as they
  // need to be migrated as well.
  Thread::UnblockList migrating_deadline_threads;
  Thread::UnblockList migrating_fair_threads;
  Thread::SaveStateList local_save_state_list;
  sched::Affine affine_transform;
  {
    Guard<MonitoredSpinLock, NoIrqSave> queue_guard{&current->queue_lock_, SOURCE_TAG};

    // Save the current affine transform to update fair threads as they are migrated.
    affine_transform = current->rust_state_.fair_affine_transform;

    if (!current->save_state_list_.is_empty()) {
      local_save_state_list = ktl::move(current->save_state_list_);
    }

    auto CreateMoveList =
        [current, current_cpu_mask](Thread::UnblockList& thread_list, RunQueue& run_queue)
            TA_REQ(current->queue_lock_) {
              (void)current;  // We cannot annotate 'current' with [[maybe_unused]] in the capture
                              // list, so suppress the warning using the old school void-cast trick.
              DEBUG_ASSERT((&run_queue == &current->rust_state_.fair_run_queue) ||
                           (&run_queue == &current->rust_state_.critical_deadline_run_queue) ||
                           (&run_queue == &current->rust_state_.deadline_run_queue));

              for (RunQueue::iterator iter = run_queue.begin(); iter.IsValid();) {
                Thread& consider = *(iter++);

                // The no-op version of this assertion should be sufficient here.  We
                // are holding the scheduler's queue lock, and iterating over the
                // scheduler's run queues.  All of the threads in those queues are
                // clearly owned by this scheduler right now.
                MarkHasOwnedThreadAccess(consider);
                const SchedulerState& ss = const_cast<const Thread&>(consider).scheduler_state();

                // If the thread can run on any other CPU, add it to our list of threads to migrate.
                if (ss.hard_affinity_ != current_cpu_mask) {
                  thread_list.push_back(&consider);
                }
              }
            };

    CreateMoveList(migrating_fair_threads, current->rust_state_.fair_run_queue);
    CreateMoveList(migrating_deadline_threads, current->rust_state_.critical_deadline_run_queue);
    CreateMoveList(migrating_deadline_threads, current->rust_state_.deadline_run_queue);
  }

  // OK, now that we have dropped our queue lock, go over our list of migrating
  // threads and finish the migration process.  Don't make any assumptions about
  // the fair/deadline nature of the threads as we migrate.  Since we dropped
  // the scheduler's queue lock, it is possible that these threads have changed
  // effective priority since being remove from their old scheduler, but before
  // arriving at a new one.
  auto MigrateThread = [current, now, &affine_transform](
                           Thread* thread, bool update_timeline,
                           const fxt::InternedString& trace_tag) -> cpu_num_t {
    SingleChainLockGuard guard{IrqSaveOption, thread->get_lock(),
                               CLT_TAG("Scheduler::MigrateUnpinnedThreads")};

    {
      // Removing a thread from a scheduler requires queue_lock acquisition.
      Guard<MonitoredSpinLock, NoIrqSave> queue_guard{&current->queue_lock_, SOURCE_TAG};

      // Even though we dropped the queue lock between when we accumulated the list of threads to
      // move, and did not hold the thread lock prior to this point, this thread should still be
      // associated with this scheduler because the scheduler was marked inactive above. This means
      // that any attempt to steal work from this scheduler should fail.
      DEBUG_ASSERT(thread->scheduler_state().curr_cpu() == current->this_cpu());
      MarkHasOwnedThreadAccess(*thread);
      current->TraceThreadQueueEvent(trace_tag, thread);
      current->EraseFromQueue(thread);
      current->RemoveThreadLocked(now, thread);
    }

    // Threads removed from the fair run queue must be transformed to the monotonic timeline
    // before migrating.
    if (update_timeline) {
      DEBUG_ASSERT(IsFairThread(thread));
      affine_transform.VariableToMonotonicInPlace(thread->scheduler_state().start_time_);
      affine_transform.VariableToMonotonicInPlace(thread->scheduler_state().finish_time_);
    }

    // Call the Save stage of the thread's migration function as it leaves
    // this CPU.
    thread->CallMigrateFnLocked(Thread::MigrateStage::Save);

    const cpu_num_t target_cpu =
        FindActiveSchedulerForThread(thread, [current, now](Thread* thread, Scheduler* target) {
          // Finish the transition and add the thread to the new target queue.  The
          // Restore stage of the migration function will be called on the new CPU the
          // next time the thread becomes scheduled.
          MarkInFindActiveSchedulerForThreadCbk(*thread, *target);
          (void)current;  // We cannot annotate 'current' with [[maybe_unused]] in the capture
                          // list, so suppress the warning using the old school void-cast trick.
          DEBUG_ASSERT(target != current);
          target->Insert(now, thread, Placement::Migration);
        });

    return target_cpu;
  };

  while (!migrating_fair_threads.is_empty()) {
    Thread* thread = migrating_fair_threads.pop_front();
    const cpu_num_t target_cpu =
        MigrateThread(thread, /*update_timeline=*/true, "tqe_deque_migrate_unpinned_fair"_intern);
    cpus_to_reschedule_mask |= cpu_num_to_mask(target_cpu);
  }

  while (!migrating_deadline_threads.is_empty()) {
    Thread* thread = migrating_deadline_threads.pop_front();
    const cpu_num_t target_cpu = MigrateThread(thread, /*update_timeline=*/false,
                                               "tqe_deque_migrate_unpinned_deadline"_intern);
    cpus_to_reschedule_mask |= cpu_num_to_mask(target_cpu);
  }

  while (!local_save_state_list.is_empty()) {
    Thread* thread = local_save_state_list.pop_front();
    const cpu_num_t target_cpu = MigrateThread(thread, /*update_timeline=*/false,
                                               "tqe_deque_migrate_unpinned_save_state"_intern);
    cpus_to_reschedule_mask |= cpu_num_to_mask(target_cpu);
  }

  trace.End();
  RescheduleMask(cpus_to_reschedule_mask);
}

void Scheduler::TimerTick(SchedTime now) {
  ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(COMMON, "sched_timer_tick");
  Thread::Current::preemption_state().PreemptSetPending();
}

void Scheduler::UpdateProcessingRates(ktl::span<zx_cpu_performance_info_t> info) {
  DEBUG_ASSERT(info.size() <= percpu::processor_count());
  InterruptDisableGuard irqd;

  cpu_num_t cpus_to_reschedule_mask = 0;
  for (auto& entry : info) {
    DEBUG_ASSERT(entry.logical_cpu_number <= percpu::processor_count());

    cpus_to_reschedule_mask |= cpu_num_to_mask(entry.logical_cpu_number);
    Scheduler* scheduler = Scheduler::Get(entry.logical_cpu_number);
    Guard<MonitoredSpinLock, NoIrqSave> guard{&scheduler->queue_lock_, SOURCE_TAG};

    // TODO(eieio): Apply a minimum value threshold.
    rust_scheduler_user_set_processing_rate(scheduler,
                                            ToSchedProcessingRate(entry.performance_scale));

    // Return the original performance scale (i.e. processing rate).
    entry.performance_scale = ToUserPerformanceScale(scheduler->processing_rate());
  }

  RescheduleMask(cpus_to_reschedule_mask);
}

void Scheduler::UpdateProcessingLimits(ktl::span<zx_cpu_perf_limit_t> limits) {
  DEBUG_ASSERT(limits.size() <= percpu::processor_count());
  InterruptDisableGuard interrupts_disabled;

  // Set the limits for each CPU in the given limits array, keeping track of one
  // CPU per domain to evaluate the domain's active power level.
  cpu_mask_t cpus_for_handled_domains = 0;
  cpu_mask_t cpus_to_evaluate_updates = 0;
  for (auto& entry : limits) {
    switch (entry.limit_type) {
      case ZX_CPU_PERF_LIMIT_TYPE_RATE: {
        Scheduler* scheduler = Scheduler::Get(entry.logical_cpu_number);
        Guard<MonitoredSpinLock, NoIrqSave> guard{&scheduler->queue_lock_, SOURCE_TAG};

        using power_management::PowerLevel;

        const SchedProcessingRate min =
            PowerLevel::ToProcessingRate(entry.min, PowerLevel::kUserProcessingRateScale);
        const SchedProcessingRate max =
            PowerLevel::ToProcessingRate(entry.max, PowerLevel::kUserProcessingRateScale);
        scheduler->power_level_control_.UserSetProcessingRateLimits(min, max);

        // A rate change only needs to be evaluated for one active CPU in each domain.
        const cpu_mask_t cpu_mask = cpu_num_to_mask(entry.logical_cpu_number);
        const bool is_domain_handled = cpus_for_handled_domains & cpu_mask;
        if (scheduler->IsSchedulerActiveLocked() && scheduler->power_level_control_.is_enabled() &&
            !is_domain_handled) {
          cpus_to_evaluate_updates |= cpu_mask;

          const cpu_mask_t domain_cpu_mask = scheduler->power_level_control_.domain()->cpus();
          cpus_for_handled_domains |= domain_cpu_mask;
        }
      } break;

      default:
        break;
    }
  }

  // Reevaluate the power level for each updated domain.
  cpu_mask_t cpu_mask = cpus_to_evaluate_updates;
  for (cpu_num_t cpu = remove_cpu_from_mask(cpu_mask); cpu != INVALID_CPU;
       cpu = remove_cpu_from_mask(cpu_mask)) {
    Scheduler* scheduler = Scheduler::Get(cpu);
    Guard<MonitoredSpinLock, NoIrqSave> guard{&scheduler->queue_lock_, SOURCE_TAG};
    scheduler->power_level_control_.ReevaluateCurrentPowerLevel();
  }

  RescheduleMask(cpus_to_evaluate_updates);
}

cpu_mask_t Scheduler::SetCpuAffinity(Thread& thread, cpu_mask_t affinity,
                                     AffinityType affinity_type) {
  DEBUG_ASSERT_MSG(
      const cpu_mask_t active_mask = PeekActiveMask();
      affinity_type == AffinityType::Soft || (affinity & active_mask) != 0,
      "Attempted to set affinity mask to %#x, which has no overlap of active CPUs %#x.", affinity,
      active_mask);

  // Utility to set the correct affinity mask based on the affinity type. Uses a
  // function pointer conversion (i.e. +) to avoid a bug with static annotations
  // on some lambdas, where the compiler complains about not holding the lambda
  // as a capability.
  const auto set_affinity = +[](Thread& thread, cpu_mask_t affinity, AffinityType affinity_type)
                                 TA_REQ(thread.get_lock()) -> cpu_mask_t {
    switch (affinity_type) {
      case AffinityType::Hard:
        ktl::swap(thread.scheduler_state().hard_affinity_, affinity);
        break;

      case AffinityType::Soft:
        ktl::swap(thread.scheduler_state().soft_affinity_, affinity);
        break;
    }
    return affinity;
  };

  const auto do_transaction =
      [&]() TA_REQ(chainlock_transaction_token) -> ChainLockTransaction::Result<cpu_mask_t> {
    thread.get_lock().AcquireFirstInChain();

    // Mutating the affinity mask in the scheduler state requires holding both
    // the thread lock and the container lock, if any.
    //
    // TODO(johngro): It is really easy to mess this requirement up. Is there a
    // better way to statically assert these requirements?
    //
    // TODO(eieio): Why does the wait queue lock need to be held when updating
    // the affinity mask of a blocked thread? Only scheduling needs to read the
    // affinity mask and make decisions based on it, and even then only tracing
    // needs to read the affinity mask while holding only the queue lock. How
    // are wait queues different than the no container case wrt affinity?

    // The thread container is a scheduler.
    if ((thread.state() == THREAD_RUNNING) || (thread.state() == THREAD_READY)) {
      ktrace::Scope trace = LOCAL_KTRACE_BEGIN_SCOPE(COMMON, "sched_migrate");

      ChainLockTransaction::Finalize();
      SchedulerState& state = thread.scheduler_state();

      DEBUG_ASSERT(state.curr_cpu() != INVALID_CPU);
      const cpu_mask_t thread_cpu_mask = cpu_num_to_mask(state.curr_cpu());

      cpu_mask_t cpus_to_reschedule_mask = 0;
      bool find_new_target_cpu = false;
      cpu_mask_t previous_affinity = 0;

      // Use a lambda to prevent the AssertInScheduler from leaking out of the
      // scope of the queue lock guard. Static lock assertions can leak out of
      // nested scopes, including conditionals and loops.
      [&]() TA_REQ(thread.get_lock()) {
        Scheduler& scheduler = *Scheduler::Get(state.curr_cpu());
        Guard<MonitoredSpinLock, NoIrqSave> queue_guard{&scheduler.queue_lock(), SOURCE_TAG};

        scheduler.AssertInScheduler(thread);
        SchedulerQueueState& queue_state = thread.scheduler_queue_state();
        const Disposition disposition = queue_state.disposition();

        // Set the affinity before determining if the thread needs to move.
        previous_affinity = set_affinity(thread, affinity, affinity_type);

        const cpu_mask_t effective_cpu_mask = state.GetEffectiveCpuMask(PeekActiveMask());
        const bool stale_curr_cpu = (thread_cpu_mask & effective_cpu_mask) == 0;

        if (thread.state() == THREAD_RUNNING) {
          DEBUG_ASSERT(disposition == Disposition::Associated);
          if (stale_curr_cpu) {
            cpus_to_reschedule_mask |= thread_cpu_mask;
          }
        } else {
          DEBUG_ASSERT(thread.state() == THREAD_READY);
          DEBUG_ASSERT(disposition != Disposition::Unassociated);

          if (disposition == Disposition::Associated) {
            // The thread is transitioning to RUNNING: curr_cpu is correct.
            CountUpdateInTransition();
            if (stale_curr_cpu) {
              cpus_to_reschedule_mask |= thread_cpu_mask;
            }
          } else if (disposition == Disposition::Stolen) {
            // The thread is being stolen: curr_cpu is incorrect, use stolen_by.
            CountUpdateInTransition();
            DEBUG_ASSERT(queue_state.stolen_by() != INVALID_CPU);
            const cpu_mask_t stealing_cpu_mask = cpu_num_to_mask(queue_state.stolen_by());
            const bool stale_stealing_cpu = (stealing_cpu_mask & effective_cpu_mask) == 0;
            if (stale_stealing_cpu) {
              cpus_to_reschedule_mask |= stealing_cpu_mask;
            }
          } else {
            // The READY thread is waiting in the run queue: curr_cpu is correct.
            DEBUG_ASSERT(disposition == Disposition::Enqueued);
            if (stale_curr_cpu) {
              scheduler.EraseFromQueue(&thread);
              scheduler.RemoveThreadLocked(CurrentTime(), &thread);

              // Execute the migrate function inline if the thread is on the local
              // CPU. Otherwise, add it to the save state list of the last CPU it
              // ran on and reschedule that CPU to process the list.
              if (thread.has_migrate_fn() && !thread.migrate_pending() &&
                  state.last_cpu() != INVALID_CPU && state.last_cpu() != arch_curr_cpu_num()) {
                scheduler.save_state_list_.push_front(&thread);
                cpus_to_reschedule_mask |= thread_cpu_mask;
              } else {
                find_new_target_cpu = true;
              }
            }
          }
        }
      }();

      // Find a new target CPU for the thread.
      if (find_new_target_cpu) {
        DEBUG_ASSERT(thread.state() == THREAD_READY);
        // Call the migration function if the thread is migrating away from this
        // CPU. This is a no-op if there is no migration function or the Save
        // state has already been invoked.
        if (state.last_cpu() == arch_curr_cpu_num()) {
          thread.CallMigrateFnLocked(Thread::MigrateStage::Save);
        }

        // Find a new CPU for the thread and add it to the queue.
        const cpu_num_t target_cpu =
            FindActiveSchedulerForThread(&thread, [](Thread* thread, Scheduler* target) {
              MarkInFindActiveSchedulerForThreadCbk(*thread, *target);
              target->Insert(CurrentTime(), thread, Placement::Migration);
            });

        // Reschedule both CPUs to handle the run queue changes.
        cpus_to_reschedule_mask |= cpu_num_to_mask(target_cpu) | thread_cpu_mask;
      }

      trace.End();
      thread.get_lock().Release();
      RescheduleMask(cpus_to_reschedule_mask);
      return previous_affinity;
    }

    // The thread container is a wait queue.
    // TODO(eieio): Why do we need to synchronize the affinity mask with the
    // wait queue?
    if (thread.state() == THREAD_BLOCKED) {
      WaitQueueBase* wait_queue = thread.wait_queue_state().blocking_wait_queue_;
      DEBUG_ASSERT(wait_queue != nullptr);

      if (!wait_queue->get_lock().AcquireOrBackoff()) {
        thread.get_lock().Release();
        return ChainLockTransaction::Action::Backoff;
      }

      ChainLockTransaction::Finalize();
      const cpu_mask_t previous_affinity = set_affinity(thread, affinity, affinity_type);

      wait_queue->get_lock().Release();
      thread.get_lock().Release();
      return previous_affinity;
    }

    // The thread has no container (e.g. SLEEPING, SUSPENDED).
    ChainLockTransaction::Finalize();
    const cpu_mask_t previous_affinity = set_affinity(thread, affinity, affinity_type);

    thread.get_lock().Release();
    return previous_affinity;
  };

  return ChainLockTransaction::UntilDone(IrqSaveOption, CLT_TAG("Scheduler::SetCpuAffinity"),
                                         do_transaction);
}

bool Scheduler::RequestPowerLevelForTesting(uint8_t power_level) {
  InterruptDisableGuard interrupts_disabled;
  bool need_reschedule;
  {
    Guard<MonitoredSpinLock, NoIrqSave> guard{&queue_lock_, SOURCE_TAG};
    need_reschedule = power_level_control_.IsValidActivePowerLevel(power_level) &&
                      power_level_control_.PendPowerLevelRequest(power_level);
  }
  if (need_reschedule) {
    RescheduleMask(cpu_num_to_mask(this_cpu()));
  }
  return need_reschedule;
}

bool Scheduler::PowerLevelControl::UserSetProcessingRateLimits(SchedProcessingRate min,
                                                               SchedProcessingRate max) {
  return rust_scheduler_user_set_processing_rate_limits(&scheduler(), min, max);
}

bool Scheduler::PowerLevelControl::PendPowerLevelRequest(uint8_t power_level) {
  return rust_scheduler_pend_power_level_request(&scheduler(), power_level);
}

bool Scheduler::PowerLevelControl::PendPowerLevelRequestForRate(
    SchedProcessingRate processing_rate) {
  return rust_scheduler_pend_power_level_request_for_rate(&scheduler(), processing_rate);
}

void Scheduler::PowerLevelControl::ReevaluateCurrentPowerLevel() {
  rust_scheduler_reevaluate_current_power_level(&scheduler());
}

cpu_mask_t Scheduler::PowerLevelControl::SendPendingPowerLevelRequest(
    Guard<MonitoredSpinLock, NoIrqSave>& queue_guard) {
  AssertHeld(scheduler().queue_lock());
  cpu_mask_t cpus_to_reschedule_mask = 0;
  if (domain() && state().has_pending_update_request && power_state().is_serving()) {
    power_management::PowerLevelUpdateRequest request = state().pending_update_request;
    state().has_pending_update_request = false;
    const fbl::RefPtr<PowerDomain>& power_domain = domain();

    ktrace::Scope trace =
        LOCAL_KTRACE_BEGIN_SCOPE(QUEUE, "sched_opp_request_fast", ("domain_id", request.domain_id),
                                 ("control_argument", request.control_argument));

    queue_guard.CallUnlocked([&] {
      if (const zx::result<cpu_mask_t> result = power_domain->controller()->Post(request);
          result.is_ok()) {
        cpus_to_reschedule_mask = result.value();
      } else {
        KERNEL_OOPS("Failed to set OPP: domain_id=%" PRIu32 " control_argument=%" PRIu64 ": %d\n",
                    request.domain_id, request.control_argument, result.status_value());
      }
    });

    trace = KTRACE_END_SCOPE(("cpus_to_reschedule_mask", cpus_to_reschedule_mask));
  }
  return cpus_to_reschedule_mask;
}

void Scheduler::SetPowerDomainSet(PowerDomainSet power_domain_set) {
  percpu::ForEach([&power_domain_set](cpu_num_t cpu_num, percpu* percpu) {
    percpu->scheduler.ExchangePowerDomainSet(power_domain_set);
  });
}

zx::result<cpu_mask_t> Scheduler::UpdatePowerLevel(uint32_t domain_id, uint64_t controller_id,
                                                   power_management::ControlInterface interface,
                                                   uint64_t arg) {
  // Get a ref to the power domain using the power domain set for the current
  // CPU.
  fbl::RefPtr<PowerDomain> domain;
  {
    Scheduler* scheduler = Get();
    Guard<MonitoredSpinLock, IrqSave> guard{&scheduler->queue_lock_, SOURCE_TAG};
    const PowerDomainSet& power_domain_set = scheduler->power_level_control_.power_domain_set();
    domain = fbl::RefPtr{power_domain_set.FindByDomainId(domain_id)};
  }

  if (!domain) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  if (domain->controller()->id() != controller_id ||
      domain->controller()->control_interface() != interface) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  ktl::optional<uint8_t> power_level = domain->model().FindPowerLevel(interface, arg);
  if (!power_level.has_value()) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  cpu_mask_t domain_cpus = domain->cpus();
  cpu_mask_t cpus_to_reschedule_mask = 0;
  while (domain_cpus != 0) {
    const cpu_num_t cpu_num = remove_cpu_from_mask(domain_cpus);
    Scheduler* scheduler = Get(cpu_num);
    if (scheduler->UpdateActivePowerLevel(*power_level).is_ok()) {
      cpus_to_reschedule_mask |= cpu_num_to_mask(cpu_num);
    }
  }

  return zx::ok(cpus_to_reschedule_mask);
}
