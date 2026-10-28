// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::lockdep_tracked::{LockdepTracked, impl_lockdep_tracked_lock_policy};
use crate::raw_lock::{ConstRawLock, ExclusiveLockPolicy, LockPolicy, RawLock};
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
    fn cpp_ksync_interrupt_save() -> InterruptSavedState;

    fn cpp_spinlock_init(lock: *mut c_void, class_id: *const c_void);
    fn cpp_spinlock_destroy(lock: *mut c_void);
    fn cpp_spinlock_acquire_irqsave(lock: *mut c_void) -> InterruptSavedState;
    fn cpp_spinlock_release_irqrestore(lock: *mut c_void, state: InterruptSavedState);
    fn cpp_spinlock_acquire_no_irqsave(lock: *mut c_void);
    fn cpp_spinlock_release_no_irqrestore(lock: *mut c_void);

    fn cpp_monitored_spinlock_init(lock: *mut c_void, class_id: *const c_void);
    fn cpp_monitored_spinlock_destroy(lock: *mut c_void);
    fn cpp_monitored_spinlock_acquire_irqsave(
        lock: *mut c_void,
        name: *const core::ffi::c_char,
    ) -> InterruptSavedState;
    fn cpp_monitored_spinlock_release_irqrestore(lock: *mut c_void, state: InterruptSavedState);
    fn cpp_monitored_spinlock_acquire_no_irqsave(lock: *mut c_void, name: *const core::ffi::c_char);
    fn cpp_monitored_spinlock_release_no_irqrestore(lock: *mut c_void);
}

#[cfg(feature = "spin_lock_tracing")]
const RAW_SPINLOCK_SIZE: usize = 16;
#[cfg(not(feature = "spin_lock_tracing"))]
const RAW_SPINLOCK_SIZE: usize = 4;

/// Opaque layout block for the Zircon C++ `SpinLock` object.
///
/// Note: When `spin_lock_tracing` is disabled, C++ `SpinLock` has size 4 and alignment 4, whereas
/// this type intentionally over-aligns to 8 bytes (`#[repr(C, align(8))]`, resulting in size 8 and
/// alignment 8) so that `RawSpinlock` and `#[repr(C)]` FFI structs embedding it (such as
/// `ControlRegion` and `TrapMap`, which pad `sizeof(LockType)` to 8 bytes in C++) maintain uniform
/// 8-byte alignment across all `lock_dep` and `spin_lock_tracing` configurations.
#[pin_data(PinnedDrop)]
#[repr(C, align(8))]
pub struct RawSpinlockStorage {
    storage: zr::OpaqueBytes<RAW_SPINLOCK_SIZE>,
}

impl RawSpinlockStorage {
    #[allow(clippy::declare_interior_mutable_const)]
    pub const INIT: Self = Self::const_init();

    /// Statically initializes a `RawSpinlockStorage` in constant context.
    pub const fn const_init() -> Self {
        Self { storage: zr::OpaqueBytes::new([0u8; RAW_SPINLOCK_SIZE]) }
    }
}

impl Default for RawSpinlockStorage {
    fn default() -> Self {
        Self::INIT
    }
}

// SAFETY: `RawSpinlockStorage` is safe to share and access across threads.
unsafe impl Sync for RawSpinlockStorage {}
// SAFETY: `RawSpinlockStorage` is safe to send across threads.
unsafe impl Send for RawSpinlockStorage {}

zr::unsafe_pinned_drop_ffi!(RawSpinlockStorage, cpp_spinlock_destroy);

/// Opaque layout block for the Zircon C++ `MonitoredSpinLock` object.
///
/// Like [`RawSpinlockStorage`], this type intentionally over-aligns to 8 bytes even when
/// `spin_lock_tracing` is disabled.
#[pin_data(PinnedDrop)]
#[repr(C, align(8))]
pub struct RawMonitoredSpinlockStorage {
    storage: zr::OpaqueBytes<RAW_SPINLOCK_SIZE>,
}

impl RawMonitoredSpinlockStorage {
    #[allow(clippy::declare_interior_mutable_const)]
    pub const INIT: Self = Self::const_init();

    /// Statically initializes a `RawMonitoredSpinlockStorage` in constant context.
    pub const fn const_init() -> Self {
        Self { storage: zr::OpaqueBytes::new([0u8; RAW_SPINLOCK_SIZE]) }
    }
}

impl Default for RawMonitoredSpinlockStorage {
    fn default() -> Self {
        Self::INIT
    }
}

// SAFETY: `RawMonitoredSpinlockStorage` is safe to share and access across threads.
unsafe impl Sync for RawMonitoredSpinlockStorage {}
// SAFETY: `RawMonitoredSpinlockStorage` is safe to send across threads.
unsafe impl Send for RawMonitoredSpinlockStorage {}

