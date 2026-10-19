// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::ArchMmuFlags;
use super::attribution::AttributionCounts;
use super::page::VmPagePtr;
use super::page_source::MultiPageRequest;
use super::stream_size_manager::StreamSizeManager;
use super::vm_mapping::VmMapping;
use super::vm_object_paged::VmObjectPaged;
use super::vm_page_list::VmPageSpliceList;
use crate::kernel::types::PAddr;
use crate::user_copy::{UserInPtr, UserOutPtr};
use bitflags::bitflags;
use core::convert::Infallible;
use core::ffi::c_void;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::{ManuallyDrop, MaybeUninit};
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{HasRefCount, Recyclable, RefPtr};
use kalloc::AllocError;
use ksync::{KMutex, KMutexGuard, LockToken, RawCriticalMutex};
use page;
use pin_init::PinInit;
use vm_object_bindings as bindings;
use zr::Opaque;
use zx_status::Status;
use zx_types::{ZX_KOID_INVALID, zx_koid_t, zx_pager_vmo_stats_t, zx_status_t};

pub use bindings::{
    Resizability, SnapshotType, VmObject_CacheOpType as CacheOpType,
    VmObject_ChildType as ChildType, VmObject_EvictionHint as EvictionHint, VmObjectChildObserver,
};
pub use zx_types::zx_vmo_lock_state_t;

/// Argument that specifies the context in which we are supplying pages.
pub type SupplyOptions = bindings::SupplyOptions;

bitflags! {
    #[repr(transparent)]
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    pub struct VmObjectReadWriteOptions: u8 {
        const NONE = 0;

        /// If set, attempts to read past the end of a VMO will not cause a failure and only copy
        /// the existing bytes instead (i.e. the requested length will be trimmed to the actual VMO
        /// size).
        const TRIM_LENGTH = (1 << 0);
    }
}

zr::static_assert!(
    VmObjectReadWriteOptions::NONE.bits() == bindings::VmObjectReadWriteOptions_None
);
zr::static_assert!(
    VmObjectReadWriteOptions::TRIM_LENGTH.bits() == bindings::VmObjectReadWriteOptions_TrimLength
);

pub use crate::vm::vm_cow_pages::VmCowPagesLockClass as VmObjectLockClass;

pub type VmObjectMutex = KMutex<VmObjectLockClass, RawCriticalMutex>;

/// The base vm object that holds a range of bytes of data
///
/// Can be created without mapping and used as a container of data, or mappable
/// into an address space via VmAddressRegion::CreateVmMapping
#[repr(C)]
pub struct VmObject {
    raw: Opaque<bindings::VmObject>,
    phantom: PhantomData<PhantomPinned>,
}

impl VmObject {
    pub const MAX_SIZE: u64 = bindings::VmObject_MAX_SIZE;

    /// Helper to round to the VMO size multiple (which is `kPageSize`) without overflowing.
    pub fn round_size(size: u64) -> Result<u64, Status> {
        let mask = page::MASK as u64;
        let rounded = size.checked_add(mask).ok_or(Status::OUT_OF_RANGE)? & !mask;
        Ok(rounded)
    }

    /// Domain-specific conversion: returns raw pointer for `VmObject`.
    pub fn as_raw(&self) -> *mut bindings::VmObject {
        self.raw.get()
    }

    /// Domain-specific conversion: constructs a `RefPtr` from an exported pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid raw `VmObject` pointer exported from C++.
    pub unsafe fn from_raw(ptr: *mut bindings::VmObject) -> Option<RefPtr<Self>> {
        unsafe { RefPtr::try_from_raw(ptr.cast::<Self>()) }
    }

    /// Returns a raw `VmObject` pointer from an underlying bindings pointer.
    ///
    /// Provides additional type safety when used instead of a `.cast()`.
    pub fn ptr_from_raw(raw: *mut bindings::VmObject) -> *mut VmObject {
        raw.cast()
    }

    /// Returns a pointer to the underlying `VmObject` structure.
    ///
    /// This method is helpful when you don't have a reference to the `VmObject`. If you do, then
    /// use `VmObject::as_raw` instead.
    pub fn cast_raw(ptr: *mut VmObject) -> *mut bindings::VmObject {
        ptr.cast()
    }

    /// Returns the size of the VMO in bytes.
    pub fn size(&self) -> u64 {
        ksync::lock!(let guard = self.lock());
        self.size_locked(guard.token())
    }

    pub fn size_locked(&self, _token: &LockToken<'_, VmObjectLockClass>) -> u64 {
        // SAFETY: self.as_raw() is a valid VmObject pointer and the VMO lock is held.
        unsafe { bindings::cpp_vm_object_size_locked(self.as_raw()) }
    }

