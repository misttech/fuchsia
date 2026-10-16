// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/thread_dispatcher.h"

#include <assert.h>
#include <inttypes.h>
#include <lib/counters.h>
#include <platform.h>
#include <string.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/syscalls/debug.h>
#include <zircon/types.h>

#include <arch/debugger.h>
#include <arch/exception.h>
#include <arch/regs.h>
#include <arch/vm.h>
#include <fbl/algorithm.h>
#include <fbl/alloc_checker.h>
#include <kernel/thread.h>
#include <object/exception_dispatcher.h>
#include <object/handle.h>
#include <object/job_dispatcher.h>
#include <object/process_dispatcher.h>
#include <vm/kstack.h>
#include <vm/vm.h>
#include <vm/vm_address_region.h>
#include <vm/vm_aspace.h>
#include <vm/vm_object_paged.h>

#define LOCAL_TRACE 0

// static
zx_status_t ThreadDispatcher::Create(fbl::RefPtr<ProcessDispatcher> process, uint32_t flags,
                                     ktl::string_view name,
                                     KernelHandle<ThreadDispatcher>* out_handle,
                                     zx_rights_t* out_rights) {
  ffi::Uninitialized<KernelHandle<ThreadDispatcher>> handle;
  ffi::Uninitialized<zx_rights_t> rights;
  zx_status_t status = rust_thread_dispatcher_create(fbl::ExportToRawPtr(&process), flags,
                                                     name.data(), name.size(), &handle, &rights);
  if (status == ZX_OK) {
    *out_handle = ktl::move(handle.Get());
    *out_rights = rights.Get();
  }
  return status;
}

// |flags| is accepted for symmetry with Create() but is unused.
ThreadDispatcher::ThreadDispatcher(fbl::RefPtr<ProcessDispatcher> process, uint32_t flags)
    : Dispatcher(0u), exceptionate_(ZX_EXCEPTION_CHANNEL_TYPE_THREAD) {
  DISPATCHER_VERIFY_OFFSET(ThreadDispatcher, kThreadDispatcherStateOffset);
  rust_thread_dispatcher_state_init(&opaque_storage_, this, fbl::ExportToRawPtr(&process));
}

Lock<CriticalMutex>* ThreadDispatcher::get_lock() const {
  return rust_thread_dispatcher_state_get_lock(&opaque_storage_);
}

ThreadDispatcher::~ThreadDispatcher() {
  LTRACE_ENTRY_OBJ;

  // The _locked accessors below are called without taking get_lock(). Destruction implies the
  // last reference is gone, so there is no concurrent access to race with.
  DEBUG_ASSERT_MSG(!HasStartedLocked() || IsDyingOrDeadLocked(),
                   "Thread %p killed in bad state: %s\n", this,
                   ThreadLifecycleToString(lifecycle_locked()));

  if (lifecycle_locked() != ThreadState::Lifecycle::INITIAL) {
    // We grew the pool in Initialize(), which transitioned the thread from its
    // inintial state.
    process()->futex_context().ShrinkFutexStatePool();
  }

  // If MakeRunnable hasn't been called, then our core_thread_ has never run and
  // we need to be the ones to remove it.
  if (!HasStartedLocked() && core_thread_ != nullptr) {
    // Since the Thread is in an initialized state we can directly destruct it.
    core_thread_->Forget();
    core_thread_ = nullptr;
  }

  // Runs last so that the code above can still reach the parent process through the Rust state.
  rust_thread_dispatcher_state_destroy(&opaque_storage_);
}

zx_status_t ThreadDispatcher::set_name(const char* name, size_t len) {
  AssertCanary();

  Guard<CriticalMutex> thread_guard{get_lock()};
  if (core_thread_ == nullptr) {
    return ZX_ERR_BAD_STATE;
  }
  core_thread_->set_name({name, len});
  KTRACE_KERNEL_OBJECT("kernel:meta", get_koid(), ZX_OBJ_TYPE_THREAD, (fxt::StringRef{name, len}),
                       ("process", ktrace::Koid(process()->get_koid())),
                       ("job", ktrace::Koid(process()->job()->get_koid())));
  return ZX_OK;
}

