// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::page::VmPagePtr;
use super::page_source::PageSource;
use super::stream_size_manager::StreamSizeManager;
use super::vm_cow_pages::{PriorityChanger, VmCowPages};
use super::vm_object::{VmObject, VmObjectLockClass, VmObjectReadWriteOptions};
use crate::user_copy::{UserInIovec, UserOutIovec};
use core::marker::PhantomPinned;
use core::mem::ManuallyDrop;
use core::ops::Deref;
use fbl::{IsOpaqueRefCounted, RefPtr};
use ksync::LockToken;
use pin_init::PinInit;
use vm_object_paged_bindings as bindings;
use zr::Opaque;
use zx_status::Status;

/// VMO representing a paged range of copy-on-write memory.
#[repr(C)]
pub struct VmObjectPaged {
    raw: Opaque<bindings::VmObjectPaged>,
    phantom: PhantomPinned,
}

impl VmObjectPaged {
    // `VmObject::options_` bitmask is extended with:
    pub const RESIZABLE: u32 = bindings::VmObjectPaged_kResizable;
    pub const CONTIGUOUS: u32 = bindings::VmObjectPaged_kContiguous;
    pub const SLICE: u32 = bindings::VmObjectPaged_kSlice;
    pub const DISCARDABLE: u32 = bindings::VmObjectPaged_kDiscardable;
    pub const ALWAYS_PINNED: u32 = bindings::VmObjectPaged_kAlwaysPinned;
    pub const REFERENCE: u32 = bindings::VmObjectPaged_kReference;
    pub const CAN_BLOCK_ON_PAGE_REQUESTS: u32 = bindings::VmObjectPaged_kCanBlockOnPageRequests;

    /// Domain-specific conversion: returns raw FFI pointer for `VmObjectPaged`.
    pub fn as_raw(&self) -> *mut bindings::VmObjectPaged {
        self.raw.get()
    }

    /// Domain-specific conversion: constructs a `RefPtr<VmObjectPaged>` from a raw FFI pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, raw `VmObjectPaged` pointer exported from C++.
    pub unsafe fn from_raw(ptr: *mut bindings::VmObjectPaged) -> Option<RefPtr<Self>> {
        unsafe { RefPtr::try_from_raw(ptr.cast::<Self>()) }
    }

    /// Create a new paged VMO.
    pub fn create(
        pmm_alloc_flags: u32,
        options: u32,
        size: u64,
    ) -> Result<RefPtr<VmObjectPaged>, Status> {
        let mut status = 0;
        let raw = unsafe {
            bindings::cpp_vm_object_paged_create(pmm_alloc_flags, options, size, &mut status)
        };
        Status::ok(status)?;
        unsafe { Self::from_raw(raw).ok_or(Status::NO_MEMORY) }
    }

    /// Create a VMO backed by a contiguous range of physical memory.  The
    /// returned vmo has all of its pages committed, and does not allow
    /// decommitting them.
    pub fn create_contiguous(
        pmm_alloc_flags: u32,
        size: u64,
        alignment_log2: u8,
    ) -> Result<RefPtr<VmObjectPaged>, Status> {
        let mut status = 0;
        // SAFETY: status is a valid local mutable reference.
        let raw = unsafe {
            bindings::cpp_vm_object_paged_create_contiguous(
                pmm_alloc_flags,
                size,
                alignment_log2,
                &mut status,
            )
        };
        Status::ok(status)?;
        unsafe { Self::from_raw(raw).ok_or(Status::NO_MEMORY) }
    }

    /// Create a new paged VMO backed by an external `PageSource`.
    pub fn create_external(
        src: RefPtr<PageSource>,
        options: u32,
        size: u64,
    ) -> Result<RefPtr<VmObjectPaged>, Status> {
        let mut status = 0;
        let src_raw = RefPtr::into_raw(src).cast_mut().cast();
        // SAFETY: `src_raw` is a valid `PageSource` pointer with an owned reference, and `status`
        // is a valid local mutable reference.
        let raw = unsafe {
            bindings::cpp_vm_object_paged_create_external(src_raw, options, size, &mut status)
        };
        Status::ok(status)?;
        // SAFETY: `raw` is a valid `VmObjectPaged` pointer on `ZX_OK`.
        unsafe { Self::from_raw(raw).ok_or(Status::NO_MEMORY) }
    }

