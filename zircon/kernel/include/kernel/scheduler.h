// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_SCHEDULER_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_SCHEDULER_H_

#include <debug.h>
#include <lib/fit/function.h>
#include <lib/fxt/interned_string.h>
#include <lib/fxt/string_ref.h>
#include <lib/power-management/bandwidth-reservation-cache.h>
#include <lib/power-management/energy-model.h>
#include <lib/power-management/power-level-controller.h>
#include <lib/power-management/power-state.h>
#include <lib/power-management/types.h>
#include <lib/relaxed_atomic.h>
#include <lib/sched/affine.h>
#include <lib/zircon-internal/macros.h>
#include <platform.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/syscalls/scheduler.h>
#include <zircon/syscalls/system.h>
#include <zircon/types.h>

#include <fbl/intrusive_pointer_traits.h>
#include <fbl/intrusive_wavl_tree.h>
#include <fbl/wavl_tree_augmented_invariant_observer.h>
#include <ffl/fixed.h>
#include <kernel/cpu_search_set.h>
#include <kernel/dpc.h>
#include <kernel/ffi.h>
#include <kernel/mp.h>
#include <kernel/owned_wait_queue.h>
#include <kernel/scheduler_state.h>
#include <kernel/spinlock.h>
#include <kernel/thread.h>
#include <kernel/wait.h>
#include <ktl/algorithm.h>
#include <ktl/atomic.h>
#include <ktl/optional.h>
#include <ktl/span.h>
#include <ktl/type_traits.h>
#include <ktl/utility.h>

// Forward declarations.
struct percpu;
struct cpu_stats;

struct RunQueueIterator {
  Thread* current;
};
static_assert(sizeof(RunQueueIterator) == 8 && alignof(RunQueueIterator) == 8);

extern "C" {
void* cpp_thread_get_queue_node(Thread* t);
void rust_kernel_run_queue_insert(void* rq, Thread* thread);
Thread* rust_kernel_run_queue_erase(void* rq, Thread* thread);
Thread* rust_kernel_run_queue_find_earliest_eligible_thread(const void* rq, SchedTime eligible_time,
                                                            bool (*predicate)(const Thread*,
                                                                              const void*),
                                                            const void* context);
size_t rust_kernel_run_queue_size(const void* rq);
bool rust_kernel_run_queue_is_empty(const void* rq);
Thread* rust_kernel_run_queue_front(const void* rq);
RunQueueIterator rust_kernel_run_queue_iterator_create(const void* rq);
bool rust_kernel_run_queue_iterator_is_valid(const RunQueueIterator* iter);
Thread* rust_kernel_run_queue_iterator_get(const RunQueueIterator* iter);
void rust_kernel_run_queue_iterator_next(RunQueueIterator* iter);
Thread* rust_kernel_run_queue_root(const void* rq);
void rust_kernel_run_queue_init(void* rq);
void rust_kernel_run_queue_destroy(void* rq);
}

// Ensure this define has a value when not defined globally by the build system.
#ifndef SCHEDULER_TRACING_LEVEL
#define SCHEDULER_TRACING_LEVEL 0
#endif

// Ensure this define has a value when not defined globally by the build system.
#ifndef SCHEDULER_QUEUE_TRACING_ENABLED
#define SCHEDULER_QUEUE_TRACING_ENABLED false
#endif

// The processing rate of a CPU relative to the fastest CPU in the system.
using SchedProcessingRate = ffl::Fixed<int64_t, 31>;

// Make sure lib/power-management and the scheduler definition stay in sync.
static_assert(ktl::is_same_v<power_management::ProcessingRate, SchedProcessingRate>);

class Scheduler;

