// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::lockdep_tracked::LockdepTracked;
use crate::raw_lock::{ConstRawLock, ExclusiveLockPolicy, LockPolicy, RawLock};
use core::ffi::c_void;
use pin_init::{PinInit, pin_data, pin_init_from_closure};

#[cfg(any(feature = "lock_name_tracing", feature = "spin_lock_tracing"))]
const RAW_MUTEX_SIZE: usize = 32;
#[cfg(not(any(feature = "lock_name_tracing", feature = "spin_lock_tracing")))]
const RAW_MUTEX_SIZE: usize = 24;

unsafe extern "C" {
    fn cpp_mutex_destroy(lock: *mut c_void);
    fn cpp_mutex_acquire(lock: *mut c_void);
    fn cpp_mutex_release(lock: *mut c_void);
    fn cpp_mutex_is_contested(lock: *mut c_void) -> bool;
    fn cpp_mutex_is_held(lock: *const c_void) -> bool;

    fn cpp_critical_mutex_acquire(lock: *mut c_void) -> bool;
    fn cpp_critical_mutex_release(lock: *mut c_void, should_clear: bool);
}

const MUTEX_MAGIC: u32 = 0x6D757478; // 'mutx'
const INVALID_CPU: u32 = u32::MAX;

const fn make_mutex_storage() -> [u8; RAW_MUTEX_SIZE] {
    let mut bytes = [0u8; RAW_MUTEX_SIZE];
    #[cfg(any(feature = "lock_name_tracing", feature = "spin_lock_tracing"))]
    let offset = 8;
    #[cfg(not(any(feature = "lock_name_tracing", feature = "spin_lock_tracing")))]
    let offset = 0;

    let magic_bytes = MUTEX_MAGIC.to_ne_bytes();
    let cpu_bytes = INVALID_CPU.to_ne_bytes();

    bytes[offset] = magic_bytes[0];
    bytes[offset + 1] = magic_bytes[1];
    bytes[offset + 2] = magic_bytes[2];
    bytes[offset + 3] = magic_bytes[3];

    bytes[offset + 4] = cpu_bytes[0];
    bytes[offset + 5] = cpu_bytes[1];
    bytes[offset + 6] = cpu_bytes[2];
    bytes[offset + 7] = cpu_bytes[3];

    bytes
}

/// Opaque layout block matching the Zircon C++ `Mutex` (and `CriticalMutex`) object.
#[pin_data(PinnedDrop)]
#[repr(C, align(8))]
pub struct RawMutexStorage {
    storage: zr::OpaqueBytes<RAW_MUTEX_SIZE>,
}

impl RawMutexStorage {
    #[allow(clippy::declare_interior_mutable_const)]
    pub const INIT: Self = Self::const_init();

    /// Statically initializes a `RawMutexStorage` in constant context.
    pub const fn const_init() -> Self {
        Self { storage: zr::OpaqueBytes::new(make_mutex_storage()) }
    }

    /// Is the mutex contested i.e. is at least one thread blocked waiting on it?
    ///
    /// The contested flag does not track threads which are spin-waiting on the
    /// Mutex and have yet to enter a blocking phase.
    #[inline]
    pub fn is_contested(&self) -> bool {
        // SAFETY: The FFI call is safe because the lock is initialized.
        unsafe { cpp_mutex_is_contested(self.as_mut_ptr()) }
    }

    /// Returns whether the mutex is currently held by the calling thread.
    #[inline]
    pub fn is_held(&self) -> bool {
        // SAFETY: The FFI call is safe because the lock is initialized.
        unsafe { cpp_mutex_is_held(self.storage.get().cast::<c_void>()) }
    }

    /// Returns a slice over the raw mutex storage bytes.
    #[cfg(any(test, ktest))]
    #[inline]
    pub fn raw_storage_slice(&self) -> &[u8] {
        // SAFETY: The storage is valid and allocated with RAW_MUTEX_SIZE bytes.
        unsafe { core::slice::from_raw_parts(self.storage.get() as *const u8, RAW_MUTEX_SIZE) }
    }
}

impl Default for RawMutexStorage {
    fn default() -> Self {
        Self::INIT
    }
}

// SAFETY: RawMutexStorage is safe to share and access across threads.
unsafe impl Sync for RawMutexStorage {}
// SAFETY: RawMutexStorage is safe to send across threads.
unsafe impl Send for RawMutexStorage {}

zr::unsafe_pinned_drop_ffi!(RawMutexStorage, cpp_mutex_destroy);

// SAFETY: `cpp_mutex_acquire` and `cpp_mutex_release` enforce mutual exclusion on
// `RawMutexStorage`.
unsafe impl LockPolicy<RawMutexStorage> for RawMutexStorage {
    type AcquireArgs = ();
    type GuardState = ();

    /// # Safety
    ///
    /// `lock` must be initialized, the calling thread must not already hold `lock`, and the
    /// caller must be in a thread context where blocking on a kernel mutex is permitted.
    #[inline]
    unsafe fn acquire(lock: &RawMutexStorage, _state: &mut Self::GuardState, _args: ()) {
        // SAFETY: The FFI call is safe because the lock is initialized.
        unsafe {
            cpp_mutex_acquire(lock.as_mut_ptr());
        }
    }

