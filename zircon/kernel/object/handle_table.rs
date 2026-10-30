// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::DispatcherOps;
use super::dispatcher::Dispatcher;
use super::handle::{
    BASE_VALUE_MUST_BE_ZERO_MASK, HANDLE_MUST_BE_ONE_MASK, HANDLE_RESERVED_BITS, Handle,
    HandleOwner, HandleRef, HandleValue,
};
use super::process_dispatcher::ProcessDispatcher;
use crate::kernel::thread::AutoExpiringPreemptDisabler;
use core::convert::Infallible;
use core::ffi::c_void;
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{
    Array, DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode, HasRefCount,
    Recyclable, RefPtr, is_sentinel_ptr, ref_counted,
};
use kalloc::AllocError;
use ksync::{BrwLockPi, guarded, kcell_init};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop, stack_pin_init};
use zx_status::Status;
use zx_types::{
    ZX_HANDLE_INVALID, ZX_KOID_INVALID, ZX_POL_BAD_HANDLE, zx_handle_t, zx_info_handle_extended_t,
    zx_koid_t, zx_rights_t,
};

/// Maps a Handle's base value and mixer to a user-visible handle value.
fn map_handle_to_value(base_value: u32, mixer: u32) -> zx_handle_t {
    // The base_value is masked to only include the index and generation.
    // The reserved bits must be zero.
    debug_assert!((mixer & HANDLE_MUST_BE_ONE_MASK) == 0);
    debug_assert!((base_value & BASE_VALUE_MUST_BE_ZERO_MASK) == 0);

    // Ensure that the last two bits of the result is not zero, and make sure we
    // don't lose any base_value bits when shifting.
    let handle_id = (base_value << HANDLE_RESERVED_BITS) | HANDLE_MUST_BE_ONE_MASK;
    mixer ^ handle_id
}

/// Maps a user-visible handle value and mixer back to a Handle pointer.
fn map_value_to_handle(value: zx_handle_t, mixer: u32) -> Option<NonNull<Handle>> {
    // Validate that the "must be one" bits are actually one.
    if (value & HANDLE_MUST_BE_ONE_MASK) != HANDLE_MUST_BE_ONE_MASK {
        return None;
    }
    let handle_id = (value ^ mixer) >> HANDLE_RESERVED_BITS;
    Handle::from_u32(handle_id)
}

/// `HandleCursor` is used to reduce lock duration while iterating over a handle table.
///
/// It allows iteration over the handle table to be broken up into multiple critical sections while
/// ensuring the iterator remains well defined in the face of handle removal (via
/// `advance_all_if` and `invalidate_all`).
///
/// # Thread-safety
///
/// Instances of this struct may be used concurrently by multiple threads as long as the `next`
/// method is only called by the thread that creates the object. Calling `next` from any other
/// thread is an error. This limitation allows the iterating thread to hold the lock in "read"
/// (a.k.a. shared) mode while iterating. Because all other methods take a write token guard, which
/// proves the lock is held in "write" (a.k.a. exclusive) mode, *and* because a reader-writer lock
/// cannot be simultaneously held in both read and write modes, there is no concurrent access of
/// the `iter` field. The write-mode operations (`advance_all_if`, `invalidate_all`) reach every
/// registered cursor through the table's `cursors` list, which is only accessible via that guard.
#[derive(DoublyLinkedListContainable)]
struct HandleCursor<'a> {
    handle_table: &'a HandleTable,
    iter: Option<NonNull<Handle>>,
    #[dll_node]
    node: DoublyLinkedListNode<HandleCursor<'a>>,
    /// Once registered, `cursors` holds a pointer to this cursor, so it must not move in memory.
    _pin: PhantomPinned,
}

impl<'a> HandleCursor<'a> {
    /// Creates a new unlinked `HandleCursor` associated with `table`.
    const fn new(table: &'a HandleTable) -> Self {
        Self {
            handle_table: table,
            iter: None,
            node: DoublyLinkedListNode::new(),
            _pin: PhantomPinned,
        }
    }

    /// Registers this cursor with the handle table and initializes its iterator to the front
    /// of `handles`.
    ///
    /// `guard` proves the handle table lock is held in Writer mode.
    fn register(self: Pin<&mut Self>, guard: &mut HandleTableLockWriteTokenGuard<'_, '_>) {
        // SAFETY: The cursor is never moved out of; the `Pin` guarantees its address stays stable
        // until `Drop`, which unregisters it from `cursors`.
        let this = unsafe { self.get_unchecked_mut() };
        this.iter = guard.handles().front().map(NonNull::from);
        // Register so this cursor can be invalidated or advanced if the handle it points to
        // is removed. Derive the pointer from the mutable borrow so that `invalidate_all` and
        // `advance_all_if`, which update `iter` through the `cursors` list, keep write provenance.
        let ptr = NonNull::from(&mut *this).cast::<HandleCursor<'static>>();
        // SAFETY: `this` is pinned for the duration of its usage, and `unregister` unlinks it
        // before it is dropped (and before `'a` ends).
        unsafe { guard.cursors_mut().push_front_raw(ptr) };
    }