extern "C" {

Scheduler* cpp_scheduler_get(cpu_num_t cpu);
void cpp_scheduler_save_state_list_push_front(Scheduler* sched, Thread* thread);

cpu_stats* cpp_get_cpu_stats(cpu_num_t cpu_num);
uint64_t cpp_scheduler_power_state_active_power_coefficient_nw(const Scheduler* sched);
uint64_t cpp_scheduler_power_level_control_max_idle_power_coefficient_nw(const Scheduler* sched);

void rust_kernel_scheduler_dump(Scheduler* sched, FILE* output_target, bool queue_state_only);
void rust_kernel_scheduler_dump_active_thread(Scheduler* sched, FILE* output_target);
void cpp_thread_dump_active(const Thread* thread, FILE* output_target);
void cpp_scheduler_lock_queue(Scheduler* sched);
void cpp_scheduler_unlock_queue(Scheduler* sched);
uint64_t cpp_scheduler_lock_queue_irq_save(Scheduler* sched);
void cpp_scheduler_unlock_queue_irq_restore(Scheduler* sched, uint64_t state);
void cpp_thread_update_runtime_stats(Thread* thread, int32_t state);
void rust_kernel_scheduler_lock_handoff();
void rust_kernel_scheduler_block(Thread* current_thread);
void rust_kernel_scheduler_timer_tick(SchedTime now);
int32_t cpp_thread_get_disposition(const Thread* thread);
bool cpp_thread_has_migrate_fn(const Thread* thread);
bool cpp_thread_migrate_pending(const Thread* thread);
SchedulerState* cpp_thread_get_scheduler_state(Thread* t);
void cpp_stall_accumulator_apply_context_switch(Thread* current_thread, Thread* next_thread);
Thread* cpp_scheduler_get_idle_thread(const Scheduler* sched);
bool cpp_scheduler_has_pending_power_work(const Scheduler* sched);

bool cpp_scheduler_power_level_control_is_enabled(const Scheduler* sched);
bool cpp_scheduler_power_state_scheduler_control_enabled(const Scheduler* sched);
bool cpp_scheduler_power_state_is_serving(const Scheduler* sched);
bool cpp_scheduler_power_state_is_valid_active_power_level(const Scheduler* sched,
                                                           uint8_t power_level);
cpu_mask_t cpp_scheduler_power_state_domain_cpus(const Scheduler* sched);
SchedProcessingRate cpp_scheduler_power_state_target_processing_rate(const Scheduler* sched);
SchedProcessingRate cpp_scheduler_power_state_preceding_target_processing_rate(
    const Scheduler* sched);
bool cpp_scheduler_power_state_has_preceding_target_power_level(const Scheduler* sched);
const power_management::PowerLevel* cpp_scheduler_power_state_find_active_power_level_for_rate(
    const Scheduler* sched, SchedProcessingRate rate);
uint8_t cpp_power_level_get_level(const power_management::PowerLevel* power_level);
bool cpp_scheduler_power_state_request_transition(
    Scheduler* sched, cpu_num_t cpu, uint8_t power_level,
    power_management::PowerLevelUpdateRequest* out_req);
cpu_mask_t cpp_scheduler_power_state_post_update_request(
    Scheduler* sched, const power_management::PowerLevelUpdateRequest* req);
zx_status_t cpp_scheduler_power_state_update_active_power_level(
    Scheduler* sched, uint8_t power_level, SchedProcessingRate* out_active_rate);
SchedulerQueueState* cpp_thread_get_queue_state(Thread* thread);
SchedUtilization cpp_scheduler_power_level_control_update_normalized_utilization(
    Scheduler* sched, SchedUtilization delta);
uint32_t cpp_scheduler_power_level_control_domain_id(const Scheduler* sched);
SchedUtilization cpp_scheduler_power_level_control_total_normalized_utilization(
    const Scheduler* sched);
const CpuSearchSet::Entry* cpp_get_cpu_search_set_entries(cpu_num_t cpu);
size_t cpp_get_cpu_search_set_count(cpu_num_t cpu);
size_t cpp_scheduler_get_power_domain_set_count(const Scheduler* sched);
uint64_t cpp_scheduler_lookup_power_cost(const Scheduler* sched, cpu_num_t cpu,
                                         SchedProcessingRate rate);
SchedUtilization cpp_scheduler_lookup_total_normalized_utilization(const Scheduler* sched,
                                                                   cpu_num_t cpu);
void cpp_timer_queue_preempt_reset(cpu_num_t cpu, zx_instant_mono_t deadline);
bool cpp_timer_queue_preempt_armed(cpu_num_t cpu);
void cpp_thread_call_context_switch_fn_locked(Thread* thread);
VmAspace* cpp_thread_get_aspace(Thread* thread);
void cpp_vmm_context_switch(VmAspace* old_aspace, VmAspace* new_aspace);
void cpp_arch_context_switch(Thread* current_thread, Thread* next_thread);
void cpp_thread_signal_check_restartable_sequence_if_needed(Thread* thread);

WaitQueueBase* cpp_thread_get_blocking_wait_queue(const Thread* thread);
void cpp_wait_queue_update_blocked_thread_effective_profile(WaitQueueBase* wq, Thread* thread);
void cpp_thread_call_migrate_fn_locked(Thread* thread, int32_t stage);
void cpp_thread_call_migrate_fn_for_cpu(cpu_num_t cpu);
const ChainLock* cpp_wait_queue_get_lock(const WaitQueueBase* wq);
SchedulerState::WaitQueueInheritedSchedulerState* cpp_owq_get_inherited_state(OwnedWaitQueue* owq);
const SchedulerState::WaitQueueInheritedSchedulerState* cpp_owq_get_inherited_state_const(
    const OwnedWaitQueue* owq);
SchedUtilization cpp_scheduler_get_normalized_utilization(const Scheduler* sched);
void cpp_thread_preempts_pending_add(Thread* thread, cpu_mask_t mask);
cpu_mask_t cpp_thread_preempts_pending(const Thread* thread);
void cpp_thread_preempts_pending_clear(Thread* thread);
bool cpp_thread_preempt_evaluate_timeslice_extension(Thread* thread);
uint32_t cpp_thread_preempt_eager_resched_disable_count(const Thread* thread);
void cpp_thread_set_ready(Thread* thread);
Thread* cpp_unblock_list_pop_back(Thread::UnblockList* list);

void rust_scheduler_unblock(Thread* thread) TA_REL(thread->get_lock());
void rust_scheduler_unblock_list(Thread::UnblockList* list);
void rust_scheduler_unblock_idle(Thread* thread);
void rust_scheduler_yield(Thread* current_thread);
void rust_scheduler_preempt(uint8_t preempt_type);
void rust_scheduler_reschedule(Thread* current_thread);
void rust_scheduler_reschedule_internal(Thread* current_thread);
cpu_mask_t rust_scheduler_peek_active_mask();
cpu_mask_t rust_scheduler_peek_idle_mask();
void rust_scheduler_set_idle(const Scheduler* sched, bool is_idle);
void rust_scheduler_set_curr_cpu_active(bool is_active);
bool rust_scheduler_enable_new_wakeup_accounting();
void rust_scheduler_on_power_domain_set_updated(Scheduler* sched, SchedProcessingRate active_rate,
                                                SchedProcessingRate max_rate, bool has_domain);
zx_status_t rust_scheduler_update_active_power_level(Scheduler* sched, uint8_t power_level);
void rust_scheduler_initialize_processing_rate(Scheduler* sched, SchedProcessingRate scale);
bool rust_scheduler_request_power_level_for_testing(Scheduler* sched, uint8_t power_level);
void rust_scheduler_update_processing_rates(zx_cpu_performance_info_t* info, size_t count);
void rust_scheduler_get_performance_scales(zx_cpu_performance_info_t* info, size_t count);
void rust_scheduler_get_default_performance_scales(zx_cpu_performance_info_t* info, size_t count);
void rust_scheduler_update_processing_limits(const zx_cpu_perf_limit_t* limits, size_t count);
void rust_scheduler_get_processing_limits(zx_cpu_perf_limit_t* limits, size_t count);

void rust_kernel_scheduler_initialize_thread(Thread* thread,
                                             const SchedulerState::BaseProfile* profile);
void rust_kernel_scheduler_initialize_first_thread(Thread* thread);
void rust_kernel_scheduler_remove_first_thread(Thread* thread);
cpu_mask_t rust_kernel_scheduler_set_cpu_affinity(Thread* thread, cpu_mask_t affinity,
                                                  int32_t affinity_type);
void rust_kernel_scheduler_migrate_unpinned_threads(Scheduler* sched);

void rust_kernel_scheduler_pi_thread_base_profile_changed(Thread* thread);
void rust_kernel_scheduler_pi_upstream_thread_base_profile_changed_thread(const Thread* upstream,
                                                                          Thread* target);
void rust_kernel_scheduler_pi_upstream_thread_base_profile_changed_owq(const Thread* upstream,
                                                                       OwnedWaitQueue* target);
void rust_kernel_scheduler_pi_join_thread_thread(const Thread* upstream, Thread* target,
                                                 ForceInheritance force_inheritance);
void rust_kernel_scheduler_pi_join_thread_owq(const Thread* upstream, OwnedWaitQueue* target,
                                              ForceInheritance force_inheritance);
void rust_kernel_scheduler_pi_join_owq_thread(const OwnedWaitQueue* upstream, Thread* target,
                                              ForceInheritance force_inheritance);
void rust_kernel_scheduler_pi_join_owq_owq(const OwnedWaitQueue* upstream, OwnedWaitQueue* target,
                                           ForceInheritance force_inheritance);
void rust_kernel_scheduler_pi_split_thread_thread(Thread* upstream, Thread* target);
void rust_kernel_scheduler_pi_split_thread_owq(
    Thread* upstream, OwnedWaitQueue* target,
    const SchedulerState::EffectiveProfile* old_target_ep);
void rust_kernel_scheduler_pi_split_owq_thread(OwnedWaitQueue* upstream, Thread* target);
void rust_kernel_scheduler_pi_split_owq_owq(OwnedWaitQueue* upstream, OwnedWaitQueue* target,
                                            const SchedulerState::EffectiveProfile* old_target_ep);
void rust_kernel_scheduler_state_recompute_effective_profile(SchedulerState* state);
int32_t rust_kernel_effective_profile_get_weight_or_packed_deadline_params(
    SchedulerState::EffectiveProfile* ep);
void rust_kernel_scheduler_get_projected_cpu_stats(cpu_num_t cpu, cpu_stats* stats);

void cpp_unblock_list_init(ffi::Uninitialized<Thread::UnblockList>* holder);
void cpp_unblock_list_destroy(Thread::UnblockList* holder);
void cpp_unblock_list_push_back(Thread::UnblockList* holder, Thread* thread);
Thread* cpp_unblock_list_pop_front(Thread::UnblockList* holder);
bool cpp_unblock_list_is_empty(const Thread::UnblockList* holder);

bool cpp_scheduler_save_state_list_is_empty(Scheduler* sched);
void cpp_scheduler_save_state_list_move(Scheduler* sched,
                                        ffi::Uninitialized<Thread::SaveStateList>* holder);
bool cpp_scheduler_take_save_state_list(Scheduler* sched,
                                        ffi::Uninitialized<Thread::SaveStateList>* holder);
Thread* cpp_save_state_list_pop(Thread::SaveStateList* holder);
void cpp_save_state_list_destroy(Thread::SaveStateList* holder);
}
// Implements fair and deadline scheduling algorithms and manages the associated
// per-CPU state.
class Scheduler {
 public:
  class RunQueue;
  using Affine = sched::Affine<10>;