    /// Returns whether the VMO is resizable.
    pub fn is_resizable(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_is_resizable(self.as_raw()) }
    }

    /// Returns whether the VMO is discardable.
    pub fn is_discardable(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_is_discardable(self.as_raw()) }
    }

    /// Returns whether the VMO is paged.
    pub fn is_paged(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_is_paged(self.as_raw()) }
    }
    /// Returns whether the VMO is contiguous.
    pub fn is_contiguous(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_is_contiguous(self.as_raw()) }
    }

    /// Returns whether the VMO is stream compatible.
    pub fn is_stream_compatible(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_is_stream_compatible(self.as_raw()) }
    }

    #[inline]
    pub fn lock(
        &self,
    ) -> impl PinInit<KMutexGuard<'_, VmObjectLockClass, RawCriticalMutex>, Infallible> {
        self.lock_ref().lock()
    }

    #[inline]
    pub fn lock_ref(&self) -> &VmObjectMutex {
        // SAFETY: The pointer returned by cpp_vm_object_lock is valid for reads for the lifetime of self.
        unsafe { &*bindings::cpp_vm_object_lock(self.as_raw()).cast::<VmObjectMutex>() }
    }

    /// Resizes the VMO to the given size.
    pub fn resize(&self, size: u64) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        let status = unsafe { bindings::cpp_vm_object_resize(self.as_raw(), size) };
        Status::ok(status)
    }

    /// Writes data from `data` slice into the VMO at `offset`.
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer and `data` points to
        // `data.len()` bytes of valid memory.
        let status = unsafe {
            bindings::cpp_vm_object_write(self.as_raw(), data.as_ptr().cast(), offset, data.len())
        };
        Status::ok(status)
    }

    /// Sets the name of the VMO.
    pub fn set_name(&self, name: &[u8]) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer and `name` points to
        // `name.len()` bytes of valid memory.
        let status = unsafe {
            bindings::cpp_vm_object_set_name(self.as_raw(), name.as_ptr().cast(), name.len())
        };
        Status::ok(status)
    }

    /// Gets the name of the VMO.
    pub fn get_name(&self, out_name: &mut [u8]) {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer and `out_name` points to
        // `out_name.len()` bytes of valid memory.
        unsafe {
            bindings::cpp_vm_object_get_name(
                self.as_raw(),
                out_name.as_mut_ptr().cast(),
                out_name.len(),
            );
        }
    }

    /// Sets the child observer for the VMO.
    ///
    /// # Safety
    ///
    /// `child_observer` must point to a valid `VmObjectChildObserver` or be null.
    pub unsafe fn set_child_observer(&self, child_observer: *mut VmObjectChildObserver) {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe {
            bindings::cpp_vm_object_set_child_observer(self.as_raw(), child_observer);
        }
    }

    /// Decommit a range of pages from the VMO.
    pub fn decommit_range(&self, offset: u64, len: u64) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        let status = unsafe { bindings::cpp_vm_object_decommit_range(self.as_raw(), offset, len) };
        Status::ok(status)
    }

    /// Commits the specified range of pages in the VMO.
    pub fn commit_range(&self, offset: u64, len: u64) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        let status = unsafe { bindings::cpp_vm_object_commit_range(self.as_raw(), offset, len) };
        Status::ok(status)
    }

    /// Commits and pins the specified range of pages in the VMO.
    pub fn commit_range_pinned(&self, offset: u64, len: u64, write: bool) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        let status = unsafe {
            bindings::cpp_vm_object_commit_range_pinned(self.as_raw(), offset, len, write)
        };
        Status::ok(status)
    }

    /// Unpins the specified range of pages in the VMO.
    pub fn unpin(&self, offset: u64, len: u64) {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe {
            bindings::cpp_vm_object_unpin(self.as_raw(), offset, len);
        }
    }

    /// Provide an eviction hint for a range of pages.
    pub fn hint_range(&self, offset: u64, len: u64, hint: EvictionHint) -> Result<(), Status> {
        let status =
            unsafe { bindings::cpp_vm_object_hint_range(self.as_raw(), offset, len, hint) };
        Status::ok(status)
    }

    /// Performs any page commits necessary for a VMO with high memory priority over the given
    /// range. This method is always safe to call as it will internally check the memory priority
    /// status and skip if necessary, so the caller does not need to worry about races with the VMO
    /// no longer being high priority. As this may need to acquire the lock even to check the memory
    /// priority, if the caller knows they have not caused this VMO to become high priority (i.e.
    /// they have not called `change_high_priority_count_locked` with a positive value), then
    /// calling this should be skipped for performance. This method has no return value as it is
    /// entirely best effort and no part of its operation is needed for correctness.
    pub fn commit_high_priority_pages(&self, offset: u64, len: u64) {
        // SAFETY: `self.as_raw()` points to a live `VmObject`.
        unsafe { bindings::cpp_vm_object_commit_high_priority_pages(self.as_raw(), offset, len) }
    }

    /// Returns the mapping cache policy of the VMO.
    pub fn get_mapping_cache_policy(&self) -> ArchMmuFlags {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_get_mapping_cache_policy(self.as_raw()) }
    }

    pub(super) fn get_mapping_cache_policy_locked(
        &self,
        _token: &LockToken<'_, VmObjectLockClass>,
    ) -> ArchMmuFlags {
        // SAFETY: The object lock is held.
        unsafe { bindings::cpp_vm_object_get_mapping_cache_policy_locked(self.as_raw()) }
    }

    /// Attempt to add a new mapping to the mapping list of this object. Can fail with
    /// `Status::NO_MEMORY`.
    pub(super) fn add_mapping_locked(
        &self,
        _token: &mut LockToken<'_, VmObjectLockClass>,
        mapping: &VmMapping,
    ) -> Result<(), Status> {
        // SAFETY: The object lock is held.
        let status = unsafe {
            bindings::cpp_vm_object_add_mapping_locked(
                self.as_raw(),
                core::ptr::from_ref(mapping).cast_mut().cast(),
            )
        };
        Status::ok(status)
    }

    pub(super) fn remove_mapping_locked(
        &self,
        _token: &mut LockToken<'_, VmObjectLockClass>,
        mapping: &VmMapping,
    ) {
        // SAFETY: The object lock is held.
        unsafe {
            bindings::cpp_vm_object_remove_mapping_locked(
                self.as_raw(),
                core::ptr::from_ref(mapping).cast_mut().cast(),
            );
        }
    }

    /// Sets the mapping cache policy of the VMO.
    ///
    /// # Safety
    ///
    /// Ensure `cache_policy` is appropriate for all future mappings of this VMO.
    pub unsafe fn set_mapping_cache_policy(
        &self,
        cache_policy: ArchMmuFlags,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        let status = unsafe {
            bindings::cpp_vm_object_set_mapping_cache_policy(self.as_raw(), cache_policy)
        };
        Status::ok(status)
    }

    /// Create a copy-on-write clone VMO at the page-aligned offset and length.
    pub fn create_clone(
        &self,
        resizable: Resizability,
        snapshot_type: SnapshotType,
        offset: u64,
        size: u64,
        copy_name: bool,
    ) -> Result<RefPtr<VmObject>, Status> {
        let mut status = 0;
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        let raw = unsafe {
            bindings::cpp_vm_object_create_clone(
                self.as_raw(),
                resizable,
                snapshot_type,
                offset,
                size,
                copy_name,
                &mut status,
            )
        };
        Status::ok(status)?;
        // SAFETY: cpp_vm_object_create_clone returns valid VmObject pointers, or null.
        let clone = unsafe { VmObject::from_raw(raw) };
        Ok(clone.expect("clone returned ZX_OK; must be non-null"))
    }

    /// Creates a child slice of this VMO.
    pub fn create_child_slice(
        &self,
        offset: u64,
        size: u64,
        copy_name: bool,
    ) -> Result<RefPtr<VmObject>, Status> {
        let mut status = 0;
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        let raw = unsafe {
            bindings::cpp_vm_object_create_child_slice(
                self.as_raw(),
                offset,
                size,
                copy_name,
                &mut status,
            )
        };
        Status::ok(status)?;
        // SAFETY: cpp_vm_object_create_child_slice returns a valid VmObject pointer on ZX_OK.
        let slice = unsafe { VmObject::from_raw(raw) };
        Ok(slice.expect("create_child_slice returned ZX_OK; must be non-null"))
    }

    /// Creates a child reference to this VMO, returning the child and whether it is
    /// the first child.
    pub fn create_child_reference(
        &self,
        resizable: Resizability,
        offset: u64,
        size: u64,
        copy_name: bool,
    ) -> Result<(RefPtr<VmObject>, bool), Status> {
        let mut status = 0;
        let mut first_child = false;
        // SAFETY: `self.as_raw()` points to a live `VmObject`, and `first_child` and `status` are
        // valid pointers to local stack memory for writing.
        let raw = unsafe {
            bindings::cpp_vm_object_create_child_reference(
                self.as_raw(),
                resizable,
                offset,
                size,
                copy_name,
                &mut first_child,
                &mut status,
            )
        };
        Status::ok(status)?;
        // SAFETY: `raw` points to a live `VmObject` returned by
        // `cpp_vm_object_create_child_reference` on `ZX_OK`.
        let child = unsafe { VmObject::from_raw(raw) };
        Ok((child.expect("create_child_reference returned ZX_OK; must be non-null"), first_child))
    }

    /// Helper variant of get_page that will retry the operation after waiting on a PageRequest if
    /// required.
    ///
    /// Must not be called with any locks held.
    pub fn get_page_blocking(
        &self,
        offset: u64,
        pf_flags: u32,
    ) -> Result<(VmPagePtr, PAddr), Status> {
        let mut page_ptr = core::ptr::null_mut();
        let mut paddr = 0;
        // SAFETY: `self.as_raw()` points to a live `VmObject`, and `page_ptr` and `paddr` point to
        // stack storage valid for writing.
        let status = unsafe {
            bindings::cpp_vm_object_get_page_blocking(
                self.as_raw(),
                offset,
                pf_flags,
                &mut page_ptr,
                &mut paddr,
            )
        };
        Status::ok(status)?;
        // SAFETY: `page_ptr` points to a live `vm_page_t` returned by `GetPageBlocking` on `ZX_OK`.
        let page = unsafe { VmPagePtr::from_ffi(page_ptr) }.expect("page pointer is non-null");
        Ok((page, PAddr(paddr)))
    }

    /// Downcasts a `RefPtr<VmObject>` by value into a `RefPtr<VmObjectPaged>` if it is a paged VMO.
    pub fn downcast_paged(this: RefPtr<Self>) -> Option<RefPtr<VmObjectPaged>> {
        let this = ManuallyDrop::new(this);
        // SAFETY: `this.as_raw()` returns a valid `VmObject` pointer.
        let raw =
            unsafe { vm_object_paged_bindings::cpp_vm_object_as_vm_object_paged(this.as_raw()) };
        if raw.is_null() {
            drop(ManuallyDrop::into_inner(this));
            None
        } else {
            // SAFETY: `raw` points to a valid `VmObjectPaged` whose reference count is owned by
            // `this`.
            unsafe { VmObjectPaged::from_raw(raw) }
        }
    }

    /// Downcasts a `&VmObject` into a `&VmObjectPaged` if it is a paged VMO.
    pub fn as_paged(&self) -> Option<&VmObjectPaged> {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        let raw =
            unsafe { vm_object_paged_bindings::cpp_vm_object_as_vm_object_paged(self.as_raw()) };
        if raw.is_null() {
            None
        } else {
            // SAFETY: `raw` points to a valid `VmObjectPaged` whose lifetime matches `self`.
            unsafe { Some(&*raw.cast::<VmObjectPaged>()) }
        }
    }

    /// Sets the user ID of the VMO.
    pub fn set_user_id(&self, user_id: u64) {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_set_user_id(self.as_raw(), user_id) }
    }

    /// Returns the user ID of the VMO.
    pub fn user_id(&self) -> u64 {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_user_id(self.as_raw()) }
    }

    /// Returns the user ID of the parent VMO, if any.
    pub fn parent_user_id(&self) -> u64 {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_parent_user_id(self.as_raw()) }
    }

    /// Returns the number of children of the VMO.
    pub fn num_children(&self) -> u32 {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_num_children(self.as_raw()) }
    }

    /// Returns the number of mappings of the VMO.
    pub fn num_mappings(&self) -> u32 {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_num_mappings(self.as_raw()) }
    }

    /// Returns an estimate of the number of unique VmAspaces that this object is mapped into.
    pub fn share_count(&self) -> u32 {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_share_count(self.as_raw()) }
    }

    /// Returns whether the VMO is backed by a user pager.
    pub fn is_user_pager_backed(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_is_user_pager_backed(self.as_raw()) }
    }

    /// Returns the child type of the VMO.
    pub fn child_type(&self) -> ChildType {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_child_type(self.as_raw()) }
    }

    /// Returns the number of bytes allocated in the kernel heap by this VMO.
    pub fn heap_allocation_bytes(&self) -> usize {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_heap_allocation_bytes(self.as_raw()) as usize }
    }

    /// Performs the specified cache operation on a range of the VMO.
    pub fn cache_op(&self, offset: u64, len: u64, op: CacheOpType) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        let status = unsafe { bindings::cpp_vm_object_cache_op(self.as_raw(), offset, len, op) };
        Status::ok(status)
    }

    /// Prefetches a range of pages in the VMO.
    pub fn prefetch_range(&self, offset: u64, len: u64) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        let status = unsafe { bindings::cpp_vm_object_prefetch_range(self.as_raw(), offset, len) };
        Status::ok(status)
    }

    /// Zeroes a range of the VMO and untracks it from dirty tracking.
    pub fn zero_range_untracked(&self, offset: u64, len: u64) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        let status =
            unsafe { bindings::cpp_vm_object_zero_range_untracked(self.as_raw(), offset, len) };
        Status::ok(status)
    }

    /// Provides the VMO with a user defined queryable byte aligned size.
    pub fn set_user_stream_size(&self, ssm: Option<RefPtr<StreamSizeManager>>) {
        let raw_ssm = match ssm {
            Some(ssm) => RefPtr::into_raw(ssm).cast_mut().cast(),
            None => core::ptr::null_mut(),
        };
        // SAFETY: `self.as_raw()` is a valid pointer and `raw_ssm` is either null or a valid
        // pointer whose ownership is transferred to `cpp_vm_object_set_user_stream_size`.
        unsafe {
            bindings::cpp_vm_object_set_user_stream_size(self.as_raw(), raw_ssm);
        }
    }

    /// execute lookup_fn on a given range of physical addresses within the vmo. Only pages that are
    /// present and writable in this VMO will be enumerated. Any copy-on-write pages in our parent
    /// will not be enumerated. The physical addresses given to the lookup_fn should not be retained
    /// in any way unless the range has also been pinned by the caller. Offsets provided will be in
    /// relation to the object being queried, even if pages are actually from a parent object where
    /// this is a slice.
    /// Ranges of length zero are considered invalid and will return
    /// ZX_ERR_INVALID_ARGS. The lookup_fn can terminate iteration early by returning ZX_ERR_STOP.
    pub fn lookup<T: Sized>(
        &self,
        offset: u64,
        len: u64,
        ctx: &mut T,
        lookup_fn: fn(u64, PAddr, &mut T) -> Result<(), Status>,
    ) -> Result<(), Status> {
        struct LookupState<'a, T> {
            ctx: &'a mut T,
            lookup_fn: fn(u64, PAddr, &mut T) -> Result<(), Status>,
        }

        /// # Safety
        ///
        /// `ctx` must point to a valid `LookupState<'_, T>` created on the stack in `lookup`
        /// that remains valid for the duration of the C++ FFI lookup callback.
        unsafe extern "C" fn lookup_callback_shim<T>(
            ctx: *mut core::ffi::c_void,
            offset: u64,
            paddr: u64,
        ) -> zx_status_t {
            // SAFETY: `ctx` is guaranteed by `cpp_vm_object_lookup` to be the non-null `ctx_ptr`
            // passed from `lookup`, which points to a live `LookupState<'_, T>` on the caller's
            // stack.
            let state = unsafe { ctx.cast::<LookupState<'_, T>>().as_mut_unchecked() };
            Status::result_into_raw((state.lookup_fn)(offset, paddr.into(), state.ctx))
        }

        let mut state = LookupState { ctx, lookup_fn };
        let state_ptr: *mut LookupState<'_, T> = &mut state;
        // Erase the Rust type so we can pass our context pointer through C++'s void* argument.
        let ctx_ptr: *mut core::ffi::c_void = state_ptr.cast();
        let status = unsafe {
            bindings::cpp_vm_object_lookup(
                self.as_raw(),
                offset,
                len,
                ctx_ptr,
                Some(lookup_callback_shim::<T>),
            )
        };
        Status::ok(status)
    }

    /// Attempts to lookup the given range in the VMO. If it exists and is physically contiguous
    /// returns the paddr of the start of the range. The offset must be page aligned.
    /// Ranges of length zero are considered invalid and will return ZX_ERR_INVALID_ARGS.
    pub fn lookup_contiguous(&self, offset: u64, len: u64) -> Result<PAddr, Status> {
        let mut paddr = 0;
        // SAFETY: `self.as_raw()` points to a live `VmObject`, and `paddr` points to
        // valid stack storage for writing.
        let status = unsafe {
            bindings::cpp_vm_object_lookup_contiguous(self.as_raw(), offset, len, &mut paddr)
        };
        Status::ok(status)?;
        Ok(PAddr(paddr))
    }

    /// Gets a pointer to the page structure at the specified offset.
    /// Valid flags are `fault::flag::*`.
    ///
    /// `page_request` must be `Some` if any flags in `fault::flag::FAULT_MASK` are set, unless
    /// the caller knows that the VMO is not paged.
    ///
    /// Returns `Err(Status::SHOULD_WAIT)` if the caller should try again after waiting on the
    /// `MultiPageRequest`.
    ///
    /// Returns `Err(Status::NEXT)` if `page_request` supports batching and the current request
    /// can be batched. The caller should continue to make successive `get_page` requests
    /// until this returns `Err(Status::SHOULD_WAIT)`. If the caller runs out of requests, it
    /// should finalize the request with `PageSource::FinalizeRequest`.
    ///
    /// # Safety
    ///
    /// Callers must satisfy all safety, batching, and lifecycle obligations described in the
    /// documentation above.
    pub unsafe fn get_page(
        &self,
        offset: u64,
        pf_flags: u32,
        page_request: Option<Pin<&mut MultiPageRequest>>,
    ) -> Result<(VmPagePtr, PAddr), Status> {
        let req_ptr = match page_request {
            Some(req) => req.as_raw(),
            None => core::ptr::null_mut(),
        };
        let mut page_ptr = core::ptr::null_mut();
        let mut paddr: zx_types::zx_paddr_t = 0;
        // SAFETY: Caller of `get_page` guarantees underlying safety obligations are met. All
        // pointers passed to `cpp_vm_object_get_page` are valid for required accesses.
        let status = unsafe {
            bindings::cpp_vm_object_get_page(
                self.as_raw(),
                offset,
                pf_flags,
                req_ptr,
                &mut page_ptr,
                &mut paddr,
            )
        };
        Status::ok(status)?;
        let page = unsafe { VmPagePtr::from_ffi(page_ptr) }.expect("page pointer is non-null");
        Ok((page, PAddr(paddr)))
    }

    /// Returns the number of physical bytes currently attributed to this VMO.
    pub fn get_attributed_memory(&self) -> AttributionCounts {
        let mut counts = core::mem::MaybeUninit::<AttributionCounts>::uninit();
        // SAFETY: `self.as_raw()` points to a live `VmObject`, and `counts` is valid for writing.
        unsafe {
            bindings::cpp_vm_object_get_attributed_memory(
                self.as_raw(),
                counts.as_mut_ptr().cast(),
            );
        }
        // SAFETY: `cpp_vm_object_get_attributed_memory` certainly wrote out the attribution counts.
        unsafe { counts.assume_init() }
    }

    /// Returns the memory attributed to the reference owner.
    pub fn get_attributed_memory_in_reference_owner(&self) -> AttributionCounts {
        let mut counts = core::mem::MaybeUninit::<AttributionCounts>::uninit();
        // SAFETY: `self.as_raw()` points to a live `VmObject`, and `counts` is valid for writing.
        unsafe {
            bindings::cpp_vm_object_get_attributed_memory_in_reference_owner(
                self.as_raw(),
                counts.as_mut_ptr().cast(),
            );
        }
        // SAFETY: `cpp_vm_object_get_attributed_memory_in_reference_owner` certainly wrote out the
        // attribution counts.
        unsafe { counts.assume_init() }
    }

    /// Returns the number of physical bytes currently attributed to a range of this VMO.
    /// The range is `[offset, offset + len)`.
    pub fn get_attributed_memory_in_range(&self, offset: u64, len: u64) -> AttributionCounts {
        let mut counts = core::mem::MaybeUninit::<AttributionCounts>::uninit();
        // SAFETY: `self.as_raw()` points to a live `VmObject`, and `counts` is valid for writing.
        unsafe {
            bindings::cpp_vm_object_get_attributed_memory_in_range(
                self.as_raw(),
                offset,
                len,
                counts.as_mut_ptr().cast(),
            );
        }
        // SAFETY: `cpp_vm_object_get_attributed_memory_in_range` certainly wrote out the
        // attribution counts.
        unsafe { counts.assume_init() }
    }

    pub fn dump(&self, depth: u32, verbose: bool) {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe {
            bindings::cpp_vm_object_dump(self.as_raw(), depth, verbose);
        }
    }

    /// Read/write operators against kernel pointers only.
    /// May block on user pager requests and must be called without locks held.
    ///
    /// Reads `data.len()` bytes from the VMO at `offset` into `data`.
    /// Returns a slice of initialized bytes on success.
    pub fn read<'a>(
        &self,
        offset: u64,
        data: &'a mut [MaybeUninit<u8>],
    ) -> Result<&'a mut [u8], Status> {
        let ptr: *mut MaybeUninit<u8> = data.as_mut_ptr();
        let ptr: *mut c_void = ptr.cast();

        // SAFETY: `self.as_raw()` points to a live `VmObject`, and `ptr` points to a
        // buffer valid for writing `data.len()` bytes.
        let status =
            unsafe { bindings::cpp_vm_object_read(self.as_raw(), ptr, offset, data.len()) };
        Status::ok(status)?;

        // SAFETY: When `cpp_vm_object_read` returns `ZX_OK`, all `data.len()` bytes in `data`
        // have been initialized by the kernel.
        Ok(unsafe { data.assume_init_mut() })
    }

    /// Zero a range of the VMO. May release physical pages in the process.
    /// May block on user pager requests and must be called without locks held.
    pub fn zero_range(&self, offset: u64, len: u64) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to a live `VmObject`.
        let status = unsafe { bindings::cpp_vm_object_zero_range(self.as_raw(), offset, len) };
        Status::ok(status)
    }

    /// Lock a range from being discarded by the kernel. Can fail if the range was already
    /// discarded.
    pub fn try_lock_range(&self, offset: u64, len: u64) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to a live `VmObject`.
        let status = unsafe { bindings::cpp_vm_object_try_lock_range(self.as_raw(), offset, len) };
        Status::ok(status)
    }

    /// Lock a range from being discarded by the kernel. Guaranteed to succeed.
    /// `zx_vmo_lock_state_t` is populated with relevant information about the locked and discarded
    /// ranges.
    pub fn lock_range(&self, offset: u64, len: u64) -> Result<zx_vmo_lock_state_t, Status> {
        let mut lock_state = MaybeUninit::uninit();
        // SAFETY: `self.as_raw()` points to a live `VmObject` and out-pointer is valid for write.
        let status = unsafe {
            bindings::cpp_vm_object_lock_range(self.as_raw(), offset, len, lock_state.as_mut_ptr())
        };
        Status::ok(status)?;
        // SAFETY: `cpp_vm_object_lock_range` initialized `lock_state` when returning `ZX_OK`.
        Ok(unsafe { lock_state.assume_init() })
    }

    /// Unlock a range, making it available for the kernel to discard. The range could have been
    /// locked either by `try_lock_range` or `lock_range`.
    pub fn unlock_range(&self, offset: u64, len: u64) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to a live `VmObject`.
        let status = unsafe { bindings::cpp_vm_object_unlock_range(self.as_raw(), offset, len) };
        Status::ok(status)
    }

    /// Indicates that page requests in the range [offset, offset + len) could not be fulfilled.
    /// `error_status` specifies the error encountered. `offset` and `len` must be page aligned.
    pub fn fail_page_requests(
        &self,
        offset: u64,
        len: u64,
        error_status: Status,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to a live `VmObject`.
        let status = unsafe {
            bindings::cpp_vm_object_fail_page_requests(
                self.as_raw(),
                offset,
                len,
                error_status.into_raw(),
            )
        };
        Status::ok(status)
    }

    /// Dirties pages in the vmo in the range [offset, offset + len).
    pub fn dirty_pages(&self, offset: u64, len: u64) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to a live `VmObject`.
        let status = unsafe { bindings::cpp_vm_object_dirty_pages(self.as_raw(), offset, len) };
        Status::ok(status)
    }

    /// Enumerates dirty ranges in the range [offset, offset + len) in ascending order, updating
    /// any relevant VMO internal state required to perform the enumeration, and calls
    /// `dirty_range_fn` on each dirty range (spanning [range_offset, range_offset + range_len)
    /// where `range_is_zero` indicates whether the range is all zeros). `dirty_range_fn` can return
    /// `Err(Status::NEXT)` to continue with the enumeration, `Err(Status::STOP)` to terminate the
    /// enumeration successfully, and any other error code to terminate the enumeration early with
    /// that error code.
    pub fn enumerate_dirty_ranges<F>(
        &self,
        offset: u64,
        len: u64,
        mut dirty_range_fn: F,
    ) -> Result<(), Status>
    where
        F: FnMut(u64, u64, bool) -> Result<(), Status>,
    {
        /// # Safety
        ///
        /// `ctx` must point to a valid `F` on the stack in `enumerate_dirty_ranges` that remains
        /// valid for the duration of the C++ FFI callback.
        unsafe extern "C" fn dirty_range_callback_shim<F>(
            ctx: *mut c_void,
            range_offset: u64,
            range_len: u64,
            range_is_zero: bool,
        ) -> zx_status_t
        where
            F: FnMut(u64, u64, bool) -> Result<(), Status>,
        {
            // SAFETY: `ctx` is guaranteed by `cpp_vm_object_enumerate_dirty_ranges` to be the
            // non-null `ctx_ptr` passed from `enumerate_dirty_ranges`, which points to a live `F`.
            let f = unsafe { &mut *ctx.cast::<F>() };
            Status::result_into_raw(f(range_offset, range_len, range_is_zero))
        }

        let ctx_ptr: *mut c_void = (&raw mut dirty_range_fn).cast();
        // SAFETY: `self.as_raw()` points to a live `VmObject`, and `ctx_ptr` and
        // `dirty_range_callback_shim::<F>` remain valid for the duration of the call.
        let status = unsafe {
            bindings::cpp_vm_object_enumerate_dirty_ranges(
                self.as_raw(),
                offset,
                len,
                ctx_ptr,
                Some(dirty_range_callback_shim::<F>),
            )
        };
        Status::ok(status)
    }

    /// Query pager relevant VMO stats, e.g. whether the VMO has been modified. If `reset` is set
    /// to true, the queried stats are reset as well, potentially affecting the queried state
    /// returned by future calls to this function.
    pub fn query_pager_vmo_stats(&self, reset: bool) -> Result<zx_pager_vmo_stats_t, Status> {
        let mut stats = MaybeUninit::<zx_pager_vmo_stats_t>::uninit();
        // SAFETY: `self.as_raw()` points to a live `VmObject` and `stats` is valid for writing.
        let status = unsafe {
            bindings::cpp_vm_object_query_pager_vmo_stats(self.as_raw(), reset, stats.as_mut_ptr())
        };
        Status::ok(status)?;
        // SAFETY: `cpp_vm_object_query_pager_vmo_stats` initialized `stats` on `ZX_OK`.
        Ok(unsafe { stats.assume_init() })
    }

    /// Indicates start of writeback for the range [offset, offset + len). Any [`Dirty`] pages in
    /// the range are transitioned to [`AwaitingClean`], in preparation for transition to [`Clean`]
    /// when the writeback is done (See [`VmCowPages::DirtyState`] for details of these states).
    /// `offset` and `len` must be page aligned. `is_zero_range` specifies whether the caller
    /// intends to write back the specified range as zeros.
    pub fn writeback_begin(
        &self,
        offset: u64,
        len: u64,
        is_zero_range: bool,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to a live `VmObject`.
        let status = unsafe {
            bindings::cpp_vm_object_writeback_begin(self.as_raw(), offset, len, is_zero_range)
        };
        Status::ok(status)
    }

    /// Indicates end of writeback for the range [offset, offset + len). Any [`AwaitingClean`] pages
    /// in the range are transitioned to [`Clean`] (See [`VmCowPages::DirtyState`] for details of
    /// these states). `offset` and `len` must be page aligned.
    pub fn writeback_end(&self, offset: u64, len: u64) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to a live `VmObject`.
        let status = unsafe { bindings::cpp_vm_object_writeback_end(self.as_raw(), offset, len) };
        Status::ok(status)
    }

    /// Number of times pages have been evicted over the lifetime of this VMO. Evicted counts for
    /// any decommit style event such as user pager eviction or zero page merging. One eviction
    /// event could count for multiple pages being evicted, if those pages were evicted as a group.
    pub fn reclamation_event_count(&self) -> u64 {
        // SAFETY: `self.as_raw()` points to a live `VmObject`.
        unsafe { bindings::cpp_vm_object_reclamation_event_count(self.as_raw()) }
    }

    /// Detaches the underlying page source, if any. All future page requests will fail and any
    /// clean pages will be released.
    pub fn detach_source(&self) {
        // SAFETY: `self.as_raw()` points to a live `VmObject`.
        unsafe { bindings::cpp_vm_object_detach_source(self.as_raw()) }
    }

    /// If this VMO has a backing page source, and that page source has a koid, then it is
    /// returned. Otherwise returns `None`.
    pub fn get_page_source_koid(&self) -> Option<zx_koid_t> {
        // SAFETY: `self.as_raw()` points to a live `VmObject`.
        let koid = unsafe { bindings::cpp_vm_object_get_page_source_koid(self.as_raw()) };
        if koid == ZX_KOID_INVALID { None } else { Some(koid) }
    }

    /// Takes pages out of this vmo and places them into the splice list.
    /// `pages` must be a valid, initialized and empty splice list. The length of the range
    /// is specified by the length of the splice list.
    /// The caller must guarantee that there are no active writes to the source range, and that
    /// pinned pages in the source range. `offset` and `len` must be page aligned.
    pub fn take_pages(
        &self,
        offset: u64,
        len: u64,
        pages: Pin<&mut VmPageSpliceList>,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to a live `VmObject`, and `pages.as_raw()` points to a
        // live `VmPageSpliceList`.
        let status = unsafe {
            bindings::cpp_vm_object_take_pages(self.as_raw(), offset, len, pages.as_raw())
        };
        Status::ok(status)
    }

    /// Supplies this vmo with pages for the range [offset, offset + len). If this vmo
    /// already has pages in the target range, the `options` field will dictate what happens:
    /// If options is SupplyOptions::TransferData, the pages in the target range will be
    /// overwritten,
    /// Otherwise, the corresponding pages in `pages` will be freed.
    /// `offset` and `len` must be page aligned.
    pub fn supply_pages(
        &self,
        offset: u64,
        len: u64,
        pages: Pin<&mut VmPageSpliceList>,
        options: SupplyOptions,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to a live `VmObject`, and `pages.as_raw()` points to a
        // live `VmPageSpliceList`.
        let status = unsafe {
            bindings::cpp_vm_object_supply_pages(
                self.as_raw(),
                offset,
                len,
                pages.as_raw(),
                options,
            )
        };
        Status::ok(status)
    }

    /// Read/write operators against user space pointers only.
    ///
    /// The number of bytes successfully processed is always returned, even upon error. This allows
    /// for callers to still pass on this bytes transferred if a particular error was expected.
    ///
    /// May block on user pager requests and must be called without locks held.
    ///
    /// Bytes are guaranteed to be transferred in order from low to high offset.
    pub fn read_user<T>(
        &self,
        buffer: UserOutPtr<T>,
        offset: u64,
        size: usize,
        options: VmObjectReadWriteOptions,
    ) -> (Result<(), Status>, usize) {
        let mut out_actual = 0usize;
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer. `out_actual` points to
        // stack-allocated memory.
        let status = unsafe {
            bindings::cpp_vm_object_read_user(
                self.as_raw(),
                buffer.as_ptr().cast(),
                offset,
                size,
                options.bits(),
                &mut out_actual,
            )
        };
        (Status::ok(status), out_actual)
    }

    /// Read/write operators against user space pointers only.
    ///
    /// The number of bytes successfully processed is always returned, even upon error. This allows
    /// for callers to still pass on this bytes transferred if a particular error was expected.
    ///
    /// May block on user pager requests and must be called without locks held.
    ///
    /// Bytes are guaranteed to be transferred in order from low to high offset.
    pub fn write_user<T>(
        &self,
        buffer: UserInPtr<T>,
        offset: u64,
        size: usize,
        options: VmObjectReadWriteOptions,
    ) -> (Result<(), Status>, usize) {
        self.write_user_impl(buffer, offset, size, options, None, core::ptr::null_mut())
    }

    /// Read/write operators against user space pointers only.
    ///
    /// The number of bytes successfully processed is always returned, even upon error. This allows
    /// for callers to still pass on this bytes transferred if a particular error was expected.
    ///
    /// May block on user pager requests and must be called without locks held.
    ///
    /// Bytes are guaranteed to be transferred in order from low to high offset.
    ///
    /// `on_bytes_transferred` is guaranteed to be called after bytes have been successfully
    /// transferred from the user source to the VMO and will be called before the VMO lock is
    /// dropped. As a result, operations performed within the callback should not take any other
    /// locks or be long-running.
    pub fn write_user_with_progress<T, F>(
        &self,
        buffer: UserInPtr<T>,
        offset: u64,
        size: usize,
        options: VmObjectReadWriteOptions,
        mut on_bytes_transferred: F,
    ) -> (Result<(), Status>, usize)
    where
        F: FnMut(u64, usize),
    {
        unsafe extern "C" fn write_user_progress_trampoline<F: FnMut(u64, usize)>(
            ctx: *mut core::ffi::c_void,
            offset: u64,
            len: usize,
        ) {
            // SAFETY: `ctx` is a valid non-null pointer to `F` passed to `cpp_vm_object_write_user`
            // for the duration of the synchronous FFI call.
            let cb = unsafe { &mut *ctx.cast::<F>() };
            cb(offset, len);
        }

        self.write_user_impl(
            buffer,
            offset,
            size,
            options,
            Some(write_user_progress_trampoline::<F>),
            (&raw mut on_bytes_transferred).cast::<core::ffi::c_void>(),
        )
    }

    fn write_user_impl<T>(
        &self,
        buffer: UserInPtr<T>,
        offset: u64,
        size: usize,
        options: VmObjectReadWriteOptions,
        progress_cb: Option<unsafe extern "C" fn(*mut core::ffi::c_void, u64, usize)>,
        ctx: *mut core::ffi::c_void,
    ) -> (Result<(), Status>, usize) {
        let mut out_actual = 0usize;
        // SAFETY: self.as_raw() is a valid VmObject pointer. out_actual points to stack-allocated
        // memory. ctx is null or points to a valid progress callback context.
        let status = unsafe {
            bindings::cpp_vm_object_write_user(
                self.as_raw(),
                buffer.as_ptr().cast(),
                offset,
                size,
                options.bits(),
                progress_cb,
                ctx,
                &mut out_actual,
            )
        };
        (Status::ok(status), out_actual)
    }
}

