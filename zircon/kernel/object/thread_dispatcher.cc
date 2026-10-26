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
#include <ktl/optional.h>
#include <ktl/type_traits.h>
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

ThreadDispatcher::ThreadDispatcher(fbl::RefPtr<ProcessDispatcher> process) : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(ThreadDispatcher, kThreadDispatcherStateOffset);
  rust_thread_dispatcher_state_init(&opaque_storage_, this, fbl::ExportToRawPtr(&process));
}

Lock<CriticalMutex>* ThreadDispatcher::get_lock() const {
  return rust_thread_dispatcher_state_get_lock(&opaque_storage_);
}

ThreadDispatcher::~ThreadDispatcher() {
  LTRACE_ENTRY_OBJ;

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

namespace {

// Prior to API level NEXT, zx_arm64_thread_state_vector_regs_t did not include the SVE `p` and
// `ffr` fields and was 520 bytes (`offsetof(zx_arm64_thread_state_vector_regs_t, p)`). Maintain
// support for that exact 520-byte size for callers compiled against earlier API levels.
template <typename T>
constexpr ktl::optional<size_t> kStateCompatSize = ktl::nullopt;

#if defined(__aarch64__)
template <>
constexpr ktl::optional<size_t> kStateCompatSize<zx_arm64_thread_state_vector_regs_t> =
    offsetof(zx_arm64_thread_state_vector_regs_t, p);
#endif

}  // namespace

// T is the state type to read.
// F is a function that gets state T and has signature |zx_status_t (F)(Thread*, T*)|.
template <typename T, typename F>
zx_status_t ThreadDispatcher::ReadStateGeneric(F get_state_func, user_out_ptr<void> buffer,
                                               size_t buffer_size) {
  size_t copy_size = sizeof(T);
  if (buffer_size < copy_size) {
    if (kStateCompatSize<T> && buffer_size == *kStateCompatSize<T>) {
      copy_size = *kStateCompatSize<T>;
    } else {
      return ZX_ERR_BUFFER_TOO_SMALL;
    }
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
  return buffer.reinterpret<uint8_t>().copy_array_to_user(reinterpret_cast<const uint8_t*>(&state),
                                                          copy_size);
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
  size_t copy_size = sizeof(T);
  if (buffer_size < copy_size) {
    if (kStateCompatSize<T> && buffer_size == *kStateCompatSize<T>) {
      copy_size = *kStateCompatSize<T>;
    } else {
      return ZX_ERR_BUFFER_TOO_SMALL;
    }
  }

  T state{};
  zx_status_t status = buffer.reinterpret<const uint8_t>().copy_array_from_user(
      reinterpret_cast<uint8_t*>(&state), copy_size);
  if (status != ZX_OK) {
    return status;
  }

  Guard<CriticalMutex> guard{get_lock()};

  // We can't be writing regs while the thread transitions from SUSPENDED to RUNNING.
  if (!SuspendedOrInExceptionLocked()) {
    return ZX_ERR_BAD_STATE;
  }

  if constexpr (ktl::is_same_v<T, zx_thread_state_vector_regs_t>) {
    if (copy_size < sizeof(T)) {
      T current{};
      zx_status_t get_status = arch_get_vector_regs(core_thread_, &current);
      if (get_status != ZX_OK) {
        return get_status;
      }
      memcpy(reinterpret_cast<uint8_t*>(&state) + copy_size,
             reinterpret_cast<const uint8_t*>(&current) + copy_size, sizeof(T) - copy_size);
    }
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
