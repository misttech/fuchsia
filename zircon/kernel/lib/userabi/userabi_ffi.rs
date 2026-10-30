// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{VDso, userboot_init};
use crate::object::HandleOwner;
use crate::vm::vm_aspace::VmAspaceLockClass;
use crate::vm::vm_mapping::VmMapping;
use crate::vm::vm_object::VmObject;
use core::ffi::c_void;
use core::mem::{MaybeUninit, take};
use core::ops::Deref;
use core::ptr::{drop_in_place, null_mut, slice_from_raw_parts_mut};
use fbl::RefPtr;
use ksync::LockToken;
use libc::stdio::FILE;
use zr::{slice_from_raw_parts, static_assert_size_and_align};
use zx_types::{ZX_MAX_NAME_LEN, zx_ticks_t};

pub const ZERO_FILL: usize = usize::MAX;
const MAX_EXTRA_HANDOFF_PHYS_VMOS: usize = 3;

#[repr(transparent)]
#[derive(Clone, Copy, Default)]
pub struct PhysMappingPermissions(usize);

impl PhysMappingPermissions {
    pub fn readable(&self) -> bool {
        (self.0 & (1 << 0)) != 0
    }

    pub fn writable(&self) -> bool {
        (self.0 & (1 << 1)) != 0
    }

    pub fn executable(&self) -> bool {
        (self.0 & (1 << 2)) != 0
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PhysMapping {
    name: [u8; ZX_MAX_NAME_LEN],
    type_: u32,
    pub vaddr: usize,
    pub size: usize,
    pub paddr: usize,
    pub perms: PhysMappingPermissions,
    kasan_shadow: bool,
}

static_assert_size_and_align!(PhysMappingPermissions, 8, 8);
static_assert_size_and_align!(PhysMapping, 80, 8);

#[repr(C)]
#[derive(Clone, Copy)]
struct CppOptionalUsize {
    value: MaybeUninit<usize>,
    has_value: bool,
}

impl Default for CppOptionalUsize {
    fn default() -> Self {
        Self { value: MaybeUninit::uninit(), has_value: false }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PhysElfImageInfo {
    pub relative_entry_point: usize,
    stack_size: CppOptionalUsize,
}

impl PhysElfImageInfo {
    pub fn stack_size(&self) -> Option<usize> {
        // SAFETY: `self.stack_size.value` is initialized by C++ `std::optional<size_t>`
        // whenever `self.stack_size.has_value` is true.
        self.stack_size.has_value.then(|| unsafe { self.stack_size.value.assume_init() })
    }
}

static_assert_size_and_align!(CppOptionalUsize, 16, 8);
static_assert_size_and_align!(PhysElfImageInfo, 24, 8);

#[repr(C)]
pub struct CppVector<T> {
    ptr: *mut T,
    capacity: usize,
    size: usize,
}

impl<T> Default for CppVector<T> {
    fn default() -> Self {
        Self { ptr: null_mut(), capacity: 0, size: 0 }
    }
}

impl<T> Deref for CppVector<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        // SAFETY: `self.ptr` is either null (when `self.size == 0`) or points to `self.size`
        // initialized elements of `T`.
        unsafe { slice_from_raw_parts(self.ptr, self.size) }
    }
}

impl<T> Drop for CppVector<T> {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe extern "C" {
                fn free(ptr: *mut c_void);
            }
            // SAFETY: `self.ptr` points to `self.size` initialized elements allocated by C++
            // `fbl::AlignedAllocatorTraits<T>::Allocate`, which uses `new char[size]` (deallocated
            // via `delete[]` -> `free`).
            unsafe {
                drop_in_place(slice_from_raw_parts_mut(self.ptr, self.size));
                free(self.ptr.cast());
            }
        }
    }
}

static_assert_size_and_align!(CppVector<PhysMapping>, 24, 8);

#[repr(C)]
#[derive(Default)]
pub struct HandoffEndElf {
    pub vmo: Option<RefPtr<VmObject>>,
    pub stream_size: usize,
    pub vmar_size: usize,
    pub mappings: CppVector<PhysMapping>,
    pub info: PhysElfImageInfo,
}