    /// Unregisters this cursor from the handle table.
    ///
    /// `guard` proves the handle table lock is held in Writer mode.
    fn unregister(&mut self, guard: &mut HandleTableLockWriteTokenGuard<'_, '_>) {
        // SAFETY: `self` was registered in `cursors` with its lifetime erased to `'static`.
        let static_ref =
            unsafe { &*core::ptr::from_ref::<Self>(self).cast::<HandleCursor<'static>>() };
        // SAFETY: `self` was registered in `cursors` by `register`.
        unsafe { guard.cursors_mut().erase(static_ref) };
    }

    /// Invalidates every cursor registered with the handle table.
    ///
    /// Once invalidated `next` will return None and `advance_all_if` will be a no-op.
    ///
    /// `guard` proves the handle table lock is held in Writer mode, and is the only way to reach
    /// the registered cursors.
    fn invalidate_all(guard: &mut HandleTableLockWriteTokenGuard<'_, '_>) {
        for cursor in guard.cursors_mut().iter_mut() {
            // SAFETY: Only `iter` is updated; the cursor itself is never moved out of the `Pin`.
            let this = unsafe { cursor.get_unchecked_mut() };
            this.iter = None;
        }
    }

    /// Advance the cursor and return the next Handle or None if at the end of the list.
    ///
    /// Once `next` has returned None, all subsequent calls will return None.
    ///
    /// `guard` proves the handle table lock is held in Reader mode.
    fn next<'g>(
        self: Pin<&mut Self>,
        _guard: &'g HandleTableLockReadGuard<'_>,
    ) -> Option<&'g Handle> {
        // SAFETY: Only `iter` is updated; the cursor itself is never moved out of the `Pin`.
        let this = unsafe { self.get_unchecked_mut() };
        let handle_ptr = this.iter?;
        // Thread safety is guaranteed by holding the read guard for the handle table lock
        // across the HandleCursor step.
        // SAFETY: Read lock is held via `_guard` for `'g`, and `handle_ptr` is valid within the
        // `handles` list.
        let handle = unsafe { handle_ptr.as_ref() };
        // SAFETY: The read lock is held, so the intrusive list links of `handle` are valid.
        let next = unsafe { *handle.get_node().next.get() };
        this.iter = if is_sentinel_ptr(next) { None } else { NonNull::new(next) };
        Some(handle)
    }

    /// For every cursor registered with the handle table, if its next element is `h`, advance
    /// the cursor past it.
    ///
    /// `guard` proves the handle table lock is held in Writer mode, and is the only way to reach
    /// the registered cursors.
    fn advance_all_if(guard: &mut HandleTableLockWriteTokenGuard<'_, '_>, h: &Handle) {
        for cursor in guard.cursors_mut().iter_mut() {
            // SAFETY: Only `iter` is updated; the cursor itself is never moved out of the `Pin`.
            let this = unsafe { cursor.get_unchecked_mut() };
            // If it points to `h`, skip over it.
            if let Some(iter) = this.iter
                && core::ptr::eq(iter.as_ptr(), h)
            {
                // SAFETY: `guard` proves the write lock is held and `h` is still linked in
                // `handles`.
                let next = unsafe { *h.get_node().next.get() };
                this.iter = if is_sentinel_ptr(next) { None } else { NonNull::new(next) };
            }
        }
    }
}

impl Drop for HandleCursor<'_> {
    fn drop(&mut self) {
        if self.node.in_container() {
            let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
            ksync::lock!(let mut guard = self.handle_table.write_lock());
            let mut token_guard = guard.as_mut().as_token_guard_mut();
            self.unregister(&mut token_guard);
        }
    }
}

/// A mutable entry for a handle in a [`HandleTable`] while holding the handle table's write lock.
///
/// Holding this entry exclusively borrows the write token guard for `'g`, preventing any other
/// table mutation from coexisting until the entry is dropped or consumed by
/// [`HandleEntry::remove`].
pub struct HandleEntry<'g, 'b, 'a> {
    table: &'g HandleTable,
    ptr: NonNull<Handle>,
    guard: &'g mut HandleTableLockWriteTokenGuard<'b, 'a>,
}

impl HandleEntry<'_, '_, '_> {
    /// Borrows this entry as a [`HandleRef`] tied to the borrow of `self`.
    #[inline]
    pub fn as_ref(&self) -> HandleRef<'_> {
        // SAFETY: `self.ptr` is a valid handle linked in `self.table`, and the handle table write
        // lock is held (via `self.guard`) for the lifetime of `&self`.
        unsafe { HandleRef::from_raw(self.ptr) }
    }

    /// Removes the handle from the handle table and returns the owned handle.
    #[inline]
    pub fn remove(self) -> HandleOwner {
        // SAFETY: `self.ptr` was looked up in `self.table` under the write lock `self.guard` still
        // holds, and the exclusive borrow of the guard ensures nothing has removed it since.
        unsafe { self.table.remove_handle_ptr_locked(self.ptr, self.guard) }
    }
}

/// A `HandleTable` stores the handles associated with one or more `ProcessDispatcher` instances.
#[guarded]
#[ref_counted]
#[derive(Recyclable)]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct HandleTable {
    // Protects `handles` and `cursors`.
    // TODO(https://fxbug.dev/42132512): Allow multiple handle table locks to be acquired at once.
    // Right now, this is required when a process closes the last handle to
    // another process, during the destruction of the handle table.
    #[brwlock(flags = lockdep::LOCK_FLAGS_MULTI_ACQUIRE)]
    lock: BrwLockPi,

    // The koid of this handle table. Used to check whether or not a handle belongs to this handle
    // table (and thus that it belongs to a process associated with this handle table).
    koid: zx_koid_t,

    // Each handle table provides pseudorandom userspace handle values. This is the
    // per-handle-table pseudorandom state.
    random_value: u32,

    // The actual handle table. When removing one or more handles from this list, be sure to
    // advance or invalidate any cursors that might point to the handles being removed.
    #[guarded_by(lock)]
    count: u32,
    #[pin]
    #[guarded_by(lock)]
    handles: DoublyLinkedList<NonNull<Handle>>,

    // A list of cursors that contain pointers to elements of handles.
    #[pin]
    #[guarded_by(lock)]
    cursors: DoublyLinkedList<NonNull<HandleCursor<'static>>>,
}

zr::static_assert!(
    core::mem::size_of::<HandleTable>() == object_constants_rs::kHandleTableStorageSize
);
zr::static_assert!(
    core::mem::align_of::<HandleTable>() == object_constants_rs::kHandleTableStorageAlign
);

