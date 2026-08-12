// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::ArchMmuFlags;
use super::attribution::AttributionCounts;
use super::page::VmPagePtr;
use super::page_source::MultiPageRequest;
use super::vm_object_paged::VmObjectPaged;
use crate::kernel::types::PAddr;
use core::ffi::c_void;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::{ManuallyDrop, MaybeUninit};
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{HasRefCount, Recyclable, RefPtr};
use kalloc::AllocError;
use vm_object_bindings as bindings;
use zr::Opaque;
use zx_status::Status;
use zx_types::zx_status_t;

pub use bindings::{Resizability, SnapshotType, VmObject_EvictionHint as EvictionHint};

/// Argument that specifies the context in which we are supplying pages.
pub type SupplyOptions = bindings::SupplyOptions;

pub type VmObjectReadWriteOptions = bindings::VmObjectReadWriteOptions;

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
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_size(self.as_raw()) }
    }

    /// Returns whether the VMO is resizable.
    pub fn is_resizable(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_is_resizable(self.as_raw()) }
    }

    /// Returns whether the VMO is contiguous.
    pub fn is_contiguous(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_is_contiguous(self.as_raw()) }
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

    /// Returns the mapping cache policy of the VMO.
    pub fn get_mapping_cache_policy(&self) -> ArchMmuFlags {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        unsafe { bindings::cpp_vm_object_get_mapping_cache_policy(self.as_raw()) }
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

    /// Helper variant of get_page that will retry the operation after waiting on a PageRequest if required.
    pub fn get_page_blocking(&self, offset: u64, pf_flags: u32) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmObject` pointer.
        let status =
            unsafe { bindings::cpp_vm_object_get_page_blocking(self.as_raw(), offset, pf_flags) };
        Status::ok(status)
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
        let page = unsafe { VmPagePtr::from_raw(page_ptr) }.expect("page pointer is non-null");
        Ok((page, PAddr(paddr)))
    }

    /// Returns the number of physical bytes currently attributed to this VMO.
    pub fn get_attributed_memory(&self) -> AttributionCounts {
        let mut counts = core::mem::MaybeUninit::uninit();
        // SAFETY: `self.as_raw()` points to a live `VmObject`, and `counts` is valid for writing.
        unsafe {
            bindings::cpp_vm_object_get_attributed_memory(self.as_raw(), counts.as_mut_ptr());
        }
        // SAFETY: `cpp_vm_object_get_attributed_memory` certainly wrote out the attribution counts.
        unsafe { counts.assume_init() }
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