  // Returns true if new scheduler wakeup accounting is enabled either by boot
  // option or gn arg.
  static bool EnableNewWakeupAccounting() { return rust_scheduler_enable_new_wakeup_accounting(); }

  // Minimum fair time slice. Set to avoid excessive reschedule overhead.
  // TODO(eieio): This needs to be revised and/or scaled for lower OPPs.
  static constexpr SchedDuration kMinimumFairCapacity = SchedMs(1);

  // The default period for fair tasks when period expansion is not in effect.
  static constexpr SchedDuration kDefaultFairPeriod = SchedDefaultFairPeriod;

  // The minimum fair utilization before period expansion is needed.
  static constexpr SchedUtilization kMinFairUtilization = kMinimumFairCapacity / kDefaultFairPeriod;

  // Reciprocal of the default fair period to minimize runtime divisions in fair
  // utilization calculations.
  static constexpr SchedUtilization kReciprocalDefaultFairPeriod = 1 / kDefaultFairPeriod;

  // Ensure that the min fair utilization is in the range (0, 1].
  static_assert(0 < kMinimumFairCapacity && kMinimumFairCapacity <= kDefaultFairPeriod);

  // The threshold for cross-cluster work stealing. Queues with an estimated
  // runtime below this value are not stolen from if the target and destination
  // CPUs are in different logical clusters. In a performance-balanced system,
  // this tunable value approximates the cost of cross-cluster migration due to
  // cache misses, assuming a task has high cache affinity in its current
  // cluster. This tunable may be increased to limit cross-cluster spill over.
  static constexpr SchedDuration kInterClusterThreshold = SchedMs(2);

  // The threshold for early termination when searching for a CPU to place a
  // task. Queues with an estimated runtime below this value are sufficiently
  // unloaded. In a performance-balanced system, this tunable value approximates
  // the cost of intra-cluster migration due to cache misses, assuming a task
  // has high cache affinity with the last CPU it ran on. This tunable may be
  // increased to limit intra-cluster spill over.
  static constexpr SchedDuration kIntraClusterThreshold = SchedUs(25);

  // The per-CPU deadline utilization limit to attempt to honor when selecting a
  // CPU to place a task. It is up to userspace to ensure that the total set of
  // deadline tasks can honor this limit. Even if userspace ensures the total
  // set of deadline utilizations is within the total available processor
  // resources, when total utilization is high enough it may not be possible to
  // honor this limit due to the bin packing problem.
  static constexpr SchedUtilization kCpuUtilizationLimit{1};

  // The maximum deadline utilization permitted for a single thread. This limit
  // is applied when scaling the utilization of a deadline task to the relative
  // performance of a candidate target processor -- placing a task on a
  // processor that would cause the scaled thread utilization to exceed this
  // value is avoided if possible.
  static constexpr SchedUtilization kThreadUtilizationMax{1};

  Scheduler() = default;
  ~Scheduler() = default;

  Scheduler(const Scheduler&) = delete;
  Scheduler& operator=(const Scheduler&) = delete;

