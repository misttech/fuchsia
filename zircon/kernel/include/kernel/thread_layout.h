// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_THREAD_LAYOUT_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_THREAD_LAYOUT_H_

#include <kernel/thread.h>

// Layout contract between the C++ Thread and its Rust mirror in //zircon/kernel/kernel/thread.rs.
//
// Thread's layout depends on the architecture and on build configuration (arch_thread,
// KernelStack, WITH_LOCK_DEP, UNITTESTS_ENABLED), so nothing here is spelled out as a literal.
// Each member that Rust treats as opaque gets a POD storage struct with the member's size and
// alignment, and the *Layout structs expose every member offset.  bindgen turns these into the
// thread_layout_bindings crate, where Rust asserts its own layout against them at compile time.
//
// Thread is not a standard-layout class (ChainLockable has a data member), so offsetof on it is
// only conditionally supported; in our compiler and ABI the layout is deterministic, hence the
// -Winvalid-offsetof suppression around the *Layout structs.

struct ThreadArchStorage {
  alignas(arch_thread) unsigned char bytes[sizeof(arch_thread)];
};

struct ThreadStackStorage {
  alignas(KernelStack) unsigned char bytes[sizeof(KernelStack)];
};

struct ThreadSchedulerStateStorage {
  alignas(SchedulerState) unsigned char bytes[sizeof(SchedulerState)];
};

struct ThreadSchedulerQueueStateStorage {
  alignas(SchedulerQueueState) unsigned char bytes[sizeof(SchedulerQueueState)];
};

struct ThreadLinebufferStorage {
  alignas(Thread::Linebuffer) unsigned char bytes[sizeof(Thread::Linebuffer)];
};

struct ThreadMigrateFnStorage {
  alignas(Thread::MigrateFn) unsigned char bytes[sizeof(Thread::MigrateFn)];
};

struct ThreadContextSwitchFnStorage {
  alignas(Thread::ContextSwitchFn) unsigned char bytes[sizeof(Thread::ContextSwitchFn)];
};

struct ThreadRseqAccessorStorage {
  alignas(page_map::Accessor<zx_rseq_t>) unsigned char bytes[sizeof(page_map::Accessor<zx_rseq_t>)];
};

#if WITH_LOCK_DEP
struct ThreadLockStateStorage {
  alignas(lockdep::ThreadLockState) unsigned char bytes[sizeof(lockdep::ThreadLockState)];
};
#endif

struct ThreadStorage {
  alignas(Thread) unsigned char bytes[sizeof(Thread)];
};

struct PreemptionStateStorage {
  alignas(PreemptionState) unsigned char bytes[sizeof(PreemptionState)];
};

struct TaskStateStorage {
  alignas(TaskState) unsigned char bytes[sizeof(TaskState)];
};

struct MemoryAllocationStateStorage {
  alignas(MemoryAllocationState) unsigned char bytes[sizeof(MemoryAllocationState)];
};

#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Winvalid-offsetof"

struct PreemptionStateLayout {
  static constexpr size_t kStateOffset = offsetof(PreemptionState, state_);
  static constexpr size_t kPreemptsPendingOffset = offsetof(PreemptionState, preempts_pending_);
  static constexpr size_t kTimesliceExtensionOffset =
      offsetof(PreemptionState, timeslice_extension_);
  static constexpr size_t kTimesliceExtensionDeadlineOffset =
      offsetof(PreemptionState, timeslice_extension_deadline_);
};

struct TaskStateLayout {
  static constexpr size_t kEntryOffset = offsetof(TaskState, entry_);
  static constexpr size_t kArgOffset = offsetof(TaskState, arg_);
  static constexpr size_t kRetcodeOffset = offsetof(TaskState, retcode_);
  static constexpr size_t kRetcodeWaitQueueOffset = offsetof(TaskState, retcode_wait_queue_);
};

struct MemoryAllocationStateLayout {
  static constexpr size_t kDisableCountOffset = offsetof(MemoryAllocationState, disable_count_);
};

// LINT.IfChange(ThreadLayout)
struct ThreadLayout {
  // Members that Rust models as plain pointers / integers must be exactly that wide.
  static_assert(sizeof(RestrictedStatePtr) == sizeof(void*));
  static_assert(sizeof(fbl::RefPtr<ThreadDispatcher>) == sizeof(void*));
  static_assert(sizeof(fbl::DoublyLinkedList<OwnedWaitQueue*>) == sizeof(void*));
  static_assert(sizeof(Thread::Canary) == sizeof(uint32_t));
  static_assert(sizeof(RelaxedAtomic<VmAspace*>) == sizeof(void*));

