// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Owner-bound proofs that a specific object's [`ChainLock`] is held.
//!
//! A [`ChainLockProof<'p, O>`] borrows a [`ChainLocked`] object `O` and carries a [`LockToken`]
//! for `O::Class`. Accessors for `O`'s lock-protected `KCell` fields are defined on
//! `ChainLockProof<'_, O>`, ensuring a proof can only access the specific instance it was minted
//! for.
//!
//! # Obtaining a Proof
//!
//! - **From an owning guard (safe, check-free):** [`OwnerGuard::proof`],
//!   [`OwnedSingleChainLockScope::proof`](crate::kconcurrent::OwnedSingleChainLockScope::proof),
//!   and `ThreadLockGuard::proof` mint a proof directly because the guard itself acquired or
//!   adopted `O`'s lock.
//! - **From a pre-existing hold (`unsafe`):** [`ChainLockProof::new`] and
//!   [`ChainLockProof::assume_held`] verify at runtime that `O`'s lock is held before minting;
//!   [`ChainLockProof::assume_held_unchecked`] skips the runtime check.
//!
//! # Soundness
//!
//! Every proof borrows its guard mutably (`'p`), so at most one proof exists per guard at a time
//! and the guard cannot release the lock while the proof is live. Because
//! [`ClassedChainLock::as_chain_lock`] is `unsafe`, safe code cannot bypass the guard to release
//! or re-adopt the underlying [`ChainLock`].
//!
//! # Performance
//!
//! [`LockToken`] is zero-sized, so `ChainLockProof<'p, O>` has the same representation as `&'p O`,
//! and `KCell` is `#[repr(transparent)]` over `UnsafeCell`. Minting a proof from an acquiring
//! guard and accessing `KCell` fields through it are `#[inline]` pointer operations with no
//! runtime checks or branches. Runtime assertions only occur on [`ChainLockProof::new`],
//! [`ChainLockProof::assume_held`], and [`ChainLockProof::retag`] (which constant-folds away when
//! retagging to a fixed-offset subobject).

use core::marker::PhantomData;

use ksync::LockToken;

use super::chainlock::{ChainLock, ChainLockGuard, ChainLockTransactionToken};

/// A [`ChainLock`] whose guarded data is tagged with lock class `Class` (a marker type).
///
/// `repr(transparent)` over [`ChainLock`], so it can replace a `ChainLock` field in a
/// layout-checked struct without changing its layout.
#[repr(transparent)]
pub struct ClassedChainLock<Class> {
    lock: ChainLock,
    // `fn() -> Class` so that auto traits and variance do not depend on the marker type.
    _class: PhantomData<fn() -> Class>,
}

zr::static_assert!(size_of::<ClassedChainLock<()>>() == size_of::<ChainLock>());
zr::static_assert!(align_of::<ClassedChainLock<()>>() == align_of::<ChainLock>());

impl<Class> ClassedChainLock<Class> {
    /// Creates a new unlocked lock, exactly like [`ChainLock::new`].
    #[inline]
    pub const fn new() -> Self {
        Self { lock: ChainLock::new(), _class: PhantomData }
    }

    /// Returns the underlying [`ChainLock`].
    ///
    /// Prefer [`OwnerGuard`], [`crate::kconcurrent::single_chain_lock_guard_owned!`], or the safe
    /// inspection methods on [`ClassedChainLock`] when possible.
    ///
    /// # Safety
    ///
    /// [`ChainLock`] exposes safe methods that can release the lock or create duplicate guards
    /// (such as `release`, `ChainLockGuard::adopt`, and `ChainLockGuard::unguard`). The caller
    /// must not use the returned [`ChainLock`] to release the lock or create overlapping guards
    /// while any [`ChainLockProof`], [`OwnerGuard`],
    /// [`OwnedSingleChainLockScope`](crate::kconcurrent::OwnedSingleChainLockScope), or
    /// `ThreadLockGuard` for its owner is live.
    #[inline]
    pub const unsafe fn as_chain_lock(&self) -> &ChainLock {
        &self.lock
    }

    /// In-module access to the class-less lock. Every use in this module only reads lock state,
    /// compares addresses, or acquires/adopts through an [`OwnerGuard`] constructor; none releases
    /// directly. Outside this module the only route is the `unsafe` [`Self::as_chain_lock`].
    #[inline]
    const fn raw(&self) -> &ChainLock {
        &self.lock
    }

