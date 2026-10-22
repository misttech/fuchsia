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

void Scheduler::CountUpdateInTransition() { counter_update_in_transition.Add(1); }

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

void Scheduler::Unblock(Thread* thread) { rust_scheduler_unblock(thread); }

void Scheduler::Unblock(Thread::UnblockList list) { rust_scheduler_unblock_list(&list); }

void Scheduler::UnblockSynchronous(Thread* thread) { rust_scheduler_unblock_synchronous(thread); }

void Scheduler::UnblockIdle(Thread* thread) { rust_scheduler_unblock_idle(thread); }

void Scheduler::Yield(Thread* const current_thread) { rust_scheduler_yield(current_thread); }

void Scheduler::Preempt(PreemptType preempt_type) {
  rust_scheduler_preempt(static_cast<uint8_t>(preempt_type));
}

void Scheduler::PreemptLocked(Thread* current_thread, PreemptType preempt_type) {
  rust_scheduler_preempt_locked(current_thread, static_cast<uint8_t>(preempt_type));
}

void Scheduler::Reschedule(Thread* const current_thread) {
  rust_scheduler_reschedule(current_thread);
}

void Scheduler::RescheduleInternal(Thread* const current_thread) {
  rust_scheduler_reschedule_internal(current_thread);
}

void Scheduler::MigrateUnpinnedThreads() { rust_kernel_scheduler_migrate_unpinned_threads(Get()); }

void Scheduler::TimerTick(SchedTime now) { rust_kernel_scheduler_timer_tick(now); }

void Scheduler::UpdateProcessingRates(ktl::span<zx_cpu_performance_info_t> info) {
  rust_scheduler_update_processing_rates(info.data(), info.size());
}

void Scheduler::UpdateProcessingLimits(ktl::span<zx_cpu_perf_limit_t> limits) {
  rust_scheduler_update_processing_limits(limits.data(), limits.size());
}

cpu_mask_t Scheduler::SetCpuAffinity(Thread& thread, cpu_mask_t affinity,
                                     AffinityType affinity_type) {
  return rust_kernel_scheduler_set_cpu_affinity(&thread, affinity,
                                                static_cast<int32_t>(affinity_type));
}

bool Scheduler::RequestPowerLevelForTesting(uint8_t power_level) {
  return rust_scheduler_request_power_level_for_testing(this, power_level);
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