  // Accessors for total weight and number of runnable tasks.
  SchedWeight GetTotalWeight() const TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    return rust_state_.weight_total;
  }
  size_t GetRunnableTasks() const TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    return static_cast<size_t>(runnable_task_count());
  }

  // Dumps the state of the run queue to the specified output target.  Note that
  // interrupts must be disabled in order to call this method.
  void Dump(FILE* output_target = stdout, bool queue_state_only = false) TA_EXCL(queue_lock_) {
    rust_kernel_scheduler_dump(this, output_target, queue_state_only);
  }
  void DumpActiveThread(FILE* output_target = stdout) TA_EXCL(queue_lock_) {
    rust_kernel_scheduler_dump_active_thread(this, output_target);
  }

  // Returns the number of the CPU this scheduler instance is associated with.
  cpu_num_t this_cpu() const TA_NO_THREAD_SAFETY_ANALYSIS { return rust_state_.this_cpu; }

  // Returns the index of the logical cluster of the CPU this scheduler instance
  // is associated with.
  size_t cluster() const TA_NO_THREAD_SAFETY_ANALYSIS { return rust_state_.cluster; }

  // Returns the lock-free value of the estimated queue time for the CPU this
  // scheduler is associated with.
  SchedDuration exported_queue_time_ns() const TA_NO_THREAD_SAFETY_ANALYSIS {
    return rust_state_.exported_queue_time_ns.load();
  }
  SchedDuration fair_period() const TA_REQ(queue_lock_) { return rust_state_.fair_period; }
  SchedUtilization reciprocal_fair_period() const TA_REQ(queue_lock_) {
    return rust_state_.reciprocal_fair_period;
  }
  SchedWeight weight_total() const TA_REQ(queue_lock_) { return rust_state_.weight_total; }
  const RunQueue* critical_deadline_run_queue() const TA_REQ(queue_lock_) {
    return &rust_state_.critical_deadline_run_queue;
  }
  const RunQueue* deadline_run_queue() const TA_REQ(queue_lock_) {
    return &rust_state_.deadline_run_queue;
  }
  const RunQueue* fair_run_queue() const TA_REQ(queue_lock_) { return &rust_state_.fair_run_queue; }
  SchedUtilization normalized_utilization() const TA_REQ(queue_lock_) {
    return power_state_.normalized_utilization();
  }

  // Returns the lock-free value of the estimated deadline utilization for the
  // CPU this scheduler is associated with.
  SchedUtilization exported_deadline_utilization() const TA_NO_THREAD_SAFETY_ANALYSIS {
    return rust_state_.exported_deadline_utilization.load();
  }

  // Returns the lock-free value of the estimated deadline utilization for the
  // CPU, clamped to the current rate limits, for the CPU this scheduler is
  // associated with.
  SchedUtilization exported_clamped_deadline_utilization() const TA_NO_THREAD_SAFETY_ANALYSIS {
    return rust_state_.exported_clamped_deadline_utilization.load();
  }

  // Returns the lock-free value of the estimated fair utilization for the
  // CPU this scheduler is associated with.
  SchedUtilization exported_fair_utilization() const TA_NO_THREAD_SAFETY_ANALYSIS {
    return rust_state_.exported_fair_utilization.load();
  }

  // Returns the lock-free value of the estimated fair utilization for the
  // CPU, clamped to the current rate limits, for the CPU this scheduler is
  // associated with.
  SchedUtilization exported_clamped_fair_utilization() const TA_NO_THREAD_SAFETY_ANALYSIS {
    return rust_state_.exported_clamped_fair_utilization.load();
  }

  // Returns the lock-free value of the total estimated utilization (deadline + fair)
  // for the CPU, clamped to the current rate limits, for the CPU this scheduler
  // is associated with.
  SchedUtilization exported_clamped_total_utilization() const TA_NO_THREAD_SAFETY_ANALYSIS {
    return rust_state_.exported_clamped_total_utilization.load();
  }

  // Returns the lock-free value of the processing rate for the CPU this
  // scheduler is associated with.
  SchedProcessingRate exported_processing_rate() const TA_NO_THREAD_SAFETY_ANALYSIS {
    return rust_state_.exported_processing_rate.load();
  }

  // Returns the lock-free value of the max processing rate for the CPU this
  // scheduler is associated with.
  SchedProcessingRate exported_max_processing_rate() const TA_NO_THREAD_SAFETY_ANALYSIS {
    return rust_state_.exported_max_processing_rate.load();
  }

  // Returns the processing rate of the CPU this scheduler instance is
  // associated with.
  SchedProcessingRate processing_rate() const TA_REQ(queue_lock_) {
    return rust_state_.power_level_control.processing_rate;
  }

  // Returns the reciprocal processing rate of the CPU this scheduler instance
  // is associated with.
  SchedProcessingRate processing_rate_reciprocal() const TA_REQ(queue_lock_) {
    return rust_state_.power_level_control.processing_rate_reciprocal;
  }
  bool cpu_deactivating() const TA_NO_THREAD_SAFETY_ANALYSIS {
    return rust_state_.cpu_deactivating.load(ktl::memory_order_relaxed);
  }
  void set_cpu_deactivating(bool deactivating) TA_NO_THREAD_SAFETY_ANALYSIS {
    rust_state_.cpu_deactivating.store(deactivating, ktl::memory_order_release);
  }
  Thread* previous_thread() const TA_NO_THREAD_SAFETY_ANALYSIS {
    return rust_state_.previous_thread.load(ktl::memory_order_relaxed);
  }
  void set_previous_thread(Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
    rust_state_.previous_thread.store(thread, ktl::memory_order_relaxed);
  }
  int32_t runnable_task_count() const TA_REQ(queue_lock_) {
    return rust_state_.runnable_task_count;
  }

  // Returns a pointer to the currently running thread, if any.
  //
  // TODO(johngro); This is not particularly safe.  As soon as we returned this
  // thread's pointer, it could (in theory) exit.  We probably need to require
  // that the scheduler's queue lock be held in order to peek at the active
  // thread member.
  Thread* active_thread() const TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    return rust_state_.active_thread;
  }

  // Public entry points.

  // See the comment in Thread::CreateEtc for why InitializeThread does not take
  // a stance on whether or not we should be in an active chain-lock
  // transaction. On the surface, demanding that we hold a threads lock without
  // also demanding that we also be involved in an active chain-lock transaction
  // is an apparent contradiction of some fundamental invariants (how can a
  // chain lock be held without being in a transaction?), but when used
  // exclusively from Thread::CreateEtc, it should be OK.
  static void InitializeThread(Thread* thread, const SchedulerState::BaseProfile& profile)
      TA_REQ(thread->get_lock()) {
    rust_kernel_scheduler_initialize_thread(thread, &profile);
  }
  static void InitializeFirstThread(Thread* thread)
      TA_REQ(chainlock_transaction_token, thread->get_lock()) {
    rust_kernel_scheduler_initialize_first_thread(thread);
  }
  static void RemoveFirstThread(Thread* thread)
      TA_REQ(chainlock_transaction_token, thread->get_lock()) {
    rust_kernel_scheduler_remove_first_thread(thread);
  }
  static inline void Block(Thread* current_thread)
      TA_REQ(chainlock_transaction_token, current_thread->get_lock()) {
    rust_kernel_scheduler_block(current_thread);
  }
  static void Yield(Thread* current_thread)
      TA_REQ(chainlock_transaction_token, current_thread->get_lock()) {
    rust_scheduler_yield(current_thread);
  }

  // Indicates the type of preemption for diagnostic/tracing purposes.
  enum class PreemptType : uint8_t {
    Irq,
    FlushPending,
    Reschedule,
  };
  // No chainlocks should be held when calling preempt.  The thread's lock will
  // be obtained unconditionally in the process.
  //
  // If holding a spinlock, the preemption will be deferred via self-IPI until
  // all spinlock are released and interrupts are re-enabled.
  static void Preempt(PreemptType preempt_type) TA_EXCL(chainlock_transaction_token) {
    rust_scheduler_preempt(static_cast<uint8_t>(preempt_type));
  }

  static void Reschedule(Thread* current_thread)
      TA_REQ(chainlock_transaction_token, current_thread->get_lock()) {
    rust_scheduler_reschedule(current_thread);
  }
  static void RescheduleInternal(Thread* current_thread)
      TA_REQ(chainlock_transaction_token, current_thread->get_lock()) {
    rust_scheduler_reschedule_internal(current_thread);
  }

  // Finish unblocking a thread.  This function expects the thread to be locked
  // when called.  It will:
  //
  // 1) Select a scheduler compatible with the thread.
  // 2) Place the thread into that scheduler's run queue.
  // 3) Drop the thread's lock.
  // 4) Reschedule if needed/permitted.
  static void Unblock(Thread* thread) TA_REQ(chainlock_transaction_token)
      TA_REL(thread->get_lock()) {
    rust_scheduler_unblock(thread);
  }

  // Unblock list expects to receive a list of threads, all of whose
  // locks are currently held.  It will drop each thread's lock after
  // successfully assigning it to a scheduler.
  static void Unblock(Thread::UnblockList thread_list) TA_REQ(chainlock_transaction_token) {
    rust_scheduler_unblock_list(&thread_list);
  }

  // UnblockIdle is used in the process of creation of the idle thread.
  static void UnblockIdle(Thread* idle_thread)
      TA_REQ(chainlock_transaction_token, idle_thread->get_lock()) {
    rust_scheduler_unblock_idle(idle_thread);
  }

  // Migrates any threads that can run on other CPUs off of the current CPU.
  static void MigrateUnpinnedThreads() {
    rust_kernel_scheduler_migrate_unpinned_threads(Get(arch_curr_cpu_num()));
  }

  // TimerTick is called when the preemption timer for a CPU has fired.
  //
  // This function is logically private and should only be called by timer.cc.
  static inline void TimerTick(SchedTime now) { rust_kernel_scheduler_timer_tick(now); }

  // Releases the lock held by the previous and current threads after a context
  // switch. This must be called by trampoline routines at some point before
  // jumping to the current thread's entry point.
  static inline void TrampolineLockHandoff() { rust_kernel_scheduler_lock_handoff(); }

  // Called when the base profile of |thread| has been changed by a program. The
  // thread must be either running or runnable when this method is called. If
  // the thread had been blocked instead, the change of the base profile would
  // have been managed at the WaitQueue level, perhaps eventually propagating to
  // the scheduler via UpstreamThreadBaseProfileChanged.
  static void ThreadBaseProfileChanged(Thread& thread)
      TA_REQ(chainlock_transaction_token, ChainLockable::GetLock(thread), preempt_disabled_token);

  // Called when the base profile of a thread (|upstream|) which exists upstream
  // of |target| has changed its base profile.  It is possible for the rules
  // regarding scheduling penalties base profile changes to be slightly
  // different for when a thread's effective profile changes as a result of its
  // own base profile changing, vs the base profile of a thread which exists
  // upstream of it in a PI graph.  Because of this, the two scenarios are
  // handled separately using two different methods.
  template <typename TargetType>
  static void UpstreamThreadBaseProfileChanged(const Thread& upstream, TargetType& target)
      TA_REQ(chainlock_transaction_token, ChainLockable::GetLock(upstream),
             ChainLockable::GetLock(target), preempt_disabled_token);

  // Called when a thread (|upstream|) blocks in an OwnedWaitQueue, and the
  // target of that OWQ (|target|) is running or runnable.  At the point that
  // this method is called, propagation of the static profile pressure
  // parameters (weight, deadline utilization, and minimum deadline) should have
  // already been propagated to |target|, and its EffectiveProfile should be
  // dirty.  This method is called in order to update the dynamic scheduling
  // parameters (start time, finish time, time slice remaining) of |target| as
  // needed because of the join operation.
  template <typename UpstreamType, typename TargetType>
  static void JoinNodeToPiGraph(const UpstreamType& upstream, TargetType& target,
                                ForceInheritance force_inheritance)
      TA_REQ(chainlock_transaction_token, ChainLockable::GetLock(upstream),
             ChainLockable::GetLock(target), preempt_disabled_token);

  // Called when a thread (|upstream|) unblocks from an OwnedWaitQueue and
  // leaves the PI graph whose running or runnable target is |target|.
  //
  // At the point that this method is called, propagation of the static profile
  // pressure parameters (weight, deadline utilization, and minimum deadline)
  // should have already been propagated to |target|, and its EffectiveProfile
  // should be dirty.  This method is called in order to update the dynamic
  // scheduling parameters (start time, finish time, time slice remaining) in
  // both |target| and |upstream| as needed because of the split operation.
  template <typename UpstreamType, typename TargetType>
  static void SplitNodeFromPiGraph(UpstreamType& upstream, TargetType& target,
                                   const SchedulerState::EffectiveProfile* old_target_ep = nullptr)
      TA_REQ(chainlock_transaction_token, ChainLockable::GetLock(upstream),
             ChainLockable::GetLock(target), preempt_disabled_token);

  // Updates the performance scales of the requested CPUs and returns the
  // effective values in place, which may be different than the requested values
  // if they are below the minimum safe values for the respective CPUs.
  //
  // Requires info.size() <= num CPUs.
  static void UpdateProcessingRates(ktl::span<zx_cpu_performance_info_t> info)
      TA_EXCL(queue_lock_) {
    rust_scheduler_update_processing_rates(info.data(), info.size());
  }

  // Gets the performance scales of up to info.size() CPUs. Returns the last
  // values requested by userspace, even if they have not yet taken effect.
  //
  // Requires info.size() <= num CPUs.
  static void GetPerformanceScales(ktl::span<zx_cpu_performance_info_t> info) TA_EXCL(queue_lock_) {
    rust_scheduler_get_performance_scales(info.data(), info.size());
  }

  // Gets the default performance scales of up to info.size() CPUs. Returns the
  // initial values determined by the system topology, or 1.0 when no topology
  // is available.
  //
  // Requires info.size() <= num CPUs.
  static void GetDefaultPerformanceScales(ktl::span<zx_cpu_performance_info_t> info)
      TA_EXCL(queue_lock_) {
    rust_scheduler_get_default_performance_scales(info.data(), info.size());
  }

  // Updates the performance limits of the requested CPUs.
  //
  // Requires limits.size() <= num CPUs.
  static void UpdateProcessingLimits(ktl::span<zx_cpu_perf_limit_t> limits) TA_EXCL(queue_lock_) {
    rust_scheduler_update_processing_limits(limits.data(), limits.size());
  }

  // Gets the processing limits of up to limits.size() CPUs.
  //
  // Requires limits.size() <= num CPUs.
  static void GetProcessingLimits(ktl::span<zx_cpu_perf_limit_t> limits) TA_EXCL(queue_lock_) {
    rust_scheduler_get_processing_limits(limits.data(), limits.size());
  }

  // Returns the CPU stats for the given CPU number, forward projected to
  // account for idle or normalized busy time elapsed since the last relevant
  // state change.
  static cpu_stats GetProjectedCpuStats(cpu_num_t cpu_num) TA_EXCL(queue_lock_);

  // Get the mask of valid CPUs that thread may run on. If a new mask
  // is set, the thread will be migrated to satisfy the new constraint.
  static cpu_mask_t SetCpuAffinity(Thread& thread, cpu_mask_t affinity, AffinityType affinity_type)
      TA_EXCL(chainlock_transaction_token, thread.get_lock()) {
    return rust_kernel_scheduler_set_cpu_affinity(&thread, affinity,
                                                  static_cast<int32_t>(affinity_type));
  }

  // Run the passed callable while holding the current CPU's scheduler's lock.
  // Used by routines in thread.cc who need to change the current thread's
  // state, but need to be holding the current scheduler's lock in order to do
  // so.
  template <typename Callable>
    requires requires(Scheduler& scheduler, Callable callable) { callable(scheduler); }
  static auto RunInLockedCurrentScheduler(Callable callable) {
    DEBUG_ASSERT(arch_ints_disabled());
    Scheduler& scheduler = *Get();
    Guard<MonitoredSpinLock, NoIrqSave> queue_guard{&scheduler.queue_lock_, SOURCE_TAG};
    return callable(scheduler);
  }

  // Run the passed callable while holding the specified thread's scheduler's
  // lock, if any. Used by SetMigrateFn in thread.cc.
  template <typename Callable>
  static auto RunInThreadsSchedulerLocked(Thread* thread, Callable callable)
      TA_REQ(thread->get_lock()) {
    const cpu_num_t thread_cpu = thread->scheduler_state().curr_cpu();
    if (thread_cpu != INVALID_CPU) {
      Scheduler& scheduler = *Get(thread_cpu);
      Guard<MonitoredSpinLock, NoIrqSave> queue_guard{&scheduler.queue_lock_, SOURCE_TAG};
      return callable();
    }
    return callable();
  }

  // Set/clears the current cpu's bit in the global mp_active_mask.  Holds the
  // scheduler's queue lock while the bit is changed, enforcing the invariant
  // that a scheduler's active bit can only ever change while holding that
  // scheduler's lock.
  static void SetCurrCpuActive(bool is_active) { rust_scheduler_set_curr_cpu_active(is_active); }

  // Peek at the current active mask for all schedulers.
  //
  // Note that this is just a lockless atomic load.  Baring special
  // circumstances, the set of active schedulers can change at any time.
  static cpu_mask_t PeekActiveMask() { return rust_scheduler_peek_active_mask(); }

  // Peek at the current active mask state for a given CPU's scheduler.
  //
  // Note that this is just a lockless atomic load.  Baring special
  // circumstances, the set of active schedulers can change at any time.
  static bool PeekIsActive(cpu_num_t cpu) {
    const cpu_mask_t mask = cpu_num_to_mask(cpu);
    return (PeekActiveMask() & mask) != 0;
  }

  // Accessors for the "idle" state mask; similar to the active state mask.
  void SetIdle(bool is_idle) const { rust_scheduler_set_idle(this, is_idle); }
  static cpu_mask_t PeekIdleMask() { return rust_scheduler_peek_idle_mask(); }
  static bool PeekIsIdle(cpu_num_t cpu) { return (PeekIdleMask() & cpu_num_to_mask(cpu)) != 0; }

  // Accumulates processor idle time.
  void AddProcessorIdleTime(SchedDuration idle_time) TA_NO_THREAD_SAFETY_ANALYSIS {
    rust_state_.processor_idle_time_ns.fetch_add(idle_time.raw_value(), ktl::memory_order_relaxed);
  }

  // Clears any accumulated processor idle time.
  void ResetProcessorIdleTime() TA_NO_THREAD_SAFETY_ANALYSIS {
    rust_state_.processor_idle_time_ns.store(0, ktl::memory_order_relaxed);
  }

  // Returns the accumulated processor idle time for assertions/diagnostics.
  SchedDuration processor_idle_time() const TA_NO_THREAD_SAFETY_ANALYSIS {
    return SchedDuration{rust_state_.processor_idle_time_ns.load(ktl::memory_order_relaxed)};
  }

  using PowerDomain = power_management::PowerDomain;
  using PowerDomainSet = power_management::PowerDomainSet;

  // Sets the power domain set for this scheduler instance, returning the previous set.
  PowerDomainSet ExchangePowerDomainSet(PowerDomainSet domain_set) TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    PowerDomainSet previous = power_state_.UpdatePowerDomainSet(ktl::move(domain_set), this_cpu());
    const SchedProcessingRate active_rate = power_state_.active_processing_rate();
    const bool has_domain = static_cast<bool>(power_state_.domain());
    const SchedProcessingRate max_rate =
        has_domain ? power_state_.domain()->model().max_processing_rate() : SchedProcessingRate{0};
    rust_scheduler_on_power_domain_set_updated(this, active_rate, max_rate, has_domain);
    return previous;
  }

  // Sets the power domain set for all CPUs.
  static void SetPowerDomainSet(PowerDomainSet power_domain_set);

  // Updates the current power level with the value reported by the power level
  // controller.
  static zx::result<cpu_mask_t> UpdatePowerLevel(uint32_t domain_id, uint64_t controller_id,
                                                 power_management::ControlInterface interface,
                                                 uint64_t arg);

  // Updates the current power level for this CPU with the value reported by the power level
  // controller. Called by kernel tests and sys_system_set_processor_power_state.
  zx::result<> UpdateActivePowerLevel(uint8_t power_level) TA_EXCL(queue_lock_) {
    return zx::make_result(rust_scheduler_update_active_power_level(this, power_level));
  }

  // Returns the current active power level, if any.
  ktl::optional<uint8_t> GetActivePowerLevel() const TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    return power_state_.active_power_level();
  }

  // Sends a power level request through this scheduler's power level controller for testing. This
  // method is called from thread and IRQ context to ensure that the underlying machinery can be
  // safely called within Block, Unblock, and other scheduling operations.
  //
  // Returns true if the request was sent, false otherwise.
  bool RequestPowerLevelForTesting(uint8_t power_level) TA_EXCL(queue_lock_) {
    return rust_scheduler_request_power_level_for_testing(this, power_level);
  }

  // Returns the current power domain set for this scheduler instance.
  PowerDomainSet GetPowerDomainSetForTesting() TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    return power_state_.power_domain_set();
  }

  // Returns the current power domain for this scheduler instance.
  fbl::RefPtr<PowerDomain> GetPowerDomainForTesting() TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    return power_state_.domain();
  }

  // Returns the current active power coefficient.
  ktl::optional<uint64_t> GetActivePowerCoefficientNwForTesting() const TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    return rust_state_.power_level_control.active_power_coefficient_nw;
  }

  // Returns the max idle power coefficient.
  ktl::optional<uint64_t> GetMaxIdlePowerCoefficientNwForTesting() const TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    return power_state_.max_idle_power_coefficient_nw();
  }

  // Const accessor for static analysis.
  const auto& queue_lock_cap() const TA_RET_CAP(queue_lock_) { return queue_lock_; }

 private:
  friend struct Thread;
  friend void* ::cpp_thread_get_queue_node(Thread* t);

  // Allow percpu to init our cpu number and performance scale.
  friend struct percpu;

  // EffectiveProfile and BaseProfile are both inner classes of SchedulerState which
  // are also frequently used by the methods in Scheduler.  Make a couple of handy
  // private aliases to keep us from needing to say
  // SchedulerState::(Effective|Base)Profile in all of the Scheduler methods.
  using EffectiveProfile = SchedulerState::EffectiveProfile;
  using BaseProfile = SchedulerState::BaseProfile;

  using Disposition = SchedulerQueueState::Disposition;

  // Sets the initial values of the CPU performance scales for this Scheduler
  // instance.
  void InitializeProcessingRate(SchedProcessingRate scale) TA_NO_THREAD_SAFETY_ANALYSIS {
    DEBUG_ASSERT(!power_state_.domain());
    rust_scheduler_initialize_processing_rate(this, scale);
  }

  static void AssertLayout() TA_NO_THREAD_SAFETY_ANALYSIS {
    static_assert(kMinimumFairCapacity.raw_value() == 1'000'000);
    static_assert(kDefaultFairPeriod.raw_value() == 10'000'000);
    static_assert(kMinFairUtilization.raw_value() == 214'748'364);
    static_assert(kReciprocalDefaultFairPeriod.raw_value() == 214);
    static_assert(kInterClusterThreshold.raw_value() == 2'000'000);
    static_assert(kIntraClusterThreshold.raw_value() == 25'000);
    static_assert(kCpuUtilizationLimit.raw_value() == (1LL << 31));
    static_assert(kThreadUtilizationMax.raw_value() == (1LL << 31));
    static_assert(sizeof(Affine) == 24);
    static_assert(alignof(Affine) == 8);
    static_assert(ktl::is_standard_layout_v<RustSchedulerState>);
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Winvalid-offsetof"
    static_assert(offsetof(Scheduler, rust_state_) == 0);
#pragma clang diagnostic pop
    static_assert(sizeof(PowerLevelControlState) == 104);
    static_assert(alignof(PowerLevelControlState) == 8);
    static_assert(offsetof(PowerLevelControlState, processing_rate) == 0);
    static_assert(offsetof(PowerLevelControlState, processing_rate_reciprocal) == 8);
    static_assert(offsetof(PowerLevelControlState, updated_processing_rate) == 16);
    static_assert(offsetof(PowerLevelControlState, default_processing_rate) == 24);
    static_assert(offsetof(PowerLevelControlState, max_processing_rate) == 32);
    static_assert(offsetof(PowerLevelControlState, processing_rate_limit_min) == 40);
    static_assert(offsetof(PowerLevelControlState, processing_rate_limit_max) == 48);
    static_assert(offsetof(PowerLevelControlState, active_power_coefficient_nw) == 56);
    static_assert(offsetof(PowerLevelControlState, pending_update_request) == 64);
    static_assert(offsetof(PowerLevelControlState, has_pending_update_request) == 96);

    // The overall size of Scheduler depends on build configuration (e.g. lockdep
    // changes sizeof(MonitoredSpinLock)); the Rust side derives it from the
    // bindgen-generated percpu bindings instead of a fixed constant.
    static_assert(sizeof(RustSchedulerState) == 624);
    static_assert(alignof(RustSchedulerState) == 8);
    static_assert(offsetof(RustSchedulerState, fair_run_queue) == 0);
    static_assert(offsetof(RustSchedulerState, deadline_run_queue) == 32);
    static_assert(offsetof(RustSchedulerState, critical_deadline_run_queue) == 64);
    static_assert(offsetof(RustSchedulerState, runnable_task_count) == 96);
    static_assert(offsetof(RustSchedulerState, weight_total) == 104);
    static_assert(offsetof(RustSchedulerState, scheduled_weight_total) == 112);
    static_assert(offsetof(RustSchedulerState, active_thread) == 120);
    static_assert(offsetof(RustSchedulerState, fair_period) == 128);
    static_assert(offsetof(RustSchedulerState, reciprocal_fair_period) == 136);
    static_assert(offsetof(RustSchedulerState, last_update_time_ns) == 144);
    static_assert(offsetof(RustSchedulerState, start_of_current_time_slice_ns) == 152);
    static_assert(offsetof(RustSchedulerState, target_preemption_time_ns) == 160);
    static_assert(offsetof(RustSchedulerState, total_expected_runtime_ns) == 168);
    static_assert(offsetof(RustSchedulerState, this_cpu) == 176);
    static_assert(offsetof(RustSchedulerState, cluster) == 184);
    static_assert(offsetof(RustSchedulerState, cpu_deactivating) == 192);
    static_assert(offsetof(RustSchedulerState, exported_queue_time_ns) == 200);
    static_assert(offsetof(RustSchedulerState, exported_deadline_utilization) == 208);
    static_assert(offsetof(RustSchedulerState, exported_clamped_deadline_utilization) == 216);
    static_assert(offsetof(RustSchedulerState, exported_fair_utilization) == 224);
    static_assert(offsetof(RustSchedulerState, exported_clamped_fair_utilization) == 232);
    static_assert(offsetof(RustSchedulerState, exported_clamped_total_utilization) == 240);
    static_assert(offsetof(RustSchedulerState, exported_processing_rate) == 248);
    static_assert(offsetof(RustSchedulerState, exported_max_processing_rate) == 256);
    static_assert(offsetof(RustSchedulerState, previous_thread) == 264);
    static_assert(offsetof(RustSchedulerState, critical_deadline_utilization) == 272);
    static_assert(offsetof(RustSchedulerState, active_thread_weight) == 280);
    static_assert(offsetof(RustSchedulerState, fair_affine_transform) == 288);
    static_assert(offsetof(RustSchedulerState, processor_idle_time_ns) == 312);
    static_assert(offsetof(RustSchedulerState, bandwidth_reservation_cache) == 320);
    static_assert(offsetof(RustSchedulerState, power_level_control) == 520);
  }

 public:
  struct SchedMonoTimeAndBootTicks {
    SchedTime mono_time;
    zx_instant_boot_ticks_t boot_ticks;
  };

  static WaitQueueBase* GetBlockingWaitQueue(const Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
    return thread->wait_queue_state().blocking_wait_queue_;
  }

  static void UpdateBlockedThreadEffectiveProfile(WaitQueueBase* wq,
                                                  Thread* thread) TA_NO_THREAD_SAFETY_ANALYSIS {
    wq->UpdateBlockedThreadEffectiveProfile(*thread);
  }

  // Returns the current monotonic time / boot ticks using coherent samples of
  // the respective timelines.
  // TODO(eieio): Look into making this non-static and requiring the queue lock
  // of the respective scheduler to ensure this is always called with the queue
  // lock held.
  static SchedMonoTimeAndBootTicks CurrentMonoTimeAndBootTicks() {
    InstantMonoTimeAndBootTicks mono_time_and_boot_ticks = current_mono_time_and_boot_ticks();
    return {.mono_time = SchedTime{mono_time_and_boot_ticks.mono_time},
            .boot_ticks = mono_time_and_boot_ticks.boot_ticks};
  }

  // Returns the Scheduler instance for the current CPU.
  static Scheduler* Get();

  // Returns the Scheduler instance for the given CPU.
  static Scheduler* Get(cpu_num_t cpu);

 private:
  friend Scheduler* cpp_scheduler_get(cpu_num_t cpu);
  friend uint64_t cpp_scheduler_power_state_active_power_coefficient_nw(const Scheduler* sched);
  friend uint64_t cpp_scheduler_power_level_control_max_idle_power_coefficient_nw(
      const Scheduler* sched);

  friend void cpp_scheduler_lock_queue(Scheduler* sched);
  friend void cpp_scheduler_unlock_queue(Scheduler* sched);
  friend uint64_t cpp_scheduler_lock_queue_irq_save(Scheduler* sched);
  friend void cpp_scheduler_unlock_queue_irq_restore(Scheduler* sched, uint64_t state);
  friend SchedUtilization cpp_scheduler_power_level_control_update_normalized_utilization(
      Scheduler* sched, SchedUtilization delta);
  friend uint32_t cpp_scheduler_power_level_control_domain_id(const Scheduler* sched);
  friend SchedUtilization cpp_scheduler_power_level_control_total_normalized_utilization(
      const Scheduler* sched);
  friend Thread* cpp_scheduler_get_idle_thread(const Scheduler* sched);
  friend bool cpp_scheduler_power_level_control_is_enabled(const Scheduler* sched);
  friend bool cpp_scheduler_power_state_scheduler_control_enabled(const Scheduler* sched);
  friend bool cpp_scheduler_power_state_is_serving(const Scheduler* sched);
  friend bool cpp_scheduler_power_state_is_valid_active_power_level(const Scheduler* sched,
                                                                    uint8_t power_level);
  friend cpu_mask_t cpp_scheduler_power_state_domain_cpus(const Scheduler* sched);
  friend SchedProcessingRate cpp_scheduler_power_state_target_processing_rate(
      const Scheduler* sched);
  friend SchedProcessingRate cpp_scheduler_power_state_preceding_target_processing_rate(
      const Scheduler* sched);
  friend bool cpp_scheduler_power_state_has_preceding_target_power_level(const Scheduler* sched);
  friend const power_management::PowerLevel*
  cpp_scheduler_power_state_find_active_power_level_for_rate(const Scheduler* sched,
                                                             SchedProcessingRate rate);
  friend bool cpp_scheduler_power_state_request_transition(
      Scheduler* sched, cpu_num_t cpu, uint8_t power_level,
      power_management::PowerLevelUpdateRequest* out_req);
  friend cpu_mask_t cpp_scheduler_power_state_post_update_request(
      Scheduler* sched, const power_management::PowerLevelUpdateRequest* req);
  friend zx_status_t cpp_scheduler_power_state_update_active_power_level(
      Scheduler* sched, uint8_t power_level, SchedProcessingRate* out_active_rate);
  friend size_t cpp_scheduler_get_power_domain_set_count(const Scheduler* sched);
  friend uint64_t cpp_scheduler_lookup_power_cost(const Scheduler* sched, cpu_num_t cpu,
                                                  SchedProcessingRate rate);
  friend SchedUtilization cpp_scheduler_lookup_total_normalized_utilization(const Scheduler* sched,
                                                                            cpu_num_t cpu);
  friend void cpp_scheduler_save_state_list_push_front(Scheduler* sched, Thread* thread);
  friend void cpp_scheduler_save_state_list_move(Scheduler* sched,
                                                 ffi::Uninitialized<Thread::SaveStateList>* holder);
  friend bool cpp_scheduler_save_state_list_is_empty(Scheduler* sched);
  friend bool cpp_scheduler_take_save_state_list(Scheduler* sched,
                                                 ffi::Uninitialized<Thread::SaveStateList>* holder);

  // Traits type to adapt the WAVLTree to Thread with node state in the
  // scheduler_state member.
 public:
  class RunQueue {
   public:
    RunQueue() { rust_kernel_run_queue_init(opaque_state_); }
    ~RunQueue() { rust_kernel_run_queue_destroy(opaque_state_); }

    RunQueue(const RunQueue&) = delete;
    RunQueue& operator=(const RunQueue&) = delete;
    RunQueue(RunQueue&&) = delete;
    RunQueue& operator=(RunQueue&&) = delete;

    void insert(Thread* thread) { rust_kernel_run_queue_insert(opaque_state_, thread); }
    Thread* erase(Thread& thread) { return rust_kernel_run_queue_erase(opaque_state_, &thread); }
    bool is_empty() const { return rust_kernel_run_queue_is_empty(opaque_state_); }
    size_t size() const { return rust_kernel_run_queue_size(opaque_state_); }
    Thread& front() {
      Thread* t = rust_kernel_run_queue_front(opaque_state_);
      DEBUG_ASSERT(t != nullptr);
      return *t;
    }
    const Thread& front() const {
      Thread* t = rust_kernel_run_queue_front(opaque_state_);
      DEBUG_ASSERT(t != nullptr);
      return *t;
    }
    const Thread* croot() const { return rust_kernel_run_queue_root(opaque_state_); }

    class iterator {
     public:
      iterator() = default;
      explicit iterator(RunQueueIterator iter) : iter_(iter) {}

      bool IsValid() const { return rust_kernel_run_queue_iterator_is_valid(&iter_); }

      Thread& operator*() const {
        Thread* t = rust_kernel_run_queue_iterator_get(&iter_);
        DEBUG_ASSERT(t != nullptr);
        return *t;
      }
      Thread* operator->() const { return rust_kernel_run_queue_iterator_get(&iter_); }

      iterator& operator++() {
        rust_kernel_run_queue_iterator_next(&iter_);
        return *this;
      }

      iterator operator++(int) {
        iterator temp = *this;
        rust_kernel_run_queue_iterator_next(&iter_);
        return temp;
      }

      bool operator==(const iterator& other) const {
        bool self_valid = IsValid();
        bool other_valid = other.IsValid();
        if (!self_valid && !other_valid) {
          return true;
        }
        if (self_valid != other_valid) {
          return false;
        }
        return rust_kernel_run_queue_iterator_get(&iter_) ==
               rust_kernel_run_queue_iterator_get(&other.iter_);
      }

      bool operator!=(const iterator& other) const { return !(*this == other); }

     private:
      RunQueueIterator iter_{};
    };

    iterator begin() const {
      return iterator(rust_kernel_run_queue_iterator_create(opaque_state_));
    }

    iterator end() const { return iterator{}; }

   private:
    alignas(8) uint8_t opaque_state_[32];
  };

  // Track bandwidth reservations for up to 8 recently blocked threads.
  static constexpr size_t kMaxBandwidthReservations = 8;
  using BandwidthReservationCache =
      power_management::BandwidthReservationCache<kMaxBandwidthReservations>;

 public:
  struct PowerLevelControlState {
    SchedProcessingRate processing_rate{1};
    SchedProcessingRate processing_rate_reciprocal{1};
    SchedProcessingRate updated_processing_rate{1};
    SchedProcessingRate default_processing_rate{1};
    SchedProcessingRate max_processing_rate{1};
    SchedProcessingRate processing_rate_limit_min{0};
    SchedProcessingRate processing_rate_limit_max{1};
    uint64_t active_power_coefficient_nw{0};
    power_management::PowerLevelUpdateRequest pending_update_request{};
    bool has_pending_update_request{false};
  };

  struct RustSchedulerState {
    Scheduler::RunQueue fair_run_queue;
    Scheduler::RunQueue deadline_run_queue;
    Scheduler::RunQueue critical_deadline_run_queue;
    int32_t runnable_task_count{0};
    SchedWeight weight_total{0};
    SchedWeight scheduled_weight_total{0};
    Thread* active_thread{nullptr};
    SchedDuration fair_period{kDefaultFairPeriod};
    SchedUtilization reciprocal_fair_period{kReciprocalDefaultFairPeriod};
    SchedTime last_update_time_ns{0};
    SchedTime start_of_current_time_slice_ns{0};
    SchedTime target_preemption_time_ns{ZX_TIME_INFINITE};
    SchedDuration total_expected_runtime_ns{0};
    cpu_num_t this_cpu;
    size_t cluster{0};
    ktl::atomic<bool> cpu_deactivating{false};
    RelaxedAtomic<SchedDuration> exported_queue_time_ns{SchedNs(0)};
    RelaxedAtomic<SchedUtilization> exported_deadline_utilization{SchedUtilization{0}};
    RelaxedAtomic<SchedUtilization> exported_clamped_deadline_utilization{SchedUtilization{0}};
    RelaxedAtomic<SchedUtilization> exported_fair_utilization{SchedUtilization{0}};
    RelaxedAtomic<SchedUtilization> exported_clamped_fair_utilization{SchedUtilization{0}};
    RelaxedAtomic<SchedUtilization> exported_clamped_total_utilization{SchedUtilization{0}};
    RelaxedAtomic<SchedProcessingRate> exported_processing_rate{SchedProcessingRate{1}};
    RelaxedAtomic<SchedProcessingRate> exported_max_processing_rate{SchedProcessingRate{1}};
    ktl::atomic<Thread*> previous_thread{nullptr};
    SchedUtilization critical_deadline_utilization{0};
    SchedWeight active_thread_weight{SchedWeight::Max()};
    Affine fair_affine_transform{};
    ktl::atomic<zx_duration_mono_t> processor_idle_time_ns{0};
    BandwidthReservationCache bandwidth_reservation_cache{};
    PowerLevelControlState power_level_control{};
  };

 private:
  void set_this_cpu(cpu_num_t cpu) TA_NO_THREAD_SAFETY_ANALYSIS { rust_state_.this_cpu = cpu; }
  void set_cluster(size_t cluster) TA_NO_THREAD_SAFETY_ANALYSIS { rust_state_.cluster = cluster; }

  TA_GUARDED(queue_lock_) RustSchedulerState rust_state_;

  static_assert(sizeof(RunQueue) == 32);
  static_assert(alignof(RunQueue) == 8);

  // Protects run queues and associated metadata for this Scheduler instance.
  // The queue lock is the bottom most lock in the system for the CPU it is
  // associated with: no other locks may be nested within a queue lock. A queue
  // lock may be acquired across CPUs, however, the no nesting rule still
  // applies.
  //
  // Note: while the queue_lock_ is the bottom-most lock in the system from the
  // scheduler's perspective, it is not (formally) a leaf-lock right now.
  // Currently, there is one place in the system (DumpThreadLocked) where the
  // queue_lock_ needs to be held, while fprintf is called.
  //
  // This happens when the lockup-detector is unhappy, and asking the Scheduler
  // to print information about its run queue state.  Most of the printf targets
  // which could be passed to the scheduler also involve holding locks.  If the
  // queue_lock_ is flagged as a leaf lock, and lockdep is enabled, and the
  // lockup-detector attempt to report a hang, it will cause lock-dep to
  // complain about obtaining a lock while holding a leaf-lock, which will cause
  // more printing, which causes more complaints, etc...
  //
  // See https://fxbug.dev/42070437 for more details.
  mutable DECLARE_SPINLOCK_WITH_TYPE(Scheduler, MonitoredSpinLock) queue_lock_
      TA_ACQ_AFTER(Thread::get_list_lock());

  // The list of threads that need to run the Save stage of their migrate functions.
  //
  // There are several invariants that must be maintained when interacting with this
  // list:
  // 1. Other CPUs can put threads into this save_state_list_, but only this Scheduler (and thus
  //    this CPU) can remove threads from it. Removal is currently handled in ProcessSaveStateList.
  // 2. Interrupts must be disabled while removing threads from this list, effectively ensuring
  //    that only one call to ProcessSaveStateList can be made at a time on a given CPU.
  // 3. Threads cannot be destroyed until they are removed from this list. This is guaranteed
  //    today by the fact that dead threads clean themselves up when they are scheduled, which is
  //    not possible if the thread is in this list.
  TA_GUARDED(queue_lock_)
  Thread::SaveStateList save_state_list_;

  TA_GUARDED(queue_lock_)
  power_management::PowerState power_state_;
};

extern "C" {
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_dump(Scheduler* scheduler, bool queue_state_only);
FFI_ALWAYS_INLINE const void* cpp_scheduler_get_queue_lock(const Scheduler* scheduler);
void cpp_current_mono_time_and_boot_ticks(Scheduler::SchedMonoTimeAndBootTicks* out);
void cpp_scheduler_block(Thread* current_thread);
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_scheduler_update_power_level(
    uint32_t domain_id, uint64_t controller_id, power_management::ControlInterface interface,
    uint64_t arg, cpu_mask_t* out_mask);
}  // extern "C"

#include <kernel/stats.h>

inline cpu_stats Scheduler::GetProjectedCpuStats(cpu_num_t cpu_num) {
  cpu_stats stats;
  rust_kernel_scheduler_get_projected_cpu_stats(cpu_num, &stats);
  return stats;
}

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_SCHEDULER_H_