  // Members that Rust models as RelaxedAtomicU32 / RelaxedAtomicU64 must match in size and align.
  static_assert(sizeof(ktl::atomic<uint32_t>) == sizeof(uint32_t));
  static_assert(alignof(ktl::atomic<uint32_t>) == alignof(uint32_t));
  static_assert(sizeof(ktl::atomic<zx_koid_t>) == sizeof(zx_koid_t));
  static_assert(alignof(ktl::atomic<zx_koid_t>) == alignof(zx_koid_t));

  static constexpr size_t kLockOffset = offsetof(Thread, lock_);
  static constexpr size_t kCanaryOffset = offsetof(Thread, canary_);
  static constexpr size_t kFlagsOffset = offsetof(Thread, flags_);
  static constexpr size_t kSignalsOffset = offsetof(Thread, signals_);
  static constexpr size_t kSchedulerStateOffset = offsetof(Thread, scheduler_state_);
  static constexpr size_t kSchedulerQueueStateOffset = offsetof(Thread, scheduler_queue_state_);
  static constexpr size_t kWaitQueueStateOffset = offsetof(Thread, wait_queue_state_);
  static constexpr size_t kOwnedWaitQueuesOffset = offsetof(Thread, owned_wait_queues_);
  static constexpr size_t kTaskStateOffset = offsetof(Thread, task_state_);
  static constexpr size_t kPreemptionStateOffset = offsetof(Thread, preemption_state_);
  static constexpr size_t kMemoryAllocationStateOffset = offsetof(Thread, memory_allocation_state_);
  static constexpr size_t kRestrictedStateOffset = offsetof(Thread, restricted_state_);
#if WITH_LOCK_DEP
  static constexpr size_t kLockStateOffset = offsetof(Thread, lock_state_);
#endif
  static constexpr size_t kAspaceOffset = offsetof(Thread, aspace_);
#ifdef UNITTESTS_ENABLED
  static constexpr size_t kRustKtraceOverrideOffset = offsetof(Thread, rust_ktrace_override_);
#endif
  static constexpr size_t kExtraPolicyExceptionCodeOffset =
      offsetof(Thread, extra_policy_exception_code_);
  static constexpr size_t kExtraPolicyExceptionDataOffset =
      offsetof(Thread, extra_policy_exception_data_);
  static constexpr size_t kCanOwnWaitQueuesOffset = offsetof(Thread, can_own_wait_queues_);
  static constexpr size_t kUserThreadOffset = offsetof(Thread, user_thread_);
  static constexpr size_t kTidOffset = offsetof(Thread, tid_);
  static constexpr size_t kPidOffset = offsetof(Thread, pid_);
  static constexpr size_t kArchOffset = offsetof(Thread, arch_);
  static constexpr size_t kStackOffset = offsetof(Thread, stack_);
  static constexpr size_t kRecursiveObjectDeletionListOffset =
      offsetof(Thread, recursive_object_deletion_list_);
  static constexpr size_t kNameOffset = offsetof(Thread, name_);
  static constexpr size_t kLinebufferOffset = offsetof(Thread, linebuffer_);
  static constexpr size_t kLockFlowIdOffset = offsetof(Thread, lock_flow_id_);
  static constexpr size_t kUserStateSavedOffset = offsetof(Thread, user_state_saved_);
  static constexpr size_t kMigratePendingOffset = offsetof(Thread, migrate_pending_);
  static constexpr size_t kMigrateFnOffset = offsetof(Thread, migrate_fn_);
  static constexpr size_t kContextSwitchFnOffset = offsetof(Thread, context_switch_fn_);
  static constexpr size_t kRseqAccessorOffset = offsetof(Thread, rseq_accessor_);
  static constexpr size_t kSamplingSessionOffset = offsetof(Thread, sampling_session_);
  static constexpr size_t kMigrateListNodeOffset = offsetof(Thread, migrate_list_node_);
  static constexpr size_t kThreadListNodeOffset = offsetof(Thread, thread_list_node_);
  static constexpr size_t kUnblockListNodeOffset = offsetof(Thread, unblock_list_node_);
  static constexpr size_t kSaveStateListNodeOffset = offsetof(Thread, save_state_list_node_);
  static constexpr size_t kMemoryStallStateOffset = offsetof(Thread, memory_stall_state_);
};
// LINT.ThenChange(
//   //zircon/kernel/include/kernel/thread.h:ThreadLayout,
//   //zircon/kernel/kernel/thread.rs:ThreadLayout,
// )

#pragma GCC diagnostic pop

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_THREAD_LAYOUT_H_
