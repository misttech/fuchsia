// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher::DispatcherOps;
use super::handle::KernelHandle;
use super::vm_object_dispatcher_ffi::{
    cpp_vm_object_dispatcher_as_child_observer, cpp_vm_object_dispatcher_create,
};
use crate::counters::define_kcounter;
use crate::user_copy::{UserInOutPtr, UserInPtr, UserOutPtr};
use crate::vm::arch_vm_aspace::ArchMmuFlags;
use crate::vm::stream_size_manager::{Operation as StreamSizeManagerOperation, StreamSizeManager};
use crate::vm::vm_object::{
    CacheOpType, ChildType, EvictionHint, Resizability, SnapshotType, VmObject,
    VmObjectReadWriteOptions, zx_vmo_lock_state_t,
};
use crate::vm::vm_object_paged::VmObjectPaged;
use boot_options::BootOptions;
use debug::ltracef;
use fbl::{Canary, RefPtr};
use ksync::{KMutex, LockToken, RawCriticalMutex, guarded};
use object_constants_rs as object_constants;
use page;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{
    ZX_DEFAULT_VMO_RIGHTS, ZX_INFO_VMO_CONTIGUOUS, ZX_INFO_VMO_DISCARDABLE, ZX_INFO_VMO_IMMUTABLE,
    ZX_INFO_VMO_IS_COW_CLONE, ZX_INFO_VMO_PAGER_BACKED, ZX_INFO_VMO_RESIZABLE,
    ZX_INFO_VMO_TYPE_PAGED, ZX_INFO_VMO_VIA_HANDLE, ZX_INFO_VMO_VIA_IOB_HANDLE,
    ZX_INFO_VMO_VIA_MAPPING, ZX_KOID_INVALID, ZX_MAX_NAME_LEN, ZX_OBJ_TYPE_VMO, ZX_RIGHT_READ,
    ZX_RIGHT_RESIZE, ZX_RIGHT_WRITE, ZX_VMO_CHILD_REFERENCE, ZX_VMO_CHILD_RESIZABLE,
    ZX_VMO_CHILD_SLICE, ZX_VMO_CHILD_SNAPSHOT, ZX_VMO_CHILD_SNAPSHOT_AT_LEAST_ON_WRITE,
    ZX_VMO_CHILD_SNAPSHOT_MODIFIED, ZX_VMO_DISCARDABLE, ZX_VMO_OP_ALWAYS_NEED,
    ZX_VMO_OP_CACHE_CLEAN, ZX_VMO_OP_CACHE_CLEAN_INVALIDATE, ZX_VMO_OP_CACHE_INVALIDATE,
    ZX_VMO_OP_CACHE_SYNC, ZX_VMO_OP_COMMIT, ZX_VMO_OP_DECOMMIT, ZX_VMO_OP_DONT_NEED,
    ZX_VMO_OP_LOCK, ZX_VMO_OP_PREFETCH, ZX_VMO_OP_TRY_LOCK, ZX_VMO_OP_UNLOCK, ZX_VMO_OP_ZERO,
    ZX_VMO_RESIZABLE, ZX_VMO_UNBOUNDED, ZX_VMO_ZERO_CHILDREN, zx_info_vmo_t, zx_koid_t,
    zx_rights_t,
};

const LOCAL_TRACE: u32 = 0;
const ZX_INFO_VMO_TYPE_PHYSICAL: u32 = 0;

define_kcounter!(DISPATCHER_VMO_CREATE_COUNT, "dispatcher.vmo.create", Sum);
define_kcounter!(DISPATCHER_VMO_DESTROY_COUNT, "dispatcher.vmo.destroy", Sum);

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

// LINT.IfChange(VmoOwnership)
/// Specifies how a VMO is owned when generating `zx_info_vmo_t`.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoOwnership {
    Handle = 0,
    Mapping = 1,
    IoBuffer = 2,
}
// LINT.ThenChange(//zircon/kernel/object/include/object/vm_object_dispatcher.h:VmoOwnership)

zr::static_assert!(core::mem::size_of::<VmoOwnership>() == 4);
zr::static_assert!(core::mem::align_of::<VmoOwnership>() == 4);

/// Internal state of a `VmObjectDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct VmObjectDispatcherState {
    canary: Canary<{ fbl::magic(b"VMOD") }>,
    // Indicates whether the VMO was immutable at creation time.
    initial_mutability: InitialMutability,
    // The immutability here is load bearing; we give a raw pointer to
    // ourselves to `vmo` so we have to ensure we don't reset `vmo`
    // except during destruction.
    vmo: RefPtr<VmObject>,
    // Manages the stream size associated with this VMO. The stream size is used by streams
    // created against this VMO. The stream size manager is lazily created, hence this field is
    // guarded by the lock, however once created it can be assumed to be constant.
    // Creating the stream size manager can be deferred as long as the stream is exactly the vmo
    // size, and there are no streams or other operations that implicitly require a stream size
    // manager to exist.
    #[guarded_by(lock)]
    stream_size_mgr: Option<RefPtr<StreamSizeManager>>,
    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