    /// Returns `true` if the lock is held by the transaction `token` belongs to.
    #[inline]
    pub fn is_held(&self, token: &ChainLockTransactionToken) -> bool {
        self.lock.is_held(token)
    }

    /// Returns `true` if the lock is unlocked.
    #[inline]
    pub fn is_unlocked(&self) -> bool {
        self.lock.is_unlocked()
    }

    /// Views an untyped [`ChainLock`] as a [`ClassedChainLock<Class>`].
    ///
    /// # Safety
    ///
    /// `lock` must be the lock that guards the `KCell<_, Class>` fields of the [`ChainLocked`]
    /// owner whose `lock()` returns this reference.
    #[inline]
    pub const unsafe fn from_chain_lock(lock: &ChainLock) -> &Self {
        // SAFETY: `Self` is `repr(transparent)` over `ChainLock`, so `lock` is a valid, properly
        // aligned `&Self` for the same lifetime.
        unsafe { &*core::ptr::from_ref(lock).cast::<Self>() }
    }
}

impl<Class> Default for ClassedChainLock<Class> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<Class> core::fmt::Debug for ClassedChainLock<Class> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClassedChainLock")
            .field("lock", &core::ptr::from_ref(&self.lock))
            .field("class", &core::any::type_name::<Class>())
            .finish()
    }
}

/// Implemented by every type that embeds a chain lock guarding `KCell<_, Self::Class>` fields.
///
/// # Safety
///
/// `lock()` must return the lock that guards every `KCell<_, Self::Class>` reachable through
/// accessors defined on `ChainLockProof<'_, Self>` or on a proof
/// [`retag`](ChainLockProof::retag)ged from it, and must return the same lock instance every time
/// it is called on the same object.
pub unsafe trait ChainLocked {
    /// The lock class tagging this object's guarded fields.
    type Class;

    /// Returns the lock guarding this object's `KCell<_, Self::Class>` fields.
    fn lock(&self) -> &ClassedChainLock<Self::Class>;
}

/// Proof that the current transaction holds `O`'s chain lock.
///
/// Borrows the owner, so accessors defined on `ChainLockProof<'_, O>` cannot be pointed at a
/// different instance. `'p` is bounded by the unique borrow of the guard holding the lock, so the
/// guard cannot release or mint a second proof while this one lives (which is what makes
/// [`Self::token_mut`] sound). Neither `Clone` nor `Copy`.
#[must_use = "a ChainLockProof only has an effect through its accessors"]
pub struct ChainLockProof<'p, O: ChainLocked> {
    // Safety invariant: `owner.lock()` is held by the current CPU's active transaction for `'p`,
    // and no other live proof or guard concurrently accesses state protected by `owner.lock()`.
    owner: &'p O,
    token: LockToken<'p, O::Class>,
}

impl<'p, O: ChainLocked> ChainLockProof<'p, O> {
    /// Mints a proof for `owner` from a raw guard that holds `owner.lock()`, checking the guard
    /// against the owner and the lock against the active transaction.
    ///
    /// # Safety
    ///
    /// No other [`ChainLockGuard`] or proof for `owner.lock()` may be live in the current
    /// transaction while the returned proof exists (e.g. a `ChainLockGuard::adopt` duplicate of
    /// `guard`), and nothing may release the lock while the proof lives.
    ///
    /// # Panics
    ///
    /// Panics if `guard` does not hold `owner.lock()` or the active transaction does not own it;
    /// inside a reserved-token window call `sync_token` on the lock first.
    #[inline]
    #[track_caller]
    pub unsafe fn new(
        owner: &'p O,
        guard: &'p mut ChainLockGuard<'_>,
        token: &'p ChainLockTransactionToken,
    ) -> Self {
        let lock = owner.lock().raw();
        assert!(
            core::ptr::eq(guard.lock(), lock),
            "ChainLockProof::new: guard is for another lock"
        );
        assert!(guard.is_held(), "ChainLockProof::new: guard does not hold its lock");
        assert!(lock.is_held(token), "ChainLockProof::new: lock is not held by this transaction");
        // SAFETY: Held: asserted above. Lifetime: `'p` is bounded by `token` and the unique borrow
        // of `guard`. Uniqueness and no release behind the proof: the caller's contract.
        let token = unsafe { LockToken::new() };
        Self { owner, token }
    }

