// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::raw_lock::RawLock;
use core::ffi::c_void;
use pin_init::{PinInit, pin_data};

#[cfg(feature = "lock_name_tracing")]
const RAW_MUTEX_SIZE: usize = 32;
#[cfg(not(feature = "lock_name_tracing"))]
const RAW_MUTEX_SIZE: usize = 24;

unsafe extern "C" {
    fn cpp_mutex_init(mutex: *mut c_void, class_id: *const c_void);
    fn cpp_mutex_destroy(mutex: *mut c_void);
    fn cpp_mutex_acquire(lock: *mut c_void, entry_storage: *mut c_void);
    fn cpp_mutex_release(lock: *mut c_void, entry_storage: *mut c_void);

    fn cpp_critical_mutex_init(mutex: *mut c_void, class_id: *const c_void);
    fn cpp_critical_mutex_destroy(mutex: *mut c_void);
    fn cpp_critical_mutex_acquire(lock: *mut c_void, entry_storage: *mut c_void) -> bool;
    fn cpp_critical_mutex_release(
        lock: *mut c_void,
        entry_storage: *mut c_void,
        should_clear: bool,
    );
}

#[repr(C, align(8))]
struct RawMutexStorage(zr::OpaqueBytes<RAW_MUTEX_SIZE>);

#[derive(Default)]
#[repr(C, align(8))]
pub struct LockEntryStorage(zr::OpaqueBytes<40>);

/// Opaque layout block matching the Zircon C++ Mutex exactly.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct RawMutex {
    #[cfg(feature = "lock_dep")]
    class_id: *const c_void,
    storage: RawMutexStorage,
}

// SAFETY: RawMutex is safe to share and access across threads.
unsafe impl Sync for RawMutex {}
unsafe impl Send for RawMutex {}

zr::unsafe_pinned_drop_ffi!(RawMutex, cpp_mutex_destroy);

impl crate::RawLock for RawMutex {
    type LockEntry = LockEntryStorage;
    type GuardState = ();

    #[inline]
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_mutex_init, class_id)
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        self as *const Self as *mut Self as *mut c_void
    }

    #[inline]
    unsafe fn acquire(&self, entry: *mut Self::LockEntry) -> Self::GuardState {
        // SAFETY: The FFI call is safe because the lock is initialized, and the caller guarantees
        // that `entry` points to valid storage for a lockdep entry.
        unsafe {
            cpp_mutex_acquire(self.as_mut_ptr(), entry as *mut c_void);
        }
    }

    #[inline]
    unsafe fn release(&self, entry: *mut Self::LockEntry, _state: Self::GuardState) {
        // SAFETY: The FFI call is safe because the lock is initialized, and the caller guarantees
        // that `entry` points to valid storage for a lockdep entry.
        unsafe {
            cpp_mutex_release(self.as_mut_ptr(), entry as *mut c_void);
        }
    }
}

/// Opaque layout block matching the Zircon C++ CriticalMutex exactly.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct RawCriticalMutex {
    #[cfg(feature = "lock_dep")]
    class_id: *const c_void,
    storage: RawMutexStorage,
}

// SAFETY: RawCriticalMutex is safe to share and access across threads.
unsafe impl Sync for RawCriticalMutex {}
unsafe impl Send for RawCriticalMutex {}

zr::unsafe_pinned_drop_ffi!(RawCriticalMutex, cpp_critical_mutex_destroy);

impl crate::RawLock for RawCriticalMutex {
    type LockEntry = LockEntryStorage;
    type GuardState = bool;

    #[inline]
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_critical_mutex_init, class_id)
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        self as *const Self as *mut Self as *mut c_void
    }

    #[inline]
    unsafe fn acquire(&self, entry: *mut Self::LockEntry) -> Self::GuardState {
        // SAFETY: The FFI call is safe because the lock is initialized, and the caller guarantees
        // that `entry` points to valid storage for a lockdep entry.
        unsafe { cpp_critical_mutex_acquire(self.as_mut_ptr(), entry as *mut c_void) }
    }

    #[inline]
    unsafe fn release(&self, entry: *mut Self::LockEntry, should_clear: Self::GuardState) {
        // SAFETY: The FFI call is safe because the lock is initialized, and the caller guarantees
        // that `entry` points to valid storage for a lockdep entry.
        unsafe {
            cpp_critical_mutex_release(self.as_mut_ptr(), entry as *mut c_void, should_clear);
        }
    }
}

const _: () = {
    #[cfg(feature = "lock_dep")]
    const EXPECTED_SIZE: usize = if cfg!(feature = "lock_name_tracing") { 40 } else { 32 };
    #[cfg(not(feature = "lock_dep"))]
    const EXPECTED_SIZE: usize = if cfg!(feature = "lock_name_tracing") { 32 } else { 24 };

    assert!(core::mem::size_of::<RawMutex>() == EXPECTED_SIZE);
    assert!(core::mem::align_of::<RawMutex>() == 8);

    assert!(core::mem::size_of::<RawCriticalMutex>() == EXPECTED_SIZE);
    assert!(core::mem::align_of::<RawCriticalMutex>() == 8);
};