static_assert_size_and_align!(HandoffEndElf, 72, 8);

/// Rust representation of the kernel handoff end state (`HandoffEnd`) consumed by `userboot_init`.
#[repr(C)]
#[derive(Default)]
pub struct HandoffEnd {
    pub zbi: Option<HandleOwner>,
    pub vdso: HandoffEndElf,
    pub userboot: HandoffEndElf,
    pub extra_phys_vmos: [Option<HandleOwner>; MAX_EXTRA_HANDOFF_PHYS_VMOS],
}

static_assert_size_and_align!(HandoffEnd, 176, 8);

pub fn boot_options_show(defaults: bool, out: &FILE) {
    // SAFETY: `out` points to a valid `FILE` for the duration of the call.
    unsafe { cpp_boot_options_show(defaults, out) }
}

#[cfg(enable_entropy_collector_test)]
pub fn entropy_was_lost() -> bool {
    // SAFETY: Reads the global `entropy_was_lost` flag.
    unsafe { cpp_entropy_was_lost() }
}

#[cfg(enable_entropy_collector_test)]
pub fn entropy_vmo() -> Option<RefPtr<VmObject>> {
    // SAFETY: `cpp_entropy_vmo` exports a reference to `entropy_vmo` or returns null.
    unsafe { VmObject::from_raw(cpp_entropy_vmo().cast()) }
}

#[cfg(enable_entropy_collector_test)]
pub fn entropy_vmo_stream_size() -> u64 {
    // SAFETY: Reads the global `entropy_vmo_stream_size`.
    unsafe { cpp_entropy_vmo_stream_size() as u64 }
}

#[cfg(all(target_arch = "aarch64", debug_assertions))]
pub fn arm64_print_midr_cpu_name(out: &FILE) {
    // SAFETY: `out` points to a valid `FILE` for the duration of the call.
    unsafe { cpp_arm64_print_midr_cpu_name(out) }
}

unsafe extern "C" {
    fn cpp_boot_options_show(defaults: bool, out: *const FILE);
    #[cfg(enable_entropy_collector_test)]
    fn cpp_entropy_was_lost() -> bool;
    #[cfg(enable_entropy_collector_test)]
    fn cpp_entropy_vmo() -> *mut c_void;
    #[cfg(enable_entropy_collector_test)]
    fn cpp_entropy_vmo_stream_size() -> usize;
    #[cfg(all(target_arch = "aarch64", debug_assertions))]
    fn cpp_arm64_print_midr_cpu_name(out: *const FILE);
}

/// FFI entry point launching the `userboot` process at the end of kernel boot handoff.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn rust_userboot_init(handoff_end: &mut HandoffEnd) {
    userboot_init(take(handoff_end));
}

/// FFI entry point for [`VDso::vmo_is_vdso`]. C++ passes a non-null `const VmObject*`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_vdso_vmo_is_vdso(vmo: &VmObject) -> bool {
    VDso::vmo_is_vdso(vmo)
}

/// FFI entry point for [`VDso::valid_code_mapping`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_vdso_valid_code_mapping(vmo_offset: u64, size: usize) -> bool {
    VDso::valid_code_mapping(vmo_offset, size)
}

/// FFI entry point for [`VDso::base_address`].
///
/// # Safety
///
/// `code_mapping` must be a valid reference, and the caller must hold `code_mapping`'s `VmAspace`
/// lock (`code_mapping->lock()`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vdso_base_address(code_mapping: &VmMapping) -> usize {
    // SAFETY: The caller guarantees `code_mapping`'s `VmAspace` lock (`code_mapping->lock()`) is
    // held (`TA_REQ(code_mapping->lock())` on `VDso::base_address` in `vdso.h`).
    let token: LockToken<'_, VmAspaceLockClass> = unsafe { LockToken::new() };
    VDso::base_address(code_mapping, &token)
}

/// FFI entry point for [`VDso::set_monotonic_ticks_offset`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_vdso_set_monotonic_ticks_offset(new_offset: zx_ticks_t) {
    VDso::set_monotonic_ticks_offset(new_offset);
}
