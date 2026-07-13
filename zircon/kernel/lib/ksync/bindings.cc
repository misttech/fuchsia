// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <stddef.h>

#include <new>

#include <kernel/brwlock.h>
#include <kernel/event.h>
#include <kernel/mutex.h>
#include <kernel/spinlock.h>
#include <lockdep/lockdep.h>

#ifdef WITH_LOCK_DEP
constexpr bool kWithLockDep = true;
#else
constexpr bool kWithLockDep = false;
#endif

constexpr size_t kExpectedMutexSize =
    kWithLockDep ? ((kSchedulerLockSpinTracingEnabled || kLockNameTracingEnabled) ? 40 : 32)
                 : ((kSchedulerLockSpinTracingEnabled || kLockNameTracingEnabled) ? 32 : 24);

#if WITH_LOCK_DEP
using SystemLockType = lockdep::LockDep<void, Mutex>;
#else
using SystemLockType = Mutex;
#endif
static_assert(sizeof(SystemLockType) == kExpectedMutexSize,
              "Rust KMutex size mismatch with C++ size");
static_assert(alignof(SystemLockType) == 8, "Mutex alignment must be exactly 8 bytes.");

#if WITH_LOCK_DEP
using SystemSpinlockType = lockdep::LockDep<void, SpinLock>;
#else
using SystemSpinlockType = SpinLock;
#endif

constexpr size_t kExpectedSpinlockSize = kWithLockDep ? (kSchedulerLockSpinTracingEnabled ? 24 : 16)
                                                      : (kSchedulerLockSpinTracingEnabled ? 16 : 4);

static_assert(sizeof(SystemSpinlockType) == kExpectedSpinlockSize,
              "Rust KSpinlock size mismatch with C++ size");

constexpr size_t kExpectedSpinlockAlign =
    (kWithLockDep || kSchedulerLockSpinTracingEnabled) ? 8 : 4;
static_assert(alignof(SystemSpinlockType) == kExpectedSpinlockAlign,
              "Rust KSpinlock alignment mismatch with C++ alignment");

static_assert(sizeof(Event) == 72, "Rust KEvent size mismatch with C++ size");
static_assert(alignof(Event) == 8, "Rust KEvent alignment mismatch with C++ alignment");

static_assert(sizeof(lockdep::AcquiredLockEntry) == 40, "AcquiredLockEntry size mismatch");
static_assert(alignof(lockdep::AcquiredLockEntry) == 8,
              "AcquiredLockEntry alignment must be exactly 8 bytes.");

#if defined(__x86_64__)
constexpr size_t kExpectedInterruptSavedStateSize = 8;
constexpr size_t kExpectedInterruptSavedStateAlign = 8;
#else
constexpr size_t kExpectedInterruptSavedStateSize = 1;
constexpr size_t kExpectedInterruptSavedStateAlign = 1;
#endif

static_assert(sizeof(interrupt_saved_state_t) == kExpectedInterruptSavedStateSize,
              "Rust InterruptSavedState size mismatch with C++");
static_assert(alignof(interrupt_saved_state_t) == kExpectedInterruptSavedStateAlign,
              "Rust InterruptSavedState alignment mismatch with C++");
#if WITH_LOCK_DEP
using SystemBrwLockType = lockdep::LockDep<void, BrwLockPi, lockdep::LockFlagsMultiAcquire>;
#else
using SystemBrwLockType = BrwLockPi;
#endif

#if defined(__riscv)
constexpr size_t kExpectedBrwLockSize = kWithLockDep ? 80 : 72;
constexpr size_t kExpectedBrwLockAlign = 8;
#else
constexpr size_t kExpectedBrwLockSize = kWithLockDep ? 144 : 128;
constexpr size_t kExpectedBrwLockAlign = 16;
#endif

static_assert(sizeof(SystemBrwLockType) == kExpectedBrwLockSize, "SystemBrwLockType size mismatch");
static_assert(alignof(SystemBrwLockType) == kExpectedBrwLockAlign,
              "SystemBrwLockType alignment mismatch");