    /// Mints a proof for `owner` when the lock is held by a holder the type system cannot see,
    /// checking that the active transaction owns `owner.lock()`.
    ///
    /// # Safety
    ///
    /// As [`Self::new`], and the invisible holder must keep the lock held for the whole of `'p`.
    ///
    /// # Panics
    ///
    /// Panics if the active transaction does not own `owner.lock()`; inside a reserved-token
    /// window call `sync_token` on the lock first.
    #[inline]
    #[track_caller]
    pub unsafe fn assume_held(owner: &'p O, token: &'p ChainLockTransactionToken) -> Self {
        assert!(
            owner.lock().is_held(token),
            "ChainLockProof::assume_held: lock is not held by this transaction"
        );
        // SAFETY: Held: asserted above. Lifetime: `'p` is bounded by `owner` and `token`. Held for
        // `'p`, uniqueness, and no release behind the proof: the caller's contract.
        let token = unsafe { LockToken::new() };
        Self { owner, token }
    }

    /// Mints a proof for `owner` without a runtime check.
    ///
    /// Prefer [`Self::retag`] when converting an existing proof to a related type that shares the
    /// same lock. Inside a reserved-token window, call `sync_token` on the lock and use the
    /// checked [`Self::new`] or [`Self::assume_held`] instead.
    ///
    /// # Safety
    ///
    /// In addition to the requirements of [`Self::assume_held`], the current execution context
    /// must hold `owner.lock()` for the entirety of `'p`.
    #[inline]
    pub unsafe fn assume_held_unchecked(owner: &'p O) -> Self {
        // SAFETY: Held for `'p`, uniqueness, and no release behind the proof: the caller's
        // contract.
        let token = unsafe { LockToken::new() };
        Self { owner, token }
    }

    /// Mints a proof for `owner` from the guard object that owns `owner.lock()`, with no
    /// runtime check.
    ///
    /// # Safety
    ///
    /// The caller is the guard object that owns `owner.lock()` (never deferred it)
    /// and holds it for the whole of `'p`, and at most one proof minted from it is live at a time
    /// (typically ensured by minting through `&mut self`).
    // `pub(crate)` spans the whole kernel crate; the `unsafe` contract is the gate.
    #[inline]
    pub(crate) unsafe fn from_acquiring_guard(owner: &'p O) -> Self {
        // SAFETY: Acquired and held for `'p`, and uniqueness: the caller's contract.
        let token = unsafe { LockToken::new() };
        Self { owner, token }
    }

    /// Returns the owner this proof is for.
    #[inline]
    pub const fn owner(&self) -> &'p O {
        self.owner
    }

    /// Returns the class token, for `KCell::get`.
    #[inline]
    pub const fn token(&self) -> &LockToken<'p, O::Class> {
        &self.token
    }

    /// Returns the class token mutably, for `KCell::get_mut`.
    #[inline]
    pub fn token_mut(&mut self) -> &mut LockToken<'p, O::Class> {
        &mut self.token
    }

    /// Splits the proof into its owner and mutable token, for `_mut` accessors that need both.
    #[inline]
    pub(crate) fn split_mut(&mut self) -> (&'p O, &mut LockToken<'p, O::Class>) {
        (self.owner, &mut self.token)
    }

    /// Reborrows `self` with a shorter lifetime tied to `&mut self`.
    #[inline]
    pub fn reborrow(&mut self) -> ChainLockProof<'_, O> {
        // SAFETY: Held: `self` proves `self.owner.lock()` is held for `'p`, which outlives the
        // borrow of `self`. Uniqueness: `self` is borrowed mutably for the lifetime of the
        // returned proof, so `self` cannot be used while the returned proof lives.
        let token = unsafe { LockToken::new() };
        ChainLockProof { owner: self.owner, token }
    }

    /// Converts this proof into a proof for `other` when both owners share the same lock instance
    /// (for example, an `OwnedWaitQueue` and its embedded `WaitQueueBase`).
    ///
    /// `self` is borrowed mutably for the lifetime of the returned proof, so only one of the two
    /// proofs can be used at a time.
    ///
    /// # Panics
    ///
    /// Panics if `self.owner().lock()` and `other.lock()` are not the same lock instance.
    #[inline]
    #[track_caller]
    pub fn retag<'s, P: ChainLocked>(&'s mut self, other: &'s P) -> ChainLockProof<'s, P> {
        assert!(
            core::ptr::eq(self.owner.lock().raw(), other.lock().raw()),
            "ChainLockProof::retag: owners do not share a lock"
        );
        // SAFETY: Held: `other.lock()` is the lock `self` proves held (asserted above), for `'s`
        // because `self` is borrowed for `'s`. Uniqueness: that borrow is unique.
        let token = unsafe { LockToken::new() };
        ChainLockProof { owner: other, token }
    }
}