zr::unsafe_pinned_drop_ffi!(RawMonitoredSpinlockStorage, cpp_monitored_spinlock_destroy);

pub struct IrqSavePolicy;

// SAFETY: `cpp_spinlock_acquire_irqsave` (or `cpp_ksync_interrupt_save` +
// `cpp_spinlock_acquire_no_irqsave`) and `cpp_spinlock_release_irqrestore` enforce mutual
// exclusion on `RawSpinlockStorage`.
unsafe impl LockPolicy<RawSpinlockStorage> for IrqSavePolicy {
    type AcquireArgs = ();
    type GuardState = InterruptSavedState;

    #[inline]
    unsafe fn pre_validate(_lock: &RawSpinlockStorage, state: &mut Self::GuardState, _args: ()) {
        // SAFETY: Saving and disabling interrupts is safe when paired with a matching release.
        *state = unsafe { cpp_ksync_interrupt_save() };
    }

    #[inline]
    unsafe fn pre_reacquire(_lock: &RawSpinlockStorage, state: &mut Self::GuardState) {
        // SAFETY: Saving and disabling interrupts is safe when paired with a matching release.
        *state = unsafe { cpp_ksync_interrupt_save() };
    }

    #[inline]
    unsafe fn acquire(lock: &RawSpinlockStorage, state: &mut Self::GuardState, _args: ()) {
        // SAFETY: The FFI call is safe because the lock is initialized.
        *state = unsafe { cpp_spinlock_acquire_irqsave(lock.as_mut_ptr()) };
    }

    #[inline]
    unsafe fn post_validate_acquire(
        lock: &RawSpinlockStorage,
        _state: &mut Self::GuardState,
        _args: (),
    ) {
        // SAFETY: The lock is initialized and interrupts were disabled in `pre_validate`.
        unsafe {
            cpp_spinlock_acquire_no_irqsave(lock.as_mut_ptr());
        }
    }

    #[inline]
    unsafe fn reacquire(lock: &RawSpinlockStorage, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe {
            Self::acquire(lock, state, ());
        }
    }

    #[inline]
    unsafe fn post_validate_reacquire(lock: &RawSpinlockStorage, _state: &mut Self::GuardState) {
        // SAFETY: The lock is initialized and interrupts were disabled in `pre_reacquire`.
        unsafe {
            cpp_spinlock_acquire_no_irqsave(lock.as_mut_ptr());
        }
    }

    #[inline]
    unsafe fn release(lock: &RawSpinlockStorage, state: &mut Self::GuardState) {
        // SAFETY: The FFI call is safe because the lock is initialized and held by the current
        // CPU with saved interrupt state `*state`.
        unsafe {
            cpp_spinlock_release_irqrestore(lock.as_mut_ptr(), *state);
        }
    }
}

// SAFETY: `IrqSavePolicy` enforces exclusive mutual exclusion on `RawSpinlockStorage`.
unsafe impl ExclusiveLockPolicy<RawSpinlockStorage> for IrqSavePolicy {}

pub struct NoIrqSavePolicy;

// SAFETY: `cpp_spinlock_acquire_no_irqsave` and `cpp_spinlock_release_no_irqrestore` enforce
// mutual exclusion on `RawSpinlockStorage`.
unsafe impl LockPolicy<RawSpinlockStorage> for NoIrqSavePolicy {
    type AcquireArgs = ();
    type GuardState = ();

    #[inline]
    unsafe fn acquire(lock: &RawSpinlockStorage, _state: &mut Self::GuardState, _args: ()) {
        // SAFETY: The FFI call is safe because the lock is initialized.
        unsafe { cpp_spinlock_acquire_no_irqsave(lock.as_mut_ptr()) }
    }

    #[inline]
    unsafe fn reacquire(lock: &RawSpinlockStorage, state: &mut Self::GuardState) {
        // SAFETY: Caller upholds the `LockPolicy::reacquire` contract.
        unsafe {
            Self::acquire(lock, state, ());
        }
    }

    #[inline]
    unsafe fn release(lock: &RawSpinlockStorage, _state: &mut Self::GuardState) {
        // SAFETY: The FFI call is safe because the lock is initialized and held by the current
        // CPU.
        unsafe {
            cpp_spinlock_release_no_irqrestore(lock.as_mut_ptr());
        }
    }
}

// SAFETY: `NoIrqSavePolicy` enforces exclusive mutual exclusion on `RawSpinlockStorage`.
unsafe impl ExclusiveLockPolicy<RawSpinlockStorage> for NoIrqSavePolicy {}

