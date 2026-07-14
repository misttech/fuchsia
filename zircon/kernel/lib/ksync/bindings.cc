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

#if WITH_LOCK_DEP
template <typename T>
using LockPtr = lockdep::Lock<T>*;

template <typename T>
struct LockInitHelper : public lockdep::Lock<T> {
  LockInitHelper(lockdep::LockClassId id) : lockdep::Lock<T>(id) {}
};
#else
template <typename T>
using LockPtr = T*;
#endif

extern "C" {

void cpp_mutex_init(LockPtr<Mutex> mutex, const void* class_id);
void cpp_mutex_destroy(LockPtr<Mutex> mutex);
void cpp_mutex_acquire(LockPtr<Mutex> lock, void* entry_storage);
void cpp_mutex_release(LockPtr<Mutex> lock, void* entry_storage);
void cpp_critical_mutex_init(LockPtr<CriticalMutex> mutex, const void* class_id);
void cpp_critical_mutex_destroy(LockPtr<CriticalMutex> mutex);
bool cpp_critical_mutex_acquire(LockPtr<CriticalMutex> lock, void* entry_storage);
void cpp_critical_mutex_release(LockPtr<CriticalMutex> lock, void* entry_storage,
                                bool should_clear);
void cpp_spinlock_init(LockPtr<SpinLock> lock, const void* class_id);
void cpp_spinlock_destroy(LockPtr<SpinLock> lock);
interrupt_saved_state_t cpp_spinlock_acquire_irqsave(LockPtr<SpinLock> lock, void* entry_storage);
void cpp_spinlock_release_irqrestore(LockPtr<SpinLock> lock, void* entry_storage,
                                     interrupt_saved_state_t state);
void cpp_event_init(Event* event, bool initial);
void cpp_event_destroy(Event* event);
void cpp_event_signal(Event* event, zx_status_t wait_result);
void cpp_event_unsignal(Event* event);
zx_status_t cpp_event_wait(Event* event, zx_instant_mono_t deadline);
void cpp_brwlock_pi_init(LockPtr<BrwLockPi> lock, const void* class_id);
void cpp_brwlock_pi_destroy(LockPtr<BrwLockPi> lock);
void cpp_brwlock_pi_acquire_read(LockPtr<BrwLockPi> lock, void* entry_storage);
void cpp_brwlock_pi_release_read(LockPtr<BrwLockPi> lock, void* entry_storage);
void cpp_brwlock_pi_acquire_write(LockPtr<BrwLockPi> lock, void* entry_storage);
void cpp_brwlock_pi_release_write(LockPtr<BrwLockPi> lock, void* entry_storage);

void cpp_mutex_init(LockPtr<Mutex> lock, const void* class_id) {
#if WITH_LOCK_DEP
  new (lock) LockInitHelper<Mutex>(reinterpret_cast<lockdep::LockClassId>(class_id));
#else
  new (lock) Mutex();
#endif
}

void cpp_mutex_destroy(LockPtr<Mutex> lock) {
#if WITH_LOCK_DEP
  using LockType = lockdep::Lock<Mutex>;
  lock->~LockType();
#else
  lock->~Mutex();
#endif
}

void cpp_mutex_acquire(LockPtr<Mutex> lock, void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = new (entry_storage) lockdep::AcquiredLockEntry(&lock->lock(), lock->id(), 0);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsNone)->Acquire(entry);
  }
  lock->lock().Acquire();
#else
  lock->Acquire();
#endif
}

void cpp_mutex_release(LockPtr<Mutex> lock, void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsNone)->Release(entry);
    entry->~AcquiredLockEntry();
  }
  lock->lock().Release();
#else
  lock->Release();
#endif
}

void cpp_critical_mutex_init(LockPtr<CriticalMutex> lock, const void* class_id) {
#if WITH_LOCK_DEP
  new (lock) LockInitHelper<CriticalMutex>(reinterpret_cast<lockdep::LockClassId>(class_id));
#else
  new (lock) CriticalMutex();
#endif
}

void cpp_critical_mutex_destroy(LockPtr<CriticalMutex> lock) {
#if WITH_LOCK_DEP
  using LockType = lockdep::Lock<CriticalMutex>;
  lock->~LockType();
#else
  lock->~CriticalMutex();
#endif
}

