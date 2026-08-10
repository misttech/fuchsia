// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/ffi.h>
#include <object/handle.h>
#include <object/job_dispatcher.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_job_dispatcher_is_root(const JobDispatcher* job) {
  return job == GetRootJobDispatcher().get();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_job_dispatcher_create(
    uint32_t flags, JobDispatcher* parent, ffi::Uninitialized<KernelHandle<JobDispatcher>>* handle,
    zx_rights_t* rights) {
  KernelHandle<JobDispatcher> new_handle;
  zx_status_t status =
      JobDispatcher::Create(flags, fbl::ImportFromRawPtr(parent), &new_handle, rights);
  if (status == ZX_OK) {
    handle->Initialize(ktl::move(new_handle));
  }
  return status;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_job_dispatcher_set_basic_policy_v1(
    JobDispatcher* job, uint32_t mode, const zx_policy_basic_v1_t* policy, size_t count) {
  return job->SetBasicPolicy(mode, policy, count);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_job_dispatcher_set_basic_policy_v2(
    JobDispatcher* job, uint32_t mode, const zx_policy_basic_v2_t* policy, size_t count) {
  return job->SetBasicPolicy(mode, policy, count);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_job_dispatcher_set_timer_slack_policy(
    JobDispatcher* job, const zx_policy_timer_slack_t* policy) {
  return job->SetTimerSlackPolicy(*policy);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE JobDispatcher* cpp_job_dispatcher_get_root_job() {
  auto root = GetRootJobDispatcher();
  return fbl::ExportToRawPtr(&root);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE JobDispatcher* cpp_job_dispatcher_parent(const JobDispatcher* job) {
  auto parent = const_cast<JobDispatcher*>(job)->parent();
  return fbl::ExportToRawPtr(&parent);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_job_dispatcher_max_height(const JobDispatcher* job) {
  return job->max_height();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_job_dispatcher_kill(JobDispatcher* job, int64_t return_code) {
  return job->Kill(return_code);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_job_dispatcher_set_kill_on_oom(JobDispatcher* job, bool value) {
  job->set_kill_on_oom(value);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_job_dispatcher_get_kill_on_oom(const JobDispatcher* job) {
  return job->get_kill_on_oom();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_job_dispatcher_kill_job_with_kill_on_oom(JobDispatcher* job) {
  return job->KillJobWithKillOnOOM();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_job_dispatcher_get_info(const JobDispatcher* job,
                                                   zx_info_job_t* info_out) {
  *info_out = job->GetInfo();
}

}  // extern "C"
