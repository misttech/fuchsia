// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::marker::PhantomPinned;
use core::pin::{Pin, pin};
use core::ptr;
use fbl::{DoublyLinkedList, DoublyLinkedListNode, is_sentinel_ptr};
use ksync::{KCell, LockToken, declare_singleton_critical_mutex};
use lazy_init::LazyInit;
use pin_init::{PinInit, pin_data, pin_init};
use vm_cow_pages_bindings as bindings;

use crate::platform_rs::timer::current_mono_time;
pub use crate::vm::vm_cow_pages::DiscardablePageCounts;
use crate::vm::vm_cow_pages::{VmCowPages, VmCowPagesLock, VmCowPagesLockClass};
use zx_status::Status;
use zx_types::{ZX_TIME_INFINITE, zx_instant_mono_t, zx_status_t};

/// Tracks the current state of a discardable VMO, depending on the lock count and whether
/// it has been discarded.
///
/// State transitions work as follows:
/// 1. `Unreclaimable` -> `Reclaimable`: When the lock count changes from 1 to 0.
/// 2. `Reclaimable` -> `Unreclaimable`: When the lock count changes from 0 to 1. The VMO
///    remains `Unreclaimable` for any non-zero lock count.
/// 3. `Reclaimable` -> `Discarded`: When a VMO with lock count 0 is discarded.
/// 4. `Discarded` -> `Unreclaimable`: When a discarded VMO is locked again.
///
/// We start off with state `Unset`, so a discardable VMO must be locked at least once to
/// opt into the above state transitions.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum DiscardableState {
    /// Initial unset state.
    #[default]
    Unset = 0,
    /// VMO is unlocked and eligible for reclamation.
    Reclaimable = 1,
    /// VMO is locked and not eligible for reclamation.
    Unreclaimable = 2,
    /// VMO has been discarded.
    Discarded = 3,
}

// Ensure DiscardableState discriminant values match C++ DiscardableVmoTracker::DiscardableState.
zr::static_assert!(core::mem::size_of::<DiscardableState>() == 1);
zr::static_assert!(DiscardableState::Unset as u8 == 0);
zr::static_assert!(DiscardableState::Reclaimable as u8 == 1);
zr::static_assert!(DiscardableState::Unreclaimable as u8 == 2);
zr::static_assert!(DiscardableState::Discarded as u8 == 3);

/// The result of a successful [`DiscardableVmoTracker::lock_discardable_locked`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct LockOutcome {
    /// Whether the VMO had been discarded (and not locked again since) prior to this lock.
    pub(crate) was_discarded: bool,
    /// Whether the VMO was moved between the discardable reclaimable and non-reclaimable lists.
    pub(crate) updated_reclaim_candidates: bool,
}

/// Tracks state relevant for discardable VMOs. This struct offers separation of the logic
/// required for discardable VMO management; the members are still protected by the owning
/// VmCowPages' lock.
///
/// All state that is mutated after construction lives in `UnsafeCell`s (directly, or via
/// `KCell` / `DoublyLinkedListNode`), so it is sound to hold a shared reference to a tracker
/// while its state is concurrently mutated (under the appropriate locks) by other threads.
//
// TODO(https://fxbug.dev/531878739): Once VmCowPages is ported to Rust, tie the `KCell`s below to
// the owning VmCowPages' lock *instance* (e.g. with `#[guarded]` / `#[guarded_by]`), rather than
// only to `VmCowPagesLockClass`, so that the `_locked` methods no longer need to be `unsafe`.
#[repr(C)]
#[derive(fbl::DoublyLinkedListContainable)]
pub struct DiscardableVmoTracker {
    #[dll_node]
    node: DoublyLinkedListNode<DiscardableVmoTracker>,

    // Count of outstanding lock operations. A non-zero count prevents the kernel from
    // discarding / evicting pages from the VMO to relieve memory pressure (currently only
    // applicable if `VmObjectPaged::kDiscardable` is set). Note that this does not prevent
    // removal of pages by other means, like decommitting or resizing, since those are
    // explicit actions driven by the user, not by the kernel directly.
    lock_count: KCell<u64, VmCowPagesLockClass>,

    // Timestamp of the last unlock operation that changed a discardable vmo's state to
    // `DiscardableState::Reclaimable`. Used to determine whether the vmo was accessed
    // too recently to be discarded.
    //
    // Note that this is currently written but never read.
    last_unlock_timestamp: KCell<zx_instant_mono_t, VmCowPagesLockClass>,

    // The current state of a discardable vmo, depending on the lock count and whether it
    // has been discarded. See `DiscardableState` for the state transitions.
    discardable_state: KCell<DiscardableState, VmCowPagesLockClass>,

    _padding: [u8; 7],

    // Back reference to the owning VmCowPages. Set at creation time.
    //
    // This is written only by `init_cow_pages` (before the tracker is on any discardable list,
    // so before any other thread can observe it) and by `remove_from_discardable_list_locked`
    // (with *both* the owning VmCowPages' lock and `DiscardableVmosLock` held). It may therefore
    // be read while holding either of those locks; in particular `debug_discardable_page_counts`
    // reads it holding only `DiscardableVmosLock`.
    cow: UnsafeCell<*mut bindings::VmCowPages>,

    _pin: PhantomPinned,
}

// Memory layout static assertions. The C++ facade (`discardable_vmo_tracker.h`) reserves
// `OpaqueStorage<48, 8>` for this struct; tie the Rust layout to the bindgen view of that facade.
zr::static_assert!(
    core::mem::size_of::<DiscardableVmoTracker>()
        == core::mem::size_of::<bindings::DiscardableVmoTracker>()
);
zr::static_assert!(
    core::mem::align_of::<DiscardableVmoTracker>()
        == core::mem::align_of::<bindings::DiscardableVmoTracker>()
);
zr::static_assert!(core::mem::size_of::<DiscardableVmoTracker>() == 48);
zr::static_assert!(core::mem::align_of::<DiscardableVmoTracker>() == 8);
zr::static_assert!(core::mem::offset_of!(DiscardableVmoTracker, node) == 0);
zr::static_assert!(core::mem::offset_of!(DiscardableVmoTracker, lock_count) == 16);
zr::static_assert!(core::mem::offset_of!(DiscardableVmoTracker, last_unlock_timestamp) == 24);
zr::static_assert!(core::mem::offset_of!(DiscardableVmoTracker, discardable_state) == 32);
zr::static_assert!(core::mem::offset_of!(DiscardableVmoTracker, _padding) == 33);
zr::static_assert!(core::mem::offset_of!(DiscardableVmoTracker, cow) == 40);

// SAFETY: DiscardableVmoTracker can be transferred across threads while owned by VmCowPages.
unsafe impl Send for DiscardableVmoTracker {}
// SAFETY: All state mutated after construction is in `UnsafeCell`s, and access to it is
// synchronized by the owning VmCowPages' lock and/or `DiscardableVmosLock` as documented on
// each field.
unsafe impl Sync for DiscardableVmoTracker {}

/// A cursor over a discardable list, maintaining its position across lock releases.
///
/// Cursors are registered in `DiscardableVmoLists::cursors` while in use, and every removal of
/// an element from a discardable list is preceded by `advance_cursors`, so `current` is always
/// either null or an element that is still on the list the cursor is walking.
#[derive(fbl::DoublyLinkedListContainable)]
struct Cursor {
    #[dll_node]
    node: DoublyLinkedListNode<Cursor>,
    // The next element to be returned by `next`, or null at the end of the list.
    //
    // Protected by `DiscardableVmosLock`. This is an `UnsafeCell` so that it can be updated
    // through the shared references handed out when iterating the cursors list in
    // `advance_cursors`.
    current: UnsafeCell<*mut DiscardableVmoTracker>,
    _pin: PhantomPinned,
}

// SAFETY: DiscardableVmoTracker pointers inside `Cursor` can safely be transferred across threads.
unsafe impl Send for Cursor {}
// SAFETY: Access to `Cursor`'s intrusive node and to `current` is protected by
// `DiscardableVmosLock`.
unsafe impl Sync for Cursor {}

// Returns the element following `cur` in its discardable list, or null if `cur` is the last
// element.
//
// # Safety
//
// `DiscardableVmosLock` must be held, and `cur` must point to a live tracker that is on a
// discardable list.
unsafe fn next_or_null(cur: *const DiscardableVmoTracker) -> *mut DiscardableVmoTracker {
    // SAFETY: The caller guarantees `cur` is a live tracker on a discardable list, and that
    // `DiscardableVmosLock`, which protects the list linkage, is held.
    let next = unsafe { *(*cur).node.next.get() };
    if is_sentinel_ptr(next) || next.is_null() { ptr::null_mut() } else { next }
}

impl Cursor {
    fn new(current: *mut DiscardableVmoTracker) -> Self {
        Self {
            node: DoublyLinkedListNode::new(),
            current: UnsafeCell::new(current),
            _pin: PhantomPinned,
        }
    }

    // Returns the next element to be returned by `next`, or null at the end of the list.
    //
    // The caller must hold `DiscardableVmosLock` (proven by `_token`).
    fn current(&self, _token: &LockToken<'_, DiscardableVmosLock>) -> *mut DiscardableVmoTracker {
        // SAFETY: `current` is protected by `DiscardableVmosLock`, which we hold.
        unsafe { *self.current.get() }
    }

