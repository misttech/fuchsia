// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{HasRefCount, Recyclable, RefCounted, RefPtr};
use page_source_bindings as bindings;
use pin_init::pin_data;
use zr::{Opaque, pin_init_ffi, unsafe_pinned_drop_ffi};
use zx_status::Status;

/// The different types of page requests that can exist.
pub type PageRequestType = bindings::page_request_type;

/// These properties are constant per `PageProvider` type, so a given `VmCowPages` can query and
/// cache these properties once (if it has a `PageSource`) and know they won't change after that.
/// This also avoids per-property plumbing via `PageSource`.
pub type PageSourceProperties = bindings::PageSourceProperties;

/// Wrapper around tracking multiple different page requests that might need waiting. Only one
/// individual request is allowed to be considered 'active' at a time as the one that next needs
/// waiting on. Tracking whether a request is active is, depending on the request type, partially
/// automatic and partially requiring additional input from the user.
/// The PageRequest and LazyPageRequest access methods do not currently have a way to enforce that
/// those specific types of requests are made with the returned objects, however this could change
/// and callers are expected to use the correct method.
/// TODO(adanis): Implement an enforcement strategy.
#[pin_data(PinnedDrop)]
pub struct MultiPageRequest {
    /// The optional inner `PageRequest` can appear in intrusive containers, so this object must be
    /// pinned.
    #[pin]
    opaque: Opaque<bindings::MultiPageRequest>,
}

unsafe_pinned_drop_ffi!(MultiPageRequest, bindings::cpp_multi_page_request_destroy);

impl MultiPageRequest {
    /// Returns an in-place initializer for stack-pinning a `MultiPageRequest`.
    pub fn new() -> impl pin_init::PinInit<Self> {
        /// # Safety
        /// `ptr` must point to uninitialized `MultiPageRequest` storage.
        unsafe fn init_shim(ptr: *mut core::ffi::c_void) {
            let req_ptr: *mut bindings::MultiPageRequest = ptr.cast();
            // SAFETY: `ptr` is guaranteed by `pin_init_ffi!` to point to valid `MultiPageRequest`
            // storage.
            unsafe { bindings::cpp_multi_page_request_construct(req_ptr) }
        }
        pin_init_ffi!(init_shim)
    }

