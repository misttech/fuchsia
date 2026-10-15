// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::marker::PhantomData;

/// A token proving that a lock of lock class `Class` is currently held by the current thread.
///
/// This token acts as a static proof to permit type-safe borrow access to cell values guarded by
/// this lock class (via `KCell`).
#[repr(transparent)]
pub struct LockToken<'a, Class> {
    _marker: PhantomData<(&'a Class, *const ())>,
}

impl<'a, Class> LockToken<'a, Class> {
    /// Creates a new proof token for the lock class `Class`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the mutual exclusion lock associated with this lock class is
    /// currently held by the current thread and that the lifetime `'a` is bounded to the duration
    /// of the lock being held. Creating a token when the lock is not held, or letting the token
    /// outlive the lock hold duration, can lead to concurrent mutation or data races, which is
    /// undefined behavior.
    #[inline]
    pub unsafe fn new() -> Self {
        Self { _marker: PhantomData }
    }

    /// Reborrows this exclusive lock token with its lifetime shortened to the borrow duration `'b`.
    #[inline]
    pub fn reborrow<'b>(&'b mut self) -> &'b mut LockToken<'b, Class> {
        // SAFETY: `LockToken` is a zero-sized type (ZST) with alignment 1. Since `&'b mut self`
        // requires `'a: 'b`, the lock is held for at least `'b`, and `self` is borrowed
        // exclusively for `'b`.
        unsafe { &mut *core::ptr::from_mut(self).cast::<LockToken<'b, Class>>() }
    }

    /// Combines two exclusive lock tokens into a single composite lock token proving exclusive
    /// ownership of both lock classes simultaneously for the borrow duration `'b`.
    #[inline]
    pub fn join<'b, 'a2, C2>(
        &'b mut self,
        _other: &'b mut LockToken<'a2, C2>,
    ) -> &'b mut LockToken<'b, (Class, C2)> {
        // SAFETY: `LockToken` is a zero-sized type (ZST) with alignment 1. Casting `self` to a
        // mutable reference of the composite `LockToken` ZST with lifetime `'b` is valid and
        // borrows both `self` and `_other` exclusively for `'b`.
        unsafe { &mut *(self as *mut Self as *mut LockToken<'b, (Class, C2)>) }
    }
}

/// Type-level index marker for exact-match lock token access.
#[doc(hidden)]
pub struct TokenIndexHere;

/// Type-level index marker for multi-lock tuple projections and permutations.
#[doc(hidden)]
pub struct TokenIndex<const N: usize>;

/// Trait implemented by lock tokens that grant shared (read) access to a cell guarded by `Class`.
///
/// When a cell is guarded by a single lock `C`, holding `LockToken<'_, C>` grants read access.
/// When a cell is guarded by multiple locks `(C0, C1, ...)`, holding ANY one of the constituent
/// locks grants read access, because mutating the cell requires holding ALL constituent locks
/// simultaneously.
pub trait LockTokenRead<Class, Idx = TokenIndexHere> {}

/// Trait implemented by lock tokens that grant exclusive (write) access to a cell guarded by
/// `Class`.
///
/// When a cell is guarded by a single lock `C`, holding `&mut LockToken<'_, C>` grants write
/// access. When a cell is guarded by multiple locks `(C0, C1, ...)`, write access requires a
/// joined mutable token proving exclusive ownership of ALL constituent locks simultaneously.
pub trait LockTokenWrite<Class, Idx = TokenIndexHere> {}

impl<Class> LockTokenRead<Class, TokenIndexHere> for LockToken<'_, Class> {}
impl<Class> LockTokenWrite<Class, TokenIndexHere> for LockToken<'_, Class> {}

// 2-lock tuple `(C0, C1)` read/write implementations.
impl<C0, C1> LockTokenRead<(C0, C1), TokenIndex<0>> for LockToken<'_, C0> {}
impl<C0, C1> LockTokenRead<(C0, C1), TokenIndex<1>> for LockToken<'_, C1> {}

impl<C0, C1> LockTokenWrite<(C0, C1), TokenIndex<0>> for LockToken<'_, (C1, C0)> {}

// 3-lock tuple `(C0, C1, C2)` read/write implementations.
impl<C0, C1, C2> LockTokenRead<(C0, C1, C2), TokenIndex<0>> for LockToken<'_, C0> {}
impl<C0, C1, C2> LockTokenRead<(C0, C1, C2), TokenIndex<1>> for LockToken<'_, C1> {}
impl<C0, C1, C2> LockTokenRead<(C0, C1, C2), TokenIndex<2>> for LockToken<'_, C2> {}

impl<C0, C1, C2> LockTokenWrite<(C0, C1, C2), TokenIndex<0>> for LockToken<'_, ((C0, C1), C2)> {}

/// Trait for lock guard types bound to a `Parent` struct instance that hold exclusive ownership
/// of a lock of class `Class` and can yield a mutable [`LockToken`].
///
/// # Safety
///
/// Implementors must guarantee that they hold exclusive ownership of the lock of class `Class`
/// on the `Parent` struct instance at [`ExclusiveLockToken::parent`] for the duration of the
/// mutable borrow of `self`.
pub unsafe trait ExclusiveLockToken<Parent: ?Sized, Class> {
    /// Returns the address of the parent struct instance to which this guard is bound.
    fn parent(&self) -> *const Parent;

    /// Returns a mutable reference to the underlying [`LockToken`].
    fn lock_token_mut(&mut self) -> &mut LockToken<'_, Class>;
}
