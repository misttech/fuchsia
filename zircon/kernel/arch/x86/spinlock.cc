// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/zircon-internal/thread_annotations.h>
#include <stdint.h>

#include <arch/spinlock.h>
#include <kernel/cpu.h>
#include <kernel/ffi.h>
#include <kernel/spin_tracing_config.h>
#include <ktl/atomic.h>

// Implemented in Rust (src/spinlock.rs). Each takes the lock's |value|, and has no thread
// safety annotations of its own: only the arch_spin_* functions below call them.
extern "C" {

void rust_arch_spin_lock_non_instrumented(ktl::atomic<cpu_num_t>* lock);
void rust_arch_spin_lock_trace_instrumented(ktl::atomic<cpu_num_t>* lock, uint64_t encoded_lock_id);
// Returns false if the lock was acquired, as arch_spin_trylock() does.
bool rust_arch_spin_trylock(ktl::atomic<cpu_num_t>* lock);
void rust_arch_spin_unlock(ktl::atomic<cpu_num_t>* lock);

}  // extern "C"

// Make sure the spin_lock is functionally a uint32_t since the Rust code operates on its value
// as an AtomicU32.
static_assert(sizeof(arch_spin_lock_t) == sizeof(cpu_num_t));
static_assert(sizeof(arch_spin_lock_t) == sizeof(uint32_t));
static_assert(alignof(arch_spin_lock_t) == alignof(uint32_t));

namespace {

// Called right after a Rust call that acquires |lock|, to tell the thread safety analysis so.
void assert_lock_held(arch_spin_lock_t* lock) TA_ASSERT(lock) {}

}  // namespace

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void arch_spin_lock_non_instrumented(arch_spin_lock_t* lock) TA_ACQ(lock) {
  rust_arch_spin_lock_non_instrumented(&lock->value);
  assert_lock_held(lock);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void arch_spin_lock_trace_instrumented(
    arch_spin_lock_t* lock, spin_tracing::EncodedLockId encoded_lock_id) TA_ACQ(lock) {
  rust_arch_spin_lock_trace_instrumented(&lock->value, encoded_lock_id.value());
  assert_lock_held(lock);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool arch_spin_trylock(arch_spin_lock_t* lock) TA_NO_THREAD_SAFETY_ANALYSIS {
  return rust_arch_spin_trylock(&lock->value);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void arch_spin_unlock(arch_spin_lock_t* lock) TA_NO_THREAD_SAFETY_ANALYSIS {
  rust_arch_spin_unlock(&lock->value);
}
