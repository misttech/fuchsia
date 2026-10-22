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
#include <lib/kconcurrent/chainlock.h>
#include <lib/kconcurrent/chainlock_transaction.h>
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
#include <kernel/cpu.h>
#include <kernel/ffi.h>
#include <kernel/lockdep.h>
#include <kernel/mp.h>
#include <kernel/percpu.h>
#include <kernel/scheduler_state.h>
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

#include <ktl/enforce.h>

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

Scheduler* Scheduler::Get() { return Get(arch_curr_cpu_num()); }

Scheduler* Scheduler::Get(cpu_num_t cpu) { return &percpu::Get(cpu).scheduler; }

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
    const PowerDomainSet& power_domain_set = scheduler->power_state_.power_domain_set();
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

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Scheduler* cpp_scheduler_get(cpu_num_t cpu) { return Scheduler::Get(cpu); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_block(Thread* current_thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  DEBUG_ASSERT(current_thread != nullptr);
  Scheduler::Block(current_thread);
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