    // Sets the next element to be returned by `next`.
    //
    // The caller must hold `DiscardableVmosLock` (proven by `_token`).
    fn set_current(
        &self,
        current: *mut DiscardableVmoTracker,
        _token: &LockToken<'_, DiscardableVmosLock>,
    ) {
        // SAFETY: `current` is protected by `DiscardableVmosLock`, which we hold. No reference to
        // the contents of the cell is ever handed out, so this write cannot alias one.
        unsafe { *self.current.get() = current };
    }

    // Advance the cursor and return the next element or null if at the end of the list.
    //
    // Once `next` has returned null, all subsequent calls will return null.
    //
    // The caller must hold `DiscardableVmosLock` (proven by `token`).
    fn next(&self, token: &LockToken<'_, DiscardableVmosLock>) -> *mut DiscardableVmoTracker {
        let cur = self.current(token);
        if cur.is_null() {
            return ptr::null_mut();
        }

        // SAFETY: We hold `DiscardableVmosLock`. `cur` is non-null, so by the cursor invariant
        // (every erase from a discardable list is preceded by `advance_cursors`) it is still a
        // live element of the list this cursor is walking.
        self.set_current(unsafe { next_or_null(cur) }, token);
        cur
    }

    // If the next element is `h`, advance the cursor past it.
    //
    // # Safety
    //
    // The caller must hold `DiscardableVmosLock`.
    unsafe fn advance_if(&self, h: *const DiscardableVmoTracker) {
        // SAFETY: The caller holds `DiscardableVmosLock`, which protects `current`.
        let current = unsafe { &mut *self.current.get() };
        if !current.is_null() && ptr::eq(*current, h) {
            // SAFETY: The caller holds `DiscardableVmosLock`. `*current` is non-null, so by the
            // cursor invariant it is still a live element of the list this cursor is walking
            // (`h` has not been erased yet; `advance_cursors` is called before the erase).
            *current = unsafe { next_or_null(*current) };
        }
    }
}

// Advances all the cursors in `lists.cursors`, calling `advance_if(h)` on each cursor.
//
// Holding a reference to `lists` proves that the caller holds `DiscardableVmosLock`, the global
// lock protecting the cursors.
fn advance_cursors(lists: &DiscardableVmoLists, h: *const DiscardableVmoTracker) {
    for cursor in lists.cursors.iter() {
        // SAFETY: A reference to the lists can only be obtained (via `get_lists`) while holding
        // `DiscardableVmosLock`.
        unsafe { cursor.advance_if(h) };
    }
}

/// RAII guard that unlinks a `Cursor` from `DiscardableVmoLists::cursors` upon scope exit.
struct CursorGuard<'a> {
    cursor: Pin<&'a Cursor>,
}

impl<'a> CursorGuard<'a> {
    // # Safety
    //
    // `cursor` must be in `DiscardableVmoLists::cursors`, and the returned guard must be dropped
    // while `DiscardableVmosLock` is held and no reference obtained from `get_lists` is live.
    unsafe fn new(cursor: Pin<&'a Cursor>) -> Self {
        Self { cursor }
    }
}

impl Drop for CursorGuard<'_> {
    fn drop(&mut self) {
        // The lists are re-derived from their static storage here, rather than holding on to a
        // reference obtained before the walk: while `DiscardableVmosLock` was temporarily released
        // in `debug_discardable_page_counts`, other threads may have obtained (and released) their
        // own references to the lists.
        //
        // SAFETY: Per `CursorGuard::new`'s contract, `DiscardableVmosLock` is held for the
        // duration of this function and no other reference to the lists is live.
        let mut token = unsafe { LockToken::<DiscardableVmosLock>::new() };
        let lists = get_lists(&mut token);
        debug_assert!(self.cursor.node.in_container());
        // SAFETY: Per `CursorGuard::new`'s contract, `cursor` is in `lists.cursors`.
        unsafe {
            lists.project().cursors.erase(self.cursor.get_ref());
        }
    }
}

// Lock that protects the global discardable lists.
// This lock can be acquired with the vmo's lock held. To prevent deadlocks, if both locks are
// required the order of locking should always be 1) vmo's lock, and then 2) DiscardableVmosLock.
declare_singleton_critical_mutex!(DiscardableVmosLock);

// Two global lists of discardable vmos:
// - `reclaim_candidates` tracks discardable vmos that are eligible for reclamation
// and haven't been reclaimed yet.
// - `non_reclaim_candidates` tracks all other discardable VMOs.
// The lists are protected by the `DiscardableVmosLock`, and updated based on a discardable vmo's
// state changes (lock, unlock, or discard).
#[pin_data]
struct DiscardableVmoLists {
    #[pin]
    reclaim_candidates: DoublyLinkedList<*mut DiscardableVmoTracker>,
    #[pin]
    non_reclaim_candidates: DoublyLinkedList<*mut DiscardableVmoTracker>,
    // The list of all outstanding cursors iterating over the discardable lists:
    // `reclaim_candidates` and `non_reclaim_candidates`. The cursors should be advanced
    // (by calling `advance_if()`) before removing any element from the discardable lists.
    #[pin]
    cursors: DoublyLinkedList<*mut Cursor>,
}

// SAFETY: The lists hold raw pointers to trackers and cursors, all accesses to which are
// synchronized by `DiscardableVmosLock`.
unsafe impl Send for DiscardableVmoLists {}

impl DiscardableVmoLists {
    fn init() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            reclaim_candidates <- DoublyLinkedList::new(),
            non_reclaim_candidates <- DoublyLinkedList::new(),
            cursors <- DoublyLinkedList::new(),
        })
    }
}

// The global discardable lists, protected by `DiscardableVmosLock`.
//
// The lists cannot be a plain `static` because `DoublyLinkedList::new()` is a `PinInit` (the
// list's sentinel encodes the address of the list itself), not a `const fn`. Instead, they are
// initialized in place during early boot by `initialize_discardable_lists`.
static LISTS: LazyInit<KCell<DiscardableVmoLists, DiscardableVmosLock>> = LazyInit::uninit();

fn initialize_discardable_lists(_level: init::LkInitLevel) {
    // SAFETY: Single-threaded initialization during early boot before concurrency. Discardable
    // VMOs can only be created by user mode, and the lists are otherwise only walked by the
    // scanner and memory stats, all of which happen well after this init level.
    unsafe {
        let _ = Pin::static_ref(&LISTS).init_pin(KCell::pin_init(DiscardableVmoLists::init()));
    }
}

init::lk_init_hook!(
    discardable_vmo_lists_init,
    initialize_discardable_lists,
    init::LK_INIT_LEVEL_EARLIEST
);

// Returns the global discardable lists.
//
// The returned reference borrows the `DiscardableVmosLock` token mutably, so for as long as it is
// live no other reference can be obtained through the same guard, and since `DiscardableVmosLock`
// is a mutex, no other thread can obtain one either. Callers must never create a second
// reference to the lists by other means (e.g. via a separately minted `LockToken`) while one
// returned from here is live.
fn get_lists<'a>(
    token: &'a mut LockToken<'_, DiscardableVmosLock>,
) -> Pin<&'a mut DiscardableVmoLists> {
    // SAFETY: `DiscardableVmosLock` is a singleton, so `token` is necessarily for the lock
    // instance that guards `LISTS`. `LISTS` is a static initialized in place via `init_pin`, so
    // the lists are never moved.
    unsafe { LISTS.get().get_pinned_mut_locked(token) }
}

impl DiscardableVmoTracker {
    /// Creates a new, uninitialized `DiscardableVmoTracker`.
    pub const fn new() -> Self {
        Self {
            node: DoublyLinkedListNode::new(),
            lock_count: KCell::new(0),
            last_unlock_timestamp: KCell::new(ZX_TIME_INFINITE),
            discardable_state: KCell::new(DiscardableState::Unset),
            _padding: [0; 7],
            cow: UnsafeCell::new(ptr::null_mut()),
            _pin: PhantomPinned,
        }
    }

    /// Initializes the back reference to the owning `VmCowPages`.
    ///
    /// # Safety
    ///
    /// `cow` must point to an initialized, live `VmCowPages` whose lifetime outlives `self`
    /// (or until `self` is unlinked via `remove_from_discardable_list_locked`). `self` must not
    /// yet be on a discardable list (i.e. it must never have been locked), so that no other
    /// thread can concurrently access the back reference.
    pub(crate) unsafe fn init_cow_pages(&self, cow: *mut bindings::VmCowPages) {
        // SAFETY: The caller guarantees that `self` is not yet published on any discardable list,
        // so nothing else can be accessing `self.cow`.
        let self_cow = unsafe { &mut *self.cow.get() };
        // Should be initializing the back reference exactly once.
        assert!(self_cow.is_null());
        assert!(!cow.is_null());
        *self_cow = cow;
    }

    // Returns the back reference to the owning VmCowPages.
    //
    // # Safety
    //
    // The caller must hold the owning VmCowPages' lock or `DiscardableVmosLock`, or otherwise
    // guarantee that `remove_from_discardable_list_locked` cannot run concurrently (see the
    // comment on the `cow` field).
    unsafe fn cow(&self) -> *mut bindings::VmCowPages {
        // SAFETY: Per the caller's guarantee, there is no concurrent write to `self.cow`.
        unsafe { *self.cow.get() }
    }