impl<O: ChainLocked> core::fmt::Debug for ChainLockProof<'_, O> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ChainLockProof")
            .field("owner", &core::ptr::from_ref(self.owner))
            .field("owner_type", &core::any::type_name::<O>())
            .finish()
    }
}

/// A [`ChainLockGuard`] that owns a [`ChainLocked`] owner's lock and remembers the owner.
///
/// Every safe constructor acquires `owner.lock()`, [`Self::adopt`] is `unsafe` and verifies that
/// `owner.lock()` is held, and the inner guard and token are never exposed, so an `OwnerGuard`
/// that exists holds `owner.lock()` until it is dropped or consumed (via [`Self::release`] or
/// `into_unguarded`). That is what lets [`Self::proof`] be safe and check-free.
#[must_use = "if unused, the owner's ChainLock will immediately be released"]
pub struct OwnerGuard<'a, O: ChainLocked> {
    // Safety invariant: `guard` was created by a constructor of this type that acquired or adopted
    // `owner.lock()`, and `guard.is_held()` stays `true` until `self` is consumed or dropped.
    guard: ChainLockGuard<'a>,
    owner: &'a O,
}

impl<'a, O: ChainLocked> OwnerGuard<'a, O> {
    /// Unconditionally acquires `owner.lock()` as the first lock in the chain.
    #[inline]
    pub fn new(owner: &'a O, token: &'a ChainLockTransactionToken) -> Self {
        Self { guard: ChainLockGuard::new(owner.lock().raw(), token), owner }
    }

    /// Adopts `owner.lock()`, which is already held by the active transaction, taking ownership of
    /// releasing it.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `owner.lock()` is currently held by the active transaction,
    /// that no other guard or proof for `owner.lock()` will conflict or double-release it, and
    /// that the returned `OwnerGuard` now owns responsibility for releasing `owner.lock()`.
    ///
    /// # Panics
    ///
    /// Panics if the active transaction does not own `owner.lock()`.
    #[inline]
    #[track_caller]
    pub unsafe fn adopt(owner: &'a O, token: &'a ChainLockTransactionToken) -> Self {
        assert!(
            owner.lock().is_held(token),
            "OwnerGuard::adopt: lock is not held by this transaction"
        );
        Self { guard: ChainLockGuard::adopt(owner.lock().raw(), token), owner }
    }

    /// Conditionally acquires `owner.lock()`, returning `None` if the transaction must back off.
    #[inline]
    #[must_use]
    pub fn acquire_or_backoff(owner: &'a O, token: &'a ChainLockTransactionToken) -> Option<Self> {
        let mut guard = ChainLockGuard::defer(owner.lock().raw(), token);
        // `Self` is only returned once the deferred guard has actually acquired the lock.
        if guard.acquire_or_backoff() { Some(Self { guard, owner }) } else { None }
    }

    /// Conditionally acquires `owner.lock()` with default options, returning `None` on failure.
    #[inline]
    #[must_use]
    pub fn try_acquire(owner: &'a O, token: &'a ChainLockTransactionToken) -> Option<Self> {
        let mut guard = ChainLockGuard::defer(owner.lock().raw(), token);
        // `Self` is only returned once the deferred guard has actually acquired the lock.
        if guard.try_acquire() { Some(Self { guard, owner }) } else { None }
    }