    /// Waits on the currently active page request.
    pub fn wait(self: Pin<&mut Self>, suspendable: bool) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` points to a valid, pinned `MultiPageRequest`.
        Status::ok(unsafe { bindings::cpp_multi_page_request_wait(self.as_raw(), suspendable) })
    }

    /// Cancel all requests and have no active request.
    pub fn cancel_requests(self: Pin<&mut Self>) {
        // SAFETY: Calling C++ CancelRequests on the pinned instance does not move it.
        unsafe { bindings::cpp_multi_page_request_cancel_requests(self.as_raw()) }
    }

    /// Returns a raw pointer to the underlying C++ `MultiPageRequest`.
    ///
    /// Callers must not use the returned raw pointer to move the object in memory.
    pub fn as_raw(self: Pin<&mut Self>) -> *mut bindings::MultiPageRequest {
        // SAFETY: Obtaining a raw pointer to `opaque` does not move the pinned object.
        unsafe { self.get_unchecked_mut().opaque.get() }
    }
}

/// A request for pages from a `PageSource`.
///
/// `PageRequest`s are allocated and owned by C++ (they serve as the allocation of all data needed
/// by all parties involved in a request), and page providers only ever observe them through
/// references or pointers handed over the FFI boundary while the request is owned by the provider
/// (i.e. from `SendAsyncRequest` or `SwapAsyncRequest` until `ClearAsyncRequest` or
/// `SwapAsyncRequest` returns). During that window C++ `PageSource` sets `provider_owned_` and
/// holds the request's `type_`, `offset_`, and `len_` fields constant so the provider can read
/// them. A `PageRequest` can appear in intrusive containers, so it must never be moved by Rust.
#[repr(C)]
pub struct PageRequest {
    raw: Opaque<bindings::PageRequest>,
    phantom: PhantomData<PhantomPinned>,
}

impl PageRequest {
    /// Domain-specific conversion: returns raw pointer for `PageRequest`.
    pub fn as_raw(&self) -> *mut bindings::PageRequest {
        self.raw.get()
    }

    /// Returns the type of this request.
    ///
    /// This mirrors the `PageProvider::GetRequestType` accessor that a page provider
    /// implementation can use to retrieve fields from a `PageRequest`. The underlying C++
    /// accessor `DEBUG_ASSERT`s `provider_owned_` and relies on `PageSource` holding `type_`
    /// constant while the request is owned by the provider.
    pub fn request_type(&self) -> PageRequestType {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageRequest` that is currently
        // owned by a `PageProvider`, so `type_` is initialized and not concurrently modified.
        let raw = unsafe { bindings::cpp_page_request_get_type(self.as_raw()) };
        match raw {
            // Request to provide the initial contents for the page.
            x if x == PageRequestType::READ as u32 => PageRequestType::READ,
            // Request to alter contents of the page, i.e. transition it from clean to dirty.
            x if x == PageRequestType::DIRTY as u32 => PageRequestType::DIRTY,
            // Request to write back modified page contents back to the source.
            x if x == PageRequestType::WRITEBACK as u32 => PageRequestType::WRITEBACK,
            _ => panic!("invalid page request type"),
        }
    }

    /// Returns the offset of this request.
    ///
    /// This mirrors the `PageProvider::GetRequestOffset` accessor that a page provider
    /// implementation can use to retrieve fields from a `PageRequest`. The underlying C++
    /// accessor `DEBUG_ASSERT`s `provider_owned_` and relies on `PageSource` holding `offset_`
    /// constant while the request is owned by the provider.
    pub fn offset(&self) -> u64 {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageRequest` that is currently
        // owned by a `PageProvider`, so `offset_` is initialized and not concurrently modified.
        unsafe { bindings::cpp_page_request_get_offset(self.as_raw()) }
    }

    /// Returns the length of this request.
    ///
    /// This mirrors the `PageProvider::GetRequestLen` accessor that a page provider
    /// implementation can use to retrieve fields from a `PageRequest`. The underlying C++
    /// accessor `DEBUG_ASSERT`s `provider_owned_` and relies on `PageSource` holding `len_`
    /// constant while the request is owned by the provider.
    // A `PageRequest` is never zero-length, so an `is_empty` companion would always return false
    // and would be meaningless to callers.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageRequest` that is currently
        // owned by a `PageProvider`, so `len_` is initialized and not concurrently modified.
        unsafe { bindings::cpp_page_request_get_len(self.as_raw()) }
    }

    /// Returns whether this request is currently in a `PageProviderTag` list.
    pub fn in_container(&self) -> bool {
        fbl::DoublyLinkedListContainable::<PageRequest, PageProviderTag>::get_node(self)
            .in_container()
    }
}

/// Tag for the list owned by the page provider, for tracking outstanding requests.
///
/// This names the same node as the C++ `PageProviderTag`, so a `PageRequest` may be in at most
/// one `PageProviderTag` list across both languages at a time. `PageRequest`s are owned by C++, so
/// lists of this tag hold unmanaged pointers and do not keep their members alive.
pub struct PageProviderTag;

// Mirrors the C++ `PageProviderNodeState` layout assertions in `page_source_ffi.cc`.
zr::static_assert!(
    core::mem::size_of::<fbl::DoublyLinkedListNode<PageRequest>>()
        == 2 * core::mem::size_of::<*mut PageRequest>()
);
zr::static_assert!(
    core::mem::align_of::<fbl::DoublyLinkedListNode<PageRequest>>()
        == core::mem::align_of::<*mut PageRequest>()
);

impl fbl::DoublyLinkedListContainable<PageRequest, PageProviderTag> for PageRequest {
    fn get_node(&self) -> &fbl::DoublyLinkedListNode<PageRequest> {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageRequest`, and the accessor
        // only computes the address of its `PageProviderTag` node state.
        let node = unsafe { bindings::cpp_page_request_provider_node(self.as_raw()) };
        // SAFETY: `node` points to the `fbl::DoublyLinkedListNodeState<PageRequest*>` within
        // `self`, and so lives as long as `self`. That type is layout-compatible with
        // `fbl::DoublyLinkedListNode<PageRequest>` (a `#[repr(C)]` pair of `next` and `prev`
        // pointers, as asserted by `page_source_ffi.cc`), and its pointers target the
        // `PageRequest` itself, which has the same address as the Rust facade. The node's fields
        // are `UnsafeCell`s, so C++ mutating them through its own lists does not invalidate this
        // shared reference.
        unsafe { &*node.cast::<fbl::DoublyLinkedListNode<PageRequest>>() }
    }
}

/// A page source, which is the interface a VMO uses to request pages from a `PageProvider`.
///
/// The object is owned by C++, and Rust only ever observes it through pointers handed over the
/// FFI boundary.
#[repr(C)]
pub struct PageSource {
    raw: Opaque<bindings::PageSource>,
    phantom: PhantomData<PhantomPinned>,
}