/// Saved interrupt state and source tag for a held `RawMonitoredSpinlock`.
#[derive(Copy, Clone, Default)]
pub struct MonitoredSpinlockGuardState {
    pub interrupt_state: InterruptSavedState,
    pub tag: crate::SourceTag,
}

// SAFETY: `cpp_monitored_spinlock_acquire_irqsave` (or `cpp_ksync_interrupt_save` +
// `cpp_monitored_spinlock_acquire_no_irqsave`) and `cpp_monitored_spinlock_release_irqrestore`
// enforce mutual exclusion on `RawMonitoredSpinlockStorage`.
unsafe impl LockPolicy<RawMonitoredSpinlockStorage> for IrqSavePolicy {
    type AcquireArgs = crate::SourceTag;
    type GuardState = MonitoredSpinlockGuardState;

    #[inline]
    unsafe fn pre_validate(
        _lock: &RawMonitoredSpinlockStorage,
        state: &mut Self::GuardState,
        tag: Self::AcquireArgs,
    ) {
        // SAFETY: Saving and disabling interrupts is safe when paired with a matching release.
        let interrupt_state = unsafe { cpp_ksync_interrupt_save() };
        *state = MonitoredSpinlockGuardState { interrupt_state, tag };
    }

    #[inline]
    unsafe fn pre_reacquire(_lock: &RawMonitoredSpinlockStorage, state: &mut Self::GuardState) {
        // SAFETY: Saving and disabling interrupts is safe when paired with a matching release.
        state.interrupt_state = unsafe { cpp_ksync_interrupt_save() };
    }

    #[inline]
    unsafe fn acquire(
        lock: &RawMonitoredSpinlockStorage,
        state: &mut Self::GuardState,
        tag: Self::AcquireArgs,
    ) {
        // SAFETY: The FFI call is safe because the lock is initialized and `tag` points to a
        // static null-terminated C string.
        let interrupt_state =
            unsafe { cpp_monitored_spinlock_acquire_irqsave(lock.as_mut_ptr(), tag.as_ptr()) };
        *state = MonitoredSpinlockGuardState { interrupt_state, tag };
    }

    #[inline]
    unsafe fn post_validate_acquire(
        lock: &RawMonitoredSpinlockStorage,
        _state: &mut Self::GuardState,
        tag: Self::AcquireArgs,
    ) {
        // SAFETY: The lock is initialized, interrupts were disabled in `pre_validate`, and `tag`
        // points to a static null-terminated C string.
        unsafe {
            cpp_monitored_spinlock_acquire_no_irqsave(lock.as_mut_ptr(), tag.as_ptr());
        }
    }

    #[inline]
    unsafe fn reacquire(lock: &RawMonitoredSpinlockStorage, state: &mut Self::GuardState) {
        // SAFETY: Same safety requirements as `acquire`.
        state.interrupt_state = unsafe {
            cpp_monitored_spinlock_acquire_irqsave(lock.as_mut_ptr(), state.tag.as_ptr())
        };
    }

    #[inline]
    unsafe fn post_validate_reacquire(
        lock: &RawMonitoredSpinlockStorage,
        state: &mut Self::GuardState,
    ) {
        // SAFETY: The lock is initialized, interrupts were disabled in `pre_reacquire`, and
        // `state.tag` points to a static null-terminated C string.
        unsafe {
            cpp_monitored_spinlock_acquire_no_irqsave(lock.as_mut_ptr(), state.tag.as_ptr());
        }
    }

    #[inline]
    unsafe fn release(lock: &RawMonitoredSpinlockStorage, state: &mut Self::GuardState) {
        // SAFETY: The FFI call is safe because the lock is initialized and held by the current
        // CPU with saved interrupt state `state.interrupt_state`.
        unsafe {
            cpp_monitored_spinlock_release_irqrestore(lock.as_mut_ptr(), state.interrupt_state);
        }
    }
}

// SAFETY: `IrqSavePolicy` enforces exclusive mutual exclusion on `RawMonitoredSpinlockStorage`.
unsafe impl ExclusiveLockPolicy<RawMonitoredSpinlockStorage> for IrqSavePolicy {}

// SAFETY: `cpp_monitored_spinlock_acquire_no_irqsave` and
// `cpp_monitored_spinlock_release_no_irqrestore` enforce mutual exclusion on
// `RawMonitoredSpinlockStorage`.
unsafe impl LockPolicy<RawMonitoredSpinlockStorage> for NoIrqSavePolicy {
    type AcquireArgs = crate::SourceTag;
    type GuardState = crate::SourceTag;

