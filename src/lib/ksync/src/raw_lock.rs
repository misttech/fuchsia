// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use pin_init::PinInit;

/// Trait defining a raw, un-instrumented synchronization lock abstraction.
///
/// Implementors of `RawLock` supply the platform-specific lock storage, in-place pinning
/// initialization logic, and raw synchronization entry points for lock validation systems.
pub trait RawLock {
    /// Opaque stack entry storage type used by the lock validation loop detector (e.g. LockDep).
    type LockEntry: Default;

    /// State returned from lock acquisition and subsequently passed to lock release.
    type GuardState: Default + Copy;

    /// Returns a PinInit block to initialize the raw mutex in-place.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `class_id` is either null or points to a valid,
    /// static `LockClassId` that remains valid for the lifetime of the lock.
    unsafe fn init(
        class_id: *const core::ffi::c_void,
    ) -> impl PinInit<Self, core::convert::Infallible>
    where
        Self: Sized;

    /// Convert the raw mutex reference to a standard c_void pointer for FFI.
    fn as_mut_ptr(&self) -> *mut core::ffi::c_void;

    /// Acquires the raw synchronization lock under a type-level lock class.
    ///
    /// # Safety
    ///
    /// 1. The `entry` pointer must point to a valid, exclusive, stack-allocated `LockEntry` slot
    ///    which will be registered in the thread's active list.
    /// 2. The caller must ensure that the `entry` memory remains pinned on the stack and is not
    ///    dropped or moved until the matching `release` call completes.
    unsafe fn acquire(&self, entry: *mut Self::LockEntry) -> Self::GuardState;

    /// Releases the raw synchronization lock, restoring the state.
    ///
    /// # Safety
    ///
    /// 1. The `entry` pointer must match the exact same stack slot pointer passed to the
    ///    corresponding `acquire` call.
    /// 2. The `state` parameter must match the exact same state value returned by the corresponding
    ///    `acquire` call.
    /// 3. The caller must guarantee that the current thread actually holds this lock (i.e. we are
    ///    releasing a lock we currently own).
    unsafe fn release(&self, entry: *mut Self::LockEntry, state: Self::GuardState);
}
