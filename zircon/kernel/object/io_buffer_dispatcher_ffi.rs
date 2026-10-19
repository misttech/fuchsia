// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::handle::KernelHandle;
use crate::object::io_buffer_dispatcher::{IoBufferDispatcher, IoBufferDispatcherState};
use crate::vm::vm_object::VmObjectChildObserver;
use core::ffi::c_char;
use core::mem::MaybeUninit;
use zx_status::Status;
use zx_types::{ZX_MAX_NAME_LEN, zx_status_t};

unsafe extern "C" {
    pub(super) fn cpp_io_buffer_dispatcher_create(
        holder: *mut (),
        endpoint_id: usize,
        shared_state: *mut (),
        handle_out: *mut MaybeUninit<KernelHandle<IoBufferDispatcher>>,
    ) -> zx_status_t;

    pub(super) fn cpp_io_buffer_dispatcher_as_child_observer(
        disp: &IoBufferDispatcher,
    ) -> *mut VmObjectChildObserver;
}

crate::object::dispatcher::impl_peered_dispatcher_state_init!(
    IoBufferDispatcher,
    IoBufferDispatcherState,
    endpoint_id: usize,
    shared_state: *mut (),
);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_io_buffer_dispatcher_on_zero_child(disp: &IoBufferDispatcher) {
    disp.on_zero_child();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_io_buffer_dispatcher_get_name(
    disp: &IoBufferDispatcher,
    out_name: *mut [u8; ZX_MAX_NAME_LEN],
) -> zx_status_t {
    let out = unsafe { &mut *out_name };
    Status::result_into_raw(disp.get_name(out))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_io_buffer_dispatcher_set_name(
    disp: &IoBufferDispatcher,
    name: *const c_char,
    len: usize,
) -> zx_status_t {
    // SAFETY: The caller guarantees `name` points to `len` initialized bytes whenever `len > 0`.
    let name_bytes = unsafe { zr::slice_from_raw_parts(name.cast(), len) };
    Status::result_into_raw(disp.set_name(name_bytes))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_io_buffer_dispatcher_region_count(
    disp: &IoBufferDispatcher,
) -> usize {
    disp.region_count()
}