    /// Remove a discardable object from whichever global discardable list it is in.
    /// Called from the VmCowPages destructor. Also resets the `cow` back reference.
    ///
    /// # Safety
    ///
    /// The caller must hold the owning VMO lock (`cow.lock()`) and `token` must be the token for
    /// that lock instance; a `LockToken<VmCowPagesLockClass>` alone only proves that *some* lock
    /// of that class is held.
    pub(crate) unsafe fn remove_from_discardable_list_locked(
        self: Pin<&Self>,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
    ) {
        ksync::lock!(let mut guard = DiscardableVmosLock::lock());
        // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
        let state = unsafe { self.discardable_state.get_mut(token) };
        if *state == DiscardableState::Unset {
            return;
        }

        // SAFETY: We hold both the owning VmCowPages' lock and `DiscardableVmosLock`.
        debug_assert!(!unsafe { self.cow() }.is_null());
        debug_assert!(self.node.in_container());

        let lists = get_lists(guard.token_mut());
        advance_cursors(&lists, self.get_ref());
        let lists = lists.project();

        if *state == DiscardableState::Reclaimable {
            // SAFETY: `self` is currently in `reclaim_candidates`, and all cursors have been
            // advanced past it.
            unsafe {
                lists.reclaim_candidates.erase(self.get_ref());
            }
        } else {
            // SAFETY: `self` is currently in `non_reclaim_candidates`, and all cursors have been
            // advanced past it.
            unsafe {
                lists.non_reclaim_candidates.erase(self.get_ref());
            }
        }

        *state = DiscardableState::Unset;
        // SAFETY: We hold both the owning VmCowPages' lock and `DiscardableVmosLock`, which is
        // what writing `cow` requires.
        unsafe {
            *self.cow.get() = ptr::null_mut();
        }
    }

    /// Lock and unlock functions. Returns `Ok` if the operation succeeded or an error code
    /// if it failed. On success, also returns whether the VMO was moved between the discardable
    /// reclaimable and non-reclaimable lists. The intent of this is to inform the caller if they
    /// might need to update any book-keeping depending on whether the VMO becomes reclaimable or
    /// unreclaimable.
    ///
    /// `lock_discardable_locked` fails (with `Status::UNAVAILABLE`) only if `try_lock` is set and
    /// the VMO has been discarded.
    ///
    /// # Safety
    ///
    /// The caller must hold the owning VMO lock (`cow.lock()`) and `token` must be the token for
    /// that lock instance.
    pub(crate) unsafe fn lock_discardable_locked(
        self: Pin<&Self>,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        try_lock: bool,
    ) -> Result<LockOutcome, Status> {
        // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
        let (state, lock_count) =
            unsafe { (*self.discardable_state.get(token), *self.lock_count.get(token)) };

        let mut was_discarded = false;
        if state == DiscardableState::Discarded {
            debug_assert!(lock_count == 0);
            was_discarded = true;
            if try_lock {
                return Err(Status::UNAVAILABLE);
            }
        }

        let mut updated_reclaim_candidates = false;
        if lock_count == 0 {
            // Lock count transition from 0 -> 1. Change state to unreclaimable.
            // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
            updated_reclaim_candidates = unsafe {
                self.update_discardable_state_locked(token, DiscardableState::Unreclaimable)
            };
        }
        // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
        *unsafe { self.lock_count.get_mut(token) } += 1;

        Ok(LockOutcome { was_discarded, updated_reclaim_candidates })
    }

    /// Unlocks a discardable VMO. See `lock_discardable_locked`. On success, returns whether the
    /// VMO was moved between the discardable reclaimable and non-reclaimable lists.
    ///
    /// # Safety
    ///
    /// The caller must hold the owning VMO lock (`cow.lock()`) and `token` must be the token for
    /// that lock instance.
    pub(crate) unsafe fn unlock_discardable_locked(
        self: Pin<&Self>,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
    ) -> Result<bool, Status> {
        // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
        let lock_count = unsafe { *self.lock_count.get(token) };
        if lock_count == 0 {
            return Err(Status::BAD_STATE);
        }

        let mut updated_reclaim_candidates = false;
        if lock_count == 1 {
            // Lock count transition from 1 -> 0. Change state to reclaimable.
            // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
            updated_reclaim_candidates = unsafe {
                self.update_discardable_state_locked(token, DiscardableState::Reclaimable)
            };
        }
        // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
        *unsafe { self.lock_count.get_mut(token) } -= 1;

        Ok(updated_reclaim_candidates)
    }

