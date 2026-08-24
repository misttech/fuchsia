// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::vm_object_dispatcher_ffi::{
    cpp_vm_object_dispatcher_get_vmo, cpp_vm_object_dispatcher_get_vmo_info,
};
use crate::vm::vm_object::VmObject;
use crate::vm::vm_object_paged::VmObjectPaged;
use core::mem::MaybeUninit;
use fbl::RefPtr;
use page;
use zx_status::Status;
use zx_types::{
    ZX_OBJ_TYPE_VMO, ZX_VMO_DISCARDABLE, ZX_VMO_RESIZABLE, ZX_VMO_UNBOUNDED, zx_info_vmo_t,
    zx_rights_t,
};

// LINT.IfChange(InitialMutability)
/// Specifies initial mutability for `VmObjectDispatcher`.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitialMutability {
    Mutable = 0,
    Immutable = 1,
}
// LINT.ThenChange(//zircon/kernel/object/include/object/vm_object_dispatcher.h:InitialMutability)

zr::static_assert!(core::mem::size_of::<InitialMutability>() == 4);
zr::static_assert!(core::mem::align_of::<InitialMutability>() == 4);

crate::object::dispatcher::impl_dispatcher_facade!(
    pub struct VmObjectDispatcher,
    ZX_OBJ_TYPE_VMO
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateStats {
    pub flags: u32,
    pub size: u64,
}

impl VmObjectDispatcher {
    /// Creates a `VmObjectDispatcher` wrapping a VMO.
    pub fn create(
        vmo: &VmObject,
        stream_size: u64,
        initial_mutability: InitialMutability,
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        let mut handle_out = MaybeUninit::<KernelHandle<Self>>::uninit();
        let mut rights_out: zx_rights_t = 0;
        // SAFETY: `vmo.as_raw()` returns a valid raw pointer to a VmObject, and `handle_out` and `rights_out`
        // point to valid uninitialized memory.
        let status = unsafe {
            super::vm_object_dispatcher_ffi::cpp_vm_object_dispatcher_create(
                vmo.as_raw() as *mut VmObject,
                stream_size,
                initial_mutability,
                &raw mut handle_out,
                &mut rights_out,
            )
        };
        Status::ok(status)?;
        // SAFETY: `cpp_vm_object_dispatcher_create` returned ZX_OK, so `handle_out` has been initialized.
        let handle = unsafe { handle_out.assume_init() };
        Ok((handle, rights_out))
    }

    /// Returns a reference to the underlying `VmObject`.
    pub fn vmo(&self) -> &RefPtr<VmObject> {
        // SAFETY: `self` is a valid `VmObjectDispatcher` reference.
        unsafe { &*cpp_vm_object_dispatcher_get_vmo(self as *const _) }
    }

    /// Parses create syscall flags for VMOs.
    ///
    /// Rounds up the size to the nearest page, or sets the size to the maximum possible VMO size if
    /// `ZX_VMO_UNBOUNDED` is used.
    pub fn parse_create_syscall_flags(mut flags: u32, size: u64) -> Result<CreateStats, Status> {
        let mut res = CreateStats { flags: 0, size };

        if (flags & ZX_VMO_RESIZABLE) != 0 {
            if (flags & ZX_VMO_UNBOUNDED) != 0 {
                return Err(Status::INVALID_ARGS);
            }
            res.flags |= VmObjectPaged::RESIZABLE;
            flags &= !ZX_VMO_RESIZABLE;
        }
        if (flags & ZX_VMO_DISCARDABLE) != 0 {
            res.flags |= VmObjectPaged::DISCARDABLE;
            flags &= !ZX_VMO_DISCARDABLE;
        }
        if (flags & ZX_VMO_UNBOUNDED) != 0 {
            flags &= !ZX_VMO_UNBOUNDED;
            res.size = VmObject::MAX_SIZE;
        } else {
            if size > VmObject::MAX_SIZE {
                return Err(Status::OUT_OF_RANGE);
            }
            res.size = page::round_up(size as usize) as u64;
        }

        if flags != 0 {
            return Err(Status::INVALID_ARGS);
        }

        // The initial stream size should not end up larger than the vmo size, as this is a state
        // that cannot exist.
        if size > res.size {
            return Err(Status::OUT_OF_RANGE);
        }

        Ok(res)
    }

    /// Returns information about this VMO.
    pub fn get_vmo_info(&self, rights: zx_rights_t) -> zx_info_vmo_t {
        // SAFETY: `self` is a valid `VmObjectDispatcher` reference.
        unsafe { cpp_vm_object_dispatcher_get_vmo_info(self as *const _ as *mut _, rights) }
    }
}

/// Kernel unit tests for `VmObjectDispatcher`.
#[cfg(ktest)]
#[unittest::suite(name = "vm_object_dispatcher_tests")]
mod tests {
    use super::{InitialMutability, VmObjectDispatcher};
    use crate::vm::vm_object_paged::VmObjectPaged;

    /// Tests creating a VmObjectDispatcher and accessing its underlying VMO.
    #[test]
    fn test_vm_object_dispatcher_create_and_vmo() {
        let paged_vmo = VmObjectPaged::create(0, 0, 4096).expect("failed to create paged VMO");
        let vmo_size = paged_vmo.size();
        unittest::expect_eq!(vmo_size, 4096);

        let (handle, rights) =
            VmObjectDispatcher::create(&paged_vmo, vmo_size, InitialMutability::Mutable)
                .expect("failed to create VmObjectDispatcher");
        unittest::expect_true!(rights != 0);

        let disp = handle.dispatcher();
        let disp_vmo = disp.vmo();
        unittest::expect_eq!(disp_vmo.size(), 4096);
    }
}