// SAFETY: A `HandleTable` is shared between the threads of every process attached to it. The
// pointer intrusive lists (`handles`, `cursors`) and `count` are only accessed while holding
// `lock` (`BrwLockPi`), as enforced by `#[guarded_by(lock)]` and the token guards. The remaining
// fields (`koid`, `random_value`) are immutable after construction, and `ref_count` is atomic.
unsafe impl Send for HandleTable {}
// SAFETY: See the `Send` impl above; all interior mutability is serialized by `lock`.
unsafe impl Sync for HandleTable {}

fn generate_random_handle_mixer() -> u32 {
    unsafe extern "C" {
        fn cpp_global_prng_draw(buffer: *mut u8, len: usize);
    }
    let mut secret: u32 = 0;
    // SAFETY: `secret` buffer is valid and appropriately sized for random draw.
    unsafe {
        cpp_global_prng_draw(
            core::ptr::from_mut(&mut secret).cast::<u8>(),
            core::mem::size_of::<u32>(),
        );
    }
    // Handle values must always have the low HANDLE_RESERVED_BITS set.  Do not
    // ever attempt to toggle these bits using the random_value xor mask.
    secret << HANDLE_RESERVED_BITS
}

impl HandleTable {
    /// Returns a pin initializer for a new `HandleTable`.
    pub fn init() -> impl PinInit<Self, Infallible> {
        // Generate handle XOR mask with top bit and bottom two bits cleared
        let random_value = generate_random_handle_mixer();
        let koid = crate::kernel::koid::generate();

        pin_init!(Self {
            ref_count: fbl::RefCounted::new(),
            __fbl_ref_counted_guard: (),
            lock <- BrwLockPi::init(),
            koid,
            random_value,
            count: 0u32.into(),
            handles <- kcell_init(DoublyLinkedList::new()),
            cursors <- kcell_init(DoublyLinkedList::new()),
        })
    }

    /// Allocates a new reference-counted `HandleTable`.
    pub fn create() -> Result<RefPtr<Self>, AllocError> {
        RefPtr::pin_init(Self::init())
    }

    /// Maps a `Handle` to an integer handle value visible to userspace.
    pub fn map_handle_to_value(&self, handle: impl core::borrow::Borrow<Handle>) -> HandleValue {
        HandleValue::new(map_handle_to_value(handle.borrow().base_value(), self.random_value))
    }

    /// Maps an owned `HandleOwner` to an integer handle value.
    pub fn map_handle_to_value_owner(&self, handle: &HandleOwner) -> zx_handle_t {
        map_handle_to_value(handle.base_value(), self.random_value)
    }

    /// Looks up the arena slot for `handle_value` and verifies that it belongs to this table,
    /// without materializing a reference to the `Handle`.
    ///
    /// The returned pointer carries the arena's original (write) provenance, so it may be used
    /// to remove the handle from the table and hand ownership to a `HandleOwner`. When `caller`
    /// is `Some`, a failed lookup enforces `ZX_POL_BAD_HANDLE` on the calling process; when it is
    /// `None`, no policy enforcement happens.
    ///
    /// `_token` proves that the table lock is held (in either mode), which is what keeps the slot
    /// from being removed and freed while the pointer is in use.
    fn lookup_handle_locked_internal(
        &self,
        caller: Option<&ProcessDispatcher>,
        handle_value: zx_handle_t,
        _token: &ksync::LockToken<'_, HandleTableLockClass>,
    ) -> Option<NonNull<Handle>> {
        if let Some(handle_ptr) = map_value_to_handle(handle_value, self.random_value) {
            // SAFETY: `handle_ptr` points to a committed arena slot returned by `Handle::from_u32`.
            if unsafe { Handle::raw_handle_table_id(handle_ptr) } == self.koid {
                return Some(handle_ptr);
            }
        }

        if let Some(proc) = caller {
            // Handle lookup failed.  We potentially generate an exception or kill the process,
            // depending on the job policy. Note that we don't use the return value from
            // enforce_basic_policy() here: ZX_POL_ACTION_ALLOW and ZX_POL_ACTION_DENY are
            // equivalent for ZX_POL_BAD_HANDLE.
            let _ = proc.enforce_basic_policy(ZX_POL_BAD_HANDLE);
        } else {
            // `caller` is almost always `Some`; mark the `None` (no-policy) path as cold.
            core::hint::cold_path();
        }
        None
    }