    /// Returns whether this object qualifies for reclamation based on whether its state is
    /// `DiscardableState::Reclaimable`.
    ///
    /// # Safety
    ///
    /// The caller must hold the owning VMO lock (`cow.lock()`) and `token` must be the token for
    /// that lock instance.
    pub(crate) unsafe fn is_eligible_for_reclamation_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
    ) -> bool {
        // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
        let (state, lock_count) =
            unsafe { (*self.discardable_state.get(token), *self.lock_count.get(token)) };

        // We've raced with a lock operation. Bail without doing anything. The lock operation will
        // have already moved it to the unreclaimable list.
        if state != DiscardableState::Reclaimable {
            return false;
        }

        // We've verified that the state is `DiscardableState::Reclaimable`, so the lock count
        // should be zero.
        debug_assert!(lock_count == 0);

        true
    }

    /// Whether the VMO has been discarded and not locked again yet.
    ///
    /// # Safety
    ///
    /// The caller must hold the owning VMO lock (`cow.lock()`) and `token` must be the token for
    /// that lock instance.
    pub(crate) unsafe fn was_discarded_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
    ) -> bool {
        // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
        unsafe { *self.discardable_state.get(token) == DiscardableState::Discarded }
    }

    /// Mark the VMO as discarded.
    ///
    /// # Safety
    ///
    /// The caller must hold the owning VMO lock (`cow.lock()`) and `token` must be the token for
    /// that lock instance.
    pub(crate) unsafe fn set_discarded_locked(
        self: Pin<&Self>,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
    ) {
        // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
        unsafe {
            self.update_discardable_state_locked(token, DiscardableState::Discarded);
        }
    }

    /// Accessor for the current discardable state.
    ///
    /// # Safety
    ///
    /// The caller must hold the owning VMO lock (`cow.lock()`) and `token` must be the token for
    /// that lock instance.
    pub(crate) unsafe fn discardable_state_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
    ) -> DiscardableState {
        // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
        unsafe { *self.discardable_state.get(token) }
    }

    // Updates the `discardable_state` of a discardable vmo, and moves it from one discardable
    // list to another.
    //
    // # Safety
    //
    // The caller must hold the owning VMO lock (`cow.lock()`) and `token` must be the token for
    // that lock instance.
    unsafe fn update_discardable_state_locked(
        self: Pin<&Self>,
        token: &mut LockToken<'_, VmCowPagesLockClass>,
        state: DiscardableState,
    ) -> bool {
        ksync::lock!(let mut guard = DiscardableVmosLock::lock());

        // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
        let (cur_state, lock_count) =
            unsafe { (*self.discardable_state.get(token), *self.lock_count.get(token)) };

        debug_assert!(state != DiscardableState::Unset);
        // SAFETY: We hold both the owning VmCowPages' lock and `DiscardableVmosLock`.
        debug_assert!(!unsafe { self.cow() }.is_null());

        if state == cur_state {
            return false;
        }

        let lists = get_lists(guard.token_mut());

        let mut updated_reclaim_candidates = false;
        match state {
            DiscardableState::Reclaimable => {
                // The only valid transition into reclaimable is from unreclaimable
                // (lock count 1 -> 0).
                debug_assert!(cur_state == DiscardableState::Unreclaimable);
                debug_assert!(lock_count == 1);

                // Update the last unlock timestamp.
                // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
                *unsafe { self.last_unlock_timestamp.get_mut(token) } = current_mono_time().0;

                // Move to reclaim candidates list.
                self.move_to_reclaim_candidates_list_locked(token, lists);
                updated_reclaim_candidates = true;
            }
            DiscardableState::Unreclaimable => {
                // The vmo could be reclaimable OR discarded OR not on any list yet. In any case,
                // the lock count should be 0.
                debug_assert!(lock_count == 0);
                debug_assert!(cur_state != DiscardableState::Unreclaimable);

                if cur_state == DiscardableState::Discarded {
                    // Should already be on the non reclaim candidates list.
                    debug_assert!(
                        lists.non_reclaim_candidates.iter().any(|d| ptr::eq(d, self.get_ref()))
                    );
                } else {
                    // Move to non reclaim candidates list.
                    self.move_to_non_reclaim_candidates_list_locked(
                        token,
                        lists,
                        cur_state == DiscardableState::Unset,
                    );
                    updated_reclaim_candidates = true;
                }
            }
            DiscardableState::Discarded => {
                // The only valid transition into discarded is from reclaimable
                // (lock count is 0).
                debug_assert!(cur_state == DiscardableState::Reclaimable);
                debug_assert!(lock_count == 0);

                // Move from reclaim candidates to non reclaim candidates list.
                self.move_to_non_reclaim_candidates_list_locked(token, lists, false);
                updated_reclaim_candidates = true;
            }
            DiscardableState::Unset => {}
        }

        // Update the state.
        // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
        *unsafe { self.discardable_state.get_mut(token) } = state;
        updated_reclaim_candidates
    }

    // Helper function to move an object from the `non_reclaim_candidates` list to the
    // `reclaim_candidates` list.
    //
    // The caller must hold the owning VMO lock (`cow.lock()`); holding `lists` proves that
    // `DiscardableVmosLock` is held.
    fn move_to_reclaim_candidates_list_locked(
        self: Pin<&Self>,
        _token: &LockToken<'_, VmCowPagesLockClass>,
        lists: Pin<&mut DiscardableVmoLists>,
    ) {
        // SAFETY: We hold `DiscardableVmosLock` (proven by `lists`).
        debug_assert!(!unsafe { self.cow() }.is_null());
        debug_assert!(self.node.in_container());

        advance_cursors(&lists, self.get_ref());
        let lists = lists.project();
        // SAFETY: `self` is currently in `non_reclaim_candidates`, and all cursors have been
        // advanced past it.
        unsafe {
            lists.non_reclaim_candidates.erase(self.get_ref());
        }

        // SAFETY: `self` is pinned, so it has a stable address and will not be moved before it is
        // dropped, and it is not currently in any list. It is removed from the discardable lists
        // (by `remove_from_discardable_list_locked`, called from the owning VmCowPages'
        // destructor) before it is dropped. The list only writes through the pointer to
        // `self.node`, whose fields are `UnsafeCell`s.
        unsafe {
            lists.reclaim_candidates.push_back_raw(ptr::from_ref(self.get_ref()).cast_mut());
        }
    }

    // Helper function to move an object from the `reclaim_candidates` list to the
    // `non_reclaim_candidates` list. If `new_candidate` is true, that indicates that
    // the object was not yet being tracked on any list, and should only be inserted into the
    // `non_reclaim_candidates` list without a corresponding list removal.
    //
    // The caller must hold the owning VMO lock (`cow.lock()`); holding `lists` proves that
    // `DiscardableVmosLock` is held.
    fn move_to_non_reclaim_candidates_list_locked(
        self: Pin<&Self>,
        _token: &LockToken<'_, VmCowPagesLockClass>,
        mut lists: Pin<&mut DiscardableVmoLists>,
        new_candidate: bool,
    ) {
        // SAFETY: We hold `DiscardableVmosLock` (proven by `lists`).
        debug_assert!(!unsafe { self.cow() }.is_null());
        if new_candidate {
            debug_assert!(!self.node.in_container());
        } else {
            debug_assert!(self.node.in_container());
            advance_cursors(&lists, self.get_ref());
            // SAFETY: `self` is currently in `reclaim_candidates`, and all cursors have been
            // advanced past it.
            unsafe {
                lists.as_mut().project().reclaim_candidates.erase(self.get_ref());
            }
        }

        // SAFETY: `self` is pinned, so it has a stable address and will not be moved before it is
        // dropped, and it is not currently in any list. It is removed from the discardable lists
        // (by `remove_from_discardable_list_locked`, called from the owning VmCowPages'
        // destructor) before it is dropped. The list only writes through the pointer to
        // `self.node`, whose fields are `UnsafeCell`s.
        unsafe {
            lists
                .project()
                .non_reclaim_candidates
                .push_back_raw(ptr::from_ref(self.get_ref()).cast_mut());
        }
    }

    // Returns whether the vmo is in either one of the `reclaim_candidates` or
    // `non_reclaim_candidates` lists, depending on whether it is a `reclaim_candidate`
    // or not.
    //
    // # Safety
    //
    // The caller must hold the owning VMO lock (`cow.lock()`) and `token` must be the token for
    // that lock instance.
    unsafe fn debug_is_in_discardable_list_locked(
        &self,
        token: &LockToken<'_, VmCowPagesLockClass>,
        reclaim_candidate: bool,
    ) -> bool {
        ksync::lock!(let mut guard = DiscardableVmosLock::lock());

        // Not on any list yet. Nothing else to verify.
        // SAFETY: The caller guarantees that `token` is for the owning VmCowPages' lock.
        if unsafe { self.discardable_state_locked(token) } == DiscardableState::Unset {
            return false;
        }

        // SAFETY: We hold both the owning VmCowPages' lock and `DiscardableVmosLock`.
        debug_assert!(!unsafe { self.cow() }.is_null());
        debug_assert!(self.node.in_container());

        let lists = get_lists(guard.token_mut());
        let iter_c = lists.reclaim_candidates.iter().any(|d| ptr::eq(d, self));
        let iter_nc = lists.non_reclaim_candidates.iter().any(|d| ptr::eq(d, self));

        if reclaim_candidate {
            // Verify that the vmo is in the `reclaim_candidates` list and NOT in the
            // `non_reclaim_candidates` list.
            iter_c && !iter_nc
        } else {
            // Verify that the vmo is in the `non_reclaim_candidates` list and NOT in
            // the `reclaim_candidates` list.
            iter_nc && !iter_c
        }
    }

    /// Returns the total number of pages locked and unlocked across all discardable vmos.
    /// Note that this might not be exact and we might miss some vmos, because the
    /// `DiscardableVmosLock` is dropped after processing each vmo on the global discardable
    /// lists. That is fine since these numbers are only used for accounting.
    pub fn debug_discardable_page_counts() -> DiscardablePageCounts {
        let mut total_counts = DiscardablePageCounts { locked: 0, unlocked: 0 };
        ksync::lock!(let mut guard = DiscardableVmosLock::lock());

        // The union of the two lists should give us a list of all discardable vmos.
        for list_idx in 0..2 {
            let lists = get_lists(guard.as_mut().token_mut());
            let list = if list_idx == 0 {
                &lists.reclaim_candidates
            } else {
                &lists.non_reclaim_candidates
            };
            let front = list.front().map_or(ptr::null_mut(), |d| ptr::from_ref(d).cast_mut());

            let cursor = pin!(Cursor::new(front));
            let cursor = cursor.into_ref();
            // SAFETY: `cursor` is pinned on the stack and is not in any list. `_cursor_guard`
            // unlinks it again (with `DiscardableVmosLock` held) before `cursor` goes out of
            // scope. The list only writes through the pointer to `cursor.node`, whose fields are
            // `UnsafeCell`s.
            unsafe {
                lists.project().cursors.push_front_raw(ptr::from_ref(cursor.get_ref()).cast_mut());
            }
            // SAFETY: `cursor` was just added to `lists.cursors`. `_cursor_guard` is dropped at
            // the end of this loop iteration, while `guard` still holds `DiscardableVmosLock` and
            // no reference obtained from `get_lists` is live.
            let _cursor_guard = unsafe { CursorGuard::new(cursor) };

            loop {
                let discardable = cursor.next(guard.token());
                if discardable.is_null() {
                    break;
                }

                // It is safe to reference `discardable.cow` like this because we found
                // `discardable` in a discardable list, which means that even if `cow` was in
                // process of being destroyed it hasn't made it far enough to have removed
                // `discardable` from the discardable list and reset `cow`, which requires the
                // `DiscardableVmosLock`.
                //
                // SAFETY: `discardable` is a live tracker on a discardable list, and we hold
                // `DiscardableVmosLock`.
                let cow_ptr = unsafe { (*discardable).cow() };
                // SAFETY: Per the above, `cow_ptr` points to a `VmCowPages` that has not yet been
                // freed, since we hold `DiscardableVmosLock`.
                if let Some(cow) = unsafe { VmCowPages::upgrade_from_raw(cow_ptr) } {
                    // Get page counts for each vmo outside of `DiscardableVmosLock`, since
                    // `debug_get_discardable_page_counts()` will acquire the VmCowPages lock.
                    // Holding the `DiscardableVmosLock` while acquiring the VmCowPages lock
                    // will violate lock ordering constraints between the two.
                    //
                    // Since we upgraded the raw pointer to a RefPtr under `DiscardableVmosLock`,
                    // we know that the object is valid. We could not have raced with
                    // destruction, since the object is removed from the discardable list on the
                    // destruction path, which requires the `DiscardableVmosLock`. We will call
                    // `next()` on our cursor after re-acquiring `DiscardableVmosLock` to safely
                    // iterate to the next element on the list.
                    let counts = guard.as_mut().call_unlocked(|| {
                        let counts = cow.debug_get_discardable_page_counts();
                        // Explicitly drop the RefPtr to force any destructor to run right now
                        // and not after `DiscardableVmosLock` has been re-acquired.
                        drop(cow);
                        counts
                    });
                    total_counts.locked += counts.locked;
                    total_counts.unlocked += counts.unlocked;
                }
            }
        }

        total_counts
    }

    /// Domain-specific conversion: returns raw pointer for `DiscardableVmoTracker`.
    pub fn as_raw(&self) -> *const bindings::DiscardableVmoTracker {
        ptr::from_ref(self).cast::<bindings::DiscardableVmoTracker>()
    }

    /// Domain-specific conversion: constructs a `&DiscardableVmoTracker` from a raw pointer.
    ///
    /// It is sound to hold the returned reference while the tracker's state is concurrently
    /// mutated, since all such state is in `UnsafeCell`s.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null pointer to `DiscardableVmoTracker` with lifetime `'a`.
    pub unsafe fn from_raw_ref<'a>(ptr: *const bindings::DiscardableVmoTracker) -> &'a Self {
        let ptr: *const Self = ptr.cast();
        // SAFETY: bindings::DiscardableVmoTracker is layout-compatible with DiscardableVmoTracker,
        // and the caller guarantees `ptr` is valid for `'a`.
        unsafe { ptr.as_ref_unchecked() }
    }

    // Returns the owning VmCowPages' lock.
    fn cow_lock(&self) -> &VmCowPagesLock {
        // SAFETY: The debug accessors that call this are reached through a reference to the
        // tracker, which is owned by the VmCowPages, so the VmCowPages is not being destroyed and
        // `remove_from_discardable_list_locked` (the only post-initialization writer of `cow`)
        // cannot be running concurrently.
        let cow = unsafe { self.cow() };
        // The owning VmCowPages must have been set via `init_cow_pages`.
        assert!(!cow.is_null());
        // SAFETY: `cow` was set via `init_cow_pages`, whose caller contract guarantees it points
        // to an initialized, live `VmCowPages`.
        let lock = unsafe { bindings::cpp_vm_cow_pages_get_lock(cow) };
        let lock: *mut VmCowPagesLock = lock.cast();
        // SAFETY: `lock` points to the `VmCowPages`'s mutex, which remains valid as long as
        // `cow` is valid per `init_cow_pages`'s safety contract.
        unsafe { lock.as_ref_unchecked() }
    }

    // Acquires the owning VmCowPages' lock and calls `f` with the token for that lock.
    fn with_cow_lock<R>(&self, f: impl FnOnce(&mut LockToken<'_, VmCowPagesLockClass>) -> R) -> R {
        ksync::lock!(let mut guard = self.cow_lock().lock());
        f(guard.token_mut())
    }

    /// Returns the lock count of the discardable VMO. Debug function exposed for testing.
    ///
    /// Acquires the owning VMO lock (`cow.lock()`).
    pub fn debug_get_lock_count(&self) -> u64 {
        self.with_cow_lock(|token| {
            // SAFETY: `token` is for the owning VmCowPages' lock, acquired by `with_cow_lock`.
            unsafe { *self.lock_count.get(token) }
        })
    }

    /// Returns whether the VMO is in the reclaimable state and on the reclaim candidates list.
    /// Debug function exposed for testing.
    ///
    /// Acquires the owning VMO lock (`cow.lock()`).
    pub fn debug_is_reclaimable(&self) -> bool {
        self.with_cow_lock(|token| {
            // SAFETY: `token` is for the owning VmCowPages' lock, acquired by `with_cow_lock`.
            unsafe {
                if self.discardable_state_locked(token) != DiscardableState::Reclaimable {
                    return false;
                }
                self.debug_is_in_discardable_list_locked(token, /*reclaim_candidate=*/ true)
            }
        })
    }

    /// Returns whether the VMO is in the unreclaimable state and on the non-reclaim candidates
    /// list. Debug function exposed for testing.
    ///
    /// Acquires the owning VMO lock (`cow.lock()`).
    pub fn debug_is_unreclaimable(&self) -> bool {
        self.with_cow_lock(|token| {
            // SAFETY: `token` is for the owning VmCowPages' lock, acquired by `with_cow_lock`.
            unsafe {
                if self.discardable_state_locked(token) != DiscardableState::Unreclaimable {
                    return false;
                }
                self.debug_is_in_discardable_list_locked(token, /*reclaim_candidate=*/ false)
            }
        })
    }

    /// Returns whether the VMO is in the discarded state and on the non-reclaim candidates list.
    /// Debug function exposed for testing.
    ///
    /// Acquires the owning VMO lock (`cow.lock()`).
    pub fn debug_is_discarded(&self) -> bool {
        self.with_cow_lock(|token| {
            // SAFETY: `token` is for the owning VmCowPages' lock, acquired by `with_cow_lock`.
            unsafe {
                if self.discardable_state_locked(token) != DiscardableState::Discarded {
                    return false;
                }
                self.debug_is_in_discardable_list_locked(token, /*reclaim_candidate=*/ false)
            }
        })
    }
}