zx_status_t ThreadDispatcher::get_name(char (&out_name)[ZX_MAX_NAME_LEN]) const {
  AssertCanary();

  memset(out_name, 0, ZX_MAX_NAME_LEN);

  Guard<CriticalMutex> thread_guard{get_lock()};
  if (core_thread_ == nullptr) {
    return ZX_ERR_BAD_STATE;
  }
  strlcpy(out_name, core_thread_->name(), ZX_MAX_NAME_LEN);
  return ZX_OK;
}

void ThreadDispatcher::ExitingCurrent() {
  AssertCanary();

  LTRACE_ENTRY_OBJ;

  // Set ourselves in the DYING state before calling the Debugger.
  {
    Guard<CriticalMutex> guard{get_lock()};
    SetStateLocked(ThreadState::Lifecycle::DYING);
  }

  // Notify a debugger if attached. Do this before marking the thread as
  // dead: the debugger expects to see the thread in the DYING state, it may
  // try to read thread registers. The debugger still has to handle the case
  // where the process is also dying (and thus the thread could transition
  // DYING->DEAD from underneath it), but that's life (or death :-)).
  //
  // Thread exit exceptions don't currently provide an iframe.
  arch_exception_context_t context{};
  HandleSingleShotException(process()->debug_exceptionate(), ZX_EXCP_THREAD_EXITING, context);

  // Mark the thread as dead. Do this before removing the thread from the
  // process because if this is the last thread then the process will be
  // marked dead, and we don't want to have a state where the process is
  // dead but one thread is not.
  {
    // Mark the thread as dead in the thread stats.  The thread is technically
    // still running, but from a perspective of user-mode observable thread
    // stats, it is now dead.  It is important to do this now, before we remove
    // ourselves from our process's collection.  If we are the last thread to
    // leave the collection, user-mode will be signaled and will expect the
    // process to be dead (implying that its thread stats should no longer be
    // increasing).  If we have not declared this thread to be dead yet, users
    // could continue to see the thread stats change even though they now
    // consider the process to be dead.
    {
      InterruptDisableGuard interrupt_disable;
      UpdateRuntimeStats(thread_state::THREAD_DEATH);
    }

    // put ourselves into the dead state
    Guard<CriticalMutex> guard{get_lock()};
    SetStateLocked(ThreadState::Lifecycle::DEAD);

    // Then obtain the core thread lock and reset our core thread pointer.
    Guard<SpinLock, IrqSave> core_thread_guard{&core_thread_lock_};
    ResetCoreThread();
  }

  // Drop our exception channel endpoint so any userspace listener
  // gets the PEER_CLOSED signal.
  exceptionate_.Shutdown();

  // remove ourselves from our parent process's view
  process()->RemoveThread(this);

  LTRACE_EXIT_OBJ;
}

// low level LK entry point for the thread
int ThreadDispatcher::StartRoutine(void* arg) {
  LTRACE_ENTRY;

  ThreadDispatcher* t = (ThreadDispatcher*)arg;

  // IWBN to dump the values just before calling |arch_enter_uspace()|
  // but at that point they're in |iframe| and may have been modified by
  // the debugger user, and fetching them out of the iframe will require
  // architecture-specific code. Instead just print them here. This is just
  // for tracing which is generally off, and then only time the values will
  // have changed is if a debugger user changes them. KISS.
  LTRACEF("arch_enter_uspace SP: %#" PRIxPTR " PC: %#" PRIxPTR ", ARG1: %#" PRIxPTR
          ", ARG2: %#" PRIxPTR "\n",
          t->user_entry_.sp, t->user_entry_.pc, t->user_entry_.arg1, t->user_entry_.arg2);

  // Initialize an iframe for entry into userspace.
  // We need all registers accessible from the ZX_EXCP_THREAD_STARTING
  // exception handler (the debugger wants the thread to look as if the
  // thread is at the first instruction). For architectural exceptions the
  // general regs are left in the iframe for speed and simplicity. To keep
  // things simple we use the same scheme.
  iframe_t iframe;
  arch_prepare_uspace(&t->user_entry_, &iframe);

  arch_exception_context_t context = {.frame = &iframe};

  // Notify job debugger if attached.
  if (t->is_initial_thread_) {
    t->process()->OnProcessStartForJobDebugger(t, &context);
  }

  // Notify debugger if attached.
  t->HandleSingleShotException(t->process()->debug_exceptionate(), ZX_EXCP_THREAD_STARTING,
                               context);

  arch_disable_ints();
  arch_iframe_process_pending_signals(&iframe);

  // switch to user mode and start the process
  arch_enter_uspace(&iframe);
  __UNREACHABLE;
}

