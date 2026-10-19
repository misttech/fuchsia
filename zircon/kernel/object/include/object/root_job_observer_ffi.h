// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_ROOT_JOB_OBSERVER_FFI_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_ROOT_JOB_OBSERVER_FFI_H_

#include <zircon/types.h>

class ProcessDispatcher;
class ThreadDispatcher;
class SignalObserver;

extern "C" {

// C++ helpers callable from Rust.
void cpp_root_job_signal_observer_init(void* storage, void* rust_ctx);
void cpp_root_job_signal_observer_destroy(SignalObserver* observer);
void cpp_start_root_job_observer();

// Test state check helpers.
bool cpp_test_process_is_dead(const ProcessDispatcher* process);
bool cpp_test_thread_is_dying_or_dead(const ThreadDispatcher* thread);
bool cpp_test_process_is_running(const ProcessDispatcher* process);

// Rust trampolines callable from C++.
void rust_root_job_observer_destroy(void* storage);

void rust_root_job_observer_on_match(void* rust_ctx, zx_signals_t signals);
void rust_root_job_observer_on_cancel(void* rust_ctx, zx_signals_t signals);

void rust_root_job_observer_set_critical_process_dying();
bool rust_root_job_observer_get_critical_process_dying();
void rust_root_job_observer_critical_process_kill(const ProcessDispatcher* dead_process);
void rust_root_job_observer_get_critical_process_name(char* out_name);
zx_koid_t rust_root_job_observer_get_critical_process_koid();

}  // extern "C"

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_ROOT_JOB_OBSERVER_FFI_H_
