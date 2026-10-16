// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <lib/counters.h>
#include <lib/object-constants.h>
#include <lib/user_copy/user_ptr.h>

#include <fbl/alloc_checker.h>
#include <kernel/ffi.h>
#include <object/channel_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/thread_dispatcher.h>

KCOUNTER(thread_legacy_yield, "thread.legacy_yield")

static_assert(sizeof(ThreadDispatcher::CoreThreadObservation) == 2 * sizeof(uintptr_t));
static_assert(alignof(ThreadDispatcher::CoreThreadObservation) == alignof(uintptr_t));

// The transitional rust_thread_dispatcher_{lifecycle,set_state,set_exception_state}_locked
// bridges pass these enumerations across the FFI boundary as raw discriminants, so the two
// numberings have to agree exactly. The Rust enums are #[repr(u32)] with explicit discriminants;
// these asserts hold the C++ side to the same values.
// LINT.IfChange(thread_lifecycle)
static_assert(static_cast<uint32_t>(ThreadState::Lifecycle::INITIAL) == 0);
static_assert(static_cast<uint32_t>(ThreadState::Lifecycle::INITIALIZED) == 1);
static_assert(static_cast<uint32_t>(ThreadState::Lifecycle::RUNNING) == 2);
static_assert(static_cast<uint32_t>(ThreadState::Lifecycle::SUSPENDED) == 3);
static_assert(static_cast<uint32_t>(ThreadState::Lifecycle::DYING) == 4);
static_assert(static_cast<uint32_t>(ThreadState::Lifecycle::DEAD) == 5);
// LINT.ThenChange(//zircon/kernel/object/thread_state.rs:Lifecycle)

