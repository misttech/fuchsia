// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_ROOT_JOB_OBSERVER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_ROOT_JOB_OBSERVER_H_

#include <zircon/assert.h>

#include <fbl/ref_ptr.h>

class ProcessDispatcher;

extern "C" {
// TODO(adanis): Remove these critical-process shims once remaining C++ callers are ported to Rust.
void rust_root_job_observer_set_critical_process_dying();
bool rust_root_job_observer_get_critical_process_dying();
void rust_root_job_observer_critical_process_kill(const ProcessDispatcher& dead_process);
}

// TODO(adanis): Remove once remaining C++ callers (`bti.cc`, `job_dispatcher.cc`,
// `process_dispatcher.cc`) are ported to Rust.
class RootJobObserver final {
 public:
  RootJobObserver() = delete;

  // Record that any critical process is in some stage of being torn down.
  static void SetCriticalProcessDying() { rust_root_job_observer_set_critical_process_dying(); }
  static bool GetCriticalProcessDying() {
    return rust_root_job_observer_get_critical_process_dying();
  }
  // Record the dead process responsible for getting the root job killed.
  static void CriticalProcessKill(const fbl::RefPtr<ProcessDispatcher>& dead_process) {
    DEBUG_ASSERT(dead_process != nullptr);
    rust_root_job_observer_critical_process_kill(*dead_process);
  }
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_ROOT_JOB_OBSERVER_H_