bool ThreadDispatcher::InExceptionLocked() {
  AssertCanary();

  LTRACE_ENTRY_OBJ;
  DEBUG_ASSERT(get_lock()->lock().IsHeld());
  return exception_ != nullptr;
}

bool ThreadDispatcher::SuspendedOrInExceptionLocked() {
  AssertCanary();
  return lifecycle_locked() == ThreadState::Lifecycle::SUSPENDED || InExceptionLocked();
}

static zx_thread_state_t ThreadLifecycleToState(ThreadState::Lifecycle lifecycle,
                                                ThreadDispatcher::Blocked blocked_reason) {
  switch (lifecycle) {
    case ThreadState::Lifecycle::INITIAL:
    case ThreadState::Lifecycle::INITIALIZED:
      return ZX_THREAD_STATE_NEW;
    case ThreadState::Lifecycle::RUNNING:
      // The thread may be "running" but be blocked in a syscall or
      // exception handler.
      switch (blocked_reason) {
        case ThreadDispatcher::Blocked::NONE:
          return ZX_THREAD_STATE_RUNNING;
        case ThreadDispatcher::Blocked::EXCEPTION:
          return ZX_THREAD_STATE_BLOCKED_EXCEPTION;
        case ThreadDispatcher::Blocked::SLEEPING:
          return ZX_THREAD_STATE_BLOCKED_SLEEPING;
        case ThreadDispatcher::Blocked::FUTEX:
          return ZX_THREAD_STATE_BLOCKED_FUTEX;
        case ThreadDispatcher::Blocked::PORT:
          return ZX_THREAD_STATE_BLOCKED_PORT;
        case ThreadDispatcher::Blocked::CHANNEL:
          return ZX_THREAD_STATE_BLOCKED_CHANNEL;
        case ThreadDispatcher::Blocked::WAIT_ONE:
          return ZX_THREAD_STATE_BLOCKED_WAIT_ONE;
        case ThreadDispatcher::Blocked::WAIT_MANY:
          return ZX_THREAD_STATE_BLOCKED_WAIT_MANY;
        case ThreadDispatcher::Blocked::INTERRUPT:
          return ZX_THREAD_STATE_BLOCKED_INTERRUPT;
        case ThreadDispatcher::Blocked::PAGER:
          return ZX_THREAD_STATE_BLOCKED_PAGER;
        default:
          DEBUG_ASSERT_MSG(false, "unexpected blocked reason: %d",
                           static_cast<int>(blocked_reason));
          return ZX_THREAD_STATE_BLOCKED;
      }
    case ThreadState::Lifecycle::SUSPENDED:
      return ZX_THREAD_STATE_SUSPENDED;
    case ThreadState::Lifecycle::DYING:
      return ZX_THREAD_STATE_DYING;
    case ThreadState::Lifecycle::DEAD:
      return ZX_THREAD_STATE_DEAD;
    default:
      DEBUG_ASSERT_MSG(false, "unexpected run state: %d", static_cast<int>(lifecycle));
      return ZX_THREAD_RUNNING;
  }
}

zx_info_thread_t ThreadDispatcher::GetInfoForUserspace() const {
  AssertCanary();

  zx_info_thread_t info = {};

  Guard<CriticalMutex> guard{get_lock()};
  info.state =
      ThreadLifecycleToState(lifecycle_locked(), blocked_reason_.load(ktl::memory_order_acquire));
  info.wait_exception_channel_type = exceptionate_type_;

  // If we've exited then return with the lifecycle and exception type, and keep
  // the cpu_affinity_mask at 0.
  if (core_thread_ == nullptr) {
    return info;
  }

  // Get CPU affinity.
  //
  // We assume that we can fit the entire mask in the first word of
  // cpu_affinity_mask.
  static_assert(SMP_MAX_CPUS <= sizeof(info.cpu_affinity_mask.mask[0]) * 8);
  info.cpu_affinity_mask.mask[0] = core_thread_->GetSoftCpuAffinity();

  return info;
}

