// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::vm_object_dispatcher::{InitialMutability, VmObjectDispatcher, VmObjectDispatcherState};
use crate::vm::stream_size_manager::StreamSizeManager;
use crate::vm::vm_object::{VmObject, VmObjectChildObserver};
use core::ffi::c_char;
use core::mem::MaybeUninit;
use fbl::RefPtr;
use zx_status::Status;
use zx_types::{ZX_MAX_NAME_LEN, ZX_OK, zx_rights_t, zx_status_t};

#[allow(improper_ctypes)]
unsafe extern "C" {
    /// Calls into C++ to allocate and initialize a `VmObjectDispatcher`.
    pub(crate) fn cpp_vm_object_dispatcher_create(
        raw_vmo: *mut VmObject,
        raw_ssm: *mut StreamSizeManager,
        initial_mutability: InitialMutability,
        out_handle: *mut MaybeUninit<KernelHandle<VmObjectDispatcher>>,
    ) -> zx_status_t;

    /// Performs the C++ multiple-inheritance upcast from `VmObjectDispatcher*` to
    /// `VmObjectChildObserver*`.
    pub(crate) fn cpp_vm_object_dispatcher_as_child_observer(
        disp: &VmObjectDispatcher,
    ) -> *mut VmObjectChildObserver;
}

/// Initializes a `VmObjectDispatcherState` in place.
///
/// # Safety
///
/// `state` must point to uninitialized memory of at least `VmObjectDispatcherState` size and
/// alignment. `vmo` must be a valid non-null pointer with ownership of one `RefPtr` count
/// transferred, and `stream_size_mgr` must be either null or a valid pointer with ownership of one
/// `RefPtr` count transferred.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_dispatcher_state_init(
    state: *mut VmObjectDispatcherState,
    dispatcher: *const VmObjectDispatcher,
    vmo: *mut VmObject,
    stream_size_mgr: *mut StreamSizeManager,
    initial_mutability: InitialMutability,
) {
    // SAFETY: The C++ constructor passes an uninitialized `opaque_storage_` buffer of sufficient
    // size and alignment, along with an exported `RefPtr` pointer for `vmo` and an optional
    // exported `RefPtr` pointer for `stream_size_mgr`.
    unsafe {
        let vmo = RefPtr::from_raw(vmo);
        let stream_size_mgr = RefPtr::try_from_raw(stream_size_mgr);
        let _ = pin_init::PinInit::__pinned_init(
            VmObjectDispatcherState::init(dispatcher, vmo, stream_size_mgr, initial_mutability),
            state,
        );
    }
}

/// Creates a `VmObjectDispatcher` from C++.
///
/// # Safety
///
/// `raw_vmo` must be a valid non-null `VmObject` pointer with ownership of one `RefPtr` count
/// transferred. `out_handle` and `out_rights` must point to valid uninitialized memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_dispatcher_create(
    raw_vmo: *mut VmObject,
    stream_size: u64,
    initial_mutability: InitialMutability,
    out_handle: *mut MaybeUninit<KernelHandle<VmObjectDispatcher>>,
    out_rights: *mut MaybeUninit<zx_rights_t>,
) -> zx_status_t {
    // SAFETY: Caller transfers ownership of one `RefPtr` reference count in `raw_vmo`.
    let vmo = unsafe { RefPtr::from_raw(raw_vmo) };
    match VmObjectDispatcher::create(&vmo, stream_size, initial_mutability) {
        Ok((handle, rights)) => {
            // SAFETY: Caller guarantees `out_handle` and `out_rights` point to valid
            // uninitialized memory.
            unsafe {
                (*out_handle).write(handle);
                (*out_rights).write(rights);
            }
            ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Returns a pointer to the `RefPtr<VmObject>` stored in `disp`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_object_dispatcher_get_vmo(
    disp: &VmObjectDispatcher,
) -> *const RefPtr<VmObject> {
    disp.vmo()
}

/// Invokes `on_zero_child` on `disp`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_object_dispatcher_on_zero_child(disp: &VmObjectDispatcher) {
    disp.on_zero_child();
}

/// Invokes `on_zero_handles` on `disp`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_object_dispatcher_on_zero_handles(disp: &VmObjectDispatcher) {
    disp.on_zero_handles();
}

/// Gets the name of `disp` from C++.
///
/// # Safety
///
/// `out_name` must point to a valid writable buffer of `ZX_MAX_NAME_LEN` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_dispatcher_get_name(
    disp: &VmObjectDispatcher,
    out_name: *mut [u8; ZX_MAX_NAME_LEN],
) -> zx_status_t {
    // SAFETY: Caller guarantees `out_name` points to a valid `[u8; ZX_MAX_NAME_LEN]`.
    let out = unsafe { &mut *out_name };
    Status::result_into_raw(disp.get_name(out))
}

/// Sets the name of `disp` from C++.
///
/// # Safety
///
/// `name` must point to `len` initialized bytes whenever `len > 0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_dispatcher_set_name(
    disp: &VmObjectDispatcher,
    name: *const c_char,
    len: usize,
) -> zx_status_t {
    // SAFETY: Caller guarantees `name` points to `len` initialized bytes whenever `len > 0`.
    let name_bytes = unsafe { zr::slice_from_raw_parts(name.cast(), len) };
    Status::result_into_raw(disp.set_name(name_bytes))
}

/// Returns the `StreamSizeManager` for `disp`, lazily creating one if needed.
///
/// # Safety
///
/// `out_ssm` must point to valid uninitialized memory for `RefPtr<StreamSizeManager>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_dispatcher_stream_size_manager(
    disp: &VmObjectDispatcher,
    out_ssm: *mut MaybeUninit<RefPtr<StreamSizeManager>>,
) -> zx_status_t {
    match disp.stream_size_manager() {
        Ok(ssm) => {
            // SAFETY: Caller guarantees `out_ssm` points to valid uninitialized memory.
            unsafe {
                (*out_ssm).write(ssm);
            }
            ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Creates a child VMO from C++.
///
/// # Safety
///
/// `out_child_vmo` must point to valid uninitialized memory for `RefPtr<VmObject>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_object_dispatcher_create_child(
    disp: &VmObjectDispatcher,
    options: u32,
    offset: u64,
    size: u64,
    copy_name: bool,
    out_child_vmo: *mut MaybeUninit<RefPtr<VmObject>>,
) -> zx_status_t {
    match disp.create_child(options, offset, size, copy_name) {
        Ok(child_vmo) => {
            // SAFETY: Caller guarantees `out_child_vmo` points to valid uninitialized memory.
            unsafe {
                (*out_child_vmo).write(child_vmo);
            }
            ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Sets the stream size of `disp` from C++.
#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_object_dispatcher_set_stream_size(
    disp: &VmObjectDispatcher,
    stream_size: u64,
) -> zx_status_t {
    Status::result_into_raw(disp.set_stream_size(stream_size))
}

/// Returns the stream size of `disp` from C++.
#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_object_dispatcher_get_stream_size(disp: &VmObjectDispatcher) -> u64 {
    disp.get_stream_size()
}