    /// Resets any pager VMO modification statistics.
    pub fn reset_pager_vmo_stats(&self) {
        // SAFETY: `self.as_raw()` returns a valid `VmObjectPaged` pointer.
        unsafe { bindings::cpp_vm_object_paged_reset_pager_vmo_stats(self.as_raw()) }
    }

    /// Creates a VMO from wired pages.
    ///
    /// Creating a VMO using this method is destructive. Once the VMO is released, its
    /// pages will be released into the general purpose page pool, so it is not possible
    /// to create multiple VMOs for the same region using this method.
    ///
    /// `exclusive` indicates whether or not the created vmo should have exclusive access to
    /// the pages. If `exclusive` is true, then `data` will be unmapped from the
    /// kernel address space (unless they lie in the physmap).
    pub fn create_from_wired_pages(
        data: &'static [u8],
        exclusive: bool,
    ) -> Result<RefPtr<VmObjectPaged>, Status> {
        let mut status = 0;
        // SAFETY: `data` points to `data.len()` bytes of wired kernel memory, and `status` is a
        // valid local mutable reference.
        let raw = unsafe {
            bindings::cpp_vm_object_paged_create_from_wired_pages(
                data.as_ptr().cast(),
                data.len(),
                exclusive,
                &mut status,
            )
        };
        Status::ok(status)?;
        // SAFETY: On `ZX_OK`, `raw` is a valid `VmObjectPaged` exported from `fbl::RefPtr`.
        unsafe { Self::from_raw(raw).ok_or(Status::NO_MEMORY) }
    }

    /// Exposed for testing.
    pub fn debug_get_cow_pages(&self) -> Option<RefPtr<VmCowPages>> {
        let raw = unsafe { bindings::cpp_vm_object_paged_debug_get_cow_pages(self.as_raw()) };
        unsafe { VmCowPages::from_raw(raw) }
    }

    pub(super) fn cow_pages(&self) -> &VmCowPages {
        // SAFETY: `cow_pages_` is a `const` `RefPtr` owned by `self`, so the pointee outlives
        // `self`.
        unsafe {
            VmCowPages::from_raw_ref(bindings::cpp_vm_object_paged_cow_pages(self.as_raw()).cast())
        }
    }