    #[inline]
    unsafe fn acquire(
        lock: &RawMonitoredSpinlockStorage,
        state: &mut Self::GuardState,
        tag: Self::AcquireArgs,
    ) {
        // SAFETY: The FFI call is safe because the lock is initialized and `tag` points to a
        // static null-terminated C string.
        unsafe {
            cpp_monitored_spinlock_acquire_no_irqsave(lock.as_mut_ptr(), tag.as_ptr());
        }
        *state = tag;
    }

    #[inline]
    unsafe fn reacquire(lock: &RawMonitoredSpinlockStorage, state: &mut Self::GuardState) {
        // SAFETY: Same safety requirements as `acquire`.
        unsafe {
            cpp_monitored_spinlock_acquire_no_irqsave(lock.as_mut_ptr(), state.as_ptr());
        }
    }

    #[inline]
    unsafe fn release(lock: &RawMonitoredSpinlockStorage, _state: &mut Self::GuardState) {
        // SAFETY: The FFI call is safe because the lock is initialized and held by the current
        // CPU.
        unsafe {
            cpp_monitored_spinlock_release_no_irqrestore(lock.as_mut_ptr());
        }
    }
}

// SAFETY: `NoIrqSavePolicy` enforces exclusive mutual exclusion on `RawMonitoredSpinlockStorage`.
unsafe impl ExclusiveLockPolicy<RawMonitoredSpinlockStorage> for NoIrqSavePolicy {}

// SAFETY: `RawSpinlockStorage` is a physical kernel spinlock with a unique storage address.
unsafe impl RawLock for RawSpinlockStorage {
    type DefaultPolicy = IrqSavePolicy;

    #[inline]
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_spinlock_init, class_id)
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        self as *const Self as *mut Self as *mut c_void
    }
}

impl ConstRawLock for RawSpinlockStorage {
    const INIT: Self = Self::INIT;
}

// SAFETY: `RawMonitoredSpinlockStorage` is a physical kernel spinlock with a unique storage
// address.
unsafe impl RawLock for RawMonitoredSpinlockStorage {
    type DefaultPolicy = IrqSavePolicy;

    #[inline]
    unsafe fn init(class_id: *const c_void) -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_monitored_spinlock_init, class_id)
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        self as *const Self as *mut Self as *mut c_void
    }
}

impl ConstRawLock for RawMonitoredSpinlockStorage {
    const INIT: Self = Self::INIT;
}

impl_lockdep_tracked_lock_policy!(IrqSavePolicy);
impl_lockdep_tracked_lock_policy!(NoIrqSavePolicy);

/// Lockdep-tracked kernel spinlock matching the Zircon C++ `Lock<SpinLock>` layout.
pub type RawSpinlock =
    LockdepTracked<RawSpinlockStorage, IrqSavePolicy, { lockdep::LOCK_FLAGS_IRQ_SAFE }>;

/// Lockdep-tracked kernel monitored spinlock matching the Zircon C++ `Lock<MonitoredSpinLock>`
/// layout.
pub type RawMonitoredSpinlock =
    LockdepTracked<RawMonitoredSpinlockStorage, IrqSavePolicy, { lockdep::LOCK_FLAGS_IRQ_SAFE }>;

const _: () = {
    #[cfg(any(feature = "lock_dep", feature = "lock_metadata_only"))]
    const BASE_SIZE: usize = 8;
    #[cfg(not(any(feature = "lock_dep", feature = "lock_metadata_only")))]
    const BASE_SIZE: usize = 0;

    // `RawSpinlockStorage` is `#[repr(C, align(8))]`, so the 4-byte non-tracing storage is padded
    // to 8 bytes.
    const EXPECTED_STORAGE_SIZE: usize = if RAW_SPINLOCK_SIZE == 4 { 8 } else { 16 };
    const EXPECTED_SPINLOCK_SIZE: usize = BASE_SIZE + EXPECTED_STORAGE_SIZE;

    assert!(core::mem::size_of::<RawSpinlockStorage>() == EXPECTED_STORAGE_SIZE);
    assert!(core::mem::align_of::<RawSpinlockStorage>() == 8);
    assert!(core::mem::size_of::<RawMonitoredSpinlockStorage>() == EXPECTED_STORAGE_SIZE);
    assert!(core::mem::align_of::<RawMonitoredSpinlockStorage>() == 8);
    assert!(core::mem::size_of::<RawSpinlock>() == EXPECTED_SPINLOCK_SIZE);
    assert!(core::mem::align_of::<RawSpinlock>() == 8);
    assert!(core::mem::size_of::<RawMonitoredSpinlock>() == EXPECTED_SPINLOCK_SIZE);
    assert!(core::mem::align_of::<RawMonitoredSpinlock>() == 8);
};