    /// Same as public `get_handle_locked` overload, except process can be None.
    /// When `caller` is None, no policy enforcement happens.
    fn get_handle_locked_internal<'a>(
        &'a self,
        caller: Option<&ProcessDispatcher>,
        handle_value: zx_handle_t,
        token: &'a ksync::LockToken<'_, HandleTableLockClass>,
    ) -> Option<HandleRef<'a>> {
        let handle_ptr = self.lookup_handle_locked_internal(caller, handle_value, token)?;
        // SAFETY: `handle_ptr` belongs to this table, and `token` statically proves that the
        // table lock is held for lifetime `'a`.
        Some(unsafe { HandleRef::from_raw(handle_ptr) })
    }

    /// Retrieves a handle reference while holding the handle table lock.
    #[inline]
    pub fn get_handle_locked<'a>(
        &'a self,
        _token: &'a ksync::LockToken<'_, HandleTableLockClass>,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Option<HandleRef<'a>> {
        self.get_handle_locked_internal(Some(caller), handle_value.raw_value(), _token)
    }

    /// Looks up the arena slot for `handle_value` while holding the table's write lock.
    ///
    /// Returns a pointer (rather than a `HandleRef`) so that the handle can subsequently be
    /// removed from the table and owned through it; see [`Self::lookup_handle_locked_internal`].
    fn lookup_handle_write_locked(
        &self,
        caller: Option<&ProcessDispatcher>,
        handle_value: zx_handle_t,
        guard: &HandleTableLockWriteTokenGuard<'_, '_>,
    ) -> Option<NonNull<Handle>> {
        self.lookup_handle_locked_internal(caller, handle_value, guard.token())
    }

    /// Retrieves a mutable handle entry while holding the handle table write lock.
    ///
    /// The returned [`HandleEntry`] exclusively borrows `guard`, so no other table mutation can
    /// coexist with it; consume it with [`HandleEntry::remove`] to take the handle out of the
    /// table.
    pub fn get_handle_entry_locked<'g, 'b, 'a>(
        &'g self,
        guard: &'g mut HandleTableLockWriteTokenGuard<'b, 'a>,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Option<HandleEntry<'g, 'b, 'a>> {
        let ptr = self.lookup_handle_write_locked(Some(caller), handle_value.raw_value(), guard)?;
        Some(HandleEntry { table: self, ptr, guard })
    }

    /// Returns the KOID of this handle table.
    #[inline]
    pub fn get_koid(&self) -> zx_koid_t {
        self.koid
    }

    /// Returns the random value mixer for this handle table.
    pub fn random_value(&self) -> u32 {
        self.random_value
    }

    /// Returns the number of handles in this handle table.
    pub fn handle_count(&self) -> u32 {
        ksync::lock!(let guard = self.read_lock());
        *guard.count()
    }

    /// Adds `handle` to this handle table.
    pub fn add_handle(&self, handle: HandleOwner) {
        let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
        ksync::lock!(let mut guard = self.write_lock());
        let mut token_guard = guard.as_mut().as_token_guard_mut();
        self.add_handle_locked_guard(handle, &mut token_guard);
    }

    /// Adds `handle` to this handle table using an existing write token guard.
    pub fn add_handle_locked_guard<'a, 'b>(
        &self,
        handle: HandleOwner,
        guard: &mut HandleTableLockWriteTokenGuard<'b, 'a>,
    ) {
        handle.set_handle_table_id(self.koid);
        let raw = handle.release();
        // SAFETY: `raw` is a valid unlinked `Handle` allocated in the arena.
        unsafe { guard.handles_mut().push_front_raw(raw) };
        *guard.count_mut() += 1;
    }

    /// Removes `handle` from this handle table while holding the write lock.
    ///
    /// # Safety
    /// `handle_ptr` must point to an initialized `Handle` currently linked in this table's
    /// `handles` list.
    unsafe fn remove_handle_ptr_locked<'a, 'b>(
        &self,
        handle_ptr: NonNull<Handle>,
        guard: &mut HandleTableLockWriteTokenGuard<'b, 'a>,
    ) -> HandleOwner {
        debug_assert!(*guard.count() > 0);
        // SAFETY: Write lock is held and `handle_ptr` is a valid member of `handles`.
        let handle = unsafe { handle_ptr.as_ref() };
        handle.set_handle_table_id(ZX_KOID_INVALID);
        // Make sure we don't leave any dangling cursors.
        HandleCursor::advance_all_if(guard, handle);
        // SAFETY: `handle` is linked in `handles`.
        unsafe { guard.handles_mut().erase(handle) };
        *guard.count_mut() -= 1;
        // SAFETY: `handle_ptr` was just unlinked from this table and is now uniquely owned.
        unsafe { HandleOwner::from_non_null(handle_ptr) }
    }

    /// Removes a handle from the table by handle value; `guard` proves the write lock is held.
    pub fn remove_handle_val_locked<'a, 'b>(
        &self,
        caller: Option<&ProcessDispatcher>,
        handle_value: zx_handle_t,
        guard: &mut HandleTableLockWriteTokenGuard<'b, 'a>,
    ) -> Option<HandleOwner> {
        let handle_ptr = self.lookup_handle_write_locked(caller, handle_value, guard)?;
        // SAFETY: `handle_ptr` was retrieved from this table and is linked in `handles`.
        Some(unsafe { self.remove_handle_ptr_locked(handle_ptr, guard) })
    }

    /// Removes a handle by user-facing handle value.
    pub fn remove_handle(
        &self,
        caller: &ProcessDispatcher,
        handle_value: impl Into<HandleValue>,
    ) -> Option<HandleOwner> {
        let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
        ksync::lock!(let mut guard = self.write_lock());
        let mut token_guard = guard.as_mut().as_token_guard_mut();
        self.remove_handle_val_locked(
            Some(caller),
            handle_value.into().raw_value(),
            &mut token_guard,
        )
    }

    /// Resolves a handle to a dispatcher of type `T` without requiring any rights.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_HANDLE` if `handle_value` is not valid.
    /// - `ZX_ERR_WRONG_TYPE` if the dispatcher's type does not match `T::TYPE`.
    #[inline]
    pub fn get_dispatcher<T>(
        &self,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Result<RefPtr<T>, Status>
    where
        T: DispatcherOps + HasRefCount + Recyclable,
    {
        self.get_dispatcher_with_rights::<T>(caller, handle_value, zx_types::ZX_RIGHT_NONE)
    }

    /// Resolves a handle to a dispatcher of type `T` and returns its associated rights.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_HANDLE` if `handle_value` is not valid.
    /// - `ZX_ERR_WRONG_TYPE` if the dispatcher's type does not match `T::TYPE`.
    #[inline]
    pub fn get_dispatcher_and_rights<T>(
        &self,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Result<(RefPtr<T>, zx_rights_t), Status>
    where
        T: DispatcherOps + HasRefCount + Recyclable,
    {
        self.get_dispatcher_and_rights_opt::<T>(Some(caller), handle_value.raw_value(), None)
    }

    /// Resolves a handle to a dispatcher of type `T` with the required `rights` and returns its
    /// actual rights.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_HANDLE` if `handle_value` is not valid.
    /// - `ZX_ERR_WRONG_TYPE` if the dispatcher's type does not match `T::TYPE`.
    /// - `ZX_ERR_ACCESS_DENIED` if `handle_value` lacks the requested `rights`.
    #[inline]
    pub fn get_dispatcher_with_rights_and_actual<T>(
        &self,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
        rights: zx_rights_t,
    ) -> Result<(RefPtr<T>, zx_rights_t), Status>
    where
        T: DispatcherOps + HasRefCount + Recyclable,
    {
        self.get_dispatcher_and_rights_opt::<T>(
            Some(caller),
            handle_value.raw_value(),
            Some(rights),
        )
    }

    /// Resolves a handle to a dispatcher of type `T` with the required `rights` in this handle
    /// table.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_HANDLE` if `handle_value` is not valid.
    /// - `ZX_ERR_WRONG_TYPE` if the dispatcher's type does not match `T::TYPE`.
    /// - `ZX_ERR_ACCESS_DENIED` if `handle_value` lacks the requested `rights`.
    #[inline]
    pub fn get_dispatcher_with_rights<T>(
        &self,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
        rights: zx_rights_t,
    ) -> Result<RefPtr<T>, Status>
    where
        T: DispatcherOps + HasRefCount + Recyclable,
    {
        self.get_dispatcher_with_rights_and_actual::<T>(caller, handle_value, rights)
            .map(|(d, _)| d)
    }

    /// Resolves a handle to a dispatcher of type `T` with the required `rights` in this handle
    /// table, without enforcing the `ZX_POL_BAD_HANDLE` job policy on lookup failure.
    ///
    /// As with [`Self::get_dispatcher_with_rights`], `ZX_ERR_WRONG_TYPE` takes precedence over
    /// `ZX_ERR_ACCESS_DENIED`.
    ///
    /// # Errors
    ///
    /// - `ZX_ERR_BAD_HANDLE` if `handle_value` is not valid.
    /// - `ZX_ERR_WRONG_TYPE` if the dispatcher's type does not match `T::TYPE` (skipped when
    ///   `T` is the generic `Dispatcher`).
    /// - `ZX_ERR_ACCESS_DENIED` if `handle_value` lacks the requested `rights`.
    #[inline]
    pub fn get_dispatcher_with_rights_no_policy_check<T>(
        &self,
        handle_value: HandleValue,
        rights: zx_rights_t,
    ) -> Result<RefPtr<T>, Status>
    where
        T: DispatcherOps + HasRefCount + Recyclable,
    {
        // A `None` caller means ZX_POL_BAD_HANDLE is not enforced on lookup failure.
        self.get_dispatcher_and_rights_opt::<T>(None, handle_value.raw_value(), Some(rights))
            .map(|(d, _)| d)
    }

    /// Removes an array of handle values from the handle table.
    #[inline]
    pub fn remove_handles(
        &self,
        caller: &ProcessDispatcher,
        handles: &[zx_handle_t],
    ) -> Result<(), Status> {
        let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
        let mut status = Ok(());
        ksync::lock!(let mut guard = self.write_lock());
        let mut token_guard = guard.as_mut().as_token_guard_mut();
        for &handle_value in handles {
            if handle_value == ZX_HANDLE_INVALID {
                continue;
            }
            if self.remove_handle_val_locked(Some(caller), handle_value, &mut token_guard).is_none()
            {
                status = Err(Status::BAD_HANDLE);
            }
        }
        status
    }

    /// Resolves a handle to its dispatcher and rights.
    ///
    /// WRONG_TYPE is returned before ACCESS_DENIED, because if the
    /// wrong handle was passed, evaluating its rights does not have
    /// much meaning and also this aids in debugging.
    pub fn get_dispatcher_and_rights_opt<T: DispatcherOps + HasRefCount + Recyclable>(
        &self,
        caller: Option<&ProcessDispatcher>,
        handle_value: zx_handle_t,
        desired_rights: Option<zx_rights_t>,
    ) -> Result<(RefPtr<T>, zx_rights_t), Status> {
        let (generic_disp, rights, has_desired_rights) = {
            // Scope utilized to reduce lock duration.
            ksync::lock!(let guard = self.read_lock());
            let handle_ref = self
                .get_handle_locked_internal(caller, handle_value, guard.token())
                .ok_or(Status::BAD_HANDLE)?;
            let has_desired_rights = desired_rights.is_none_or(|r| handle_ref.has_rights(r));
            (handle_ref.dispatcher(), handle_ref.rights(), has_desired_rights)
        };

        // Wrong type takes precedence over access denied.
        if T::TYPE != zx_types::ZX_OBJ_TYPE_NONE && generic_disp.get_type() != T::TYPE {
            return Err(Status::WRONG_TYPE);
        }

        if !has_desired_rights {
            return Err(Status::ACCESS_DENIED);
        }

        // SAFETY: We verified that generic_disp matches T::TYPE.
        let typed_disp = unsafe { generic_disp.cast::<T>() };
        Ok((typed_disp, rights))
    }

    /// Calls the provided `func(handle, rights, dispatcher)` on every handle owned by the handle
    /// table. Stops if `func` returns an error, returning the error value.
    ///
    /// `func` must not perform any operations on the handle table (e.g. adding or removing
    /// handles).
    ///
    /// The handle table lock is held in read (shared) mode for the duration of this call.
    pub fn for_each_handle<F>(&self, func: F) -> Result<(), Status>
    where
        F: FnMut(zx_handle_t, zx_rights_t, &Dispatcher) -> Result<(), Status>,
    {
        ksync::lock!(let guard = self.read_lock());
        let read_guard = guard.as_token_guard();
        self.for_each_handle_locked(&read_guard, func)
    }

    /// Similar to [`Self::for_each_handle`], but requires the caller to be holding the read lock
    /// (proven by `guard`).
    ///
    /// `func` must not perform any operations on the handle table (e.g. adding or removing
    /// handles).
    fn for_each_handle_locked<'a, 'b, F>(
        &self,
        guard: &HandleTableLockReadTokenGuard<'b, 'a>,
        mut func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(zx_handle_t, zx_rights_t, &Dispatcher) -> Result<(), Status>,
    {
        for handle in guard.handles().iter() {
            let handle_value = self.map_handle_to_value(handle).raw_value();
            func(handle_value, handle.rights(), handle.dispatcher())?;
        }
        Ok(())
    }

    /// Iterates over every handle in batches to minimize lock contention.
    ///
    /// Batch size is bounded to ensure that the lock is not held for an arbitrary amount of time.
    ///
    /// Calls `func(handle, rights, dispatcher)` on every handle.
    ///
    /// `func` may perform operations on the handle table, including adding and removing handles.
    ///
    /// Stops if `func` returns an error, returning the error value.
    ///
    /// While the method acquires the handle table lock, it does not hold the lock while calling
    /// `func`. In other words, the iteration over the handle table is not atomic. This means
    /// that the set of handles `func` sees may be different from the set held by the handle
    /// table at the start or end of the call.
    ///
    /// Handles being added or removed concurrent with `for_each_handle_batched` may or may not be
    /// observed by `func`.
    ///
    /// A Handle observed by `func` may or may not be owned by the handle table at the moment
    /// `func` is invoked, however, it is guaranteed it was held at some point between the
    /// invocation of this method and `func`.
    pub fn for_each_handle_batched<F>(&self, mut func: F) -> Result<(), Status>
    where
        F: FnMut(zx_handle_t, zx_rights_t, &Dispatcher) -> Result<(), Status>,
    {
        // The cursor is linked into `cursors` by raw pointer, so pin it to the stack for the
        // duration of the iteration. `Drop` unregisters it if `func` returns early.
        let mut cursor = core::pin::pin!(HandleCursor::new(self));

        {
            let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
            ksync::lock!(let mut guard = self.write_lock());
            let mut token_guard = guard.as_mut().as_token_guard_mut();
            cursor.as_mut().register(&mut token_guard);
        }

        let mut done = false;
        while !done {
            struct Args {
                handle_value: zx_handle_t,
                rights: zx_rights_t,
                // Use a RefPtr to ensure the dispatcher isn't destroyed out from under func.
                dispatcher: RefPtr<Dispatcher>,
            }
            // The smaller this value is, the more we'll acquire/release the handle table lock.  The
            // larger it is, the longer the duration we'll hold the lock.  This value also impacts
            // the required stack size.
            const MAX_BATCH_SIZE: usize = 64;
            // Option<Args> has the same 16-byte size as Args due to RefPtr's NonNull niche,
            // avoiding unsafe MaybeUninit while automatically dropping any remaining dispatchers
            // if func returns early with an error.
            type Batch = [Option<Args>; MAX_BATCH_SIZE];
            let mut batch: Batch = [const { None }; MAX_BATCH_SIZE];

            // Don't use too much stack space.  The limit here is somewhat arbitrary.
            zr::static_assert!(core::mem::size_of::<Batch>() <= 1024);

            let mut count = 0;

            // Gather a batch of arguments while holding the handle table lock.
            {
                ksync::lock!(let guard = self.read_lock());
                while count < MAX_BATCH_SIZE {
                    let Some(handle) = cursor.as_mut().next(&guard) else {
                        done = true;
                        break;
                    };
                    batch[count] = Some(Args {
                        handle_value: self.map_handle_to_value(handle).raw_value(),
                        rights: handle.rights(),
                        dispatcher: handle.dispatcher().clone(),
                    });
                    count += 1;
                }
            }

            // Now that we have a batch of handles, call func on each one.
            for item in &mut batch[..count] {
                let args = item.take().unwrap();
                func(args.handle_value, args.rights, &args.dispatcher)?;
            }
        }

        Ok(())
    }

    /// Populates extended handle information into a newly allocated buffer.
    pub fn get_handle_info(&self) -> Result<Array<zx_info_handle_extended_t>, Status> {
        loop {
            let count = self.handle_count() as usize;
            // TODO: Bug 45685. This memory allocation should come from a different pool since it
            // can be larger than one page.
            let mut slice = Array::<zx_info_handle_extended_t>::try_new_zeroed_slice(count)
                .map_err(|_| Status::NO_MEMORY)?;

            {
                ksync::lock!(let guard = self.read_lock());
                if count != *guard.count() as usize {
                    continue;
                }

                let mut index = 0;
                let read_guard = guard.as_token_guard();
                self.for_each_handle_locked(&read_guard, |handle, rights, disp| {
                    debug_assert!(index < slice.len());
                    slice[index] = zx_info_handle_extended_t {
                        type_: disp.get_type(),
                        handle_value: handle,
                        rights,
                        reserved: 0,
                        koid: disp.get_koid(),
                        related_koid: disp.get_related_koid(),
                        peer_owner_koid: 0,
                    };
                    index += 1;
                    Ok(())
                })?;
            }
            return Ok(slice);
        }
    }

    /// Cleans up all handles in the handle table when all associated processes terminate.
    pub fn clean(&self) {
        stack_pin_init!(let to_clean = DoublyLinkedList::<NonNull<Handle>>::new());
        {
            let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
            ksync::lock!(let mut guard = self.write_lock());
            let mut token_guard = guard.as_mut().as_token_guard_mut();
            HandleCursor::invalidate_all(&mut token_guard);
            *token_guard.count_mut() = 0;
            let handles = token_guard.handles_mut();
            for handle in handles.iter() {
                handle.set_handle_table_id(ZX_KOID_INVALID);
            }
            to_clean.as_mut().splice(handles);
        }

        // This needs to be done outside of the critical section above.
        while let Some(handle_ptr) = to_clean.as_mut().pop_front() {
            // Delete handle via HandleOwner dtor.
            // SAFETY: `handle_ptr` was unlinked from the table and is a valid allocated Handle.
            drop(unsafe { HandleOwner::from_non_null(handle_ptr) });
        }
    }
}

#[pinned_drop]
impl PinnedDrop for HandleTable {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        debug_assert!(this.handles.get_pinned_mut().is_empty());
        debug_assert_eq!(*this.count.get_inner_mut(), 0);
        debug_assert!(this.cursors.get_pinned_mut().is_empty());
    }
}

/// RAII reader lock guard for a handle table.
pub type HandleTableReadGuard<'a> = HandleTableLockReadGuard<'a>;

/// RAII writer lock guard for a handle table.
pub type HandleTableWriteGuard<'a> = HandleTableLockWriteGuard<'a>;

impl HandleTableLockReadGuard<'_> {
    /// Retrieves a handle reference while holding the handle table lock.
    #[inline]
    pub fn get_handle(
        &self,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Option<HandleRef<'_>> {
        self.parent.get_handle_locked_internal(Some(caller), handle_value.raw_value(), self.token())
    }
}