    /// See the comments on `PriorityChanger` for how to use this object.
    pub fn make_priority_changer(&self, delta: i64) -> impl PinInit<PriorityChanger<'_>> {
        PriorityChanger::new(self.cow_pages(), delta)
    }

    /// Debug helper to fetch backing page pointer.
    pub fn debug_get_page(&self, offset: u64) -> Option<VmPagePtr> {
        let raw = unsafe { bindings::cpp_vm_object_paged_debug_get_page(self.as_raw(), offset) };
        unsafe { VmPagePtr::from_ffi(raw) }
    }

    /// Converts a `RefPtr<VmObjectPaged>` into a base `RefPtr<VmObject>`.
    pub fn into_vm_object(this: RefPtr<Self>) -> RefPtr<VmObject> {
        let this = ManuallyDrop::new(this);
        // SAFETY: `this.as_raw()` returns a valid `VmObjectPaged` pointer.
        // `cpp_vm_object_paged_as_vm_object` converts the derived type pointer
        // to its base `VmObject` pointer.
        let raw_base = unsafe { bindings::cpp_vm_object_paged_as_vm_object(this.as_raw()) };
        // SAFETY: `raw_base` points to a valid ref-counted `VmObject` whose
        // reference count is owned by `this`.
        unsafe { VmObject::from_raw(raw_base) }
            .expect("RefPtr guarantees this is non-null and valid")
    }

    /// Reads data from the VMO into user vectors.
    pub fn read_user_vector(
        &self,
        user_data: UserOutIovec,
        mut offset: u64,
        mut length: usize,
    ) -> (Result<(), Status>, usize) {
        if length == 0 {
            return (Ok(()), 0);
        }
        if (length as u64) > u64::MAX - offset {
            return (Err(Status::OUT_OF_RANGE), 0);
        }

        let mut total = 0usize;
        let mut status = user_data.for_each(|ptr, mut capacity| {
            if capacity > length {
                capacity = length;
            }

            let (read_status, chunk_actual) =
                self.read_user(ptr, offset, capacity, VmObjectReadWriteOptions::NONE);

            // Always add `chunk_actual` since some bytes may have been transferred, even on error
            total += chunk_actual;
            if let Err(status) = read_status {
                return status;
            }

            debug_assert!(chunk_actual == capacity);

            offset += chunk_actual as u64;
            length -= chunk_actual;
            if length > 0 { Status::NEXT } else { Status::STOP }
        });

        // Return `Status::BUFFER_TOO_SMALL` if all of `length` was not transferred.
        if status.is_ok() && length > 0 {
            status = Err(Status::BUFFER_TOO_SMALL);
        }

        (status, total)
    }

    /// Writes data from user vectors into the VMO.
    pub fn write_user_vector(
        &self,
        user_data: UserInIovec,
        offset: u64,
        length: usize,
    ) -> (Result<(), Status>, usize) {
        self.write_user_vector_impl(user_data, offset, length, None)
    }

    /// Writes data from user vectors into the VMO with progress callback.
    pub fn write_user_vector_with_progress<F: FnMut(u64, usize)>(
        &self,
        user_data: UserInIovec,
        offset: u64,
        length: usize,
        mut on_bytes_transferred: F,
    ) -> (Result<(), Status>, usize) {
        self.write_user_vector_impl(user_data, offset, length, Some(&mut on_bytes_transferred))
    }

    fn write_user_vector_impl(
        &self,
        user_data: UserInIovec,
        mut offset: u64,
        mut length: usize,
        mut on_bytes_transferred: Option<&mut dyn FnMut(u64, usize)>,
    ) -> (Result<(), Status>, usize) {
        if length == 0 {
            return (Ok(()), 0);
        }
        if (length as u64) > u64::MAX - offset {
            return (Err(Status::OUT_OF_RANGE), 0);
        }

        let mut total = 0usize;
        let mut status = user_data.for_each(|ptr, mut capacity| {
            if capacity > length {
                capacity = length;
            }

            let (write_status, chunk_actual) = match on_bytes_transferred.as_mut() {
                Some(cb) => self.write_user_with_progress(
                    ptr,
                    offset,
                    capacity,
                    VmObjectReadWriteOptions::NONE,
                    &mut **cb,
                ),
                None => self.write_user(ptr, offset, capacity, VmObjectReadWriteOptions::NONE),
            };

            // Always add `chunk_actual` since some bytes may have been transferred, even on error
            total += chunk_actual;
            if let Err(status) = write_status {
                return status;
            }

            debug_assert!(chunk_actual == capacity);

            offset += chunk_actual as u64;
            length -= chunk_actual;
            if length > 0 { Status::NEXT } else { Status::STOP }
        });

        // Return `Status::BUFFER_TOO_SMALL` if all of `length` was not transferred.
        if status.is_ok() && length > 0 {
            status = Err(Status::BUFFER_TOO_SMALL);
        }

        (status, total)
    }

    /// Zero a range of the VMO. May release physical pages in the process.
    /// May block on user pager requests and must be called without locks held.
    pub fn zero_range(&self, offset: u64, length: u64) -> Result<(), Status> {
        if length == 0 {
            return Ok(());
        }
        let status =
            unsafe { bindings::cpp_vm_object_paged_zero_range(self.as_raw(), offset, length) };
        Status::ok(status)
    }

    /// Zero a range of the VMO and also untrack it from any kind of dirty tracking. For committed
    /// pages, this means that they are released. And any kind of zero markers or intervals that are
    /// inserted will not subscribe to dirty tracking.
    pub fn zero_range_untracked(&self, offset: u64, length: u64) -> Result<(), Status> {
        if length == 0 {
            return Ok(());
        }
        let status = unsafe {
            bindings::cpp_vm_object_paged_zero_range_untracked(self.as_raw(), offset, length)
        };
        Status::ok(status)
    }

    /// Resizes the VMO.
    pub fn resize(&self, size: u64) -> Result<(), Status> {
        let status = unsafe { bindings::cpp_vm_object_paged_resize(self.as_raw(), size) };
        Status::ok(status)
    }

    /// Unmaps pages in the given range and invokes `cb` atomically while holding the VMO lock.
    pub fn unmap_pages_and_call<F: FnOnce()>(&self, offset: u64, length: u64, cb: F) {
        struct Ctx<F: FnOnce()> {
            cb: Option<F>,
        }

        unsafe extern "C" fn trampoline<F: FnOnce()>(ctx: *mut core::ffi::c_void) {
            // SAFETY: `ctx` points to `Ctx<F>` on caller's stack.
            let ctx = unsafe { &mut *ctx.cast::<Ctx<F>>() };
            if let Some(cb) = ctx.cb.take() {
                cb();
            }
        }

        let mut ctx = Ctx { cb: Some(cb) };
        let cookie = (&raw mut ctx).cast::<core::ffi::c_void>();
        // SAFETY: `self.as_raw()` is a valid `VmObjectPaged`.
        unsafe {
            bindings::cpp_vm_object_paged_unmap_and_call(
                self.as_raw(),
                offset,
                length,
                Some(trampoline::<F>),
                cookie,
            );
        }
    }

    /// Provides the VMO with a user defined queryable byte aligned size. This provided size
    /// can then be referenced in other operations, but otherwise has no effect. The VMO will
    /// never read or act on this value unless instructed by user operations, and it is
    /// therefore the responsibility of the user to ensure any synchronization of the
    /// reported value with the operation being requested.
    pub fn set_user_stream_size(&self, ssm: RefPtr<StreamSizeManager>) {
        let raw_ssm = RefPtr::into_raw(ssm).cast_mut();
        // SAFETY: `self.as_raw()` and `raw_ssm` are valid pointers.
        unsafe {
            bindings::cpp_vm_object_paged_set_user_stream_size(self.as_raw(), raw_ssm.cast());
        }
    }

    /// Queries the user defined stream size, which is distinct from the VMO size.
    ///
    /// Stream size is byte-aligned and is not guaranteed to be in the range of the VMO. The lock
    /// does not guard the user changing the value via a syscall, so multiple calls under the same
    /// lock acquisition can have different results.
    pub fn user_stream_size_locked(
        &self,
        _token: &LockToken<'_, VmObjectLockClass>,
    ) -> Option<u64> {
        let mut stream_size = 0u64;
        // SAFETY: `self.as_raw()` is a valid pointer.
        let has_value = unsafe {
            bindings::cpp_vm_object_paged_user_stream_size_locked(self.as_raw(), &mut stream_size)
        };
        if has_value { Some(stream_size) } else { None }
    }

    pub fn size_locked(&self, _token: &LockToken<'_, VmObjectLockClass>) -> u64 {
        // SAFETY: `self.as_raw()` is a valid `VmObjectPaged` pointer and the VMO lock is held.
        unsafe { bindings::cpp_vm_object_paged_size_locked(self.as_raw()) }
    }

    pub fn lock_is_held(&self) -> bool {
        // SAFETY: `self.as_raw()` is a valid `VmObjectPaged` pointer.
        unsafe { bindings::cpp_vm_object_paged_lock_is_held(self.as_raw()) }
    }

    /// Calculates the minimum of the VMO size and the page-aligned user stream size.
    pub fn saturating_stream_size_locked(
        &self,
        token: &LockToken<'_, VmObjectLockClass>,
    ) -> Option<u64> {
        let user_stream_size = self.user_stream_size_locked(token)?;

        let vmo_size = self.size_locked(token);

        // If user stream size is larger, trim to the VMO.
        // TODO(https://fxbug.dev/380960681): remove check when stream size <= VMO size invariant is
        // enforced.
        if user_stream_size > vmo_size {
            return Some(vmo_size);
        }

        Some(::page::round_up(user_stream_size as usize) as u64)
    }
}

unsafe impl IsOpaqueRefCounted for VmObjectPaged {
    type TargetBase = VmObject;
}

impl Deref for VmObjectPaged {
    type Target = VmObject;
    fn deref(&self) -> &Self::Target {
        let raw = unsafe { bindings::cpp_vm_object_paged_as_vm_object(self.as_raw()) };
        let ptr = VmObject::ptr_from_raw(raw);
        // SAFETY: cpp_vm_object_paged_as_vm_object returns a valid pointer with the same lifetime
        // as its input, so `raw`, and trivially `ptr`, are valid.
        unsafe { ptr.as_ref_unchecked() }
    }
}