impl HasRefCount for VmObject {
    fn ref_count(&self) -> &fbl::RefCounted {
        let raw = unsafe { bindings::cpp_vm_object_get_ref_counted(self.as_raw()) };
        unsafe { &*(raw.cast::<fbl::RefCounted>()) }
    }
}

unsafe impl Recyclable for VmObject {
    unsafe fn recycle(ptr: NonNull<Self>) {
        unsafe {
            bindings::cpp_vm_object_free(VmObject::cast_raw(ptr.as_ptr()));
        }
    }

    fn allocate(_value: Self) -> Result<NonNull<Self>, AllocError> {
        Err(AllocError)
    }
}

/// Kernel unit tests for `VmObject`.
#[cfg(ktest)]
#[unittest::suite(name = "vm_object_tests")]
mod tests {
    use super::{VmObject, VmObjectReadWriteOptions};
    use crate::user_memory::UserMemory;
    use crate::vm::pmm::ALLOC_FLAG_ANY;
    use crate::vm::scanner::AutoVmScannerDisable;
    use page::SIZE as PAGE_SIZE_USIZE;
    use unittest::{expect_eq, expect_ok, unwrap_ok, unwrap_some};
    const PAGE_SIZE: u64 = PAGE_SIZE_USIZE as u64;

    /// Tests rounding sizes to page boundaries without overflowing.
    #[test]
    fn test_round_size() {
        unittest::expect_eq!(VmObject::round_size(0).unwrap(), 0);
        unittest::expect_eq!(VmObject::round_size(1).unwrap(), page::SIZE as u64);
        unittest::expect_eq!(VmObject::round_size(page::SIZE as u64).unwrap(), page::SIZE as u64);
        unittest::expect_eq!(
            VmObject::round_size(page::SIZE as u64 + 1).unwrap(),
            2 * page::SIZE as u64
        );
        unittest::expect_true!(VmObject::round_size(u64::MAX).is_err());
    }