    /// Mints a proof that `owner.lock()` is held: identity and hold by construction, uniqueness
    /// by the `&mut` borrow.
    #[inline]
    pub fn proof(&mut self) -> ChainLockProof<'_, O> {
        // SAFETY: Acquired and held: the type's safety invariant. Lifetime and uniqueness: the
        // proof is bounded by this `&mut self` borrow.
        unsafe { ChainLockProof::from_acquiring_guard(self.owner) }
    }

    /// Releases the lock. Consumes the guard so that a released guard cannot mint a proof.
    #[inline]
    pub fn release(self) {
        // Dropping the inner guard releases the lock exactly once.
        drop(self);
    }

    /// Consumes this guard without releasing `owner.lock()` and returns `owner`, transferring
    /// responsibility for releasing `owner.lock()` to the caller (e.g., to a `TA_REL(get_lock())`
    /// routine).
    #[inline]
    pub(crate) fn into_unguarded(mut self) -> &'a O {
        self.guard.unguard();
        self.owner
    }

    /// Returns the owner whose lock this guard holds.
    #[inline]
    pub const fn owner(&self) -> &'a O {
        self.owner
    }

    /// Always `true` by the type's invariant; forwards to [`ChainLockGuard::is_held`] for parity.
    #[inline]
    pub const fn is_held(&self) -> bool {
        self.guard.is_held()
    }
}

impl<O: ChainLocked> core::fmt::Debug for OwnerGuard<'_, O> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OwnerGuard")
            .field("owner", &core::ptr::from_ref(self.owner))
            .field("owner_type", &core::any::type_name::<O>())
            .field("held", &self.guard.is_held())
            .finish()
    }
}

/// Tests for [`ClassedChainLock`], [`ChainLocked`], [`ChainLockProof`], and [`OwnerGuard`].
// Named `proof_tests` rather than `tests` because `#[unittest::suite]` makes the module `pub`
// and `mod.rs` glob re-exports this module alongside `chainlock_transaction` (which has `tests`).
//
// A wrong-lock / not-held mint (which must panic) is deliberately not tested: ktests cannot catch
// panics. `OwnerGuard::acquire_or_backoff`'s backoff path needs a contending transaction on
// another CPU; the underlying `ChainLock::acquire_or_backoff` is exercised across CPUs by
// `multi_cpu_contention` in `chainlock_transaction.rs`, so only the success path is tested here.
#[cfg(ktest)]
#[unittest::suite(name = "chainlock_proof")]
mod proof_tests {
    use concurrent::{Action, DONE, TransactionResult};
    use ksync::KCell;
    use unittest::{expect_eq, expect_true};

    use crate::kconcurrent::{
        ChainLockTransaction, IRQ_SAVE_OPTION, clt_tag, single_chain_lock_guard_owned,
    };

    /// Marker class for the tests below.
    enum TestClass {}

    /// Owning type pairing a classed lock with the data it guards, in the style of
    /// `WaitQueueBase`.
    struct Guarded {
        lock: ClassedChainLock<TestClass>,
        value: KCell<u32, TestClass>,
    }

    // SAFETY: `lock` guards `value`, the only `KCell<_, TestClass>` field.
    unsafe impl ChainLocked for Guarded {
        type Class = TestClass;
        fn lock(&self) -> &ClassedChainLock<TestClass> {
            &self.lock
        }
    }

    impl ChainLockProof<'_, Guarded> {
        fn value(&self) -> &u32 {
            // SAFETY: `self` proves `owner.lock()` is held, and `owner.lock()` guards `value`.
            unsafe { self.owner().value.get(self.token()) }
        }