impl Default for DiscardableVmoTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DiscardableVmoTracker {
    fn drop(&mut self) {
        // The tracker must already have been removed from the discardable lists, which the owning
        // VmCowPages' destructor does via `remove_from_discardable_list_locked`.
        debug_assert!(!self.node.in_container());
        debug_assert!(*self.discardable_state.get_inner_mut() == DiscardableState::Unset);
    }
}

// C FFI exports

/// Constructs and initializes a `DiscardableVmoTracker` in uninitialized storage.
///
/// # Safety
///
/// `tracker` must be a valid, writable pointer to `DiscardableVmoTracker` storage.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_discardable_vmo_tracker_init(tracker: *mut DiscardableVmoTracker) {
    // SAFETY: `tracker` is valid for writes.
    unsafe {
        tracker.write(DiscardableVmoTracker::new());
    }
}

/// Destroys a `DiscardableVmoTracker`.
///
/// # Safety
///
/// `tracker` must be a valid, aligned, dereferenceable pointer to an initialized
/// `DiscardableVmoTracker` that is not on a discardable list.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_discardable_vmo_tracker_destroy(tracker: *mut DiscardableVmoTracker) {
    // SAFETY: `tracker` is valid for dropping in place.
    unsafe {
        ptr::drop_in_place(tracker);
    }
}

/// Initializes the back reference to the owning `VmCowPages`.
///
/// # Safety
///
/// `tracker` must point to a live `DiscardableVmoTracker` that is not yet on a discardable list.
/// `cow` must point to an initialized, live `VmCowPages` whose lifetime outlives `tracker`
/// (or until `tracker` is unlinked via `remove_from_discardable_list_locked`).
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_discardable_vmo_tracker_init_cow_pages(
    tracker: *const DiscardableVmoTracker,
    cow: *mut bindings::VmCowPages,
) {
    // SAFETY: The caller guarantees `tracker` points to a live instance that is not yet on any
    // list, and that `cow` points to an initialized, live `VmCowPages` meeting the lifetime
    // requirements.
    unsafe {
        (*tracker).init_cow_pages(cow);
    }
}

/// Returns the back reference to the owning `VmCowPages`. Used by the C++ facade to assert that
/// the lock it was passed is the owning `VmCowPages`' lock.
///
/// # Safety
///
/// `tracker` must point to a live `DiscardableVmoTracker`, and the caller must hold the owning
/// `VmCowPages`' lock or otherwise guarantee that the tracker is not concurrently being removed
/// from the discardable lists.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_discardable_vmo_tracker_debug_get_cow(
    tracker: *const DiscardableVmoTracker,
) -> *mut bindings::VmCowPages {
    // SAFETY: The caller guarantees `tracker` is live and that `cow` is not concurrently written.
    unsafe { (*tracker).cow() }
}

/// Removes a tracker from the global discardable lists.
///
/// # Safety
///
/// `tracker` must point to a live, pinned `DiscardableVmoTracker`, and the caller must hold the
/// owning `VmCowPages`' lock.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_discardable_vmo_tracker_remove_from_discardable_list_locked(
    tracker: *const DiscardableVmoTracker,
) {
    // SAFETY: The caller guarantees `tracker` points to a live instance. The C++ facade object is
    // neither copyable nor movable, so its address is stable.
    let tracker = unsafe { Pin::new_unchecked(&*tracker) };
    // SAFETY: The C++ facade is annotated with TA_REQ(cow_lock) and DEBUG_ASSERTs that `cow_lock`
    // is the owning VmCowPages' lock, so that lock is held for the duration of this call.
    let mut token = unsafe { LockToken::<VmCowPagesLockClass>::new() };
    // SAFETY: `token` is for the owning VmCowPages' lock, per the above.
    unsafe { tracker.remove_from_discardable_list_locked(&mut token) };
}

/// Locks a discardable tracker while the owning VMO lock is held.
///
/// # Safety
///
/// `tracker` must point to a live, pinned `DiscardableVmoTracker` whose address remains
/// stable until it is unlinked from the discardable lists, and the caller must hold the owning
/// `VmCowPages`' lock.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_discardable_vmo_tracker_lock_discardable_locked(
    tracker: *const DiscardableVmoTracker,
    try_lock: bool,
    was_discarded_out: &mut bool,
    updated_reclaim_candidates_out: &mut bool,
) -> zx_status_t {
    // SAFETY: The caller guarantees `tracker` points to a live instance. The C++ facade object is
    // neither copyable nor movable, so its address is stable.
    let tracker = unsafe { Pin::new_unchecked(&*tracker) };
    // SAFETY: The C++ facade is annotated with TA_REQ(cow_lock) and DEBUG_ASSERTs that `cow_lock`
    // is the owning VmCowPages' lock, so that lock is held for the duration of this call.
    let mut token = unsafe { LockToken::<VmCowPagesLockClass>::new() };
    // SAFETY: `token` is for the owning VmCowPages' lock, per the above.
    let res = unsafe { tracker.lock_discardable_locked(&mut token, try_lock) };
    // The only failure is `UNAVAILABLE`, from try-locking a discarded VMO.
    debug_assert!(res.is_ok() || res == Err(Status::UNAVAILABLE));
    let outcome =
        res.unwrap_or(LockOutcome { was_discarded: true, updated_reclaim_candidates: false });
    *was_discarded_out = outcome.was_discarded;
    *updated_reclaim_candidates_out = outcome.updated_reclaim_candidates;
    Status::result_into_raw(res.map(|_| ()))
}

/// Unlocks a discardable tracker while the owning VMO lock is held.
///
/// # Safety
///
/// `tracker` must point to a live, pinned `DiscardableVmoTracker`, and the caller must hold the
/// owning `VmCowPages`' lock.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_discardable_vmo_tracker_unlock_discardable_locked(
    tracker: *const DiscardableVmoTracker,
    updated_reclaim_candidates_out: &mut bool,
) -> zx_status_t {
    // SAFETY: The caller guarantees `tracker` points to a live instance. The C++ facade object is
    // neither copyable nor movable, so its address is stable.
    let tracker = unsafe { Pin::new_unchecked(&*tracker) };
    // SAFETY: The C++ facade is annotated with TA_REQ(cow_lock) and DEBUG_ASSERTs that `cow_lock`
    // is the owning VmCowPages' lock, so that lock is held for the duration of this call.
    let mut token = unsafe { LockToken::<VmCowPagesLockClass>::new() };
    // SAFETY: `token` is for the owning VmCowPages' lock, per the above.
    let res = unsafe { tracker.unlock_discardable_locked(&mut token) };
    *updated_reclaim_candidates_out = res.unwrap_or(false);
    Status::result_into_raw(res.map(|_| ()))
}

