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

// Counts the number of times a thread's effective profile or affinity changed
// while transitioning from READY to RUNNING.
KCOUNTER(counter_update_in_transition, "scheduler.update_in_transition")

// Counts the number of times we had to run FindTargetCpu again because our
// selected Target became in-active after we chose it.
KCOUNTER(counter_find_target_cpu_retries, "scheduler.find_target_cpu.retries")

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
  if (IdlePowerThread::PendingPowerWorkPredicate pending_work =
          percpu::Get(sched->this_cpu()).idle_power_thread.pending_power_work()) {
    // If the idle power thread is in the (offline, offline) state, CPU hot
    // plugging attempted to block before restoring the idle power thread.
    DEBUG_ASSERT(pending_work.current() != IdlePowerThread::State::Offline ||
                 pending_work.target() != IdlePowerThread::State::Offline);
    return true;
  }
  return false;
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

    scheduler->rust_state_.weight_total += state.effective_profile_.weight();
    DEBUG_ASSERT(scheduler->rust_state_.weight_total > SchedWeight{0});
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
    DEBUG_ASSERT(scheduler->rust_state_.weight_total >= SchedWeight{0});
    scheduler->rust_state_.runnable_task_count--;
    scheduler->UpdateTotalExpectedRuntime(-state.expected_runtime_ns_);

    state.base_profile_.fair.weight = SchedulerState::ConvertPriorityToWeight(IDLE_PRIORITY);
    state.effective_profile_.MarkBaseProfileChanged();
    state.RecomputeEffectiveProfile();
  }
}

// Attempts to steal work from other busy CPUs and move it to the local run
// queues. Returns a pointer to the stolen thread that is now associated with
// the local Scheduler instance, or nullptr is no work was stolen.
Scheduler::DequeueResult Scheduler::StealWork(SchedTime now, SchedProcessingRate processing_rate) {
  DequeueResult result;
  rust_scheduler_steal_work(this, now, processing_rate, &result);
  return result;
}

Thread* Scheduler::FindEarlierDeadlineThread(SchedTime eligible_time,
                                             const Thread* reference_thread) {
  return rust_scheduler_find_earlier_deadline_thread(this, eligible_time, reference_thread);
}

SchedTime Scheduler::GetNextEligibleTime() const {
  return rust_scheduler_get_next_eligible_time(this);
}

Scheduler::DequeueResult Scheduler::EvaluateNextThread(
    SchedTime now, Thread* current_thread, bool current_is_migrating,
    Guard<MonitoredSpinLock, NoIrqSave>& queue_guard) {
  DequeueResult result;
  if (rust_scheduler_evaluate_next_thread(this, now, current_thread, current_is_migrating,
                                          &result)) {
    return result;
  }

  // Release the queue lock while attempting to steal work, leaving IRQs
  // disabled.
  queue_guard.CallUnlocked([&, processing_rate = power_level_control_.processing_rate()] {
    ChainLockTransaction::AssertActive();
    result = StealWork(now, processing_rate);
  });

  if (result) {
    return result;
  }

  return &percpu::Get(this_cpu()).idle_power_thread.thread();
}

cpu_num_t Scheduler::FindTargetCpu(Thread* thread, UnblockType unblock_type) {
  return rust_scheduler_find_target_cpu(thread, unblock_type == UnblockType::Synchronous ? 1 : 0);
}

void Scheduler::IncFindTargetCpuRetriesKcounter() { counter_find_target_cpu_retries.Add(1u); }

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
  // Pass a null callback when there is no outer trace to end (the common case
  // when tracing is disabled) so the Rust side can skip the erased scope.
  if (!end_outer_trace) {
    rust_scheduler_reschedule_common(this, current_thread, nullptr, nullptr);
    return;
  }
  auto cb = [](void* ctx) { (*static_cast<EndTraceCallback*>(ctx))(); };
  rust_scheduler_reschedule_common(this, current_thread, cb, &end_outer_trace);
}

void Scheduler::TrampolineLockHandoff() { rust_kernel_scheduler_lock_handoff(); }

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

void Scheduler::Block(Thread* const current_thread) { rust_kernel_scheduler_block(current_thread); }

Scheduler::RescheduleTargets Scheduler::UnblockCommon(Thread* thread, SchedTime now,
                                                      UnblockType unblock_type) {
  RescheduleTargets targets;
  rust_scheduler_unblock_common(thread, now, unblock_type == UnblockType::Synchronous ? 1 : 0,
                                &targets);
  return targets;
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

void Scheduler::TimerTick(SchedTime now) { rust_kernel_scheduler_timer_tick(now); }

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