impl HandleTableLockWriteGuard<'_> {
    /// Retrieves a handle reference while holding the handle table write lock.
    #[inline]
    pub fn get_handle(
        &self,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Option<HandleRef<'_>> {
        self.parent.get_handle_locked_internal(Some(caller), handle_value.raw_value(), self.token())
    }

    /// Adds an owned handle to the handle table under the write lock this guard holds.
    #[inline]
    pub fn add_handle(self: Pin<&mut Self>, handle: HandleOwner) {
        let parent = self.parent;
        let mut guard = self.as_token_guard_mut();
        parent.add_handle_locked_guard(handle, &mut guard);
    }

    /// Removes a handle by value from the handle table under the write lock this guard holds.
    #[inline]
    pub fn remove_handle(
        self: Pin<&mut Self>,
        caller: &ProcessDispatcher,
        handle_value: HandleValue,
    ) -> Option<HandleOwner> {
        let parent = self.parent;
        let mut guard = self.as_token_guard_mut();
        parent.remove_handle_val_locked(Some(caller), handle_value.raw_value(), &mut guard)
    }
}

// C-compatible FFI exported symbols.

/// Initializes a newly allocated `HandleTable` instance in-place.
///
/// # Safety
/// `table` must point to valid, alloc-aligned memory for `HandleTable`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_handle_table_init(table: *mut HandleTable) {
    let init = HandleTable::init();
    // SAFETY: table is a valid pointer in C++.
    unsafe {
        let _ = pin_init::PinInit::__pinned_init(init, table);
    }
}