        fn value_mut(&mut self) -> &mut u32 {
            let (owner, token) = self.split_mut();
            // SAFETY: As in `value`; the unique borrow of `self` (and transitively of the guard)
            // ensures no other reference to `value` is live.
            unsafe { owner.value.get_mut(token) }
        }
    }

    /// Wrapper sharing `inner`'s lock, in the style of `OwnedWaitQueue` over `WaitQueueBase`.
    struct Outer {
        inner: Guarded,
        extra: KCell<u32, TestClass>,
    }

    // SAFETY: `inner.lock` guards `extra` (and, via `inner`, `inner.value`).
    unsafe impl ChainLocked for Outer {
        type Class = TestClass;
        fn lock(&self) -> &ClassedChainLock<TestClass> {
            self.inner.lock()
        }
    }

    impl ChainLockProof<'_, Outer> {
        fn extra_mut(&mut self) -> &mut u32 {
            let (owner, token) = self.split_mut();
            // SAFETY: `self` proves `owner.lock()` is held, which guards `extra`; unique borrow.
            unsafe { owner.extra.get_mut(token) }
        }

        fn as_inner(&mut self) -> ChainLockProof<'_, Guarded> {
            let inner = &self.owner().inner;
            self.retag(inner)
        }
    }

    /// Verifies minting from a held guard, reading/writing through proof accessors, and `retag`.
    #[test]
    fn proof_from_guard() {
        let outer = Outer {
            inner: Guarded { lock: ClassedChainLock::new(), value: KCell::new(7) },
            extra: KCell::new(0),
        };
        let mut owner_matches = false;
        let mut before = 0;
        let mut after = 0;
        let mut extra = 0;

        ChainLockTransaction::until_done(IRQ_SAVE_OPTION, clt_tag!("proof_from_guard"), |token| {
            // SAFETY: The raw lock is used only to build `guard`, which is the sole holder; it is
            // released through `guard.release()` after every proof has been dropped.
            let lock = unsafe { outer.lock().as_chain_lock() };
            let mut guard = ChainLockGuard::new(lock, token);
            ChainLockTransaction::finalize(token);
            {
                // SAFETY: `guard` is the only holder of `outer`'s lock in this transaction.
                let mut proof = unsafe { ChainLockProof::new(&outer, &mut guard, token) };
                owner_matches = core::ptr::eq(proof.owner(), &outer);
                *proof.extra_mut() = 1;
                {
                    let mut inner = proof.as_inner();
                    before = *inner.value();
                    *inner.value_mut() += 35;
                    after = *inner.value();
                }
                *proof.extra_mut() += 1;
                extra = *proof.as_inner().retag(&outer).extra_mut();
                // NOTE: `guard.release()` here would be rejected by the borrow checker while
                // `proof` is live, since `proof` borrows `guard` uniquely.
            }
            guard.release();
            DONE
        });

        expect_true!(owner_matches);
        expect_eq!(before, 7);
        expect_eq!(after, 42);
        expect_eq!(extra, 2);
        expect_true!(outer.lock().is_unlocked());
    }

    /// Verifies `assume_held` round trip inside a transaction that holds the lock directly.
    #[test]
    fn assume_held_round_trip() {
        let owner = Guarded { lock: ClassedChainLock::new(), value: KCell::new(3) };
        let mut held_in_tx = false;
        let mut value = 0;

        ChainLockTransaction::until_done(IRQ_SAVE_OPTION, clt_tag!("assume_held"), |token| {
            // SAFETY: The raw lock is acquired here and released below only after `proof` has
            // been dropped; no proof is live across either call.
            let lock = unsafe { owner.lock().as_chain_lock() };
            lock.acquire_first_in_chain(token);
            ChainLockTransaction::finalize(token);
            held_in_tx = owner.lock().is_held(token);
            {
                // SAFETY: The lock was acquired above by this transaction, is released only after
                // `proof` is dropped, and has no other holder in this transaction.
                let mut proof = unsafe { ChainLockProof::assume_held(&owner, token) };
                *proof.value_mut() *= 3;
                value = *proof.value();
            }
            lock.release(token);
            DONE
        });

        expect_true!(held_in_tx);
        expect_eq!(value, 9);
        expect_true!(owner.lock().is_unlocked());
    }

    /// Verifies `OwnerGuard::{new, proof, into_unguarded, adopt, release}` and `reborrow`.
    #[test]
    fn owner_guard_proof() {
        let owner = Guarded { lock: ClassedChainLock::new(), value: KCell::new(10) };
        let mut held = false;
        let mut owner_matches = false;
        let mut before = 0;
        let mut after = 0;
        let mut unlocked_after_release = false;

        ChainLockTransaction::until_done(IRQ_SAVE_OPTION, clt_tag!("owner_guard"), |token| {
            let mut guard = OwnerGuard::new(&owner, token);
            ChainLockTransaction::finalize(token);
            held = guard.is_held() && owner.lock().is_held(token);
            owner_matches = core::ptr::eq(guard.owner(), &owner);
            {
                let mut proof = guard.proof();
                before = *proof.reborrow().value();
                *proof.reborrow().value_mut() += 5;
            }
            let unguarded = guard.into_unguarded();
            // SAFETY: `guard.into_unguarded()` disarmed `guard` without releasing `owner.lock()`,
            // so `owner.lock()` remains held by this transaction with no other live guard or
            // proof.
            let mut adopted = unsafe { OwnerGuard::adopt(unguarded, token) };
            after = *adopted.proof().value();
            // NOTE: `adopted.release()` consumes the guard, so neither a live proof nor a later
            // `adopted.proof()` can exist past this point.
            adopted.release();
            unlocked_after_release = owner.lock().is_unlocked();
            DONE
        });

        expect_true!(held);
        expect_true!(owner_matches);
        expect_eq!(before, 10);
        expect_eq!(after, 15);
        expect_true!(unlocked_after_release);
        expect_true!(owner.lock().is_unlocked());
    }

    /// Verifies `OwnerGuard::{acquire_or_backoff, try_acquire}` success paths and drop-release.
    #[test]
    fn owner_guard_acquire_or_backoff() {
        let first = Guarded { lock: ClassedChainLock::new(), value: KCell::new(1) };
        let second = Guarded { lock: ClassedChainLock::new(), value: KCell::new(2) };
        let mut sum = 0;
        let mut both_held = false;

        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("owner_backoff"),
            |token| -> TransactionResult<()> {
                let mut g1 = OwnerGuard::new(&first, token);
                // Uncontended, so these succeed on the first attempt: the `acquire_or_backoff`
                // composition is success-path tested only. The backoff arm shows the idiom
                // (`until_done` requires no locks held when returning `Backoff`).
                let Some(g2) = OwnerGuard::acquire_or_backoff(&second, token) else {
                    drop(g1);
                    return Action::Backoff.into();
                };
                // Dropping releases; re-acquiring with `try_acquire` must also happen before
                // `finalize`, since no new lock may be acquired in a finalized transaction.
                drop(g2);
                let Some(mut g2) = OwnerGuard::try_acquire(&second, token) else {
                    panic!("uncontended try_acquire failed");
                };
                ChainLockTransaction::finalize(token);
                both_held = first.lock().is_held(token) && second.lock().is_held(token);
                sum = *g1.proof().value() + *g2.proof().value();
                drop(g2);
                drop(g1);
                TransactionResult::DONE
            },
        );

        expect_true!(both_held);
        expect_eq!(sum, 3);
        expect_true!(first.lock().is_unlocked());
        expect_true!(second.lock().is_unlocked());
    }

    /// Verifies the safe, check-free owned scope path via `single_chain_lock_guard_owned!`.
    #[test]
    fn owned_scope_proof() {
        let owner = Guarded { lock: ClassedChainLock::new(), value: KCell::new(20) };
        let value;
        {
            single_chain_lock_guard_owned!(
                scope,
                &owner,
                IRQ_SAVE_OPTION,
                clt_tag!("owned_scope_proof")
            );
            expect_true!(owner.lock().is_held(scope.token()));
            expect_true!(core::ptr::eq(scope.owner(), &owner));
            {
                let mut proof = scope.proof();
                *proof.value_mut() += 22;
            }
            value = *scope.proof().value();
        }
        expect_eq!(value, 42);
        expect_true!(owner.lock().is_unlocked());
    }

    /// Verifies `assume_held_unchecked` round trip after a direct acquire.
    #[test]
    fn assume_held_unchecked_round_trip() {
        let owner = Guarded { lock: ClassedChainLock::new(), value: KCell::new(6) };
        let mut value = 0;

        ChainLockTransaction::until_done(IRQ_SAVE_OPTION, clt_tag!("assume_unchecked"), |token| {
            // SAFETY: The raw lock is acquired here and released below only after `proof` has
            // been dropped; no proof is live across either call.
            let lock = unsafe { owner.lock().as_chain_lock() };
            lock.acquire_first_in_chain(token);
            ChainLockTransaction::finalize(token);
            {
                // SAFETY: The lock was acquired above by this transaction, is released only after
                // `proof` is dropped, and has no other holder in this transaction.
                let mut proof = unsafe { ChainLockProof::assume_held_unchecked(&owner) };
                *proof.value_mut() *= 7;
                value = *proof.value();
            }
            lock.release(token);
            DONE
        });

        expect_eq!(value, 42);
        expect_true!(owner.lock().is_unlocked());
    }
}
