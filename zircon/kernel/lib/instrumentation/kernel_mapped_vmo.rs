// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Safe Rust facade for C++ `KernelMappedVmo`.

use core::ffi::CStr;
use fbl::{OpaqueFacade, RefPtr};
use zx_status::Status;

use crate::vm::vm_object::VmObject;

use super::kernel_mapped_vmo_ffi::{
    cpp_kernel_mapped_vmo_base_locking, cpp_kernel_mapped_vmo_init,
};

/// Opaque handle to a C++ `KernelMappedVmo`.
#[repr(C)]
pub struct KernelMappedVmo {
    _facade: OpaqueFacade,
}

impl KernelMappedVmo {
    /// Initializes this `KernelMappedVmo` with a kernel mapping from `vmo`.
    pub(super) fn init(
        &self,
        vmo: RefPtr<VmObject>,
        offset: usize,
        size: usize,
        name: &CStr,
    ) -> Result<(), Status> {
        let raw = RefPtr::into_raw(vmo).cast_mut();
        // SAFETY: `self` is a valid `KernelMappedVmo` wrapping `OpaqueFacade` (`UnsafeCell`), `raw`
        // transfers an owned reference to C++, and `name` is a valid NUL-terminated C string.
        Status::ok(unsafe {
            cpp_kernel_mapped_vmo_init(
                core::ptr::from_ref(self).cast_mut(),
                raw,
                offset,
                size,
                name.as_ptr(),
            )
        })
    }

    /// Returns the base address of the mapping in the kernel address space.
    ///
    /// # Safety
    ///
    /// `self.init(...)` must have previously succeeded on this `KernelMappedVmo`.
    pub(super) unsafe fn base_locking(&self) -> usize {
        // SAFETY: Caller guarantees `self` has been initialized with a valid mapping.
        unsafe { cpp_kernel_mapped_vmo_base_locking(self) }
    }
}
