// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stddef.h>

#include <new>

#include <kernel/brwlock.h>
#include <kernel/ffi.h>
#include <kernel/lock_validation_guard.h>
#include <kernel/mutex.h>
#include <kernel/spinlock.h>
#include <lockdep/lockdep.h>

class OwnedWaitQueue;

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

#if WITH_LOCK_DEP
using SystemMonitoredSpinlockType = lockdep::LockDep<void, MonitoredSpinLock>;
#else
using SystemMonitoredSpinlockType = MonitoredSpinLock;
#endif

static_assert(sizeof(SystemMonitoredSpinlockType) == kExpectedSpinlockSize,
              "Rust KMonitoredSpinlock size mismatch with C++ size");
static_assert(alignof(SystemMonitoredSpinlockType) == kExpectedSpinlockAlign,
              "Rust KMonitoredSpinlock alignment mismatch with C++ alignment");

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

interrupt_saved_state_t cpp_ksync_interrupt_save();
void cpp_mutex_destroy(Mutex* lock);
void cpp_mutex_acquire(Mutex* lock);
void cpp_mutex_release(Mutex* lock);
bool cpp_mutex_is_contested(Mutex* lock);
bool cpp_critical_mutex_acquire(CriticalMutex* lock);
void cpp_critical_mutex_release(CriticalMutex* lock, bool should_clear);
void cpp_spinlock_init(SpinLock* lock, const void* class_id);
void cpp_spinlock_destroy(SpinLock* lock);
interrupt_saved_state_t cpp_spinlock_acquire_irqsave(SpinLock* lock);
void cpp_spinlock_release_irqrestore(SpinLock* lock, interrupt_saved_state_t state);
void cpp_spinlock_acquire_no_irqsave(SpinLock* lock);
void cpp_spinlock_release_no_irqrestore(SpinLock* lock);
void cpp_monitored_spinlock_init(MonitoredSpinLock* lock, const void* class_id);
void cpp_monitored_spinlock_destroy(MonitoredSpinLock* lock);
interrupt_saved_state_t cpp_monitored_spinlock_acquire_irqsave(MonitoredSpinLock* lock,
                                                               const char* name);
void cpp_monitored_spinlock_release_irqrestore(MonitoredSpinLock* lock,
                                               interrupt_saved_state_t state);
void cpp_monitored_spinlock_acquire_no_irqsave(MonitoredSpinLock* lock, const char* name);
void cpp_monitored_spinlock_release_no_irqrestore(MonitoredSpinLock* lock);
void cpp_brwlock_pi_init(BrwLockPi* lock, const void* class_id);
void cpp_brwlock_pi_destroy(BrwLockPi* lock);
void cpp_brwlock_pi_acquire_read(BrwLockPi* lock);
void cpp_brwlock_pi_release_read(BrwLockPi* lock);
void cpp_brwlock_pi_acquire_write(BrwLockPi* lock);
void cpp_brwlock_pi_release_write(BrwLockPi* lock);
void cpp_lock_dep_acquire(void* lock, const void* class_id, void* entry_storage,
                          lockdep::LockFlags flags);
void cpp_lock_dep_release(void* entry_storage, lockdep::LockFlags flags);
void cpp_lock_validate_release(void* entry_storage, lockdep::LockFlags flags);
void cpp_lock_validate_acquire(void* entry_storage, lockdep::LockFlags flags);

FFI_ALWAYS_INLINE interrupt_saved_state_t cpp_ksync_interrupt_save() {
  return arch_interrupt_save();
}

FFI_ALWAYS_INLINE void cpp_mutex_destroy(Mutex* lock) { lock->~Mutex(); }

FFI_ALWAYS_INLINE void cpp_mutex_acquire(Mutex* lock) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->Acquire();
}

FFI_ALWAYS_INLINE void cpp_mutex_release(Mutex* lock) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->Release();
}

FFI_ALWAYS_INLINE bool cpp_mutex_is_contested(Mutex* lock) { return lock->IsContested(); }

FFI_ALWAYS_INLINE bool cpp_critical_mutex_acquire(CriticalMutex* lock)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  auto should_clear = lock->Acquire();
  return should_clear == CriticalMutex::ShouldClear::Yes;
}

FFI_ALWAYS_INLINE void cpp_critical_mutex_release(CriticalMutex* lock,
                                                  bool should_clear) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->Release(should_clear ? CriticalMutex::ShouldClear::Yes : CriticalMutex::ShouldClear::No);
}

FFI_ALWAYS_INLINE void cpp_spinlock_init(SpinLock* lock, const void* class_id) {
  new (lock) SpinLock();
  lock->SetLockClassId(reinterpret_cast<lockdep::LockClassId>(class_id));
}

FFI_ALWAYS_INLINE void cpp_spinlock_destroy(SpinLock* lock) { lock->~SpinLock(); }

FFI_ALWAYS_INLINE interrupt_saved_state_t cpp_spinlock_acquire_irqsave(SpinLock* lock)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  interrupt_saved_state_t state;
  lock->AcquireIrqSave(state);
  return state;
}

