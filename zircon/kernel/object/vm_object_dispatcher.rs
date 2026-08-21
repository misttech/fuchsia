// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::vm_object_dispatcher_ffi::cpp_vm_object_dispatcher_get_vmo;
use crate::vm::vm_object::VmObject;
use crate::vm::vm_object_paged::VmObjectPaged;
use fbl::RefPtr;
use page;
use zx_status::Status;
use zx_types::{ZX_OBJ_TYPE_VMO, ZX_VMO_DISCARDABLE, ZX_VMO_RESIZABLE, ZX_VMO_UNBOUNDED};

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
}
