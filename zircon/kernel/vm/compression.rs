// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use compression_bindings as bindings;
use core::marker::{PhantomData, PhantomPinned};
use pin_init::pin_data;
use zr::Opaque;

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
    /// Domain-specific conversion: returns raw pointer for `VmCompression`.
    pub fn as_raw(&self) -> *mut bindings::VmCompression {
        self.raw.get()
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

unsafe impl fbl::Recyclable for VmCompression {
    unsafe fn recycle(ptr: core::ptr::NonNull<Self>) {
        unsafe { bindings::cpp_vmcompression_free(ptr.as_ptr() as *mut bindings::VmCompression) }
    }
}