zx_status_t ThreadDispatcher::GetStatsForUserspace(zx_info_thread_stats_t* info) {
  AssertCanary();

  LTRACE_ENTRY_OBJ;

  *info = {};

  Guard<CriticalMutex> guard{get_lock()};

  if (core_thread_ == nullptr) {
    return ZX_ERR_BAD_STATE;
  }

  info->total_runtime = core_thread_->Runtime();
  info->last_scheduled_cpu = core_thread_->LastCpu();
  return ZX_OK;
}

TaskRuntimeStats ThreadDispatcher::GetCompensatedTaskRuntimeStats() const {
  AssertCanary();
  return runtime_stats_.GetCompensatedTaskRuntimeStats();
}

void ThreadDispatcher::UpdateRuntimeStats(thread_state new_state) {
  AssertCanary();
  DEBUG_ASSERT(arch_ints_disabled());
  runtime_stats_.Update(new_state, ThreadRuntimeStats::NoIrqSave);
}

void ThreadDispatcher::AddPageFaultTicks(zx_duration_mono_ticks_t ticks) {
  AssertCanary();
  runtime_stats_.AddPageFaultTicks(ticks);
}

void ThreadDispatcher::AddLockContentionTicks(zx_duration_mono_ticks_t ticks) {
  AssertCanary();
  runtime_stats_.AddLockContentionTicks(ticks);
}

zx_status_t ThreadDispatcher::GetExceptionReport(zx_exception_report_t* report) {
  AssertCanary();

  LTRACE_ENTRY_OBJ;
  Guard<CriticalMutex> guard{get_lock()};

  if (InExceptionLocked()) {
    // We always leave exception handling before the report gets wiped
    // so this must succeed.
    [[maybe_unused]] bool success = exception_->FillReport(report);
    DEBUG_ASSERT(success);
  } else {
    return ZX_ERR_BAD_STATE;
  }

  return ZX_OK;
}

Exceptionate* ThreadDispatcher::exceptionate() {
  AssertCanary();
  return &exceptionate_;
}

zx_status_t ThreadDispatcher::HandleException(Exceptionate* exceptionate,
                                              fbl::RefPtr<ExceptionDispatcher> exception,
                                              bool* sent) {
  AssertCanary();

  LTRACE_ENTRY_OBJ;

  *sent = false;

  // Mark as blocked before sending the exception, otherwise a handler could
  // potentially query our state before we've marked ourself blocked.
  ThreadDispatcher::AutoBlocked by(ThreadDispatcher::Blocked::EXCEPTION);

  {
    Guard<CriticalMutex> guard{get_lock()};

    // We send the exception while locked so that if it succeeds we can
    // atomically update our state.
    zx_status_t status = exceptionate->SendException(exception);
    if (status != ZX_OK) {
      return status;
    }
    *sent = true;

    // This state is needed by GetInfoForUserspace().
    exception_ = exception;
    exceptionate_type_ = exceptionate->type();
    SetExceptionStateLocked(ThreadState::Exception::UNPROCESSED);
  }

  LTRACEF("blocking on exception response\n");

  zx_status_t status = exception->WaitForHandleClose();

  LTRACEF("received exception response %d\n", status);

  Guard<CriticalMutex> guard{get_lock()};

  // If both the thread was killed and the exception was resumed before we
  // started waiting, the exception resume status (ZX_OK) might be returned,
  // but we want thread kill to always take priority and stop exception
  // handling immediately.
  //
  // Note that this logic will always trigger for ZX_EXCP_THREAD_EXITING
  // whether the thread was killed or is exiting normally, but that's fine
  // because that exception only ever goes to a single handler so we ignore
  // the handler return value anyway.
  if (IsDyingOrDeadLocked()) {
    status = ZX_ERR_INTERNAL_INTR_KILLED;
  }

  exception_.reset();
  SetExceptionStateLocked(ThreadState::Exception::IDLE);

  return status;
}

