// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::vm_object_dispatcher_ffi::{
    cpp_vm_object_dispatcher_get_vmo, cpp_vm_object_dispatcher_get_vmo_info,
};
use crate::user_copy::{UserInOutPtr, UserInPtr, UserOutPtr};
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
                vmo.as_raw().cast(),
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
        unsafe { &*cpp_vm_object_dispatcher_get_vmo(self.as_ffi()) }
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
        unsafe { cpp_vm_object_dispatcher_get_vmo_info(self.as_ffi_mut(), rights) }
    }

    /// Reads data from this VMO into a user buffer.
    pub fn read(&self, buffer: UserOutPtr<u8>, offset: u64, len: usize) -> Result<(), Status> {
        // SAFETY: `self` is a valid `VmObjectDispatcher` reference.
        let status = unsafe {
            super::vm_object_dispatcher_ffi::cpp_vm_object_dispatcher_read(
                self.as_ffi_mut(),
                buffer.as_ptr(),
                offset,
                len,
            )
        };
        Status::ok(status)
    }

    /// Writes data from a user buffer into this VMO.
    pub fn write(&self, buffer: UserInPtr<u8>, offset: u64, len: usize) -> Result<(), Status> {
        // SAFETY: `self` is a valid `VmObjectDispatcher` reference.
        let status = unsafe {
            super::vm_object_dispatcher_ffi::cpp_vm_object_dispatcher_write(
                self.as_ffi_mut(),
                buffer.as_ptr(),
                offset,
                len,
            )
        };
        Status::ok(status)
    }

    /// Returns the size of the underlying VMO in bytes.
    pub fn get_size(&self) -> Result<u64, Status> {
        let mut size = 0u64;
        // SAFETY: `self` is a valid `VmObjectDispatcher` reference, and `size` is a valid out pointer.
        let status = unsafe {
            super::vm_object_dispatcher_ffi::cpp_vm_object_dispatcher_get_size(
                self.as_ffi_mut(),
                &mut size,
            )
        };
        Status::ok(status)?;
        Ok(size)
    }

    /// Returns the stream size of the underlying VMO in bytes.
    pub fn get_stream_size(&self) -> u64 {
        // SAFETY: `self` is a valid `VmObjectDispatcher` reference.
        unsafe {
            super::vm_object_dispatcher_ffi::cpp_vm_object_dispatcher_get_stream_size(self.as_ffi())
        }
    }

    /// Sets the size of the underlying VMO in bytes.
    pub fn set_size(&self, size: u64) -> Result<(), Status> {
        // SAFETY: `self` is a valid `VmObjectDispatcher` reference.
        let status = unsafe {
            super::vm_object_dispatcher_ffi::cpp_vm_object_dispatcher_set_size(
                self.as_ffi_mut(),
                size,
            )
        };
        Status::ok(status)
    }

    /// Sets the stream size of the underlying VMO in bytes.
    pub fn set_stream_size(&self, size: u64) -> Result<(), Status> {
        // SAFETY: `self` is a valid `VmObjectDispatcher` reference.
        let status = unsafe {
            super::vm_object_dispatcher_ffi::cpp_vm_object_dispatcher_set_stream_size(
                self.as_ffi_mut(),
                size,
            )
        };
        Status::ok(status)
    }

    /// Performs an operation on a range of the VMO.
    pub fn range_op(
        &self,
        op: u32,
        offset: u64,
        size: u64,
        buffer: UserInOutPtr<u8>,
        buffer_size: usize,
        rights: zx_rights_t,
    ) -> Result<(), Status> {
        // SAFETY: `self` is a valid `VmObjectDispatcher` reference.
        let status = unsafe {
            super::vm_object_dispatcher_ffi::cpp_vm_object_dispatcher_range_op(
                self.as_ffi_mut(),
                op,
                offset,
                size,
                buffer.as_ptr(),
                buffer_size,
                rights,
            )
        };
        Status::ok(status)
    }

    /// Sets the mapping cache policy for the underlying VMO.
    pub fn set_mapping_cache_policy(&self, cache_policy: u32) -> Result<(), Status> {
        // SAFETY: `self` is a valid `VmObjectDispatcher` reference.
        let status = unsafe {
            super::vm_object_dispatcher_ffi::cpp_vm_object_dispatcher_set_mapping_cache_policy(
                self.as_ffi_mut(),
                cache_policy,
            )
        };
        Status::ok(status)
    }

    /// Creates a child VMO clone/slice/reference.
    pub fn create_child(
        &self,
        options: u32,
        offset: u64,
        size: u64,
        copy_name: bool,
    ) -> Result<RefPtr<VmObject>, Status> {
        let mut out_child = MaybeUninit::<RefPtr<VmObject>>::uninit();
        // SAFETY: `self` is a valid `VmObjectDispatcher` reference and `out_child` points to uninitialized storage.
        let status = unsafe {
            super::vm_object_dispatcher_ffi::cpp_vm_object_dispatcher_create_child(
                self.as_ffi_mut(),
                options,
                offset,
                size,
                copy_name,
                &raw mut out_child,
            )
        };
        Status::ok(status)?;
        // SAFETY: `cpp_vm_object_dispatcher_create_child` succeeded and initialized `out_child`.
        unsafe { Ok(out_child.assume_init()) }
    }

    /// Creates a child `VmObjectDispatcher` for the given child VMO.
    ///
    /// For reference children on stream-compatible VMOs, the child dispatcher shares the parent's
    /// `StreamSizeManager` so stream size updates are shared between parent and reference children.
    /// Otherwise, a new dispatcher is created with its stream size tracking initialized to `size`.
    pub fn create_child_dispatcher(
        &self,
        child_vmo: &VmObject,
        size: u64,
        options: u32,
        initial_mutability: InitialMutability,
    ) -> Result<(KernelHandle<VmObjectDispatcher>, zx_rights_t), Status> {
        if (options & zx_types::ZX_VMO_CHILD_REFERENCE) != 0 && self.vmo().is_stream_compatible() {
            self.create_child_with_parent_stream_size(child_vmo, initial_mutability)
        } else {
            Self::create(child_vmo, size, initial_mutability)
        }
    }

    /// Creates a child `VmObjectDispatcher` sharing this dispatcher's `StreamSizeManager`.
    pub fn create_child_with_parent_stream_size(
        &self,
        child_vmo: &VmObject,
        initial_mutability: InitialMutability,
    ) -> Result<(KernelHandle<VmObjectDispatcher>, zx_rights_t), Status> {
        let mut handle_out = MaybeUninit::<KernelHandle<VmObjectDispatcher>>::uninit();
        let mut rights_out: zx_rights_t = 0;
        // SAFETY: `self` and `child_vmo` are valid references, `handle_out` and `rights_out` point
        // to valid writable memory.
        let status = unsafe {
            super::vm_object_dispatcher_ffi::cpp_vm_object_dispatcher_create_with_parent_stream_size(
                self.as_ffi_mut(),
                child_vmo.as_raw().cast(),
                initial_mutability,
                &raw mut handle_out,
                &mut rights_out,
            )
        };
        Status::ok(status)?;
        // SAFETY: `cpp_vm_object_dispatcher_create_with_parent_stream_size` succeeded and initialized `handle_out`.
        let handle = unsafe { handle_out.assume_init() };
        Ok((handle, rights_out))
    }

    fn as_ffi(&self) -> *const VmObjectDispatcher {
        &raw const *self
    }

    fn as_ffi_mut(&self) -> *mut VmObjectDispatcher {
        self.as_ffi().cast_mut()
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
        let paged_vmo =
            VmObjectPaged::create(0, 0, page::SIZE as u64).expect("failed to create paged VMO");
        let vmo_size = paged_vmo.size();
        unittest::expect_eq!(vmo_size, page::SIZE as u64);

        let (handle, rights) =
            VmObjectDispatcher::create(&paged_vmo, vmo_size, InitialMutability::Mutable)
                .expect("failed to create VmObjectDispatcher");
        unittest::expect_true!(rights != 0);

        let disp = handle.dispatcher();
        let disp_vmo = disp.vmo();
        unittest::expect_eq!(disp_vmo.size(), page::SIZE as u64);
    }

    /// Tests getting and setting size and stream size.
    #[test]
    fn test_vm_object_dispatcher_size_and_stream_size() {
        let paged_vmo =
            VmObjectPaged::create(0, 0, page::SIZE as u64).expect("failed to create paged VMO");
        let (handle, _) =
            VmObjectDispatcher::create(&paged_vmo, page::SIZE as u64, InitialMutability::Mutable)
                .expect("failed to create VmObjectDispatcher");
        let disp = handle.dispatcher();

        let size = disp.get_size().expect("failed to get size");
        unittest::expect_eq!(size, page::SIZE as u64);

        let stream_size = disp.get_stream_size();
        unittest::expect_eq!(stream_size, page::SIZE as u64);

        disp.set_stream_size(100).expect("failed to set stream size");
        unittest::expect_eq!(disp.get_stream_size(), 100);
    }

    /// Tests parsing create syscall flags.
    #[test]
    fn test_vm_object_dispatcher_parse_create_syscall_flags() {
        let stats = VmObjectDispatcher::parse_create_syscall_flags(0, 100).expect("parse flags");
        unittest::expect_eq!(stats.flags, 0);
        unittest::expect_eq!(stats.size, page::SIZE as u64);

        let stats = VmObjectDispatcher::parse_create_syscall_flags(
            zx_types::ZX_VMO_RESIZABLE,
            page::SIZE as u64,
        )
        .expect("parse flags resizable");
        unittest::expect_true!((stats.flags & VmObjectPaged::RESIZABLE) != 0);
        unittest::expect_eq!(stats.size, page::SIZE as u64);

        let stats = VmObjectDispatcher::parse_create_syscall_flags(zx_types::ZX_VMO_UNBOUNDED, 0)
            .expect("parse flags unbounded");
        unittest::expect_eq!(stats.size, crate::vm::vm_object::VmObject::MAX_SIZE);
    }

    /// Tests child creation and child dispatcher creation.
    #[test]
    fn test_vm_object_dispatcher_create_child() {
        let paged_vmo =
            VmObjectPaged::create(0, 0, page::SIZE as u64).expect("failed to create paged VMO");
        let (handle, _) =
            VmObjectDispatcher::create(&paged_vmo, page::SIZE as u64, InitialMutability::Mutable)
                .expect("failed to create VmObjectDispatcher");
        let disp = handle.dispatcher();

        // Test snapshot child.
        let child_vmo = disp
            .create_child(zx_types::ZX_VMO_CHILD_SNAPSHOT, 0, page::SIZE as u64, true)
            .expect("failed to create child");
        unittest::expect_eq!(child_vmo.size(), page::SIZE as u64);

        let (child_handle, child_rights) = disp
            .create_child_dispatcher(
                &child_vmo,
                page::SIZE as u64,
                zx_types::ZX_VMO_CHILD_SNAPSHOT,
                InitialMutability::Mutable,
            )
            .expect("failed to create child dispatcher");
        unittest::expect_true!(child_rights != 0);
        unittest::expect_eq!(child_handle.dispatcher().vmo().size(), page::SIZE as u64);

        // Test reference child (sharing parent's StreamSizeManager).
        let ref_child_vmo = disp
            .create_child(zx_types::ZX_VMO_CHILD_REFERENCE, 0, 0, true)
            .expect("failed to create ref child");
        let (ref_child_handle, ref_child_rights) = disp
            .create_child_dispatcher(
                &ref_child_vmo,
                0,
                zx_types::ZX_VMO_CHILD_REFERENCE,
                InitialMutability::Mutable,
            )
            .expect("failed to create ref child dispatcher");
        unittest::expect_true!(ref_child_rights != 0);
        unittest::expect_eq!(ref_child_handle.dispatcher().vmo().size(), page::SIZE as u64);
    }
}