bool cpp_critical_mutex_acquire(LockPtr<CriticalMutex> lock,
                                void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = new (entry_storage) lockdep::AcquiredLockEntry(&lock->lock(), lock->id(), 0);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsNone)->Acquire(entry);
  }
  auto should_clear = lock->lock().Acquire();
#else
  auto should_clear = lock->Acquire();
#endif
  return should_clear == CriticalMutex::ShouldClear::Yes;
}

void cpp_critical_mutex_release(LockPtr<CriticalMutex> lock, void* entry_storage,
                                bool should_clear) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsNone)->Release(entry);
    entry->~AcquiredLockEntry();
  }
  lock->lock().Release(should_clear ? CriticalMutex::ShouldClear::Yes
                                    : CriticalMutex::ShouldClear::No);
#else
  lock->Release(should_clear ? CriticalMutex::ShouldClear::Yes : CriticalMutex::ShouldClear::No);
#endif
}

void cpp_spinlock_init(LockPtr<SpinLock> lock, const void* class_id) {
#if WITH_LOCK_DEP
  new (lock) LockInitHelper<SpinLock>(reinterpret_cast<lockdep::LockClassId>(class_id));
#else
  new (lock) SpinLock();
#endif
}

void cpp_spinlock_destroy(LockPtr<SpinLock> lock) {
#if WITH_LOCK_DEP
  using LockType = lockdep::Lock<SpinLock>;
  lock->~LockType();
#else
  lock->~SpinLock();
#endif
}

interrupt_saved_state_t cpp_spinlock_acquire_irqsave(LockPtr<SpinLock> lock, void* entry_storage)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  interrupt_saved_state_t state = arch_interrupt_save();
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = new (entry_storage) lockdep::AcquiredLockEntry(&lock->lock(), lock->id(), 0);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsIrqSafe)->Acquire(entry);
  }
  lock->lock().Acquire();
#else
  lock->Acquire();
#endif
  return state;
}

void cpp_spinlock_release_irqrestore(LockPtr<SpinLock> lock, void* entry_storage,
                                     interrupt_saved_state_t state) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsIrqSafe)->Release(entry);
    entry->~AcquiredLockEntry();
  }
  lock->lock().ReleaseIrqRestore(state);
#else
  lock->ReleaseIrqRestore(state);
#endif
}

void cpp_event_init(Event* event, bool initial) { new (event) Event(initial); }

void cpp_event_destroy(Event* event) { event->~Event(); }

void cpp_event_signal(Event* event, zx_status_t wait_result) { event->Signal(wait_result); }

void cpp_event_unsignal(Event* event) { event->Unsignal(); }

zx_status_t cpp_event_wait(Event* event, zx_instant_mono_t deadline) {
  return event->Wait(Deadline::no_slack(deadline));
}

void cpp_brwlock_pi_init(LockPtr<BrwLockPi> lock, const void* class_id) {
#if WITH_LOCK_DEP
  new (lock) LockInitHelper<BrwLockPi>(reinterpret_cast<lockdep::LockClassId>(class_id));
#else
  new (lock) BrwLockPi();
#endif
}

void cpp_brwlock_pi_destroy(LockPtr<BrwLockPi> lock) {
#if WITH_LOCK_DEP
  using LockType = lockdep::Lock<BrwLockPi>;
  lock->~LockType();
#else
  lock->~BrwLockPi();
#endif
}

void cpp_brwlock_pi_acquire_read(LockPtr<BrwLockPi> lock,
                                 void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = new (entry_storage) lockdep::AcquiredLockEntry(&lock->lock(), lock->id(), 0);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsMultiAcquire)->Acquire(entry);
  }
  lock->lock().ReadAcquire();
#else
  lock->ReadAcquire();
#endif
}

void cpp_brwlock_pi_release_read(LockPtr<BrwLockPi> lock,
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

void cpp_brwlock_pi_acquire_write(LockPtr<BrwLockPi> lock,
                                  void* entry_storage) TA_NO_THREAD_SAFETY_ANALYSIS {
#if WITH_LOCK_DEP
  if (entry_storage != nullptr) {
    auto* entry = new (entry_storage) lockdep::AcquiredLockEntry(&lock->lock(), lock->id(), 0);
    lockdep::ThreadLockState::Get(lockdep::LockFlagsMultiAcquire)->Acquire(entry);
  }
  lock->lock().WriteAcquire();
#else
  lock->WriteAcquire();
#endif
}

void cpp_brwlock_pi_release_write(LockPtr<BrwLockPi> lock,
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