bool ThreadDispatcher::HandleSingleShotException(Exceptionate* exceptionate,
                                                 zx_excp_type_t exception_type,
                                                 const arch_exception_context_t& arch_context) {
  AssertCanary();

  LTRACE_ENTRY_OBJ;

  // Do a quick check for valid channel first. It's still possible that the
  // channel will become invalid immediately after this check, but that will
  // be caught when we try to send the exception. This is just an
  // optimization to avoid unnecessary setup/teardown in the common case.
  if (!exceptionate->HasValidChannel()) {
    return false;
  }

  zx_exception_report_t report = ExceptionDispatcher::BuildArchReport(exception_type, arch_context);

  fbl::RefPtr<ExceptionDispatcher> exception =
      ExceptionDispatcher::Create(fbl::RefPtr(this), exception_type, &report, &arch_context);
  if (!exception) {
    printf("KERN: failed to allocate memory for exception type %u in thread %lu.%lu\n",
           exception_type, process()->get_koid(), get_koid());
    return false;
  }

  // We're about to handle the exception (|HandleException|).  Use a |ScopedThreadExceptionContext|
  // to make the thread's user register state available to debuggers and exception handlers while
  // the thread is "in exception".
  ScopedThreadExceptionContext context(&arch_context);

  bool sent = false;
  HandleException(exceptionate, exception, &sent);

  exception->Clear();

  return sent;
}

// T is the state type to read.
// F is a function that gets state T and has signature |zx_status_t (F)(Thread*, T*)|.
template <typename T, typename F>
zx_status_t ThreadDispatcher::ReadStateGeneric(F get_state_func, user_out_ptr<void> buffer,
                                               size_t buffer_size) {
  if (buffer_size < sizeof(T)) {
    return ZX_ERR_BUFFER_TOO_SMALL;
  }

  T state{};

  {
    Guard<CriticalMutex> guard{get_lock()};
    // We can't be reading regs while the thread transitions from SUSPENDED to RUNNING.
    if (!SuspendedOrInExceptionLocked()) {
      return ZX_ERR_BAD_STATE;
    }
    zx_status_t status = get_state_func(core_thread_, &state);
    if (status != ZX_OK) {
      return status;
    }
  }

  // Since copy may fault, copy only after releasing the lock.
  return buffer.reinterpret<T>().copy_to_user(state);
}

zx_status_t ThreadDispatcher::ReadState(zx_thread_state_topic_t state_kind,
                                        user_out_ptr<void> buffer, size_t buffer_size) {
  AssertCanary();

  LTRACE_ENTRY_OBJ;

  switch (state_kind) {
    case ZX_THREAD_STATE_GENERAL_REGS:
      return ReadStateGeneric<zx_thread_state_general_regs_t>(arch_get_general_regs, buffer,
                                                              buffer_size);
    case ZX_THREAD_STATE_FP_REGS:
      return ReadStateGeneric<zx_thread_state_fp_regs_t>(arch_get_fp_regs, buffer, buffer_size);
    case ZX_THREAD_STATE_VECTOR_REGS:
      return ReadStateGeneric<zx_thread_state_vector_regs_t>(arch_get_vector_regs, buffer,
                                                             buffer_size);
    case ZX_THREAD_STATE_DEBUG_REGS:
      return ReadStateGeneric<zx_thread_state_debug_regs_t>(arch_get_debug_regs, buffer,
                                                            buffer_size);
    case ZX_THREAD_STATE_SINGLE_STEP:
      return ReadStateGeneric<zx_thread_state_single_step_t>(arch_get_single_step, buffer,
                                                             buffer_size);
    default:
      return ZX_ERR_INVALID_ARGS;
  }
}

// T is the state type to write.
// F is a function that sets state T and has signature |zx_status_t (F)(Thread*, const T*)|.
template <typename T, typename F>
zx_status_t ThreadDispatcher::WriteStateGeneric(F set_state_func, user_in_ptr<const void> buffer,
                                                size_t buffer_size) {
  if (buffer_size < sizeof(T)) {
    return ZX_ERR_BUFFER_TOO_SMALL;
  }

  T state{};
  zx_status_t status = buffer.reinterpret<const T>().copy_from_user(&state);
  if (status != ZX_OK) {
    return status;
  }

  Guard<CriticalMutex> guard{get_lock()};

  // We can't be writing regs while the thread transitions from SUSPENDED to RUNNING.
  if (!SuspendedOrInExceptionLocked()) {
    return ZX_ERR_BAD_STATE;
  }

  return set_state_func(core_thread_, &state);
}

