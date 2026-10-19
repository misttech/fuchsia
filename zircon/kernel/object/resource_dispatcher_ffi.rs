// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::mem::MaybeUninit;
use zx_status::Status;
use zx_types::{ZX_MAX_NAME_LEN, ZX_OK, zx_rsrc_kind_t, zx_status_t};

use region_alloc::Region;

use super::handle::KernelHandle;
use super::resource_dispatcher::{ResourceDispatcher, ResourceDispatcherState, ResourceStorage};

// C++ FFI declarations
#[allow(improper_ctypes)]
unsafe extern "C" {
    /// Calls into C++ implementation to allocate and construct a ResourceDispatcher.
    ///
    /// # Safety
    ///
    /// `handle_out` must point to writable, uninitialized memory allocated for
    /// `KernelHandle<ResourceDispatcher>`. If `region` is non-null, it must point to a valid
    /// `Region` allocated for this resource.
    pub(super) fn cpp_resource_dispatcher_create(
        kind: zx_rsrc_kind_t,
        base: u64,
        size: usize,
        flags: u32,
        name: *const core::ffi::c_char,
        name_size: usize,
        storage: *mut ResourceStorage,
        region: *mut Region,
        handle_out: *mut MaybeUninit<KernelHandle<ResourceDispatcher>>,
    ) -> zx_status_t;
}

// FFI trampolines for C++ calling into Rust ResourceDispatcherState

crate::object::dispatcher::impl_dispatcher_state_init!(
    ResourceDispatcher,
    ResourceDispatcherState,
    kind: zx_rsrc_kind_t,
    base: u64,
    size: usize,
    flags: u32,
    name: *const core::ffi::c_char,
    name_size: usize,
    storage: *mut ResourceStorage,
    region: *mut Region,
);

/// Initializes allocator for `kind`.
///
/// # Safety
///
/// This function is safe to call from C with any arguments; it acquires the internal resource lock
/// and validates `kind`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_resource_dispatcher_initialize_allocator(
    kind: zx_rsrc_kind_t,
    base: u64,
    size: usize,
) -> zx_status_t {
    Status::result_into_raw(ResourceDispatcher::initialize_allocator(kind, base, size))
}

/// Copies the name of `disp` to `out_name`.
///
/// # Safety
///
/// `disp` and `out_name` must be valid references for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_resource_dispatcher_get_name(
    disp: &ResourceDispatcher,
    out_name: &mut [u8; ZX_MAX_NAME_LEN],
) -> zx_status_t {
    disp.get_name(out_name);
    ZX_OK
}
