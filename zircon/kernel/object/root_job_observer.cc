// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/ffi.h>
#include <object/root_job_observer.h>
#include <object/root_job_observer_ffi.h>

static_assert(sizeof(RootJobObserver) == kRootJobObserverStorageSize);
static_assert(alignof(RootJobObserver) == kRootJobObserverStorageAlign);

RootJobObserver::~RootJobObserver() { rust_root_job_observer_destroy(&opaque_storage_); }

void RootJobObserver::SetCriticalProcessDying() {
  rust_root_job_observer_set_critical_process_dying();
}

bool RootJobObserver::GetCriticalProcessDying() {
  return rust_root_job_observer_get_critical_process_dying();
}

void RootJobObserver::CriticalProcessKill(fbl::RefPtr<ProcessDispatcher> dead_process) {
  rust_root_job_observer_critical_process_kill(dead_process.get());
}

ktl::array<char, ZX_MAX_NAME_LEN> RootJobObserver::GetCriticalProcessName() {
  ktl::array<char, ZX_MAX_NAME_LEN> name{};
  rust_root_job_observer_get_critical_process_name(name.data());
  return name;
}

zx_koid_t RootJobObserver::GetCriticalProcessKoid() {
  return rust_root_job_observer_get_critical_process_koid();
}