    /// Tests querying size under the VMO object lock.
    #[test]
    fn vmo_object_lock_size_test() {
        let _scanner_disable = AutoVmScannerDisable::new();
        let vmo = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, PAGE_SIZE));
        ksync::lock!(let guard = vmo.lock());
        expect_eq!(vmo.size_locked(guard.token()), PAGE_SIZE);
        let _ = vmo.lock_ref();
    }

    /// Tests that `read_user` respects `TRIM_LENGTH` when straddling the end of a VMO.
    #[test]
    fn vmo_read_user_trim_length_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        let vmo = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, PAGE_SIZE));
        unwrap_ok!(vmo.commit_range(0, PAGE_SIZE));

        let user_memory = unwrap_some!(UserMemory::create(PAGE_SIZE_USIZE));
        unwrap_ok!(user_memory.commit_and_map(0..PAGE_SIZE_USIZE));

        let offset = PAGE_SIZE / 2;
        let size = PAGE_SIZE_USIZE;
        let expected_trimmed = (PAGE_SIZE - offset) as usize;

        let (res, actual) = vmo.read_user(
            user_memory.user_out::<u8>(),
            offset,
            size,
            VmObjectReadWriteOptions::TRIM_LENGTH,
        );
        expect_ok!(res);
        expect_eq!(actual, expected_trimmed);

        let (res, actual) = vmo.read_user(
            user_memory.user_out::<u8>(),
            offset,
            size,
            VmObjectReadWriteOptions::NONE,
        );
        expect_eq!(Status::result_into_raw(res), Status::OUT_OF_RANGE.into_raw());
        expect_eq!(actual, 0);

        let (res, actual) = vmo.write_user(
            user_memory.user_in::<u8>(),
            offset,
            size,
            VmObjectReadWriteOptions::TRIM_LENGTH,
        );
        expect_ok!(res);
        expect_eq!(actual, expected_trimmed);

        let (res, actual) = vmo.write_user(
            user_memory.user_in::<u8>(),
            offset,
            size,
            VmObjectReadWriteOptions::NONE,
        );
        expect_eq!(Status::result_into_raw(res), Status::OUT_OF_RANGE.into_raw());
        expect_eq!(actual, 0);
    }
}