    /// # Safety
    ///
    /// `lock` must be initialized, `state` must have been previously populated by `acquire`, the
    /// calling thread must not currently hold `lock`, and blocking on a kernel mutex must be
    /// permitted.
    #[inline]
    unsafe fn reacquire(lock: &RawMutexStorage, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe {
            Self::acquire(lock, state, ());
        }
    }

    /// # Safety
    ///
    /// `lock` must be initialized and currently held by the calling thread with `state` produced
    /// by the corresponding `acquire` or `reacquire` call.
    #[inline]
    unsafe fn release(lock: &RawMutexStorage, _state: &mut Self::GuardState) {
        // SAFETY: The FFI call is safe because the lock is initialized and held by the current
        // thread.
        unsafe {
            cpp_mutex_release(lock.as_mut_ptr());
        }
    }
}

// SAFETY: `cpp_mutex_acquire` and `cpp_mutex_release` enforce exclusive mutual exclusion on
// `RawMutexStorage`.
unsafe impl ExclusiveLockPolicy<RawMutexStorage> for RawMutexStorage {}

// SAFETY: `RawMutexStorage` is a physical kernel mutex with a unique storage address.
unsafe impl RawLock for RawMutexStorage {
    type DefaultPolicy = Self;

    /// # Safety
    ///
    /// `_class_id` must be a valid lockdep lock class identifier or null; the returned `PinInit`
    /// must be used to initialize pinned storage before the mutex is acquired.
    #[inline]
    unsafe fn init(_class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: The closure initializes the provided slot in place before returning Ok(()).
        unsafe {
            pin_init_from_closure(move |slot: *mut Self| {
                slot.write(Self::const_init());
                Ok(())
            })
        }
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        self as *const Self as *mut Self as *mut c_void
    }
}

impl ConstRawLock for RawMutexStorage {
    const INIT: Self = Self::INIT;
}

/// Lock policy for acquiring and releasing a `RawMutexStorage` as a C++ `CriticalMutex`.
pub struct RawCriticalMutexPolicy;

// SAFETY: `cpp_critical_mutex_acquire` and `cpp_critical_mutex_release` enforce mutual exclusion
// on `RawMutexStorage`.
unsafe impl LockPolicy<RawMutexStorage> for RawCriticalMutexPolicy {
    type AcquireArgs = ();
    type GuardState = bool;

    /// # Safety
    ///
    /// `lock` must be initialized, the calling thread must not already hold `lock`, and the
    /// caller must be in a thread context where blocking on a kernel critical mutex is permitted.
    #[inline]
    unsafe fn acquire(lock: &RawMutexStorage, state: &mut Self::GuardState, _args: ()) {
        // SAFETY: The FFI call is safe because the lock is initialized.
        *state = unsafe { cpp_critical_mutex_acquire(lock.as_mut_ptr()) };
    }

    /// # Safety
    ///
    /// `lock` must be initialized, `state` must have been previously populated by `acquire`, the
    /// calling thread must not currently hold `lock`, and blocking on a kernel critical mutex must
    /// be permitted.
    #[inline]
    unsafe fn reacquire(lock: &RawMutexStorage, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe {
            Self::acquire(lock, state, ());
        }
    }

    /// # Safety
    ///
    /// `lock` must be initialized and currently held by the calling thread with `should_clear`
    /// produced by the corresponding `acquire` or `reacquire` call.
    #[inline]
    unsafe fn release(lock: &RawMutexStorage, should_clear: &mut Self::GuardState) {
        // SAFETY: The FFI call is safe because the lock is initialized and held by the current
        // thread.
        unsafe {
            cpp_critical_mutex_release(lock.as_mut_ptr(), *should_clear);
        }
    }
}

// SAFETY: `cpp_critical_mutex_acquire` and `cpp_critical_mutex_release` enforce exclusive mutual
// exclusion on `RawMutexStorage`.
unsafe impl ExclusiveLockPolicy<RawMutexStorage> for RawCriticalMutexPolicy {}

/// Lockdep-tracked kernel mutex matching the Zircon C++ `Lock<Mutex>` layout.
pub type RawMutex = LockdepTracked<RawMutexStorage>;

/// Lockdep-tracked kernel critical mutex matching the Zircon C++ `Lock<CriticalMutex>` layout.
pub type RawCriticalMutex = LockdepTracked<RawMutexStorage, RawCriticalMutexPolicy>;

const _: () = {
    const HAS_NAME_STORAGE: bool =
        cfg!(any(feature = "lock_name_tracing", feature = "spin_lock_tracing"));
    #[cfg(any(feature = "lock_dep", feature = "lock_metadata_only"))]
    const EXPECTED_SIZE: usize = if HAS_NAME_STORAGE { 40 } else { 32 };
    #[cfg(not(any(feature = "lock_dep", feature = "lock_metadata_only")))]
    const EXPECTED_SIZE: usize = if HAS_NAME_STORAGE { 32 } else { 24 };

    assert!(core::mem::size_of::<RawMutex>() == EXPECTED_SIZE);
    assert!(core::mem::align_of::<RawMutex>() == 8);

    assert!(core::mem::size_of::<RawCriticalMutex>() == EXPECTED_SIZE);
    assert!(core::mem::align_of::<RawCriticalMutex>() == 8);
};