FFI_ALWAYS_INLINE void cpp_spinlock_release_irqrestore(
    SpinLock* lock, interrupt_saved_state_t state) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->ReleaseIrqRestore(state);
}

FFI_ALWAYS_INLINE void cpp_spinlock_acquire_no_irqsave(SpinLock* lock)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->Acquire();
}

FFI_ALWAYS_INLINE void cpp_spinlock_release_no_irqrestore(SpinLock* lock)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->Release();
}

FFI_ALWAYS_INLINE void cpp_monitored_spinlock_init(MonitoredSpinLock* lock, const void* class_id) {
  new (lock) MonitoredSpinLock();
  lock->SetLockClassId(reinterpret_cast<lockdep::LockClassId>(class_id));
}

FFI_ALWAYS_INLINE void cpp_monitored_spinlock_destroy(MonitoredSpinLock* lock) {
  lock->~MonitoredSpinLock();
}

FFI_ALWAYS_INLINE interrupt_saved_state_t cpp_monitored_spinlock_acquire_irqsave(
    MonitoredSpinLock* lock, const char* name) TA_NO_THREAD_SAFETY_ANALYSIS {
  interrupt_saved_state_t state;
  lock->AcquireIrqSave(state, name);
  return state;
}

FFI_ALWAYS_INLINE void cpp_monitored_spinlock_release_irqrestore(
    MonitoredSpinLock* lock, interrupt_saved_state_t state) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->ReleaseIrqRestore(state);
}

FFI_ALWAYS_INLINE void cpp_monitored_spinlock_acquire_no_irqsave(
    MonitoredSpinLock* lock, const char* name) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->Acquire(name);
}

FFI_ALWAYS_INLINE void cpp_monitored_spinlock_release_no_irqrestore(MonitoredSpinLock* lock)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->Release();
}

FFI_ALWAYS_INLINE void cpp_brwlock_pi_init(BrwLockPi* lock, const void* class_id) {
  new (lock) BrwLockPi();
  lock->SetLockClassId(reinterpret_cast<lockdep::LockClassId>(class_id));
}

FFI_ALWAYS_INLINE void cpp_brwlock_pi_destroy(BrwLockPi* lock) { lock->~BrwLockPi(); }

FFI_ALWAYS_INLINE void cpp_brwlock_pi_acquire_read(BrwLockPi* lock) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->ReadAcquire();
}

FFI_ALWAYS_INLINE void cpp_brwlock_pi_release_read(BrwLockPi* lock) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->ReadRelease();
}

FFI_ALWAYS_INLINE void cpp_brwlock_pi_acquire_write(BrwLockPi* lock) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->WriteAcquire();
}

FFI_ALWAYS_INLINE void cpp_brwlock_pi_release_write(BrwLockPi* lock) TA_NO_THREAD_SAFETY_ANALYSIS {
  lock->WriteRelease();
}

FFI_ALWAYS_INLINE void cpp_lock_dep_acquire(void* lock, const void* class_id, void* entry_storage,
                                            lockdep::LockFlags flags) {
#if WITH_LOCK_DEP
  if constexpr (lockdep::kLockValidationEnabled) {
    if (entry_storage != nullptr) {
      LockValidationGuard guard;
      auto* entry = new (entry_storage)
          lockdep::AcquiredLockEntry(lock, reinterpret_cast<lockdep::LockClassId>(class_id), 0);
      if (class_id != nullptr) {
        lockdep::ThreadLockState::Get(flags)->Acquire(entry);
      }
    }
  }
#endif
}

FFI_ALWAYS_INLINE void cpp_lock_dep_release(void* entry_storage, lockdep::LockFlags flags) {
#if WITH_LOCK_DEP
  if constexpr (lockdep::kLockValidationEnabled) {
    if (entry_storage != nullptr) {
      LockValidationGuard guard;
      auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
      if (entry->id() != lockdep::kInvalidLockClassId) {
        lockdep::ThreadLockState::Get(flags)->Release(entry);
      }
      entry->~AcquiredLockEntry();
    }
  }
#endif
}

FFI_ALWAYS_INLINE void cpp_lock_validate_release(void* entry_storage, lockdep::LockFlags flags) {
#if WITH_LOCK_DEP
  if constexpr (lockdep::kLockValidationEnabled) {
    if (entry_storage != nullptr) {
      LockValidationGuard guard;
      auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
      if (entry->id() != lockdep::kInvalidLockClassId) {
        lockdep::ThreadLockState::Get(flags)->Release(entry);
      }
    }
  }
#endif
}

FFI_ALWAYS_INLINE void cpp_lock_validate_acquire(void* entry_storage, lockdep::LockFlags flags) {
#if WITH_LOCK_DEP
  if constexpr (lockdep::kLockValidationEnabled) {
    if (entry_storage != nullptr) {
      LockValidationGuard guard;
      auto* entry = static_cast<lockdep::AcquiredLockEntry*>(entry_storage);
      if (entry->id() != lockdep::kInvalidLockClassId) {
        lockdep::ThreadLockState::Get(flags)->Acquire(entry);
      }
    }
  }
#endif
}

}  // extern "C"