zx_status_t ThreadDispatcher::WriteState(zx_thread_state_topic_t state_kind,
                                         user_in_ptr<const void> buffer, size_t buffer_size) {
  AssertCanary();

  LTRACE_ENTRY_OBJ;

  switch (state_kind) {
    case ZX_THREAD_STATE_GENERAL_REGS:
      return WriteStateGeneric<zx_thread_state_general_regs_t>(arch_set_general_regs, buffer,
                                                               buffer_size);
    case ZX_THREAD_STATE_FP_REGS:
      return WriteStateGeneric<zx_thread_state_fp_regs_t>(arch_set_fp_regs, buffer, buffer_size);
    case ZX_THREAD_STATE_VECTOR_REGS:
      return WriteStateGeneric<zx_thread_state_vector_regs_t>(arch_set_vector_regs, buffer,
                                                              buffer_size);
    case ZX_THREAD_STATE_DEBUG_REGS:
      return WriteStateGeneric<zx_thread_state_debug_regs_t>(arch_set_debug_regs, buffer,
                                                             buffer_size);
    case ZX_THREAD_STATE_SINGLE_STEP:
      return WriteStateGeneric<zx_thread_state_single_step_t>(arch_set_single_step, buffer,
                                                              buffer_size);
    default:
      return ZX_ERR_INVALID_ARGS;
  }
}

zx_status_t ThreadDispatcher::SetBaseProfile(const SchedulerState::BaseProfile& profile) {
  Guard<CriticalMutex> guard{get_lock()};
  if ((lifecycle_locked() == ThreadState::Lifecycle::INITIAL) ||
      (lifecycle_locked() == ThreadState::Lifecycle::DYING) ||
      (lifecycle_locked() == ThreadState::Lifecycle::DEAD)) {
    return ZX_ERR_BAD_STATE;
  }

  // The profile was already validated by the Profile dispatcher.
  core_thread_->SetBaseProfile(profile);
  return ZX_OK;
}

zx_status_t ThreadDispatcher::SetSoftAffinity(cpu_mask_t mask) {
  Guard<CriticalMutex> guard{get_lock()};
  if ((lifecycle_locked() == ThreadState::Lifecycle::INITIAL) ||
      (lifecycle_locked() == ThreadState::Lifecycle::DYING) ||
      (lifecycle_locked() == ThreadState::Lifecycle::DEAD)) {
    return ZX_ERR_BAD_STATE;
  }
  // The mask was already validated by the Profile dispatcher.
  core_thread_->SetSoftCpuAffinity(mask);
  return ZX_OK;
}

// TRANSITIONAL. Duplicates lifecycle_to_string() in thread_state.rs. Two callers remain, both
// in this file: the DEBUG_ASSERT_MSG in ~ThreadDispatcher() and the LTRACEF in Suspend(). Once
// both move to Rust this definition and its declaration in thread_state.h are deleted. Do not
// add callers.
//
// LINT.IfChange(lifecycle_to_string)
const char* ThreadLifecycleToString(ThreadState::Lifecycle lifecycle) {
  switch (lifecycle) {
    case ThreadState::Lifecycle::INITIAL:
      return "initial";
    case ThreadState::Lifecycle::INITIALIZED:
      return "initialized";
    case ThreadState::Lifecycle::RUNNING:
      return "running";
    case ThreadState::Lifecycle::SUSPENDED:
      return "suspended";
    case ThreadState::Lifecycle::DYING:
      return "dying";
    case ThreadState::Lifecycle::DEAD:
      return "dead";
  }
  // Fallback for an out-of-range discriminant. lifecycle_locked() range-checks the value it
  // returns with a DEBUG_ASSERT, which is compiled out at LK_DEBUGLEVEL <= 1; in those builds an
  // invalid value matches no case above and must still yield a well-formed string.
  return "unknown";
}
// LINT.ThenChange(//zircon/kernel/object/thread_state.rs:lifecycle_to_string)