/// Cleans up a `HandleTable` before C++ member destructors run.
///
/// # Safety
/// `table` must point to a valid `HandleTable`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_handle_table_destroy(table: *mut HandleTable) {
    // SAFETY: `table` is a valid, initialized HandleTable pointer that is being destroyed.
    unsafe {
        core::ptr::drop_in_place(table);
    }
}

/// Returns the current number of handles in `table`.
///
/// # Safety
/// `table` must be a valid reference to an initialized `HandleTable`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_handle_table_count(table: &HandleTable) -> u32 {
    table.handle_count()
}

/// Adds a raw `Handle` pointer to `table`, transferring ownership to the table.
///
/// # Safety
/// `handle` must point to a valid, unowned `Handle` allocated in the arena.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_handle_table_add_handle(table: &HandleTable, handle: *mut Handle) {
    // SAFETY: `handle` is checked for non-null and is a valid allocated Handle.
    if let Some(owner) = unsafe { HandleOwner::from_raw(handle) } {
        table.add_handle(owner);
    }
}

/// Cleans and invalidates all handles in `table`.
///
/// # Safety
/// `table` must be a valid reference to an initialized `HandleTable`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_handle_table_clean(table: &HandleTable) -> zx_types::zx_status_t {
    table.clean();
    zx_types::ZX_OK
}

