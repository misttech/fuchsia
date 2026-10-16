// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::vm::compressor::{
    CompressResult, CompressedRef, FailTag, PageAndMetadata, VmCompressor,
};
use crate::vm::page::VmPagePtr;
use crate::vm::vm_page_list::ReferenceValue;
use compression_bindings as bindings;
use core::ffi::c_void;
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use pin_init::{PinInit, pin_data};
use zr::{Opaque, pin_init_ffi};

pub type Stats = bindings::VmCompression_Stats;

// Note: The upstream C++ `VmCompression` comments and documentation are intentionally not copied
// over yet.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct VmCompression {
    raw: Opaque<bindings::VmCompression>,
    phantom: PhantomData<PhantomPinned>,
}

zr::unsafe_pinned_drop_ffi!(VmCompression, bindings::cpp_vmcompression_destroy);

impl VmCompression {
    pub const NUM_LOG_BUCKETS: usize = bindings::VmCompression_kNumLogBuckets;

    /// Domain-specific conversion: returns raw pointer for `VmCompression`.
    pub fn as_raw(&self) -> *mut bindings::VmCompression {
        self.raw.get()
    }

    /// Retrieve a reference to a VmCompressor, wrapped in the RAII CompressorGuard. Once the
    /// compressor is finished with it can be destroyed, which will release it for re-use.
    /// This method may block until a compressor becomes available and callers should be prepared
    /// for extended wait times.
    pub fn acquire_compressor<'a>(&'a self) -> impl PinInit<CompressorGuard<'a>> {
        pin_init_ffi!(bindings::cpp_vmcompression_acquire_compressor, self.as_raw())
    }

    /// Attempts to compress the page of data at `page_src`, which is assumed to be `page::SIZE`
    /// bytes.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `page_src` is non-null, points to an initialized, readable
    /// memory region of at least `page::SIZE` bytes within a single allocation, and that those
    /// bytes are not concurrently mutated for the duration of this call.
    pub unsafe fn compress(&self, page_src: *const c_void) -> CompressResult {
        // SAFETY: `self.as_raw()` points to a live `VmCompression`, and `page_src` is non-null and
        // valid for reading `page::SIZE` unmodified bytes per caller `# Safety` precondition.
        let res = unsafe { bindings::cpp_vmcompression_compress(self.as_raw(), page_src) };
        match res.tag {
            0 => CompressResult::CompressedRef(CompressedRef::new(res.ref_)),
            1 => CompressResult::Zero,
            2 => {
                // SAFETY: `res.fail_page` was produced by C++ `VmCompression::Compress` and is
                // either null or a valid `vm_page_t` pointer.
                let page = unsafe { VmPagePtr::from_ffi(res.fail_page) };
                CompressResult::Fail(FailTag {
                    src_page: PageAndMetadata { page, metadata: res.fail_metadata },
                })
            }
            _ => unreachable!(),
        }
    }

    /// Sets the metadata for the original page referred to by `reference`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the VMO lock for the VMO that `reference` was placed into is
    /// held for the duration of this call, and that `reference` is a valid reference belonging to
    /// this `VmCompression`.
    pub unsafe fn set_metadata(&self, reference: ReferenceValue, metadata: u32) {
        // SAFETY: `self.as_raw()` points to a live `VmCompression`, and by this function's
        // `# Safety` precondition the caller holds the VMO lock and `reference` belongs to `self`,
        // discharging the contract of `VmCompression::SetMetadata`.
        unsafe {
            bindings::cpp_vmcompression_set_metadata(self.as_raw(), reference.value(), metadata)
        }
    }

    /// Frees the compressed reference without decompressing it. `reference` may be the temporary
    /// reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `reference` is owned by the caller, is not used again after
    /// this call, and belongs to this `VmCompression`. If `reference` may be the temporary
    /// reference, the caller must hold the lock of the VMO it was placed into for the duration of
    /// this call.
    pub unsafe fn free(&self, reference: ReferenceValue) {
        // SAFETY: `self.as_raw()` points to a live `VmCompression`, and by this function's
        // `# Safety` precondition `reference` is owned by the caller, belongs to `self`, and (if
        // temporary) its VMO lock is held, discharging `VmCompression::Free`.
        unsafe { bindings::cpp_vmcompression_free_reference(self.as_raw(), reference.value()) }
    }

    /// Returns a snapshot of the compression statistics.
    pub fn get_stats(&self) -> Stats {
        // SAFETY: `self.as_raw()` points to a live `VmCompression`.
        unsafe { bindings::cpp_vmcompression_get_stats(self.as_raw()) }
    }
}

/// An RAII wrapper around holding a locked reference to a `VmCompressor`.
///
/// Acquiring a compressor may block until one becomes available, and callers should be prepared
/// for extended wait times. The guard must not outlive the `VmCompression` it was acquired from.
#[pin_data(PinnedDrop)]
#[repr(transparent)]
pub struct CompressorGuard<'a> {
    opaque: Opaque<bindings::VmCompression_CompressorGuard>,
    phantom: PhantomData<&'a VmCompression>,
    // The C++ guard owns a held `Guard<Mutex>`, which may only be relocated by its move
    // constructor. Marking this field `#[pin]` is what makes the type `!Unpin`, and so prevents
    // safe code from moving it out of the `Pin` it is constructed in.
    #[pin]
    _pin: PhantomPinned,
}

zr::unsafe_pinned_drop_ffi!(
    CompressorGuard<'_>,
    bindings::cpp_vmcompression_compressor_guard_destroy
);

impl<'a> CompressorGuard<'a> {
    /// Returns a reference to the guarded `VmCompressor`.
    pub fn get(self: Pin<&mut Self>) -> &VmCompressor {
        // SAFETY: `self.opaque.get()` points to a live `CompressorGuard` for the duration of the
        // `self` borrow.
        let raw = unsafe { bindings::cpp_vmcompression_compressor_guard_get(self.opaque.get()) };
        // SAFETY: The shim returns the address of a C++ `VmCompressor&` reference
        // (`&guard->get()`), so `raw` is non-null, aligned, and points to an initialized
        // `VmCompressor`. C++ `VmCompressor` has the exact same size, alignment, and representation
        // as Rust `VmCompressor` (verified by `zr::static_assert!`), and all mutable fields are
        // wrapped in `UnsafeCell`. The compressor is owned by the guard, so it stays live for as
        // long as `self` is borrowed.
        unsafe { &*raw.cast::<VmCompressor>() }
    }
}

impl fbl::HasRefCount for VmCompression {
    fn ref_count(&self) -> &fbl::RefCounted {
        // SAFETY: `cpp_vmcompression_get_ref_counted` returns a valid pointer to the C++
        // `fbl::RefCounted` subobject of `VmCompression`.
        unsafe {
            &*(bindings::cpp_vmcompression_get_ref_counted(self.as_raw()) as *const fbl::RefCounted)
        }
    }
}

// SAFETY: `VmCompression` represents a C++ `fbl::RefCounted` object.
unsafe impl fbl::Recyclable for VmCompression {
    unsafe fn recycle(ptr: core::ptr::NonNull<Self>) {
        // SAFETY: Caller guarantees `ptr` points to a valid `VmCompression` with zero remaining
        // references ready to be deleted.
        unsafe { bindings::cpp_vmcompression_free(ptr.as_ptr() as *mut bindings::VmCompression) }
    }
}
