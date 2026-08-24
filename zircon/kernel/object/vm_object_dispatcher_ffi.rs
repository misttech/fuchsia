// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::vm_object_dispatcher::{InitialMutability, VmObjectDispatcher};
use crate::vm::vm_object::VmObject;
use core::mem::MaybeUninit;
use fbl::RefPtr;
use zx_types::{zx_rights_t, zx_status_t};

unsafe extern "C" {
    pub(crate) fn cpp_vm_object_dispatcher_get_vmo(
        disp: *const VmObjectDispatcher,
    ) -> *const RefPtr<VmObject>;
    pub(crate) fn cpp_vm_object_dispatcher_create(
        raw_vmo: *mut VmObject,
        stream_size: u64,
        initial_mutability: InitialMutability,
        out_handle: *mut MaybeUninit<KernelHandle<VmObjectDispatcher>>,
        out_rights: *mut zx_rights_t,
    ) -> zx_status_t;
    pub(crate) fn cpp_vm_object_dispatcher_get_vmo_info(
        vmo: *mut VmObjectDispatcher,
        rights: zx_types::zx_rights_t,
    ) -> zx_types::zx_info_vmo_t;
}