/// Callback function signature invoked for each handle in a `HandleTable`.
pub type HandleTableForEachCallback = unsafe extern "C" fn(
    handle: zx_handle_t,
    rights: zx_rights_t,
    disp: *const Dispatcher,
    ctx: *mut c_void,
) -> zx_types::zx_status_t;

/// Iterates over all handles in `table` holding the read lock and invokes `cb`.
///
/// # Safety
/// `cb` must be a valid function pointer. `ctx` is passed directly to `cb`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_handle_table_for_each_handle(
    table: &HandleTable,
    cb: HandleTableForEachCallback,
    ctx: *mut c_void,
) -> zx_types::zx_status_t {
    let res = table.for_each_handle(|handle, rights, disp| {
        // SAFETY: `cb` is a valid callback function pointer.
        let s = unsafe { cb(handle, rights, disp as *const Dispatcher, ctx) };
        Status::ok(s)
    });
    Status::result_into_raw(res)
}

/// Unit tests for HandleTable.
#[cfg(ktest)]
#[unittest::suite(name = "handle_table_rust")]
mod tests {
    use super::{Handle, HandleTable, map_value_to_handle};
    use crate::object::{
        EventDispatcher, EventPairDispatcher, KernelHandle, ProcessDispatcher,
        get_root_job_dispatcher,
    };
    use fbl::RefPtr;
    use unittest::{expect_eq, expect_ok, expect_true};
    use zx_status::Status;
    use zx_types::{ZX_HANDLE_INVALID, zx_rights_t};

    /// Tests handle table creation and count.
    #[test]
    fn test_handle_table_create_and_count() {
        let table = HandleTable::create().expect("failed to create handle table");
        expect_eq!(table.handle_count(), 0);
        expect_true!(table.get_koid() != 0);
    }

    /// Tests adding and removing a handle from the table.
    #[test]
    fn test_handle_table_add_remove_handle() {
        let table = HandleTable::create().expect("failed to create handle table");
        let (ep0, ep1, rights) = EventPairDispatcher::create().expect("failed to create eventpair");
        let handle_owner = Handle::make_from_kernel_handle(ep0.cast(), rights).unwrap();

        let val = table.map_handle_to_value_owner(&handle_owner);
        table.add_handle(handle_owner);
        expect_eq!(table.handle_count(), 1);

        let retrieved = {
            ksync::lock!(let guard = table.read_lock());
            let handle_ref = table.get_handle_locked_internal(None, val, guard.token());
            expect_true!(handle_ref.is_some());
            expect_eq!(handle_ref.unwrap().rights(), rights);
            handle_ref.unwrap().dispatcher().get_koid()
        };
        expect_eq!(retrieved, ep1.dispatcher().get_related_koid());

        let removed = {
            ksync::lock!(let mut guard = table.write_lock());
            let mut token_guard = guard.as_mut().as_token_guard_mut();
            let handle_ptr = map_value_to_handle(val, table.random_value).unwrap();
            // SAFETY: `handle_ptr` was added to `table` and is currently linked in `table`.
            unsafe { table.remove_handle_ptr_locked(handle_ptr, &mut token_guard) }
        };
        expect_eq!(table.handle_count(), 0);
        drop(removed);
        drop(ep1);
    }