extern "C" {

void cpp_mutex_init(Mutex* mutex);
void cpp_mutex_destroy(Mutex* mutex);
void cpp_mutex_acquire(Mutex* mutex, lockdep::LockClassId lcid, void* entry_storage);
void cpp_mutex_release(Mutex* mutex, void* entry_storage);
void cpp_critical_mutex_init(CriticalMutex* mutex);
void cpp_critical_mutex_destroy(CriticalMutex* mutex);
bool cpp_critical_mutex_acquire(CriticalMutex* mutex, lockdep::LockClassId lcid,
                                void* entry_storage);
void cpp_critical_mutex_release(CriticalMutex* mutex, void* entry_storage, bool should_clear);
void cpp_spinlock_init(SpinLock* lock);
void cpp_spinlock_destroy(SpinLock* lock);
interrupt_saved_state_t cpp_spinlock_acquire_irqsave(SpinLock* lock, lockdep::LockClassId lcid,
                                                     void* entry_storage);
void cpp_spinlock_release_irqrestore(SpinLock* lock, void* entry_storage,
                                     interrupt_saved_state_t state);
void cpp_event_init(Event* event, bool initial);
void cpp_event_destroy(Event* event);
void cpp_event_signal(Event* event, zx_status_t wait_result);
void cpp_event_unsignal(Event* event);
zx_status_t cpp_event_wait(Event* event, zx_instant_mono_t deadline);
void cpp_brwlock_pi_init(SystemBrwLockType* lock);
void cpp_brwlock_pi_destroy(SystemBrwLockType* lock);
void cpp_brwlock_pi_acquire_read(SystemBrwLockType* lock, lockdep::LockClassId lcid,
                                 void* entry_storage);
void cpp_brwlock_pi_release_read(SystemBrwLockType* lock, void* entry_storage);
void cpp_brwlock_pi_acquire_write(SystemBrwLockType* lock, lockdep::LockClassId lcid,
                                  void* entry_storage);
void cpp_brwlock_pi_release_write(SystemBrwLockType* lock, void* entry_storage);

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

void cpp_spinlock_init(SpinLock* lock) { new (lock) SpinLock(); }

void cpp_spinlock_destroy(SpinLock* lock) { lock->~SpinLock(); }

interrupt_saved_state_t cpp_spinlock_acquire_irqsave(
    SpinLock* lock, lockdep::LockClassId lcid, void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
  interrupt_saved_state_t state = arch_interrupt_save();
#if WITH_LOCK_DEP
  if (lcid != nullptr && entry_storage != nullptr) {
    auto* entry = new (entry_storage) lockdep::AcquiredLockEntry(lock, lcid, 0);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsIrqSafe)->Acquire(entry);
  }
#endif
  lock->Acquire();
  return state;
}

void cpp_spinlock_release_irqrestore(SpinLock* lock, void* entry_storage,
                                     interrupt_saved_state_t state) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsIrqSafe)->Release(entry);
    entry->~AcquiredLockEntry();
  }
#endif
  lock->ReleaseIrqRestore(state);
}

void cpp_event_init(Event* event, bool initial) { new (event) Event(initial); }

void cpp_event_destroy(Event* event) { event->~Event(); }

void cpp_event_signal(Event* event, zx_status_t wait_result) { event->Signal(wait_result); }

void cpp_event_unsignal(Event* event) { event->Unsignal(); }

zx_status_t cpp_event_wait(Event* event, zx_instant_mono_t deadline) {
  return event->Wait(Deadline::no_slack(deadline));
}

void cpp_brwlock_pi_init(SystemBrwLockType* lock) { new (lock) SystemBrwLockType(); }

void cpp_brwlock_pi_destroy(SystemBrwLockType* lock) { lock->~SystemBrwLockType(); }

void cpp_brwlock_pi_acquire_read(SystemBrwLockType* lock, lockdep::LockClassId lcid,
                                 void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (lcid != nullptr && entry_storage != nullptr) {
    auto* entry = new (entry_storage) lockdep::AcquiredLockEntry(lock, lcid, 0);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsMultiAcquire)->Acquire(entry);
  }
  lock->lock().ReadAcquire();
#else
  lock->ReadAcquire();
#endif
}

void cpp_brwlock_pi_release_read(SystemBrwLockType* lock,
                                 void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsMultiAcquire)->Release(entry);
    entry->~AcquiredLockEntry();
  }
  lock->lock().ReadRelease();
#else
  lock->ReadRelease();
#endif
}

void cpp_brwlock_pi_acquire_write(SystemBrwLockType* lock, lockdep::LockClassId lcid,
                                  void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (lcid != nullptr && entry_storage != nullptr) {
    auto* entry = new (entry_storage) lockdep::AcquiredLockEntry(lock, lcid, 0);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsMultiAcquire)->Acquire(entry);
  }
  lock->lock().WriteAcquire();
#else
  lock->WriteAcquire();
#endif
}

void cpp_brwlock_pi_release_write(SystemBrwLockType* lock,
                                  void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsMultiAcquire)->Release(entry);
    entry->~AcquiredLockEntry();
  }
  lock->lock().WriteRelease();
#else
  lock->WriteRelease();
#endif
}

}  // extern "C"
