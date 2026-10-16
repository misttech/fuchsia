// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! C++ FFI declarations for `KernelMappedVmo`.

use core::ffi::c_char;
use zx_status::sys::zx_status_t;

use crate::vm::vm_object::VmObject;

use super::kernel_mapped_vmo::KernelMappedVmo;

unsafe extern "C" {
    /// Initializes `mapped_vmo` with a kernel mapping of `vmo`.
    ///
    /// # Safety
    ///
    /// `mapped_vmo` must point to a valid `KernelMappedVmo`, `vmo` must be a valid owned
    /// `fbl::RefPtr<VmObject>` raw pointer, and `name` must be a valid NUL-terminated C string.
    pub(super) fn cpp_kernel_mapped_vmo_init(
        mapped_vmo: *mut KernelMappedVmo,
        vmo: *mut VmObject,
        offset: usize,
        size: usize,
        name: *const c_char,
    ) -> zx_status_t;

    /// Returns the base virtual address of `mapped_vmo`'s mapping.
    ///
    /// # Safety
    ///
    /// `mapped_vmo` must point to a `KernelMappedVmo` on which `cpp_kernel_mapped_vmo_init`
    /// succeeded.
    pub(super) fn cpp_kernel_mapped_vmo_base_locking(mapped_vmo: *const KernelMappedVmo) -> usize;
}