zr::static_assert_size_and_align!(
    VmObjectDispatcherState,
    object_constants::kVmObjectDispatcherStateSize,
    object_constants::kVmObjectDispatcherStateAlign,
);

impl VmObjectDispatcherState {
    /// Initializes a `VmObjectDispatcherState`.
    pub fn init(
        _dispatcher: *const VmObjectDispatcher,
        vmo: RefPtr<VmObject>,
        stream_size_mgr: Option<RefPtr<StreamSizeManager>>,
        initial_mutability: InitialMutability,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            canary: {
                DISPATCHER_VMO_CREATE_COUNT.add(1);
                Canary::new()
            },
            initial_mutability,
            vmo,
            stream_size_mgr: stream_size_mgr.into(),
            lock <- KMutex::init(),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for VmObjectDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        self.canary.assert();
        DISPATCHER_VMO_DESTROY_COUNT.add(1);
        // Intentionally leave `self.vmo.user_id()` set to our koid even though we're
        // dying and the koid will no longer map to a Dispatcher. koids are never
        // recycled, and it could be a useful breadcrumb.
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    pub struct VmObjectDispatcher,
    VmObjectDispatcherState,
    ZX_OBJ_TYPE_VMO,
    object_constants::kVmObjectDispatcherStateOffset
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateStats {
    pub flags: u32,
    pub size: u64,
}

/// Populates a `zx_info_vmo_t` entry for `vmo` with the specified `ownership` and `handle_rights`.
pub fn vmo_to_info_entry(
    vmo: &VmObject,
    ownership: VmoOwnership,
    handle_rights: zx_rights_t,
) -> zx_info_vmo_t {
    let mut entry = zx_info_vmo_t::default();
    entry.koid = vmo.user_id();
    vmo.get_name(&mut entry.name);
    entry.size_bytes = vmo.size();
    entry.parent_koid = vmo.parent_user_id();
    entry.num_children = vmo.num_children() as usize;
    entry.num_mappings = vmo.num_mappings() as usize;
    entry.share_count = vmo.share_count() as usize;
    entry.flags = (if vmo.is_paged() { ZX_INFO_VMO_TYPE_PAGED } else { ZX_INFO_VMO_TYPE_PHYSICAL })
        | (if vmo.is_resizable() { ZX_INFO_VMO_RESIZABLE } else { 0 })
        | (if vmo.is_discardable() { ZX_INFO_VMO_DISCARDABLE } else { 0 })
        | (if vmo.is_user_pager_backed() { ZX_INFO_VMO_PAGER_BACKED } else { 0 })
        | (if vmo.is_contiguous() { ZX_INFO_VMO_CONTIGUOUS } else { 0 });
    // As an implementation detail, both ends of an IOBuffer keep a child reference to a shared
    // parent which is dropped. Since references aren't normally attributed memory otherwise, we
    // specifically request their attribution counts.
    let counts = if ownership == VmoOwnership::IoBuffer {
        vmo.get_attributed_memory_in_reference_owner()
    } else {
        vmo.get_attributed_memory()
    };
    let total_scaled_bytes = counts.total_scaled_bytes();
    entry.committed_bytes = counts.uncompressed_bytes as u64;
    entry.populated_bytes = counts.total_bytes() as u64;
    entry.committed_private_bytes = counts.private_uncompressed_bytes as u64;
    entry.populated_private_bytes = counts.total_private_bytes() as u64;
    entry.committed_scaled_bytes = counts.scaled_uncompressed_bytes.integral as u64;
    entry.populated_scaled_bytes = total_scaled_bytes.integral as u64;
    entry.committed_fractional_scaled_bytes = counts.scaled_uncompressed_bytes.fractional;
    entry.populated_fractional_scaled_bytes = total_scaled_bytes.fractional;
    entry.cache_policy = vmo.get_mapping_cache_policy() as u32;
    match ownership {
        VmoOwnership::Handle => {
            entry.flags |= ZX_INFO_VMO_VIA_HANDLE;
            entry.handle_rights = handle_rights;
        }
        VmoOwnership::Mapping => {
            entry.flags |= ZX_INFO_VMO_VIA_MAPPING;
        }
        VmoOwnership::IoBuffer => {
            entry.flags |= ZX_INFO_VMO_VIA_IOB_HANDLE;
            entry.handle_rights = handle_rights;
        }
    }
    if vmo.child_type() == ChildType::kCowClone {
        entry.flags |= ZX_INFO_VMO_IS_COW_CLONE;
    }
    entry.metadata_bytes = vmo.heap_allocation_bytes() as u64;
    // Only events that change committed pages are different kinds of reclamation.
    entry.committed_change_events = vmo.reclamation_event_count();
    entry
}

impl VmObjectDispatcher {
    /// Returns the default handle rights for a `VmObjectDispatcher`.
    pub const fn default_rights() -> zx_rights_t {
        ZX_DEFAULT_VMO_RIGHTS
    }

    /// Creates a `VmObjectDispatcher` wrapping `vmo` and an optional `StreamSizeManager`.
    pub fn create_with_ssm(
        vmo: &VmObject,
        stream_size_manager: Option<RefPtr<StreamSizeManager>>,
        initial_mutability: InitialMutability,
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        let vmo_ref = RefPtr::from_ref(vmo);
        let raw_ssm = match stream_size_manager.clone() {
            Some(ssm) => RefPtr::into_raw(ssm).cast_mut(),
            None => core::ptr::null_mut(),
        };
        // SAFETY: `vmo_ref` and `raw_ssm` transfer ownership of their reference counts to
        // `cpp_vm_object_dispatcher_create`, which initializes `handle_out` on `ZX_OK`.
        let new_handle = unsafe {
            KernelHandle::create(|handle_out| {
                cpp_vm_object_dispatcher_create(
                    RefPtr::into_raw(vmo_ref).cast_mut(),
                    raw_ssm,
                    initial_mutability,
                    handle_out,
                )
            })
        }?;

        let disp = new_handle.dispatcher();
        // SAFETY: `cpp_vm_object_dispatcher_as_child_observer` adjusts `disp` to its
        // `VmObjectChildObserver` base subobject, which remains valid until cleared in
        // `on_zero_handles`.
        unsafe {
            let observer = cpp_vm_object_dispatcher_as_child_observer(disp);
            disp.vmo().set_child_observer(observer);
        }

        disp.vmo().set_user_stream_size(stream_size_manager);

        disp.vmo().set_user_id(disp.get_koid());
        let rights =
            Self::default_rights() | if disp.vmo().is_resizable() { ZX_RIGHT_RESIZE } else { 0 };
        Ok((new_handle, rights))
    }

    /// Creates a `VmObjectDispatcher` wrapping a VMO.
    pub fn create(
        vmo: &VmObject,
        stream_size: u64,
        initial_mutability: InitialMutability,
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        let mut ssm = None;
        // If the initial stream size we want to track is exactly equal to the current VMO size
        // then we can defer creating the stream size manager till later.
        let vmo_size = vmo.size();
        if stream_size != vmo_size && vmo.is_stream_compatible() {
            debug_assert!(stream_size <= vmo_size);
            ssm = Some(StreamSizeManager::create(stream_size)?);

            let aligned_stream_size =
                round_up_page_size(stream_size).ok_or(Status::OUT_OF_RANGE)?;
            // The stream_size cannot be larger than the VMO size, so this cannot overflow.
            debug_assert!(aligned_stream_size >= stream_size);
            if aligned_stream_size < vmo_size {
                // The range beyond the (rounded up) stream size to the VMO size is
                // dirty-untracked zero.
                vmo.zero_range_untracked(aligned_stream_size, vmo_size - aligned_stream_size)?;
            }
        }
        Self::create_with_ssm(vmo, ssm, initial_mutability)
    }

    /// Returns a reference to the underlying `VmObject`.
    pub fn vmo(&self) -> &RefPtr<VmObject> {
        &self.state().vmo
    }

    /// Returns the koid of the backing pager, or `ZX_KOID_INVALID` if none.
    pub fn pager_koid(&self) -> zx_koid_t {
        self.vmo().get_page_source_koid().unwrap_or(ZX_KOID_INVALID)
    }

    /// `VmObjectChildObserver` callback invoked when the VMO's child count reaches zero.
    pub fn on_zero_child(&self) {
        ksync::lock!(let guard = self.state().lock.lock());
        // Double check the number of children now that we are holding the dispatcher lock and so
        // are serialized with our calls to `create_child`. This allows us to atomically observe
        // that there are indeed zero children, and then set the signal. The double check is needed
        // since `on_zero_child` is called without the VMO lock held, and so there is a race where
        // before we could acquire the dispatcher lock a new child got created.
        if self.vmo().num_children() == 0 {
            self.update_state_locked(guard.token(), 0, ZX_VMO_ZERO_CHILDREN);
        }
    }

    /// Gets the name of the underlying VMO.
    pub fn get_name(&self, out_name: &mut [u8; ZX_MAX_NAME_LEN]) -> Result<(), Status> {
        self.state().canary.assert();
        self.vmo().get_name(out_name);
        Ok(())
    }

    /// Sets the name of the underlying VMO.
    pub fn set_name(&self, name: &[u8]) -> Result<(), Status> {
        self.state().canary.assert();
        self.vmo().set_name(name)
    }

    /// Invoked when the last handle to this dispatcher is closed.
    pub fn on_zero_handles(&self) {
        // Clear when handle count reaches zero rather in the destructor because we're retaining a
        // VmObject that might call back into `self` via VmObjectChildObserver when it's destroyed.
        // SAFETY: Passing null clears the child observer.
        unsafe {
            self.vmo().set_child_observer(core::ptr::null_mut());
        }
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
            res.size = VmObject::round_size(size)?;
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
        let mut info = vmo_to_info_entry(self.vmo(), VmoOwnership::Handle, rights);
        if self.state().initial_mutability == InitialMutability::Immutable {
            info.flags |= ZX_INFO_VMO_IMMUTABLE;
        }
        info
    }

    /// Reads data from this VMO into a user buffer.
    pub fn read(
        &self,
        user_data: UserOutPtr<u8>,
        offset: u64,
        length: usize,
    ) -> Result<(), Status> {
        self.state().canary.assert();

        self.vmo().read_user(user_data, offset, length, VmObjectReadWriteOptions::NONE).0
    }

    /// Writes data from a user buffer into this VMO.
    pub fn write(
        &self,
        user_data: UserInPtr<u8>,
        offset: u64,
        length: usize,
    ) -> Result<(), Status> {
        self.state().canary.assert();

        self.vmo().write_user(user_data, offset, length, VmObjectReadWriteOptions::NONE).0
    }

    /// Returns the size of the underlying VMO in bytes.
    pub fn get_size(&self) -> Result<u64, Status> {
        self.state().canary.assert();

        Ok(self.vmo().size())
    }

    /// Returns the number of bytes in the data stream stored within the VMO.
    ///
    /// This returns the property previously known as the content size.
    pub fn get_stream_size(&self) -> u64 {
        let state = self.state();
        state.canary.assert();

        // Stream size is always reported as 0 if the VMO doesn't support streams.
        if !state.vmo.is_stream_compatible() {
            return 0;
        }

        // Retrieving the stream size needs to be a non-fallible operation, so we avoid allocating
        // one if it doesn't exist, since the allocation could fail.
        let ssm: *const StreamSizeManager = {
            ksync::lock!(let guard = state.lock_lock());
            match guard.fields().stream_size_mgr {
                Some(ssm) => RefPtr::as_ptr(ssm),
                None => return state.vmo.size(),
            }
        };

        // SAFETY: `stream_size_mgr` is never cleared or replaced once initialized, so `ssm`
        // remains valid for the lifetime of `self`.
        unsafe { (*ssm).get_stream_size() }
    }

    /// Sets the size of the underlying VMO in bytes.
    pub fn set_size(&self, size: u64) -> Result<(), Status> {
        self.state().canary.assert();

        if !self.vmo().is_stream_compatible() {
            debug_assert!(!self.vmo().is_resizable());
            return Err(Status::UNAVAILABLE);
        }

        // TODO(https://fxbug.dev/341218975) SetSize should only change stream size to maintain
        // invariant that stream size isn't larger than VMO size.

        let ssm = self.stream_size_manager()?;

        self.set_size_with_ssm(&ssm, size)
    }

    /// Sets the stream size of the underlying VMO in bytes.
    pub fn set_stream_size(&self, stream_size: u64) -> Result<(), Status> {
        self.state().canary.assert();

        if !self.vmo().is_stream_compatible() {
            return Err(Status::NOT_SUPPORTED);
        }

        let ssm = self.stream_size_manager()?;

        self.set_stream_size_with_ssm(&ssm, stream_size)
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
        let state = self.state();
        state.canary.assert();

        ltracef!(
            "op {} offset {:#x} size {:#x} buffer {:p} buffer_size {} rights {:#x}\n",
            op,
            offset,
            size,
            buffer.as_ptr(),
            buffer_size,
            rights
        );

        let vmo = &state.vmo;
        match op {
            ZX_VMO_OP_COMMIT => {
                if (rights & ZX_RIGHT_WRITE) == 0 {
                    return Err(Status::ACCESS_DENIED);
                }
                // TODO: handle partial commits
                vmo.commit_range(offset, size)
            }
            ZX_VMO_OP_DECOMMIT => {
                if (rights & ZX_RIGHT_WRITE) == 0 {
                    return Err(Status::ACCESS_DENIED);
                }
                // TODO: handle partial decommits
                vmo.decommit_range(offset, size)
            }
            ZX_VMO_OP_LOCK => {
                if (rights & (ZX_RIGHT_READ | ZX_RIGHT_WRITE)) == 0 {
                    return Err(Status::ACCESS_DENIED);
                }

                let lock_state = vmo.lock_range(offset, size)?;
                // If an error is encountered from this point on, the lock operation MUST be
                // reverted before returning.

                if buffer_size < core::mem::size_of::<zx_vmo_lock_state_t>() {
                    // Undo the lock before returning an error.
                    let _ = vmo.unlock_range(offset, size);
                    return Err(Status::INVALID_ARGS);
                }

                let lock_state_out = buffer.reinterpret::<zx_vmo_lock_state_t>();
                if let Err(status) = lock_state_out.copy_to_user(&lock_state) {
                    // Undo the lock before returning an error.
                    let _ = vmo.unlock_range(offset, size);
                    return Err(status);
                }

                Ok(())
            }
            ZX_VMO_OP_TRY_LOCK => {
                if (rights & (ZX_RIGHT_READ | ZX_RIGHT_WRITE)) == 0 {
                    return Err(Status::ACCESS_DENIED);
                }
                vmo.try_lock_range(offset, size)
            }
            ZX_VMO_OP_UNLOCK => {
                if (rights & (ZX_RIGHT_READ | ZX_RIGHT_WRITE)) == 0 {
                    return Err(Status::ACCESS_DENIED);
                }
                vmo.unlock_range(offset, size)
            }
            ZX_VMO_OP_CACHE_SYNC => {
                if (rights & ZX_RIGHT_READ) == 0 {
                    return Err(Status::ACCESS_DENIED);
                }
                vmo.cache_op(offset, size, CacheOpType::Sync)
            }
            ZX_VMO_OP_CACHE_INVALIDATE => {
                if !BootOptions::get().enable_debugging_syscalls {
                    return Err(Status::NOT_SUPPORTED);
                }
                // A straight invalidate op requires the write right since
                // it may drop dirty cache lines, thus modifying the contents
                // of the VMO.
                if (rights & ZX_RIGHT_WRITE) == 0 {
                    return Err(Status::ACCESS_DENIED);
                }
                vmo.cache_op(offset, size, CacheOpType::Invalidate)
            }
            ZX_VMO_OP_CACHE_CLEAN => {
                if (rights & ZX_RIGHT_READ) == 0 {
                    return Err(Status::ACCESS_DENIED);
                }
                vmo.cache_op(offset, size, CacheOpType::Clean)
            }
            ZX_VMO_OP_CACHE_CLEAN_INVALIDATE => {
                if (rights & ZX_RIGHT_READ) == 0 {
                    return Err(Status::ACCESS_DENIED);
                }
                vmo.cache_op(offset, size, CacheOpType::CleanInvalidate)
            }
            ZX_VMO_OP_ZERO => {
                if (rights & ZX_RIGHT_WRITE) == 0 {
                    return Err(Status::ACCESS_DENIED);
                }
                vmo.zero_range(offset, size)
            }
            ZX_VMO_OP_ALWAYS_NEED => vmo.hint_range(offset, size, EvictionHint::AlwaysNeed),
            ZX_VMO_OP_DONT_NEED => vmo.hint_range(offset, size, EvictionHint::DontNeed),
            ZX_VMO_OP_PREFETCH => {
                if (rights & ZX_RIGHT_READ) == 0 {
                    return Err(Status::ACCESS_DENIED);
                }
                vmo.prefetch_range(offset, size)
            }
            _ => Err(Status::INVALID_ARGS),
        }
    }

    /// Sets the mapping cache policy for the underlying VMO.
    pub fn set_mapping_cache_policy(&self, cache_policy: u32) -> Result<(), Status> {
        let cache_policy =
            ArchMmuFlags::try_from(cache_policy).map_err(|_| Status::INVALID_ARGS)?;
        // SAFETY: `set_mapping_cache_policy` delegates validation to the underlying VMO.
        unsafe { self.vmo().set_mapping_cache_policy(cache_policy) }
    }

    /// Returns the `StreamSizeManager` associated with this VMO, lazily allocating one if needed.
    pub fn stream_size_manager(&self) -> Result<RefPtr<StreamSizeManager>, Status> {
        let state = self.state();
        ksync::lock!(let mut guard = state.lock_lock());
        let ssm_opt = guard.as_mut().fields_mut().stream_size_mgr;
        if let Some(ssm) = ssm_opt {
            return Ok(ssm.clone());
        }
        if !state.vmo.is_stream_compatible() {
            return Err(Status::NOT_SUPPORTED);
        }
        let ssm = StreamSizeManager::create(state.vmo.size())?;
        state.vmo.set_user_stream_size(Some(ssm.clone()));
        *ssm_opt = Some(ssm.clone());
        Ok(ssm)
    }

    /// Ensures that a `StreamSizeManager` is allocated and attached to the underlying VMO.
    pub fn ensure_stream_size_manager(&self) -> Result<(), Status> {
        self.stream_size_manager().map(|_| ())
    }

    fn create_child_internal(
        &self,
        mut options: u32,
        offset: u64,
        size: u64,
        copy_name: bool,
        _token: &LockToken<'_, VmObjectDispatcherStateLockClass>,
    ) -> Result<RefPtr<VmObject>, Status> {
        let vmo = self.vmo();
        // Clones are not supported for discardable VMOs.
        if vmo.is_discardable() {
            return Err(Status::NOT_SUPPORTED);
        }

        if (options & ZX_VMO_CHILD_SLICE) != 0 {
            // No other flags are valid for slices.
            options &= !ZX_VMO_CHILD_SLICE;
            if options != 0 {
                return Err(Status::INVALID_ARGS);
            }
            return vmo.create_child_slice(offset, size, copy_name);
        }

        let mut resizable = Resizability::NonResizable;
        if (options & ZX_VMO_CHILD_REFERENCE) != 0 {
            options &= !ZX_VMO_CHILD_REFERENCE;
            if (options & ZX_VMO_CHILD_RESIZABLE) != 0 {
                resizable = Resizability::Resizable;
                options &= !ZX_VMO_CHILD_RESIZABLE;
            }
            if options != 0 {
                return Err(Status::INVALID_ARGS);
            }
            let (child, _first_child) =
                vmo.create_child_reference(resizable, offset, size, copy_name)?;
            return Ok(child);
        }

        // Check for mutually-exclusive child type flags.
        let snapshot_type = if (options & ZX_VMO_CHILD_SNAPSHOT) != 0 {
            options &= !ZX_VMO_CHILD_SNAPSHOT;
            SnapshotType::Full
        } else if (options & ZX_VMO_CHILD_SNAPSHOT_AT_LEAST_ON_WRITE) != 0 {
            options &= !ZX_VMO_CHILD_SNAPSHOT_AT_LEAST_ON_WRITE;
            SnapshotType::OnWrite
        } else if (options & ZX_VMO_CHILD_SNAPSHOT_MODIFIED) != 0 {
            options &= !ZX_VMO_CHILD_SNAPSHOT_MODIFIED;
            SnapshotType::Modified
        } else {
            return Err(Status::INVALID_ARGS);
        };

        if (options & ZX_VMO_CHILD_RESIZABLE) != 0 {
            resizable = Resizability::Resizable;
            options &= !ZX_VMO_CHILD_RESIZABLE;
        }

        if options != 0 {
            return Err(Status::INVALID_ARGS);
        }

        vmo.create_clone(resizable, snapshot_type, offset, size, copy_name)
    }

    /// Creates a child VMO clone/slice/reference.
    pub fn create_child(
        &self,
        options: u32,
        offset: u64,
        size: u64,
        copy_name: bool,
    ) -> Result<RefPtr<VmObject>, Status> {
        let state = self.state();
        state.canary.assert();

        ltracef!("options {:#x} offset {:#x} size {:#x}\n", options, offset, size);

        // To synchronize the zero child signal the dispatcher lock is used over the create child
        // path to ensure we can atomically know that a child exists, and clear the zero child
        // signal.
        ksync::lock!(let guard = state.lock.lock());
        let child_vmo =
            self.create_child_internal(options, offset, size, copy_name, guard.token())?;
        // We know definitively that a child exists, as we have a refptr to it, and so we can clear
        // the zero children signal.
        self.update_state_locked(guard.token(), ZX_VMO_ZERO_CHILDREN, 0);
        Ok(child_vmo)
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
        if (options & ZX_VMO_CHILD_REFERENCE) != 0 && self.vmo().is_stream_compatible() {
            self.create_child_with_parent_stream_size(child_vmo, initial_mutability)
        } else {
            Self::create(child_vmo, size, initial_mutability)
        }
    }

    /// Creates a child `VmObjectDispatcher` sharing this dispatcher's `StreamSizeManager`.
    ///
    /// Reference children share their size and stream size with the parent VMO dispatcher.
    /// This function obtains the parent's `StreamSizeManager` (allocating one if not yet
    /// created) and attaches it to the child dispatcher via `VmObjectDispatcher::create_with_ssm`.
    pub fn create_child_with_parent_stream_size(
        &self,
        child_vmo: &VmObject,
        initial_mutability: InitialMutability,
    ) -> Result<(KernelHandle<VmObjectDispatcher>, zx_rights_t), Status> {
        let ssm = self.stream_size_manager()?;
        Self::create_with_ssm(child_vmo, Some(ssm), initial_mutability)
    }

    /// Sets the stream size of the underlying VMO, coordinating with `StreamSizeManager`.
    pub fn set_stream_size_with_ssm(
        &self,
        ssm: &StreamSizeManager,
        target_size: u64,
    ) -> Result<(), Status> {
        let Some(paged) = self.vmo().as_paged() else {
            return Err(Status::NOT_SUPPORTED);
        };

        pin_init::stack_pin_init!(let op = StreamSizeManagerOperation::init(ssm));
        ksync::lock!(let mut guard = ksync::aliased_lock(ssm.lock(), op.lock()));

        let vmo_size = paged.size();
        let old_stream_size = ssm.get_stream_size();

        if target_size == old_stream_size {
            return Ok(());
        }

        // Can't resize the stream beyond the VMO size.
        if target_size > vmo_size {
            return Err(Status::OUT_OF_RANGE);
        }

        ssm.begin_set_stream_size_locked(&mut guard.as_mut().inner_guard(), target_size, &op);

        // Zero the range from min(target size, old stream size) to the end of the VMO.
        let zero_start = core::cmp::min(target_size, old_stream_size);
        let Some(aligned_stream_size) = round_up_page_size(target_size) else {
            let (_, op_token) = guard.as_mut().tokens_mut();
            op.cancel_locked(op_token);
            return Err(Status::OUT_OF_RANGE);
        };
        debug_assert!(aligned_stream_size >= target_size);

        // Dropping the lock here is fine, as an Operation only needs to be locked when
        // initializing, committing, or cancelling.
        let res = guard.as_mut().call_unlocked(|| {
            paged.zero_range(zero_start, aligned_stream_size - zero_start)?;
            paged.zero_range_untracked(aligned_stream_size, vmo_size - aligned_stream_size)
        });

        if let Err(status) = res {
            let (_, op_token) = guard.as_mut().tokens_mut();
            op.cancel_locked(op_token);
            return Err(status);
        }

        // Ensure pages between min(target size, old stream size) and the end of the VMO are
        // unmapped before committing new stream size.
        let aligned_zero_start = round_down_page_size(zero_start);
        let (_, op_token) = guard.as_mut().tokens_mut();
        paged.unmap_pages_and_call(aligned_zero_start, vmo_size - aligned_zero_start, || {
            op.commit_locked(op_token);
        });

        Ok(())
    }

    /// Sets the size of the underlying VMO, coordinating with `StreamSizeManager`.
    pub fn set_size_with_ssm(&self, ssm: &StreamSizeManager, size: u64) -> Result<(), Status> {
        let Some(paged) = self.vmo().as_paged() else {
            return Err(Status::UNAVAILABLE);
        };

        pin_init::stack_pin_init!(let op = StreamSizeManagerOperation::init(ssm));
        ksync::lock!(let mut guard = ksync::aliased_lock(ssm.lock(), op.lock()));

        ssm.begin_set_stream_size_locked(&mut guard.as_mut().inner_guard(), size, &op);

        let size_aligned = match VmObject::round_size(size) {
            Ok(size_aligned) => size_aligned,
            Err(status) => {
                let (_, op_token) = guard.as_mut().tokens_mut();
                op.cancel_locked(op_token);
                return Err(status);
            }
        };

        if let Err(status) = paged.resize(size_aligned) {
            let (_, op_token) = guard.as_mut().tokens_mut();
            op.cancel_locked(op_token);
            return Err(status);
        }

        let remaining = size_aligned - size;
        if remaining > 0 {
            // TODO(https://fxbug.dev/42053728): Determine whether failure to ZeroRange here should
            // undo this operation.
            //
            // Dropping the lock here is fine, as an Operation only needs to be locked when
            // initializing, committing, or cancelling.
            let _ = guard.as_mut().call_unlocked(|| paged.zero_range(size, remaining));
        }

        let (_, op_token) = guard.as_mut().tokens_mut();
        op.commit_locked(op_token);
        Ok(())
    }
}

const fn round_up_page_size(val: u64) -> Option<u64> {
    const MASK: u64 = page::MASK as u64;
    match val.checked_add(MASK) {
        Some(v) => Some(v & !MASK),
        None => None,
    }
}

const fn round_down_page_size(val: u64) -> u64 {
    const MASK: u64 = page::MASK as u64;
    val & !MASK
}

/// Kernel unit tests for `VmObjectDispatcher`.
#[cfg(ktest)]
#[unittest::suite(name = "vm_object_dispatcher_tests")]
mod tests {
    use super::{InitialMutability, VmObjectDispatcher, VmoOwnership, vmo_to_info_entry};
    use crate::user_copy::UserInOutPtr;
    use crate::vm::stream_size_manager::StreamSizeManager;
    use crate::vm::vm_object_paged::VmObjectPaged;
    use zx_status::Status;
    use zx_types::{
        ZX_CACHE_POLICY_CACHED, ZX_CACHE_POLICY_UNCACHED, ZX_INFO_VMO_IMMUTABLE,
        ZX_INFO_VMO_IS_COW_CLONE, ZX_INFO_VMO_TYPE_PAGED, ZX_INFO_VMO_VIA_HANDLE,
        ZX_INFO_VMO_VIA_IOB_HANDLE, ZX_INFO_VMO_VIA_MAPPING, ZX_MAX_NAME_LEN, ZX_RIGHT_READ,
        ZX_RIGHT_WRITE, ZX_VMO_OP_COMMIT, ZX_VMO_OP_DECOMMIT, ZX_VMO_OP_ZERO,
    };

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
        let child_info = child_handle.dispatcher().get_vmo_info(child_rights);
        unittest::expect_true!((child_info.flags & ZX_INFO_VMO_IS_COW_CLONE) != 0);

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
        let ref_info = ref_child_handle.dispatcher().get_vmo_info(ref_child_rights);
        unittest::expect_true!((ref_info.flags & ZX_INFO_VMO_IS_COW_CLONE) == 0);

        let iob_info = vmo_to_info_entry(&ref_child_vmo, VmoOwnership::IoBuffer, ref_child_rights);
        unittest::expect_true!((iob_info.flags & ZX_INFO_VMO_VIA_IOB_HANDLE) != 0);
        unittest::expect_eq!(iob_info.handle_rights, ref_child_rights);
    }

    /// Tests set_size and set_stream_size on VmObjectDispatcher.
    #[test]
    fn test_vm_object_dispatcher_set_size_and_stream_size() {
        let paged_vmo = VmObjectPaged::create(0, VmObjectPaged::RESIZABLE, 8192)
            .expect("failed to create paged VMO");
        let ssm = StreamSizeManager::create(4096).expect("failed to create StreamSizeManager");
        paged_vmo.set_user_stream_size(ssm.clone());

        let (handle, _rights) =
            VmObjectDispatcher::create(&paged_vmo, 4096, InitialMutability::Mutable)
                .expect("failed to create VmObjectDispatcher");
        let disp = handle.dispatcher();

        unittest::assert_ok!(disp.set_stream_size_with_ssm(&ssm, 2048));
        unittest::expect_eq!(ssm.get_stream_size(), 2048);

        unittest::assert_ok!(disp.set_size_with_ssm(&ssm, 4096));
        unittest::expect_eq!(disp.vmo().size(), 4096);
    }

    /// Tests that resizing a VMO to u64::MAX returns OUT_OF_RANGE without panicking.
    #[test]
    fn test_vm_object_dispatcher_set_size_overflow() {
        let paged_vmo = VmObjectPaged::create(0, VmObjectPaged::RESIZABLE, 4096)
            .expect("failed to create paged VMO");
        let ssm = StreamSizeManager::create(4096).expect("failed to create StreamSizeManager");
        let (handle, _rights) =
            VmObjectDispatcher::create(&paged_vmo, 4096, InitialMutability::Mutable)
                .expect("failed to create VmObjectDispatcher");
        let disp = handle.dispatcher();

        let res = disp.set_size_with_ssm(&ssm, u64::MAX);
        unittest::expect_eq!(Status::result_into_raw(res), Status::OUT_OF_RANGE.into_raw());
    }

    /// Tests get_name, set_name, get_vmo_info, vmo_to_info_entry, and range_op.
    #[test]
    fn test_vm_object_dispatcher_info_name_and_range_op() {
        let paged_vmo =
            VmObjectPaged::create(0, 0, page::SIZE as u64).expect("failed to create paged VMO");
        let (handle, rights) =
            VmObjectDispatcher::create(&paged_vmo, page::SIZE as u64, InitialMutability::Immutable)
                .expect("failed to create VmObjectDispatcher");
        let disp = handle.dispatcher();

        unittest::assert_ok!(disp.set_name(b"test-vmo"));
        let mut name_buf = [0u8; ZX_MAX_NAME_LEN];
        unittest::assert_ok!(disp.get_name(&mut name_buf));
        unittest::expect_true!(&name_buf[..8] == b"test-vmo");

        let info = disp.get_vmo_info(rights);
        unittest::expect_eq!(info.koid, disp.get_koid());
        unittest::expect_eq!(info.size_bytes, page::SIZE as u64);
        unittest::expect_true!((info.flags & ZX_INFO_VMO_TYPE_PAGED) != 0);
        unittest::expect_true!((info.flags & ZX_INFO_VMO_VIA_HANDLE) != 0);
        unittest::expect_true!((info.flags & ZX_INFO_VMO_IMMUTABLE) != 0);
        unittest::expect_eq!(info.cache_policy, ZX_CACHE_POLICY_CACHED);

        let mapping_info = vmo_to_info_entry(disp.vmo(), VmoOwnership::Mapping, 0);
        unittest::expect_true!((mapping_info.flags & ZX_INFO_VMO_VIA_MAPPING) != 0);
        unittest::expect_eq!(mapping_info.handle_rights, 0);

        // Verify set_mapping_cache_policy rejects invalid flags and updates cache_policy on valid
        // flags.
        let bad_policy = disp.set_mapping_cache_policy(u32::MAX);
        unittest::expect_eq!(Status::result_into_raw(bad_policy), Status::INVALID_ARGS.into_raw());
        unittest::assert_ok!(disp.set_mapping_cache_policy(ZX_CACHE_POLICY_UNCACHED));
        let updated_info = disp.get_vmo_info(rights);
        unittest::expect_eq!(updated_info.cache_policy, ZX_CACHE_POLICY_UNCACHED);
        unittest::assert_ok!(disp.set_mapping_cache_policy(ZX_CACHE_POLICY_CACHED));

        // Commit, zero, and decommit via range_op.
        let null_buf = UserInOutPtr::new(core::ptr::null_mut());
        unittest::assert_ok!(disp.range_op(
            ZX_VMO_OP_COMMIT,
            0,
            page::SIZE as u64,
            null_buf,
            0,
            ZX_RIGHT_WRITE,
        ));
        unittest::assert_ok!(disp.range_op(
            ZX_VMO_OP_ZERO,
            0,
            page::SIZE as u64,
            null_buf,
            0,
            ZX_RIGHT_WRITE,
        ));
        unittest::assert_ok!(disp.range_op(
            ZX_VMO_OP_DECOMMIT,
            0,
            page::SIZE as u64,
            null_buf,
            0,
            ZX_RIGHT_WRITE,
        ));

        // Verify missing write right fails with ACCESS_DENIED.
        let res = disp.range_op(ZX_VMO_OP_COMMIT, 0, page::SIZE as u64, null_buf, 0, ZX_RIGHT_READ);
        unittest::expect_eq!(Status::result_into_raw(res), Status::ACCESS_DENIED.into_raw());
    }
}
