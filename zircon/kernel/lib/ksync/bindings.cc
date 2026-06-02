// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <stddef.h>

#include <new>

#include <kernel/mutex.h>
#include <lockdep/lockdep.h>

#if WITH_LOCK_DEP
#if kSchedulerLockSpinTracingEnabled || kLockNameTracingEnabled
constexpr size_t kExpectedMutexSize = 40;
#else
constexpr size_t kExpectedMutexSize = 32;
#endif
#else
#if kSchedulerLockSpinTracingEnabled || kLockNameTracingEnabled
constexpr size_t kExpectedMutexSize = 32;
#else
constexpr size_t kExpectedMutexSize = 24;
#endif
#endif

#if WITH_LOCK_DEP
using SystemLockType = lockdep::LockDep<void, Mutex>;
#else
using SystemLockType = Mutex;
#endif
static_assert(sizeof(SystemLockType) == kExpectedMutexSize,
              "Rust KMutex size mismatch with C++ size");
static_assert(alignof(SystemLockType) == 8, "Mutex alignment must be exactly 8 bytes.");

static_assert(sizeof(lockdep::AcquiredLockEntry) == 40, "AcquiredLockEntry size mismatch");
static_assert(alignof(lockdep::AcquiredLockEntry) == 8,
              "AcquiredLockEntry alignment must be exactly 8 bytes.");

extern "C" {

void cpp_mutex_init(Mutex* mutex) { new (mutex) Mutex(); }

void cpp_mutex_destroy(Mutex* mutex) { mutex->~Mutex(); }

void cpp_mutex_acquire(Mutex* mutex, lockdep::LockClassId lcid,
                       void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (lcid != nullptr && entry_storage != nullptr) {
    auto* entry = new (entry_storage) lockdep::AcquiredLockEntry(mutex, lcid, 0);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsNone)->Acquire(entry);
  }
#endif
  mutex->Acquire();
}

void cpp_mutex_release(Mutex* mutex, void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsNone)->Release(entry);
    entry->~AcquiredLockEntry();
  }
#endif
  mutex->Release();
}

void cpp_critical_mutex_init(CriticalMutex* mutex) { new (mutex) CriticalMutex(); }

void cpp_critical_mutex_destroy(CriticalMutex* mutex) { mutex->~CriticalMutex(); }

bool cpp_critical_mutex_acquire(CriticalMutex* mutex, lockdep::LockClassId lcid,
                                void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (lcid != nullptr && entry_storage != nullptr) {
    auto* entry = new (entry_storage) lockdep::AcquiredLockEntry(mutex, lcid, 0);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsNone)->Acquire(entry);
  }
#endif
  auto should_clear = mutex->Acquire();
  return should_clear == CriticalMutex::ShouldClear::Yes;
}

void cpp_critical_mutex_release(CriticalMutex* mutex, void* entry_storage,
                                bool should_clear) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsNone)->Release(entry);
    entry->~AcquiredLockEntry();
  }
#endif
  mutex->Release(should_clear ? CriticalMutex::ShouldClear::Yes : CriticalMutex::ShouldClear::No);
}

}  // extern "C"