/// Returns whether this tracker is eligible for reclamation.
///
/// # Safety
///
/// `tracker` must point to a live `DiscardableVmoTracker`, and the caller must hold the owning
/// `VmCowPages`' lock.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_discardable_vmo_tracker_is_eligible_for_reclamation_locked(
    tracker: *const DiscardableVmoTracker,
) -> bool {
    // SAFETY: The C++ facade is annotated with TA_REQ(cow_lock) and DEBUG_ASSERTs that `cow_lock`
    // is the owning VmCowPages' lock, so that lock is held for the duration of this call.
    let token = unsafe { LockToken::<VmCowPagesLockClass>::new() };
    // SAFETY: The caller guarantees `tracker` is live, and `token` is for its owning
    // VmCowPages' lock, per the above.
    unsafe { (*tracker).is_eligible_for_reclamation_locked(&token) }
}

/// Returns whether this tracker has been discarded.
///
/// # Safety
///
/// `tracker` must point to a live `DiscardableVmoTracker`, and the caller must hold the owning
/// `VmCowPages`' lock.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_discardable_vmo_tracker_was_discarded_locked(
    tracker: *const DiscardableVmoTracker,
) -> bool {
    // SAFETY: The C++ facade is annotated with TA_REQ(cow_lock) and DEBUG_ASSERTs that `cow_lock`
    // is the owning VmCowPages' lock, so that lock is held for the duration of this call.
    let token = unsafe { LockToken::<VmCowPagesLockClass>::new() };
    // SAFETY: The caller guarantees `tracker` is live, and `token` is for its owning
    // VmCowPages' lock, per the above.
    unsafe { (*tracker).was_discarded_locked(&token) }
}

/// Marks the tracker as discarded.
///
/// # Safety
///
/// `tracker` must point to a live, pinned `DiscardableVmoTracker`, and the caller must hold the
/// owning `VmCowPages`' lock.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_discardable_vmo_tracker_set_discarded_locked(
    tracker: *const DiscardableVmoTracker,
) {
    // SAFETY: The caller guarantees `tracker` points to a live instance. The C++ facade object is
    // neither copyable nor movable, so its address is stable.
    let tracker = unsafe { Pin::new_unchecked(&*tracker) };
    // SAFETY: The C++ facade is annotated with TA_REQ(cow_lock) and DEBUG_ASSERTs that `cow_lock`
    // is the owning VmCowPages' lock, so that lock is held for the duration of this call.
    let mut token = unsafe { LockToken::<VmCowPagesLockClass>::new() };
    // SAFETY: `token` is for the owning VmCowPages' lock, per the above.
    unsafe { tracker.set_discarded_locked(&mut token) };
}

/// Calculates the aggregate page counts across all discardable VMOs.
#[unsafe(no_mangle)]
extern "C" fn rust_discardable_vmo_tracker_debug_discardable_page_counts(
    out_counts: &mut DiscardablePageCounts,
) {
    *out_counts = DiscardableVmoTracker::debug_discardable_page_counts();
}

/// Returns the current state of a discardable tracker.
///
/// # Safety
///
/// `tracker` must point to a live `DiscardableVmoTracker`, and the caller must hold the owning
/// `VmCowPages`' lock.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_discardable_vmo_tracker_discardable_state_locked(
    tracker: *const DiscardableVmoTracker,
) -> u8 {
    // SAFETY: The C++ facade is annotated with TA_REQ(cow_lock) and DEBUG_ASSERTs that `cow_lock`
    // is the owning VmCowPages' lock, so that lock is held for the duration of this call.
    let token = unsafe { LockToken::<VmCowPagesLockClass>::new() };
    // SAFETY: The caller guarantees `tracker` is live, and `token` is for its owning
    // VmCowPages' lock, per the above.
    unsafe { (*tracker).discardable_state_locked(&token) as u8 }
}

/// Returns the lock count of a discardable tracker.
#[unsafe(no_mangle)]
extern "C" fn rust_discardable_vmo_tracker_debug_get_lock_count(
    tracker: &DiscardableVmoTracker,
) -> u64 {
    tracker.debug_get_lock_count()
}

/// Returns whether the tracker is in the reclaimable list.
#[unsafe(no_mangle)]
extern "C" fn rust_discardable_vmo_tracker_debug_is_reclaimable(
    tracker: &DiscardableVmoTracker,
) -> bool {
    tracker.debug_is_reclaimable()
}

/// Returns whether the tracker is in the non-reclaimable list with unreclaimable state.
#[unsafe(no_mangle)]
extern "C" fn rust_discardable_vmo_tracker_debug_is_unreclaimable(
    tracker: &DiscardableVmoTracker,
) -> bool {
    tracker.debug_is_unreclaimable()
}

/// Returns whether the tracker is in the non-reclaimable list with discarded state.
#[unsafe(no_mangle)]
extern "C" fn rust_discardable_vmo_tracker_debug_is_discarded(
    tracker: &DiscardableVmoTracker,
) -> bool {
    tracker.debug_is_discarded()
}

#[cfg(ktest)]
/// Unit tests for `DiscardableVmoTracker`.
#[unittest::suite(name = "discardable_vmo_tracker_rust")]
mod tests {
    use super::{
        Cursor, DiscardablePageCounts, DiscardableState, DiscardableVmoTracker,
        DiscardableVmosLock, LockOutcome, get_lists,
    };
    use crate::kernel::thread::{self, ThreadPtr};
    use crate::platform_rs::timer::InstantMono;
    use crate::vm::pmm;
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm_cow_pages::{EvictionAction, VmCowPages, VmCowPagesLockClass};
    use crate::vm::vm_object_paged::VmObjectPaged;
    use core::ffi::c_void;
    use core::pin::{Pin, pin};
    use core::ptr;
    use fbl::RefPtr;
    use ksync::LockToken;
    use unittest::{
        assert_true, expect_eq, expect_err, expect_false, expect_le, expect_ok, expect_true,
    };
    use zx_status::Status;

    const PAGE_SIZE: u64 = page::SIZE as u64;

    // A minimal xorshift PRNG, used by tests that make random choices.
    struct Rand(u32);

    impl Rand {
        fn new(seed: u32) -> Self {
            Self(seed | 1)
        }

        fn random_bool(&mut self) -> bool {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            (self.0 >> 16) & 1 == 1
        }
    }

    // A (non-discardable) VMO whose `VmCowPages` is used as the owner of a standalone tracker.
    //
    // The tracker is driven directly through its API while holding this `VmCowPages`' lock,
    // rather than using the tracker of a discardable VMO, so as not to bypass the VMO's own
    // bookkeeping. Since the `VmCowPages` is not itself discardable, the page counts walk in
    // `debug_discardable_page_counts` sees zero counts for it if it runs concurrently.
    struct TestCow {
        _vmo: RefPtr<VmObjectPaged>,
        cow: RefPtr<VmCowPages>,
    }

