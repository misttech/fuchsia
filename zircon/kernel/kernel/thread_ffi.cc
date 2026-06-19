// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <assert.h>
#include <lib/kconcurrent/chainlock.h>
#include <lib/kconcurrent/chainlock_transaction.h>
#include <zircon/types.h>

#include <kernel/thread.h>

extern "C" {

void* rust_thread_create_default(const char* name, thread_start_routine entry, void* arg) {
  return Thread::Create(name, entry, arg, DEFAULT_PRIORITY);
}

void rust_thread_resume(void* thread) {
  DEBUG_ASSERT(thread != nullptr);
  static_cast<Thread*>(thread)->Resume();
}

zx_status_t rust_thread_join(void* thread, int* out_retcode, zx_instant_mono_t deadline) {
  DEBUG_ASSERT(thread != nullptr);
  return static_cast<Thread*>(thread)->Join(out_retcode, deadline);
}

void rust_thread_yield() { Thread::Current::Yield(); }

void rust_thread_kill(void* thread) {
  DEBUG_ASSERT(thread != nullptr);
  static_cast<Thread*>(thread)->Kill();
}

bool rust_thread_is_blocked(void* thread) {
  DEBUG_ASSERT(thread != nullptr);
  Thread* t = static_cast<Thread*>(thread);
  SingleChainLockGuard guard{IrqSaveOption, t->get_lock(), CLT_TAG("rust_thread_is_blocked")};
  return t->state() == THREAD_BLOCKED || t->state() == THREAD_BLOCKED_READ_LOCK;
}

}  // extern "C"