// LINT.IfChange(thread_exception)
static_assert(static_cast<uint32_t>(ThreadState::Exception::IDLE) == 0);
static_assert(static_cast<uint32_t>(ThreadState::Exception::UNPROCESSED) == 1);
static_assert(static_cast<uint32_t>(ThreadState::Exception::TRY_NEXT) == 2);
static_assert(static_cast<uint32_t>(ThreadState::Exception::RESUME) == 3);
// LINT.ThenChange(//zircon/kernel/object/thread_state.rs:Exception)

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE ThreadDispatcher* cpp_thread_dispatcher_current() {
  return ThreadDispatcher::GetCurrent();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_dispatcher_is_current(const ThreadDispatcher* thread) {
  return thread == ThreadDispatcher::GetCurrent();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uintptr_t
cpp_thread_dispatcher_blocking_futex_id(const ThreadDispatcher* thread) {
  return thread->blocking_futex_id().get();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dispatcher_set_blocking_futex_id(ThreadDispatcher* thread,
                                                                   uintptr_t id) {
  thread->set_blocking_futex_id(FutexId(id));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_thread_dispatcher_lock_acquire(ThreadDispatcher* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread->get_lock()->lock().Acquire() == CriticalMutex::ShouldClear::Yes;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dispatcher_lock_release(
    ThreadDispatcher* thread, bool should_clear) TA_NO_THREAD_SAFETY_ANALYSIS {
  thread->get_lock()->lock().Release(should_clear ? CriticalMutex::ShouldClear::Yes
                                                  : CriticalMutex::ShouldClear::No);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dispatcher_observe_core_thread(
    ThreadDispatcher* thread, ffi::Uninitialized<ThreadDispatcher::CoreThreadObservation>* out_obs)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  out_obs->Initialize(thread->ObserveCoreThread());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dispatcher_release_core_thread_observation(
    ThreadDispatcher::CoreThreadObservation* obs) TA_NO_THREAD_SAFETY_ANALYSIS {
  obs->Release();
}

zx_status_t cpp_thread_dispatcher_alloc(
    ProcessDispatcher* process, uint32_t flags,
    ffi::Uninitialized<KernelHandle<ThreadDispatcher>>* out_handle) {
  fbl::RefPtr<ProcessDispatcher> process_ref = fbl::ImportFromRawPtr(process);
  fbl::AllocChecker ac;
  auto user_thread = fbl::AdoptRef(new (&ac) ThreadDispatcher(ktl::move(process_ref), flags));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  out_handle->Initialize(KernelHandle<ThreadDispatcher>(ktl::move(user_thread)));
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dispatcher_set_core_thread(ThreadDispatcher* thread,
                                                             Thread* core_thread) {
  thread->set_core_thread(core_thread);
}

// |core_thread_| is TA_GUARDED(get_lock()), and every caller of this shim reads it while holding
// that lock -- but it holds it from Rust, through a ksync guard rather than a C++ Guard, so
// Clang's analysis cannot see the acquisition and would reject the access. The suppression is
// scoped to this shim, which is exactly the seam the analysis cannot cross; the accessor it
// calls keeps its TA_REQ.
//
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Thread* cpp_thread_dispatcher_get_core_thread(const ThreadDispatcher* thread)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread->core_thread();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dispatcher_set_user_entry(ThreadDispatcher* thread,
                                                            const UserEntryState* entry) {
  thread->set_user_entry(*entry);
}

// Attaches |thread| to its lower level kernel thread. This takes out an additional reference on
// the ThreadDispatcher, which the kernel Thread holds until it exits.
//
// Thread::SetUsermodeThread() is a Thread method, so this would otherwise belong beside the
// other Thread shims in kernel/thread_ffi.cc. It lives here because it names a ThreadDispatcher,
// and the Rust `kernel` module must not depend on the `object` module.
//
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dispatcher_attach_to_core_thread(ThreadDispatcher* thread,
                                                                   Thread* core_thread) {
  core_thread->SetUsermodeThread(fbl::RefPtr<ThreadDispatcher>(thread));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE int cpp_thread_start_routine_entry(void* thread) {
  return ThreadDispatcher::StartRoutine(thread);
}

zx_status_t cpp_thread_dispatcher_read_state(ThreadDispatcher* thread, uint32_t state_kind,
                                             void* buffer, size_t buffer_size) {
  return thread->ReadState(static_cast<zx_thread_state_topic_t>(state_kind),
                           make_user_out_ptr(buffer), buffer_size);
}

zx_status_t cpp_thread_dispatcher_write_state(ThreadDispatcher* thread, uint32_t state_kind,
                                              const void* buffer, size_t buffer_size) {
  if ((state_kind & ZX_THREAD_STATE_DEBUG_REGS) && !BootOptions::Get()->enable_debugging_syscalls) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  return thread->WriteState(static_cast<zx_thread_state_topic_t>(state_kind),
                            make_user_in_ptr(buffer), buffer_size);
}

zx_status_t cpp_thread_dispatcher_set_base_profile(ThreadDispatcher* thread,
                                                   const SchedulerState::BaseProfile* profile) {
  static_assert(sizeof(SchedulerState::BaseProfile) == kSchedulerStateBaseProfileSize,
                "SchedulerState::BaseProfile size mismatch");
  static_assert(alignof(SchedulerState::BaseProfile) == kSchedulerStateBaseProfileAlign,
                "SchedulerState::BaseProfile align mismatch");
  return thread->SetBaseProfile(*profile);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_dispatcher_set_soft_affinity(ThreadDispatcher* thread,
                                                                      cpu_mask_t mask) {
  return thread->SetSoftAffinity(mask);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dispatcher_get_info_for_userspace(
    const ThreadDispatcher* thread, ffi::Uninitialized<zx_info_thread_t>* out_info) {
  out_info->Initialize(thread->GetInfoForUserspace());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_dispatcher_get_stats_for_userspace(
    ThreadDispatcher* thread, ffi::Uninitialized<zx_info_thread_stats_t>* out_info) {
  return thread->GetStatsForUserspace(out_info->GetAddressUnchecked());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_thread_dispatcher_get_runtime_stats(
    const ThreadDispatcher* thread, ffi::Uninitialized<zx_info_task_runtime_t>* out_info) {
  out_info->Initialize(thread->GetRuntimeStats());
}

zx_status_t cpp_sys_thread_raise_exception(uint32_t options, zx_excp_type_t type,
                                           const zx_exception_context_t* user_context) {
  if (options != ZX_EXCEPTION_TARGET_JOB_DEBUGGER || type != ZX_EXCP_USER || !user_context) {
    return ZX_ERR_INVALID_ARGS;
  }

  arch_exception_context_t context = {};
  context.user_synth_code = user_context->synth_code;
  context.user_synth_data = user_context->synth_data;

  auto thread = ThreadDispatcher::GetCurrent();
  thread->process()->OnUserExceptionForJobDebugger(thread, &context);
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_sys_thread_legacy_yield(uint32_t options) {
  if (options != 0) {
    return ZX_ERR_INVALID_ARGS;
  }
  kcounter_add(thread_legacy_yield, 1);
  Thread::Current::Yield();
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE ThreadDispatcher::Blocked cpp_thread_dispatcher_set_blocked_reason(
    ThreadDispatcher::Blocked reason) {
  return ThreadDispatcher::SetBlockedReason(reason);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_thread_dispatcher_get_exception_report(
    const ThreadDispatcher* thread, ffi::Uninitialized<zx_exception_report_t>* out_report) {
  return const_cast<ThreadDispatcher*>(thread)->GetExceptionReport(
      out_report->GetAddressUnchecked());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE ChannelDispatcher::MessageWaiter*
cpp_thread_dispatcher_get_current_message_waiter() {
  return ThreadDispatcher::GetCurrent()->GetMessageWaiter();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Exceptionate* cpp_thread_dispatcher_exceptionate(ThreadDispatcher* thread) {
  return thread->exceptionate();
}

}  // extern "C"
