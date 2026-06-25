// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::raw_kernel_mutex::LockEntryStorage;
use crate::raw_lock::RawLock;
use core::ffi::c_void;
use pin_init::{PinInit, pin_data};

#[cfg(target_arch = "x86_64")]
type InnerSavedState = u64;
#[cfg(not(target_arch = "x86_64"))]
type InnerSavedState = bool;

/// Opaque token representing the saved interrupt state.
#[repr(transparent)]
#[derive(Copy, Clone, Default)]
pub struct InterruptSavedState(InnerSavedState);

unsafe extern "C" {
    fn cpp_spinlock_init(lock: *mut c_void);
    fn cpp_spinlock_destroy(lock: *mut c_void);
    fn cpp_spinlock_acquire_irqsave(
        lock: *mut c_void,
        lcid: *mut c_void,
        entry_storage: *mut c_void,
    ) -> InterruptSavedState;
    fn cpp_spinlock_release_irqrestore(
        lock: *mut c_void,
        entry_storage: *mut c_void,
        state: InterruptSavedState,
    );
}

#[cfg(feature = "spin_lock_tracing")]
const RAW_SPINLOCK_SIZE: usize = 16;
#[cfg(not(feature = "spin_lock_tracing"))]
const RAW_SPINLOCK_SIZE: usize = 4;

#[repr(C, align(8))]
struct RawSpinlockStorage(zr::OpaqueBytes<RAW_SPINLOCK_SIZE>);

/// Opaque layout block matching the Zircon C++ SpinLock exactly.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct RawSpinlock {
    #[cfg(feature = "lock_dep")]
    class_id: *const c_void,
    storage: RawSpinlockStorage,
}

// SAFETY: RawSpinlock is safe to share and access across threads.
unsafe impl Sync for RawSpinlock {}
unsafe impl Send for RawSpinlock {}

zr::unsafe_pinned_drop_ffi!(RawSpinlock, cpp_spinlock_destroy);

impl crate::RawLock for RawSpinlock {
    type LockEntry = LockEntryStorage;
    type GuardState = InterruptSavedState;

    #[inline]
    fn init() -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_spinlock_init)
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        self as *const Self as *mut Self as *mut c_void
    }

    #[inline]
    unsafe fn acquire(&self, lcid: *mut c_void, entry: *mut Self::LockEntry) -> Self::GuardState {
        unsafe { cpp_spinlock_acquire_irqsave(self.as_mut_ptr(), lcid, entry as *mut c_void) }
    }

    #[inline]
    unsafe fn release(&self, entry: *mut Self::LockEntry, state: Self::GuardState) {
        unsafe {
            cpp_spinlock_release_irqrestore(self.as_mut_ptr(), entry as *mut c_void, state);
        }
    }
}

const _: () = {
    #[cfg(feature = "lock_dep")]
    const BASE_SIZE: usize = 8;
    #[cfg(not(feature = "lock_dep"))]
    const BASE_SIZE: usize = 0;

    const EXPECTED_SPINLOCK_SIZE: usize = BASE_SIZE + if RAW_SPINLOCK_SIZE == 4 { 8 } else { 16 };

    assert!(core::mem::size_of::<RawSpinlock>() == EXPECTED_SPINLOCK_SIZE);
    assert!(core::mem::align_of::<RawSpinlock>() == 8);
};
