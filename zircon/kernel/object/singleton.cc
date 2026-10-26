// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// This file defines:
// * Initialization code for kernel/object module
// * Singleton instances

#include <kernel/ffi.h>
#include <lk/init.h>
#include <object/executor.h>
#include <object/job_dispatcher.h>
#include <object/memory_watchdog.h>

extern "C" void rust_executor_start_root_job_observer(Executor* executor);

static Executor gExecutor;

const fbl::RefPtr<JobDispatcher>& GetRootJobDispatcher() {
  return gExecutor.GetRootJobDispatcher();
}

const Handle& GetRootJobHandle() { return gExecutor.GetRootJobHandle(); }

MemoryWatchdog& GetMemoryWatchdog() { return gExecutor.GetMemoryWatchdog(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE void cpp_start_root_job_observer() {
  rust_executor_start_root_job_observer(&gExecutor);
}

static void object_init(uint level) TA_NO_THREAD_SAFETY_ANALYSIS {
  Handle::Init();

  gExecutor.Init();
}

LK_INIT_HOOK(libobject, object_init, LK_INIT_LEVEL_THREADING)
