// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::vm::compressor::VmCompressor;
use crate::vm::vm_page_list::ReferenceValue;
use compression_bindings as bindings;
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

    /// Frees the compressed reference without decompressing it. `reference` may be the temporary
    /// reference.
    ///
    /// # Safety
    ///
    /// `reference` must be owned by the caller, must not be used again, and must belong to this
    /// `VmCompression`. If it may be the temporary reference, the lock of the VMO it was placed
    /// into must be held.
    pub unsafe fn free(&self, reference: ReferenceValue) {
        // SAFETY: `self.as_raw()` points to a live `VmCompression`, and the caller upholds the
        // requirements documented in `# Safety`.
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
        // SAFETY: Nothing is moved out of the returned reference; it is only used to read
        // `opaque`'s address.
        let this = unsafe { self.get_unchecked_mut() };
        // SAFETY: `this.opaque.get()` points to a live `CompressorGuard`.
        let raw = unsafe { bindings::cpp_vmcompression_compressor_guard_get(this.opaque.get()) };
        // SAFETY: The shim returns the address of a reference, so it is never null, and
        // `VmCompressor` is a `repr(C)` wrapper whose only field is the corresponding opaque
        // binding type. The compressor is owned by the guard, so it stays live for as long as
        // `this` is borrowed.
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
