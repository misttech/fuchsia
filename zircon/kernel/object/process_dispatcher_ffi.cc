// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/ffi.h>
#include <object/handle.h>
#include <object/job_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/thread_dispatcher.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE ProcessDispatcher* cpp_process_dispatcher_current() {
  return ProcessDispatcher::GetCurrent();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_process_dispatcher_is_current(const ProcessDispatcher* process) {
  return process == ProcessDispatcher::GetCurrent();
}

zx_status_t cpp_process_dispatcher_start(ProcessDispatcher* process, ThreadDispatcher* thread,
                                         zx_vaddr_t pc, zx_vaddr_t sp, Handle* arg_handle,
                                         uintptr_t arg2) {
  return process->Start(fbl::RefPtr<ThreadDispatcher>(thread), pc, sp, HandleOwner(arg_handle),
                        arg2);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_process_dispatcher_kill(ProcessDispatcher* process, int64_t retcode) {
  process->Kill(retcode);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_process_dispatcher_suspend(ProcessDispatcher* process) {
  return process->Suspend();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_process_dispatcher_resume(ProcessDispatcher* process) {
  process->Resume();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_process_dispatcher_make_and_add_handle(
    ProcessDispatcher* process, KernelHandle<Dispatcher>* handle, zx_rights_t rights,
    zx_handle_t* out_handle) {
  return process->MakeAndAddHandle(ktl::move(*handle), rights, out_handle);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_process_dispatcher_make_and_add_handle_from_ref(
    ProcessDispatcher* process, Dispatcher* dispatcher, zx_rights_t rights,
    zx_handle_t* out_handle) {
  return process->MakeAndAddHandle(fbl::ImportFromRawPtr(dispatcher), rights, out_handle);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_handle_table_get_dispatcher(
    zx_handle_t handle, ffi::Uninitialized<fbl::RefPtr<Dispatcher>>* out_disp,
    zx_rights_t* out_rights) {
  auto up = ProcessDispatcher::GetCurrent();
  fbl::RefPtr<Dispatcher> disp;
  zx_status_t status = up->handle_table().GetDispatcherAndRights(*up, handle, &disp, out_rights);
  if (status == ZX_OK) {
    out_disp->Initialize(ktl::move(disp));
  }
  return status;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t
cpp_process_dispatcher_enforce_basic_policy(const ProcessDispatcher* process, uint32_t policy) {
  return const_cast<ProcessDispatcher*>(process)->EnforceBasicPolicy(policy);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE int64_t
cpp_process_dispatcher_get_timer_slack_policy_amount(const ProcessDispatcher* process) {
  return process->GetTimerSlackPolicy().amount();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_info_process_t
cpp_process_dispatcher_get_info(const ProcessDispatcher* process) {
  return process->GetInfo();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_process_dispatcher_set_critical_to_job(ProcessDispatcher* process,
                                                                         JobDispatcher* job,
                                                                         bool retcode_nonzero) {
  return process->SetCriticalToJob(fbl::ImportFromRawPtr(job), retcode_nonzero);
}

}  // extern "C"