    fn create_test_cow() -> TestCow {
        let vmo = VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, PAGE_SIZE).unwrap();
        let cow = vmo.debug_get_cow_pages().unwrap();
        TestCow { _vmo: vmo, cow }
    }

    // Runs `f` with `cow`'s lock held, passing it the token for that lock.
    fn with_lock<R>(
        cow: &VmCowPages,
        f: impl FnOnce(&mut LockToken<'_, VmCowPagesLockClass>) -> R,
    ) -> R {
        ksync::lock!(let mut guard = cow.lock());
        f(guard.token_mut())
    }

    // Removes `tracker` from the discardable lists when dropped, as the owning `VmCowPages`'
    // destructor does, so that the tracker is never dropped while still on a list.
    struct RemoveOnDrop<'a> {
        cow: &'a VmCowPages,
        tracker: Pin<&'a DiscardableVmoTracker>,
    }

    impl Drop for RemoveOnDrop<'_> {
        fn drop(&mut self) {
            with_lock(self.cow, |token| {
                // SAFETY: `token` is for the lock of `self.cow`, which owns `self.tracker`.
                unsafe { self.tracker.remove_from_discardable_list_locked(token) }
            });
        }
    }

    // Initializes `tracker`'s back reference to `t.cow`.
    fn init_tracker(t: &TestCow, tracker: Pin<&DiscardableVmoTracker>) {
        // SAFETY: `tracker` has never been locked, so it is not on any list, and `t.cow` outlives
        // it in all tests (`TestCow` is always declared before the tracker).
        unsafe { tracker.init_cow_pages(t.cow.as_raw()) };
    }

    // The helpers below perform the corresponding tracker operation with `cow`'s lock held.
    //
    // SAFETY (for all of them): `token` is for the lock of `cow`, which owns `tracker`.

    fn lock(
        cow: &VmCowPages,
        tracker: Pin<&DiscardableVmoTracker>,
        try_lock: bool,
    ) -> Result<LockOutcome, Status> {
        // SAFETY: See above.
        with_lock(cow, |token| unsafe { tracker.lock_discardable_locked(token, try_lock) })
    }

    fn unlock(cow: &VmCowPages, tracker: Pin<&DiscardableVmoTracker>) -> Result<bool, Status> {
        // SAFETY: See above.
        with_lock(cow, |token| unsafe { tracker.unlock_discardable_locked(token) })
    }

    fn set_discarded(cow: &VmCowPages, tracker: Pin<&DiscardableVmoTracker>) {
        // SAFETY: See above.
        with_lock(cow, |token| unsafe { tracker.set_discarded_locked(token) })
    }

    fn is_eligible_for_reclamation(cow: &VmCowPages, tracker: &DiscardableVmoTracker) -> bool {
        // SAFETY: See above.
        with_lock(cow, |token| unsafe { tracker.is_eligible_for_reclamation_locked(token) })
    }

    fn was_discarded(cow: &VmCowPages, tracker: &DiscardableVmoTracker) -> bool {
        // SAFETY: See above.
        with_lock(cow, |token| unsafe { tracker.was_discarded_locked(token) })
    }

    fn state(cow: &VmCowPages, tracker: &DiscardableVmoTracker) -> DiscardableState {
        // SAFETY: See above.
        with_lock(cow, |token| unsafe { tracker.discardable_state_locked(token) })
    }

    /// Test that the discardable tracker's lock count is updated via lock and unlock ops.
    #[test]
    fn discardable_lock_count_test() {
        // Create a tracker to lock and unlock from multiple threads.
        let t = create_test_cow();
        let tracker = pin!(DiscardableVmoTracker::new());
        let tracker = tracker.into_ref();
        init_tracker(&t, tracker);
        let _remove = RemoveOnDrop { cow: &t.cow, tracker };

        const K_NUM_THREADS: usize = 5;
        let mut threads: [Option<ThreadPtr>; K_NUM_THREADS] = [None; K_NUM_THREADS];
        struct ThreadState {
            cow: *const VmCowPages,
            tracker: *const DiscardableVmoTracker,
            did_unlock: bool,
        }
        let mut state =
            [const { ThreadState { cow: ptr::null(), tracker: ptr::null(), did_unlock: false } };
                K_NUM_THREADS];

        extern "C" fn worker(arg: *mut c_void) -> i32 {
            let state: *mut ThreadState = arg.cast();
            // SAFETY: `state` is a valid pointer to a `ThreadState` that lives for the duration
            // of the thread.
            let state = unsafe { state.as_mut_unchecked() };
            // SAFETY: `state.cow` points to a live `VmCowPages` that outlives this thread.
            let cow = unsafe { state.cow.as_ref_unchecked() };
            // SAFETY: `state.tracker` points to a live, pinned tracker that outlives this thread.
            let tracker = unsafe { Pin::new_unchecked(state.tracker.as_ref_unchecked()) };
            let mut rand = Rand::new(ptr::from_mut(state).addr() as u32);

            // Randomly decide between try-lock and lock.
            let try_lock = rand.random_bool();
            if let Err(status) = lock(cow, tracker, try_lock) {
                return status.into_raw();
            }

            // Randomly decide whether to unlock, or leave the tracker locked.
            if rand.random_bool() {
                if let Err(status) = unlock(cow, tracker) {
                    return status.into_raw();
                }
                state.did_unlock = true;
            }

            0
        }

        for i in 0..K_NUM_THREADS {
            state[i].cow = &*t.cow;
            state[i].tracker = tracker.get_ref();
            state[i].did_unlock = false;

            let state_ptr: *mut ThreadState = &mut state[i];
            let arg: *mut c_void = state_ptr.cast();
            // SAFETY: `worker` is a valid entry point and `arg` points to a live `ThreadState`.
            threads[i] = Some(unsafe { thread::create(c"worker".as_ptr(), worker, arg) }.unwrap());
        }

        for th in &threads {
            // SAFETY: `th` is a valid thread created above and has not been joined or
            // destroyed.
            unsafe { th.unwrap().resume() };
        }

        for th in &threads {
            // SAFETY: `th` is a valid thread that has not yet been joined.
            let ret = unsafe { th.unwrap().join(InstantMono::INFINITE) }.unwrap();
            expect_eq!(0, ret);
        }

        let mut expected_lock_count = K_NUM_THREADS as u64;
        for s in &state {
            if s.did_unlock {
                expected_lock_count -= 1;
            }
        }

        expect_eq!(expected_lock_count, tracker.debug_get_lock_count());
    }

    // Verifies that a discardable tracker is eligible for discard only when unlocked, and can be
    // locked / unlocked again after the discard.
    /// Tests the state transitions for a discardable tracker.
    #[test]
    fn discardable_states_test() {
        let t = create_test_cow();
        let cow = &*t.cow;
        let tracker = pin!(DiscardableVmoTracker::new());
        let tracker = tracker.into_ref();
        init_tracker(&t, tracker);
        let _remove = RemoveOnDrop { cow, tracker };

        // A newly created discardable vmo is not on any list yet.
        expect_false!(tracker.debug_is_unreclaimable());
        expect_false!(tracker.debug_is_reclaimable());
        expect_false!(tracker.debug_is_discarded());
        expect_eq!(DiscardableState::Unset, state(cow, &tracker));
        expect_eq!(0, tracker.debug_get_lock_count());

        // Lock (with try-lock, from the unset state).
        expect_true!(
            lock(cow, tracker, /*try_lock=*/ true)
                == Ok(LockOutcome { was_discarded: false, updated_reclaim_candidates: true })
        );
        expect_true!(tracker.debug_is_unreclaimable());
        expect_false!(tracker.debug_is_reclaimable());
        expect_false!(tracker.debug_is_discarded());

        // Cannot discard when locked.
        expect_false!(is_eligible_for_reclamation(cow, &tracker));

        // Unlock.
        expect_true!(unlock(cow, tracker) == Ok(true));
        expect_true!(tracker.debug_is_reclaimable());
        expect_false!(tracker.debug_is_unreclaimable());
        expect_false!(tracker.debug_is_discarded());

        // Should be able to discard now.
        expect_true!(is_eligible_for_reclamation(cow, &tracker));
        set_discarded(cow, tracker);
        expect_true!(tracker.debug_is_discarded());
        expect_false!(tracker.debug_is_unreclaimable());
        expect_false!(tracker.debug_is_reclaimable());
        expect_true!(was_discarded(cow, &tracker));
        expect_false!(is_eligible_for_reclamation(cow, &tracker));

        // Try lock should fail after discard.
        expect_err!(lock(cow, tracker, /*try_lock=*/ true), Status::UNAVAILABLE);
        expect_true!(tracker.debug_is_discarded());

        // Lock should succeed, and report that the vmo was discarded.
        expect_true!(
            lock(cow, tracker, /*try_lock=*/ false)
                == Ok(LockOutcome { was_discarded: true, updated_reclaim_candidates: false })
        );
        expect_true!(tracker.debug_is_unreclaimable());
        expect_false!(tracker.debug_is_reclaimable());
        expect_false!(tracker.debug_is_discarded());
        expect_false!(was_discarded(cow, &tracker));

        // Try lock should succeed now.
        expect_true!(
            lock(cow, tracker, /*try_lock=*/ true)
                == Ok(LockOutcome { was_discarded: false, updated_reclaim_candidates: false })
        );
        expect_true!(tracker.debug_is_unreclaimable());
        expect_eq!(2, tracker.debug_get_lock_count());

        // Lock count 2->1. So no change in reclaimable state.
        expect_true!(unlock(cow, tracker) == Ok(false));
        expect_true!(tracker.debug_is_unreclaimable());

        // Unlock.
        expect_true!(unlock(cow, tracker) == Ok(true));
        expect_true!(tracker.debug_is_reclaimable());
        expect_false!(tracker.debug_is_unreclaimable());
        expect_false!(tracker.debug_is_discarded());

        // Lock again and verify the lock state returned without a discard.
        expect_true!(
            lock(cow, tracker, /*try_lock=*/ false)
                == Ok(LockOutcome { was_discarded: false, updated_reclaim_candidates: true })
        );
        expect_true!(tracker.debug_is_unreclaimable());
        expect_false!(tracker.debug_is_reclaimable());
        expect_false!(tracker.debug_is_discarded());

        // Unlock and discard again.
        expect_true!(unlock(cow, tracker) == Ok(true));
        expect_true!(tracker.debug_is_reclaimable());
        expect_false!(tracker.debug_is_unreclaimable());
        expect_false!(tracker.debug_is_discarded());

        expect_true!(is_eligible_for_reclamation(cow, &tracker));
        set_discarded(cow, tracker);
        expect_true!(tracker.debug_is_discarded());
        expect_false!(tracker.debug_is_unreclaimable());
        expect_false!(tracker.debug_is_reclaimable());
    }

    // Page counts can only be produced by real discardable VMOs, so this test uses VMO operations
    // rather than a standalone tracker.
    /// Tests discardable page counts.
    #[test]
    fn discardable_counts_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        const NUM_VMOS: usize = 10;
        let mut vmos: [Option<RefPtr<VmObjectPaged>>; NUM_VMOS] = Default::default();

        // Create some discardable vmos.
        for (i, vmo) in vmos.iter_mut().enumerate() {
            *vmo = Some(
                VmObjectPaged::create(
                    pmm::ALLOC_FLAG_ANY,
                    VmObjectPaged::DISCARDABLE,
                    (i as u64 + 1) * PAGE_SIZE,
                )
                .unwrap(),
            );
        }

        let mut rand = Rand::new(ptr::from_ref(vmos[0].as_ref().unwrap()).addr() as u32);
        let mut expected = DiscardablePageCounts { locked: 0, unlocked: 0 };

        // Lock all vmos. Unlock a few. And discard a few unlocked ones.
        // Compute the expected page counts as a result of these operations.
        for (i, vmo) in vmos.iter().enumerate() {
            let vmo = vmo.as_ref().unwrap();
            expect_ok!(vmo.try_lock_range(0, (i as u64 + 1) * PAGE_SIZE));
            expect_ok!(vmo.commit_range(0, (i as u64 + 1) * PAGE_SIZE));

            if rand.random_bool() {
                expect_ok!(vmo.unlock_range(0, (i as u64 + 1) * PAGE_SIZE));

                if rand.random_bool() {
                    // Discarded pages won't show up under locked or unlocked counts.
                    let (page, _) = vmo.get_page_blocking(0, 0).unwrap();
                    let cow = vmo.debug_get_cow_pages().expect("vmo has cow pages");
                    // SAFETY: It is sound to reclaim `page` at offset 0.
                    let reclaimed =
                        unsafe { cow.reclaim_page(page, 0, EvictionAction::FollowHint, None) };
                    assert_true!(reclaimed.is_ok());
                    expect_eq!((i + 1) as u64, reclaimed.unwrap().num_pages);
                } else {
                    // Unlocked but not discarded.
                    expected.unlocked += (i + 1) as u64;
                }
            } else {
                // Locked.
                expected.locked += (i + 1) as u64;
            }
        }

        let counts = DiscardableVmoTracker::debug_discardable_page_counts();
        // There might be other discardable vmos in the rest of the system, so the actual page
        // counts might be higher than the expected counts.
        expect_le!(expected.locked, counts.locked);
        expect_le!(expected.unlocked, counts.unlocked);

        // Additionally, the per-vmo counts of just the vmos created above should sum to exactly
        // the expected counts.
        let mut ours = DiscardablePageCounts { locked: 0, unlocked: 0 };
        for vmo in &vmos {
            let cow = vmo.as_ref().unwrap().debug_get_cow_pages().expect("vmo has cow pages");
            let c = cow.debug_get_discardable_page_counts();
            ours.locked += c.locked;
            ours.unlocked += c.unlocked;
        }
        expect_eq!(expected.locked, ours.locked);
        expect_eq!(expected.unlocked, ours.unlocked);
    }

    // Unlocking with a zero lock count fails with `BAD_STATE`, both before the tracker has ever
    // been locked and after a discard.
    /// Tests the error paths of unlocking.
    #[test]
    fn discardable_unlock_errors_test() {
        let t = create_test_cow();
        let cow = &*t.cow;
        let tracker = pin!(DiscardableVmoTracker::new());
        let tracker = tracker.into_ref();
        init_tracker(&t, tracker);
        let _remove = RemoveOnDrop { cow, tracker };

        // Never locked.
        expect_err!(unlock(cow, tracker), Status::BAD_STATE);
        expect_eq!(DiscardableState::Unset, state(cow, &tracker));

        // Lock, unlock, and discard; a further unlock must fail and leave the state unchanged.
        expect_ok!(lock(cow, tracker, /*try_lock=*/ false).map(|_| ()));
        expect_true!(unlock(cow, tracker) == Ok(true));
        set_discarded(cow, tracker);
        expect_err!(unlock(cow, tracker), Status::BAD_STATE);
        expect_true!(tracker.debug_is_discarded());
        expect_eq!(0, tracker.debug_get_lock_count());
    }

    /// Tests removing trackers from each of the discardable lists.
    #[test]
    fn discardable_remove_from_list_test() {
        let t = create_test_cow();
        let cow = &*t.cow;

        // Remove from the non-reclaim candidates list.
        let unreclaimable = pin!(DiscardableVmoTracker::new());
        let unreclaimable = unreclaimable.into_ref();
        init_tracker(&t, unreclaimable);
        let _remove_unreclaimable = RemoveOnDrop { cow, tracker: unreclaimable };
        expect_ok!(lock(cow, unreclaimable, /*try_lock=*/ false).map(|_| ()));
        expect_true!(unreclaimable.debug_is_unreclaimable());

        // SAFETY: `token` is for the lock of `cow`, which owns `unreclaimable`.
        with_lock(cow, |token| unsafe { unreclaimable.remove_from_discardable_list_locked(token) });
        expect_false!(unreclaimable.node.in_container());
        // The back reference has been reset, so the debug accessors (which acquire the owning
        // lock through it) can no longer be used; query the state with `cow`'s lock instead.
        expect_eq!(DiscardableState::Unset, state(cow, &unreclaimable));

        // Removing again when already in the unset state is a no-op.
        // SAFETY: `token` is for the lock of `cow`, which owned `unreclaimable`.
        with_lock(cow, |token| unsafe { unreclaimable.remove_from_discardable_list_locked(token) });
        expect_false!(unreclaimable.node.in_container());
        expect_eq!(DiscardableState::Unset, state(cow, &unreclaimable));

        // Remove from the reclaim candidates list.
        let reclaimable = pin!(DiscardableVmoTracker::new());
        let reclaimable = reclaimable.into_ref();
        init_tracker(&t, reclaimable);
        let _remove_reclaimable = RemoveOnDrop { cow, tracker: reclaimable };
        expect_ok!(lock(cow, reclaimable, /*try_lock=*/ false).map(|_| ()));
        expect_true!(unlock(cow, reclaimable) == Ok(true));
        expect_true!(reclaimable.debug_is_reclaimable());

        // SAFETY: `token` is for the lock of `cow`, which owns `reclaimable`.
        with_lock(cow, |token| unsafe { reclaimable.remove_from_discardable_list_locked(token) });
        expect_false!(reclaimable.node.in_container());
        expect_eq!(DiscardableState::Unset, state(cow, &reclaimable));
    }

    // A cursor walking a discardable list (as `debug_discardable_page_counts` does while
    // `DiscardableVmosLock` is temporarily dropped) must be advanced past elements that are
    // removed from, or moved off of, the list it is walking.
    /// Tests that cursors are advanced past removed elements.
    #[test]
    fn discardable_cursor_advance_test() {
        let t = create_test_cow();
        let cow = &*t.cow;

        let a = pin!(DiscardableVmoTracker::new());
        let a = a.into_ref();
        let b = pin!(DiscardableVmoTracker::new());
        let b = b.into_ref();
        let c = pin!(DiscardableVmoTracker::new());
        let c = c.into_ref();
        for tracker in [a, b, c] {
            init_tracker(&t, tracker);
        }
        let _remove_a = RemoveOnDrop { cow, tracker: a };
        let _remove_b = RemoveOnDrop { cow, tracker: b };
        let _remove_c = RemoveOnDrop { cow, tracker: c };

        // Put `a`, `b`, and `c` on the non-reclaim candidates list, in that order (other
        // discardable vmos in the system may be interleaved with them).
        for tracker in [a, b, c] {
            expect_ok!(lock(cow, tracker, /*try_lock=*/ false).map(|_| ()));
        }

        // Register a cursor pointing at `a`.
        let cursor = pin!(Cursor::new(ptr::from_ref(a.get_ref()).cast_mut()));
        let cursor = cursor.into_ref();
        {
            ksync::lock!(let mut guard = DiscardableVmosLock::lock());
            let lists = get_lists(guard.token_mut());
            let cursor_ptr = ptr::from_ref(cursor.get_ref()).cast_mut();
            // SAFETY: `cursor` is pinned on the stack and not in any list. It is unlinked below,
            // before it goes out of scope; no `assert_*` (which would return early) is used until
            // then.
            unsafe { lists.project().cursors.push_front_raw(cursor_ptr) };
        }

        // Removing `a` while the cursor points at it must advance the cursor past `a`.
        // SAFETY: `token` is for the lock of `cow`, which owns `a`.
        with_lock(cow, |token| unsafe { a.remove_from_discardable_list_locked(token) });
        {
            ksync::lock!(let guard = DiscardableVmosLock::lock());
            expect_false!(ptr::eq(cursor.current(guard.token()), a.get_ref()));
            // `b` and `c` are still reachable from the cursor.
            let (mut saw_b, mut saw_c) = (false, false);
            loop {
                let d = cursor.next(guard.token());
                if d.is_null() {
                    break;
                }
                expect_false!(ptr::eq(d, a.get_ref()));
                saw_b |= ptr::eq(d, b.get_ref());
                saw_c |= ptr::eq(d, c.get_ref());
            }
            expect_true!(saw_b);
            expect_true!(saw_c);

            // Point the cursor at `b` for the next part of the test.
            cursor.set_current(ptr::from_ref(b.get_ref()).cast_mut(), guard.token());
        }

        // Moving `b` to the reclaim candidates list (by unlocking it) while the cursor points at
        // it must advance the cursor past `b`, staying on the non-reclaim candidates list.
        expect_true!(unlock(cow, b) == Ok(true));
        {
            ksync::lock!(let guard = DiscardableVmosLock::lock());
            expect_false!(ptr::eq(cursor.current(guard.token()), b.get_ref()));
            let mut saw_c = false;
            loop {
                let d = cursor.next(guard.token());
                if d.is_null() {
                    break;
                }
                expect_false!(ptr::eq(d, b.get_ref()));
                saw_c |= ptr::eq(d, c.get_ref());
            }
            expect_true!(saw_c);
        }

        // Unregister the cursor.
        {
            ksync::lock!(let mut guard = DiscardableVmosLock::lock());
            let lists = get_lists(guard.token_mut());
            // SAFETY: `cursor` is in `lists.cursors`.
            unsafe { lists.project().cursors.erase(cursor.get_ref()) };
        }
    }
}
