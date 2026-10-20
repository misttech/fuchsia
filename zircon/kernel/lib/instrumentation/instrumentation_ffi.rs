// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! C++ FFI entry points for `KernelMappedVmo` and `memory_profile_vmo`.

use super::kernel_mapped_vmo::KernelMappedVmo;
use super::memory_profile::memory_profile_vmo;
use crate::kernel::types::VAddr;
use crate::object::HandleOwner;
use crate::vm::vm_object::VmObject;
use core::ffi::c_char;
use core::mem::{MaybeUninit, offset_of};
use zx_status::Status;

/// Constructs an empty `KernelMappedVmo` into `storage`.
///
/// # Safety
///
/// `storage` must be valid for writes, properly aligned, and not aliased for the duration of the
/// call.  The resulting `KernelMappedVmo` must eventually be destroyed with
/// `rust_kernel_mapped_vmo_drop_in_place`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_mapped_vmo_new(storage: &mut MaybeUninit<KernelMappedVmo>) {
    storage.write(KernelMappedVmo::new());
}

/// Drops the `KernelMappedVmo` in `this` in place.
///
/// # Safety
///
/// `this` must be a valid, aligned pointer to a `KernelMappedVmo` constructed by
/// `rust_kernel_mapped_vmo_new` (whether or not `rust_kernel_mapped_vmo_init` was subsequently
/// called), not aliased for the duration of the call, and not used again afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_mapped_vmo_drop_in_place(this: *mut KernelMappedVmo) {
    // SAFETY: Caller guarantees `this` is valid for dropping in place.
    unsafe {
        core::ptr::drop_in_place(this);
    }
}

/// Initializes `this` with a kernel mapping from `vmo`.
///
/// # Safety
///
/// - `this` must point to a `KernelMappedVmo` constructed by `rust_kernel_mapped_vmo_new` and not
///   aliased for the duration of the call.
/// - `vmo` must be a valid `VmObject` pointer exported from C++ via `fbl::ExportToRawPtr`.
/// - If `name_len > 0`, `name` must point to `name_len` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_mapped_vmo_init(
    this: &mut KernelMappedVmo,
    vmo: *mut VmObject,
    offset: usize,
    size: usize,
    name: *const c_char,
    name_len: usize,
) -> Result<(), Status> {
    // SAFETY: Caller guarantees `vmo` is an owned reference exported from `fbl::RefPtr<VmObject>`.
    let vmo = unsafe { VmObject::from_raw(VmObject::cast_raw(vmo)) }.expect("vmo must not be null");
    let name_slice = if name.is_null() || name_len == 0 {
        &[]
    } else {
        // SAFETY: Caller guarantees `name` points to `name_len` valid bytes.
        unsafe { core::slice::from_raw_parts(name.cast::<u8>(), name_len) }
    };
    this.init(vmo, offset, size, name_slice)
}

/// Publishes `this` VMO to userland as a read-only handle using the given name and stream size.
///
/// # Safety
///
/// - `this` must point to a `KernelMappedVmo` on which `rust_kernel_mapped_vmo_init` succeeded,
///   and must not be mutably aliased for the duration of the call.
/// - If `vmo_name_len > 0`, `vmo_name` must point to `vmo_name_len` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_mapped_vmo_publish(
    this: &KernelMappedVmo,
    vmo_name: *const c_char,
    vmo_name_len: usize,
    stream_size: usize,
) -> Option<HandleOwner> {
    let vmo_name_slice = if vmo_name.is_null() || vmo_name_len == 0 {
        &[]
    } else {
        // SAFETY: Caller guarantees `vmo_name` points to `vmo_name_len` valid bytes.
        unsafe { core::slice::from_raw_parts(vmo_name.cast::<u8>(), vmo_name_len) }
    };
    this.publish(vmo_name_slice, stream_size)
}

/// Returns the base address of the mapping in the kernel address space.
///
/// `this` must point to a `KernelMappedVmo` on which `rust_kernel_mapped_vmo_init` succeeded.
#[unsafe(no_mangle)]
pub extern "C" fn rust_kernel_mapped_vmo_base_locking(this: &KernelMappedVmo) -> VAddr {
    this.base_locking()
}

/// Returns the size of the mapping in the kernel address space.
///
/// `this` must point to a `KernelMappedVmo` on which `rust_kernel_mapped_vmo_init` succeeded.
#[unsafe(no_mangle)]
pub extern "C" fn rust_kernel_mapped_vmo_size_locking(this: &KernelMappedVmo) -> usize {
    this.size_locking()
}

/// C ABI projection of `InstrumentationDataVmo`.  Matches `InstrumentationDataVmoFfi` in
/// private.h.
#[repr(C)]
pub struct InstrumentationDataVmoFfi {
    announce: *const c_char,
    announce_len: usize,
    sink_name: *const c_char,
    sink_name_len: usize,
    units: *const c_char,
    units_len: usize,
    scale: usize,
    handle: Option<HandleOwner>,
}

zr::static_assert_size_and_align!(InstrumentationDataVmoFfi, 64, 8);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, announce) == 0);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, announce_len) == 8);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, sink_name) == 16);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, sink_name_len) == 24);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, units) == 32);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, units_len) == 40);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, scale) == 48);
zr::static_assert!(offset_of!(InstrumentationDataVmoFfi, handle) == 56);

/// Populates `out` with the data for `MemoryProfileVmo()`.  All strings are `'static`.
///
/// # Safety
///
/// `out` must be valid for writes, properly aligned, and not aliased for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_memory_profile_vmo(out: &mut MaybeUninit<InstrumentationDataVmoFfi>) {
    let data = memory_profile_vmo();
    out.write(InstrumentationDataVmoFfi {
        announce: data.announce.as_ptr().cast(),
        announce_len: data.announce.len(),
        sink_name: data.sink_name.as_ptr().cast(),
        sink_name_len: data.sink_name.len(),
        units: data.units.as_ptr().cast(),
        units_len: data.units.len(),
        scale: data.scale,
        handle: data.handle,
    });
}