impl HasRefCount for PageSource {
    fn ref_count(&self) -> &RefCounted {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`, and
        // `cpp_page_source_get_ref_counted` returns a valid pointer to its `fbl::RefCounted` base.
        let raw = unsafe { bindings::cpp_page_source_get_ref_counted(self.as_raw()) };
        unsafe { &*(raw.cast::<RefCounted>()) }
    }
}

// SAFETY: `PageSource` is a C++ `fbl::RefCounted` object freed via `cpp_page_source_free`.
unsafe impl Recyclable for PageSource {
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: Caller guarantees `ptr` points to a valid `PageSource` with zero remaining
        // references.
        unsafe {
            bindings::cpp_page_source_free(ptr.as_ptr().cast());
        }
    }
}

impl PageSource {
    /// Domain-specific conversion: returns raw pointer for `PageSource`.
    pub fn as_raw(&self) -> *mut bindings::PageSource {
        self.raw.get()
    }

    /// Domain-specific conversion: constructs a `RefPtr<PageSource>` from an exported pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid pointer to a C++ `PageSource` with an owned reference count, or null.
    pub unsafe fn from_raw(ptr: *mut bindings::PageSource) -> Option<RefPtr<Self>> {
        // SAFETY: Caller guarantees `ptr` is null or a valid `PageSource` with an owned reference.
        unsafe { RefPtr::try_from_raw(ptr.cast::<Self>()) }
    }

    /// Domain-specific conversion: constructs a `&PageSource` from a raw pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null pointer to an initialized C++ `PageSource` that remains
    /// live for the lifetime `'a`.
    pub unsafe fn from_raw_ref<'a>(ptr: *const bindings::PageSource) -> &'a Self {
        let ptr: *const Self = ptr.cast();
        // SAFETY: `bindings::PageSource` is layout-compatible with `PageSource`, and the caller
        // guarantees `ptr` is valid for `'a`.
        unsafe { ptr.as_ref_unchecked() }
    }

    /// Fails outstanding page requests in the range [offset, offset + len). Events associated with
    /// the failed page requests are signaled with the `error_status`, and any waiting threads are
    /// unblocked.
    pub fn on_pages_failed(&self, offset: u64, len: u64, error_status: Status) {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`.
        unsafe {
            bindings::cpp_page_source_on_pages_failed(
                self.as_raw(),
                offset,
                len,
                error_status.into_raw(),
            )
        }
    }

    /// Returns whether this `PageSource`'s `paged_vmo_lock` is held by the current thread.
    ///
    /// Intended for `debug_assert!`s that both document and check a caller's obligation to hold
    /// the `paged_vmo_lock` across a call.
    pub fn paged_vmo_lock_is_held(&self) -> bool {
        // SAFETY: `self.as_raw()` points to an initialized C++ `PageSource`.
        unsafe { bindings::cpp_page_source_paged_vmo_lock_is_held(self.as_raw()) }
    }

    /// Returns true if `error_status` is a valid `ZX_PAGER_OP_FAIL` failure error code.
    pub fn is_valid_external_failure_code(error_status: Status) -> bool {
        // SAFETY: Pure function checking status code validity.
        unsafe { bindings::cpp_page_source_is_valid_external_failure_code(error_status.into_raw()) }
    }

    /// Returns true if `error_status` is a valid provider failure error code, which can be used
    /// with `on_pages_failed`.
    ///
    /// This returns true for every error code that `IsValidExternalFailureCode` returns true for,
    /// plus any additional error codes that are valid as an internal `PageProvider` status but not
    /// valid for ZX_PAGER_OP_FAIL.
    ///
    /// `Status::NO_MEMORY` will return true, unlike
    /// `IsValidExternalFailureCode(ZX_ERR_NO_MEMORY)` which returns false.
    ///
    /// Not every error code is supported, since these errors can get returned via a zx_vmo_read()
    /// or a zx_vmo_op_range(), if those calls resulted in a page fault.  So the `error_status`
    /// should be a supported return error code for those syscalls.  An error code need not be
    /// specifiable via ZX_PAGER_OP_FAIL for this function to return true.
    pub fn is_valid_internal_failure_code(error_status: Status) -> bool {
        // SAFETY: The C++ helper only inspects the status value.
        unsafe { bindings::cpp_page_source_is_valid_internal_failure_code(error_status.into_raw()) }
    }
}