    /// Tests batched handle iteration with >64 handles.
    #[test]
    fn test_handle_table_for_each_handle_batched() {
        let table = HandleTable::create().expect("failed to create handle table");
        const COUNT: usize = 70;
        let mut peers: [Option<KernelHandle<EventPairDispatcher>>; COUNT] = [const { None }; COUNT];

        for peer in &mut peers {
            let (ep0, ep1, rights) =
                EventPairDispatcher::create().expect("failed to create eventpair");
            let ho = Handle::make_from_kernel_handle(ep0.cast(), rights).unwrap();
            table.add_handle(ho);
            *peer = Some(ep1);
        }

        expect_eq!(table.handle_count(), COUNT as u32);

        let mut observed_count = 0;
        let res = table.for_each_handle_batched(|_handle, _rights, _disp| {
            observed_count += 1;
            Ok(())
        });
        expect_ok!(res);
        expect_eq!(observed_count, COUNT);

        table.clean();
        expect_eq!(table.handle_count(), 0);
        drop(peers);
    }

    /// Tests process termination cleanup parity.
    #[test]
    fn test_handle_table_shareable_process_state_parity() {
        let table = HandleTable::create().expect("failed to create handle table");
        let (ep0, ep1, rights) = EventPairDispatcher::create().expect("failed to create eventpair");
        expect_eq!(table.handle_count(), 0);

        let ho = Handle::make_from_kernel_handle(ep0.cast(), rights).unwrap();
        table.add_handle(ho);
        expect_eq!(table.handle_count(), 1);

        table.clean();
        expect_eq!(table.handle_count(), 0);
        expect_true!(
            ep1.dispatcher().user_signal_peer(0, zx_types::ZX_USER_SIGNAL_0)
                == Err(Status::PEER_CLOSED)
        );
    }

    /// Tests removing multiple handles with `remove_handles`.
    #[test]
    fn test_handle_table_remove_handles() {
        let (proc_handle, _proc_rights, _vmar, _vmar_rights) =
            ProcessDispatcher::create(get_root_job_dispatcher().clone(), b"test-proc", 0)
                .expect("failed to create process");
        let process = proc_handle.dispatcher();
        let table = process.handle_table();

        let (ep0_a, ep1_a, rights_a) =
            EventPairDispatcher::create().expect("failed to create eventpair");
        let (ep0_b, ep1_b, rights_b) =
            EventPairDispatcher::create().expect("failed to create eventpair");

        let ho_a = Handle::make_from_kernel_handle(ep0_a.cast(), rights_a).unwrap();
        let ho_b = Handle::make_from_kernel_handle(ep0_b.cast(), rights_b).unwrap();

        let val_a = table.map_handle_to_value_owner(&ho_a);
        let val_b = table.map_handle_to_value_owner(&ho_b);

        table.add_handle(ho_a);
        table.add_handle(ho_b);
        expect_eq!(table.handle_count(), 2);

        // Include ZX_HANDLE_INVALID in the array to verify it is skipped.
        let handles = [val_a, ZX_HANDLE_INVALID, val_b];
        let status = table.remove_handles(process, &handles);
        expect_ok!(status);
        expect_eq!(table.handle_count(), 0);

        // Unknown / already-removed handle values must return Err(Status::BAD_HANDLE).
        let bad_handles = [val_a, ZX_HANDLE_INVALID];
        let status = table.remove_handles(process, &bad_handles);
        expect_true!(status == Err(Status::BAD_HANDLE));

        drop(ep1_a);
        drop(ep1_b);
    }

    /// Tests retrieving dispatcher and rights with various types and rights checks.
    #[test]
    fn test_handle_table_get_dispatcher_and_rights() {
        let table = HandleTable::create().expect("failed to create handle table");
        let (ep0, ep1, rights) = EventPairDispatcher::create().expect("failed to create eventpair");
        let ho = Handle::make_from_kernel_handle(ep0.cast(), rights).unwrap();
        let val = table.map_handle_to_value_owner(&ho);
        table.add_handle(ho);

        // Correct retrieval with exact rights:
        let res: Result<(RefPtr<EventPairDispatcher>, zx_rights_t), Status> =
            table.get_dispatcher_and_rights_opt(None, val, Some(rights));
        expect_true!(res.is_ok());
        let (disp, retrieved_rights) = res.unwrap();
        expect_eq!(retrieved_rights, rights);
        expect_eq!(disp.get_related_koid(), ep1.dispatcher().get_koid());

        // WRONG_TYPE when querying a different type (e.g., EventDispatcher instead of
        // EventPairDispatcher):
        let res_wrong_type: Result<(RefPtr<EventDispatcher>, zx_rights_t), Status> =
            table.get_dispatcher_and_rights_opt(None, val, None);
        expect_true!(res_wrong_type.err() == Some(Status::WRONG_TYPE));

        // ACCESS_DENIED when querying with missing rights:
        let excess_rights = rights | zx_types::ZX_RIGHT_EXECUTE;
        let res_access_denied: Result<(RefPtr<EventPairDispatcher>, zx_rights_t), Status> =
            table.get_dispatcher_and_rights_opt(None, val, Some(excess_rights));
        expect_true!(res_access_denied.err() == Some(Status::ACCESS_DENIED));

        // Cleanup
        table.clean();
        drop(ep1);
    }
}
