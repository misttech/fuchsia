// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::ArchMmuFlags;
use super::vm_aspace::{
    VmAspace, VmAspaceLockClass, VmAspaceMutex, VmAspaceRegionLockClass, VmAspaceRegionMutex,
};
use super::vm_mapping::VmMapping;
use super::vm_object::{VmObject, VmObjectLockClass};
use crate::kernel::types::VAddr;
use crate::user_copy::UserInOutPtr;
use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char};
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::ptr::NonNull;
use fbl::{Canary, HasRefCount, Recyclable, RefCounted, RefPtr};
use ksync::{KCell, LockToken};
use vm_address_region_bindings as bindings;
use vm_constants_rs as constants;
use zr::Opaque;
use zx_status::Status;
use zx_types::ZX_MAX_NAME_LEN;

fn name_to_c_buf(name: &[u8]) -> [u8; ZX_MAX_NAME_LEN] {
    let mut buf = [0u8; ZX_MAX_NAME_LEN];
    let len = name.len().min(buf.len() - 1);
    buf[..len].copy_from_slice(&name[..len]);
    buf
}

pub mod flag {
    use vm_address_region_bindings as bindings;

    /// When randomly allocating subregions, reduce sprawl by placing allocations near each other.
    pub const COMPACT: u32 = bindings::VMAR_FLAG_COMPACT;
    /// Request that the new region be at the specified offset in its parent region.
    pub const SPECIFIC: u32 = bindings::VMAR_FLAG_SPECIFIC;
    /// Like `VMAR_FLAG_SPECIFIC`, but permits overwriting existing mappings.
    pub const SPECIFIC_OVERWRITE: u32 = bindings::VMAR_FLAG_SPECIFIC_OVERWRITE;
    /// Allow `VmMappings` to be created inside the new region with the `SPECIFIC` or
    /// `OFFSET_IS_UPPER_LIMIT` flag.
    pub const CAN_MAP_SPECIFIC: u32 = bindings::VMAR_FLAG_CAN_MAP_SPECIFIC;
    /// Allow `VmMappings` to be created inside the region with read permissions.
    pub const CAN_MAP_READ: u32 = bindings::VMAR_FLAG_CAN_MAP_READ;
    /// Allow `VmMappings` to be created inside the region with write permissions.
    pub const CAN_MAP_WRITE: u32 = bindings::VMAR_FLAG_CAN_MAP_WRITE;
    /// Allow `VmMappings` to be created inside the region with execute permissions.
    pub const CAN_MAP_EXECUTE: u32 = bindings::VMAR_FLAG_CAN_MAP_EXECUTE;
    /// Require that VMO backing the mapping is non-resizable.
    pub const REQUIRE_NON_RESIZABLE: u32 = bindings::VMAR_FLAG_REQUIRE_NON_RESIZABLE;
    /// Allow VMO backings that could result in faults.
    pub const ALLOW_FAULTS: u32 = bindings::VMAR_FLAG_ALLOW_FAULTS;
    /// Treat the offset as an upper limit when allocating a VMO or child VMAR.
    pub const OFFSET_IS_UPPER_LIMIT: u32 = bindings::VMAR_FLAG_OFFSET_IS_UPPER_LIMIT;
    /// Opt this VMAR out of certain debugging checks. This allows for kernel mappings that have a
    /// more dynamic management strategy, that the regular checks would otherwise spuriously trip
    /// on.
    pub const DEBUG_DYNAMIC_KERNEL_MAPPING: u32 = bindings::VMAR_FLAG_DEBUG_DYNAMIC_KERNEL_MAPPING;
    /// Memory accesses past the stream size rounded up to the page boundary will fault.
    pub const FAULT_BEYOND_STREAM_SIZE: u32 = bindings::VMAR_FLAG_FAULT_BEYOND_STREAM_SIZE;

    /// Mask of read, write, and execute permission flags.
    pub const CAN_RWX_FLAGS: u32 = bindings::VMAR_CAN_RWX_FLAGS;
}

/// Memory priorities that can be applied to VMARs and mappings to propagate to VMOs and page
/// tables.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPriority {
    /// Default overcommit priority where reclamation is allowed.
    Default = 0,
    /// High priority prevents all reclamation.
    High = 1,
}

zr::static_assert!(
    core::mem::size_of::<MemoryPriority>()
        == core::mem::size_of::<bindings::VmAddressRegionOrMapping_MemoryPriority>()
);
zr::static_assert!(
    MemoryPriority::Default as u8
        == bindings::VmAddressRegionOrMapping_MemoryPriority::DEFAULT as u8
);
zr::static_assert!(
    MemoryPriority::High as u8 == bindings::VmAddressRegionOrMapping_MemoryPriority::HIGH as u8
);

impl From<MemoryPriority> for bindings::VmAddressRegionOrMapping_MemoryPriority {
    fn from(priority: MemoryPriority) -> Self {
        match priority {
            MemoryPriority::Default => Self::DEFAULT,
            MemoryPriority::High => Self::HIGH,
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifeCycleState {
    /// Initial state: not yet in the address space layout, so no `destroy` is needed.
    NotReady = 0,
    /// Usual state: information is representative of the address space layout. Must be destroyed
    /// before the last reference is dropped.
    Alive = 1,
    /// Object is invalid.
    Dead = 2,
}

zr::static_assert!(
    core::mem::size_of::<LifeCycleState>()
        == core::mem::size_of::<bindings::VmAddressRegionOrMapping_LifeCycleState>()
);
zr::static_assert!(
    LifeCycleState::NotReady as u8
        == bindings::VmAddressRegionOrMapping_LifeCycleState::NOT_READY as u8
);
zr::static_assert!(
    LifeCycleState::Alive as u8 == bindings::VmAddressRegionOrMapping_LifeCycleState::ALIVE as u8
);
zr::static_assert!(
    LifeCycleState::Dead as u8 == bindings::VmAddressRegionOrMapping_LifeCycleState::DEAD as u8
);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmAddressRegionOpChildren {
    Yes = 0,
    No = 1,
}

zr::static_assert!(
    core::mem::size_of::<VmAddressRegionOpChildren>()
        == core::mem::size_of::<bindings::VmAddressRegionOpChildren>()
);
zr::static_assert!(
    VmAddressRegionOpChildren::Yes as u8 == bindings::VmAddressRegionOpChildren::Yes as u8
);
zr::static_assert!(
    VmAddressRegionOpChildren::No as u8 == bindings::VmAddressRegionOpChildren::No as u8
);

impl From<VmAddressRegionOpChildren> for bindings::VmAddressRegionOpChildren {
    fn from(op_children: VmAddressRegionOpChildren) -> Self {
        match op_children {
            VmAddressRegionOpChildren::Yes => Self::Yes,
            VmAddressRegionOpChildren::No => Self::No,
        }
    }
}
pub type RangeOpType = bindings::VmAddressRegion_RangeOpType;
pub type UnmapOptions = bindings::VmMapping_UnmapOptions;

/// Internal fully locked version of Destroy. Has controls to both skip the arch aspace unmapping
/// as well as removal from the parent subregions list. These controls facilitate the fine grained
/// control needed when splitting, merging and replacing mappings.
///
/// If unmap is `No` then this method is defined to never fail. `remove_region` does not impact
/// success or failure of the operation.
pub type DestroyUnmap = bindings::VmMapping_DestroyUnmap;
pub type DestroyRemoveFromParent = bindings::VmMapping_DestroyRemoveFromParent;

/// Fully locked version of Activate that can additionally control whether the region is installed
/// into the parent subregion list and vmo mapping list or not. This control exists to facilitate
/// the fine grained control needed for splitting, merging and replacing of mappings as when set to
/// `No` this method is defined as never failing.
pub type ActivateInsertRegions = bindings::VmMapping_ActivateInsertRegions;

/// Result of calling [`VmAddressRegion::create_vm_mapping`].
pub struct MapResult {
    /// The newly created mapping.
    pub mapping: RefPtr<VmMapping>,
    /// The virtual address of the mapping at creation time.
    pub base: usize,
}

/// A `VmAddressRegion` represents a contiguous region of the virtual address
/// space.  It is partitioned by non-overlapping children of the following types:
/// 1) child `VmAddressRegion`
/// 2) child `VmMapping` (leafs that map `VmObject`s into the address space)
/// 3) gaps (logical, not actually objects).
///
/// `VmAddressRegionOrMapping` represents a tagged union of the two types.
///
/// A `VmAddressRegion`/`VmMapping` may be in one of two states: `Alive` or `Dead`.  If
/// it is `Alive`, then the `VmAddressRegion` is a description of the virtual memory
/// mappings of the address range it represents in its parent `VmAspace`.  If it is
/// `Dead`, then the `VmAddressRegion` is invalid and has no meaning.
///
/// All `VmAddressRegion` and `VmMapping` state is protected by the aspace lock.
#[repr(transparent)]
pub struct VmAddressRegionOrMapping {
    raw: Opaque<bindings::VmAddressRegionOrMapping>,
}

#[repr(C)]
pub struct VmAddressRegionOrMappingState {
    ref_count: RefCounted,
    canary: Canary<{ fbl::magic(b"VMRM") }>,
    is_mapping: bool,
    /// current state of the VMAR. If `LifeCycleState::Dead`, then all other
    /// fields are invalid.
    life_cycle: UnsafeCell<LifeCycleState>,
    /// Priority of the VMAR. This starts at `Default` and must be reset back to `Default` as part
    /// of the destroy path to ensure any propagation is undone correctly.
    memory_priority: KCell<MemoryPriority, VmAspaceLockClass>,
    /// flags from VMAR creation time
    flags: u32,
    /// address/size within the container address space
    base: VAddr,
    size: usize,
    /// pointer back to our member address space. The aspace's lock is used
    /// to serialize all modifications.
    aspace: RefPtr<VmAspace>,
    /// pointer back to our parent region (null if root or destroyed)
    parent: KCell<*mut VmAddressRegion, VmAspaceLockClass>,
}

::zr::static_assert!(cfg!(debug_assertions) == constants::kRefCountedAdoptionValidator);

::zr::static_assert_size_and_align!(
    VmAddressRegionOrMappingState,
    constants::kVmAddressRegionOrMappingStateSize,
    constants::kVmAddressRegionOrMappingStateAlign,
);

impl VmAddressRegionOrMappingState {
    fn new(
        base: usize,
        size: usize,
        flags: u32,
        aspace: &VmAspace,
        parent: *mut VmAddressRegion,
        is_mapping: bool,
        life_cycle: LifeCycleState,
    ) -> Self {
        Self {
            ref_count: RefCounted::new(),
            canary: Canary::new(),
            is_mapping,
            life_cycle: UnsafeCell::new(life_cycle),
            memory_priority: KCell::new(MemoryPriority::Default),
            flags,
            base: VAddr(base),
            size,
            aspace: RefPtr::from_ref(aspace),
            parent: KCell::new(parent),
        }
    }
}

impl HasRefCount for VmAddressRegionOrMapping {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        &self.state().ref_count
    }
}

// SAFETY: `recycle` releases the allocation exactly once when the last reference is dropped.
unsafe impl Recyclable for VmAddressRegionOrMapping {
    #[inline]
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: `ptr` is the last reference to an allocated region or mapping.
        unsafe { Self::free(ptr) }
    }
}

// SAFETY: `VmAddressRegionOrMapping` is not tied to the thread that created it.
unsafe impl Send for VmAddressRegionOrMapping {}
// SAFETY: Mutable state of `VmAddressRegionOrMapping` is only accessed with the aspace lock or the
// region lock held.
unsafe impl Sync for VmAddressRegionOrMapping {}

impl VmAddressRegionOrMapping {
    #[inline]
    fn state(&self) -> &VmAddressRegionOrMappingState {
        // SAFETY: `self` points to a live `VmAddressRegionOrMapping`.
        unsafe { &*bindings::cpp_vm_address_region_or_mapping_state(self.raw.get()).cast() }
    }

    // Accessors.
    #[inline]
    pub fn base(&self) -> VAddr {
        self.state().base
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.state().size
    }

    #[inline]
    pub fn flags(&self) -> u32 {
        self.state().flags
    }

    #[inline]
    pub fn aspace(&self) -> &RefPtr<VmAspace> {
        &self.state().aspace
    }

    /// Subtype information and safe down-casting.
    #[inline]
    pub fn is_mapping(&self) -> bool {
        self.state().is_mapping
    }

    /// Expose our backing lock for annotation purposes.
    #[inline]
    pub fn lock(&self) -> &VmAspaceMutex {
        self.state().aspace.lock()
    }

    /// Expose our backing lock for annotation purposes.
    #[inline]
    pub fn region_lock(&self) -> &VmAspaceRegionMutex {
        self.state().aspace.region_lock()
    }

    #[inline]
    fn state_locked(&self, _token: &LockToken<'_, VmAspaceLockClass>) -> LifeCycleState {
        // SAFETY: The aspace lock is held.
        unsafe { *self.state().life_cycle.get() }
    }

    #[inline]
    fn state_locked_region(
        &self,
        _token: &LockToken<'_, VmAspaceRegionLockClass>,
    ) -> LifeCycleState {
        // SAFETY: The region lock is held.
        unsafe { *self.state().life_cycle.get() }
    }

    /// For a `VmMapping` the state is only modified either with the `object` lock held, or if
    /// there is no `object`. Therefore it is safe to read the state if just the object lock is
    /// held.
    #[inline]
    fn state_locked_object(&self, _token: &LockToken<'_, VmObjectLockClass>) -> LifeCycleState {
        // SAFETY: The object lock is held.
        unsafe { *self.state().life_cycle.get() }
    }

    #[inline]
    fn set_state_locked(
        &self,
        _region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        _aspace_token: &LockToken<'_, VmAspaceLockClass>,
        state: LifeCycleState,
    ) {
        // SAFETY: Both guarding locks are held.
        unsafe {
            *self.state().life_cycle.get() = state;
        }
    }

    #[inline]
    fn memory_priority_locked(&self, token: &LockToken<'_, VmAspaceLockClass>) -> MemoryPriority {
        // SAFETY: The aspace lock is held.
        unsafe { *self.state().memory_priority.get(token) }
    }

    #[inline]
    fn set_memory_priority_field_locked(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        priority: MemoryPriority,
    ) {
        // SAFETY: The aspace lock is held.
        *unsafe { self.state().memory_priority.get_mut(token) } = priority;
    }

    #[inline]
    fn set_parent_locked(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        parent: *mut VmAddressRegion,
    ) {
        // SAFETY: The aspace lock is held.
        *unsafe { self.state().parent.get_mut(token) } = parent;
    }

    /// # Safety
    ///
    /// `ptr` must point to a live region or mapping to which no reference remains.
    #[inline]
    unsafe fn free(ptr: NonNull<Self>) {
        // SAFETY: `ptr` points to a live region or mapping.
        if unsafe { ptr.as_ref() }.is_mapping() {
            // SAFETY: `ptr` points to a live `VmMapping` to which no reference remains.
            unsafe { bindings::cpp_vm_mapping_free(ptr.as_ptr().cast()) }
        } else {
            // SAFETY: `ptr` points to a live `VmAddressRegion` to which no reference remains.
            unsafe { bindings::cpp_vm_address_region_free(ptr.as_ptr().cast()) }
        }
    }
}

/// A representation of a contiguous range of virtual address space
#[repr(transparent)]
pub struct VmAddressRegion {
    raw: Opaque<bindings::VmAddressRegion>,
}

#[repr(C)]
pub struct VmAddressRegionState {
    canary: Canary<{ fbl::magic(b"VMAR") }>,
    name: [u8; ZX_MAX_NAME_LEN],
}

::zr::static_assert_size_and_align!(
    VmAddressRegionState,
    constants::kVmAddressRegionStateSize,
    constants::kVmAddressRegionStateAlign,
);

impl VmAddressRegionState {
    fn new(name: &CStr) -> Self {
        Self { canary: Canary::new(), name: name_to_c_buf(name.to_bytes()) }
    }
}

impl HasRefCount for VmAddressRegion {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        (**self).ref_count()
    }
}

// SAFETY: `recycle` releases the allocation exactly once when the last reference is dropped.
unsafe impl Recyclable for VmAddressRegion {
    #[inline]
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: `ptr` points to a live `VmAddressRegion` to which no reference remains.
        unsafe { bindings::cpp_vm_address_region_free(ptr.as_ptr().cast()) }
    }
}

impl Deref for VmAddressRegion {
    type Target = VmAddressRegionOrMapping;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `VmAddressRegionOrMapping` is the first base of `VmAddressRegion`.
        unsafe { &*core::ptr::from_ref(self).cast() }
    }
}

// SAFETY: `VmAddressRegion` is not tied to the thread that created it.
unsafe impl Send for VmAddressRegion {}
// SAFETY: Mutable state of `VmAddressRegion` is only accessed with the aspace lock held.
unsafe impl Sync for VmAddressRegion {}

impl VmAddressRegion {
    fn as_ffi_ptr(&self) -> *mut bindings::VmAddressRegion {
        self.raw.get()
    }

    fn state(&self) -> &VmAddressRegionState {
        // SAFETY: `self` points to a live `VmAddressRegion`, whose state is initialized.
        unsafe { &*bindings::cpp_vm_address_region_state(self.as_ffi_ptr()).cast() }
    }

    /// Creates a subregion of this region.
    pub fn create_sub_vmar(
        &self,
        offset: usize,
        size: usize,
        align_pow2: u8,
        vmar_flags: u32,
        name: &[u8],
    ) -> Result<RefPtr<VmAddressRegion>, Status> {
        let name_buf = name_to_c_buf(name);
        let mut status = 0;
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        let raw = unsafe {
            bindings::cpp_vm_address_region_create_sub_vmar(
                self.as_ffi_ptr(),
                offset,
                size,
                align_pow2,
                vmar_flags,
                name_buf.as_ptr().cast(),
                &mut status,
            )
        };
        Status::ok(status)?;
        // SAFETY: `raw` is null or an owned reference to a live `VmAddressRegion`.
        unsafe { RefPtr::try_from_raw(raw.cast()).ok_or(Status::NO_MEMORY) }
    }

    /// Creates a [`VmMapping`] within this region.
    ///
    /// To avoid leaks, this should be paired with a call to [`VmMapping::destroy`] if desired;
    /// dropping `MapResult::mapping` will not destroy the mapping.
    #[allow(clippy::too_many_arguments)]
    pub fn create_vm_mapping(
        &self,
        mapping_offset: usize,
        size: usize,
        align_pow2: u8,
        vmar_flags: u32,
        vmo: RefPtr<VmObject>,
        vmo_offset: u64,
        arch_mmu_flags: ArchMmuFlags,
        name: &[u8],
    ) -> Result<MapResult, Status> {
        let name_buf = name_to_c_buf(name);
        let mut base = 0;
        let mut status = 0;
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        let raw = unsafe {
            bindings::cpp_vm_address_region_create_vm_mapping(
                self.as_ffi_ptr(),
                mapping_offset,
                size,
                align_pow2,
                vmar_flags,
                RefPtr::into_raw(vmo).cast(),
                vmo_offset,
                arch_mmu_flags,
                name_buf.as_ptr().cast(),
                &mut base,
                &mut status,
            )
        };
        Status::ok(status)?;
        // SAFETY: `raw` is null or an owned reference to a live `VmMapping`.
        let mapping = unsafe { RefPtr::try_from_raw(raw.cast()).ok_or(Status::NO_MEMORY)? };
        Ok(MapResult { mapping, base })
    }

    /// Destroys this region and recursively destroys child VMARs.
    pub fn destroy(&self) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        Status::ok(unsafe { bindings::cpp_vm_address_region_destroy(self.as_ffi_ptr()) })
    }

    /// Returns the name of this region.
    pub fn name(&self) -> &CStr {
        CStr::from_bytes_until_nul(&self.state().name).expect("name is null-terminated")
    }

    /// Returns true if this region has a parent region.
    pub fn has_parent(&self) -> bool {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        unsafe { bindings::cpp_vm_address_region_has_parent(self.as_ffi_ptr()) }
    }

    /// Applies the given memory priority to this region and all subregions.
    pub fn set_memory_priority(&self, priority: MemoryPriority) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        Status::ok(unsafe {
            bindings::cpp_vm_address_region_set_memory_priority(self.as_ffi_ptr(), priority.into())
        })
    }

    /// Unmaps a subset of the region of memory in the containing address space.
    ///
    /// # Safety
    ///
    /// Caller must ensure the specified virtual address region to unmap is no longer used.
    pub unsafe fn unmap(
        &self,
        base: VAddr,
        size: usize,
        op_children: VmAddressRegionOpChildren,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        Status::ok(unsafe {
            bindings::cpp_vm_address_region_unmap(
                self.as_ffi_ptr(),
                base.0,
                size,
                op_children.into(),
            )
        })
    }

    /// Changes protections on a subset of the region of memory in the containing address space.
    pub fn protect(
        &self,
        base: VAddr,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
        op_children: VmAddressRegionOpChildren,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        Status::ok(unsafe {
            bindings::cpp_vm_address_region_protect(
                self.as_ffi_ptr(),
                base.0,
                size,
                new_arch_mmu_flags,
                op_children.into(),
            )
        })
    }

    /// Performs a VMO or mapping operation (`op`) across mappings in `[base, base + len)`.
    pub fn range_op(
        &self,
        op: RangeOpType,
        base: VAddr,
        len: usize,
        op_children: VmAddressRegionOpChildren,
        buffer: UserInOutPtr<u8>,
        buffer_size: usize,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        Status::ok(unsafe {
            bindings::cpp_vm_address_region_range_op(
                self.as_ffi_ptr(),
                op,
                base.0,
                len,
                op_children.into(),
                buffer.as_ptr().cast(),
                buffer_size,
            )
        })
    }

    /// Reserves a memory region within this VMAR without allocating physical pages.
    pub fn reserve_space(
        &self,
        name: &[u8],
        base: usize,
        size: usize,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        let name_buf = name_to_c_buf(name);
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAddressRegion`.
        Status::ok(unsafe {
            bindings::cpp_vm_address_region_reserve_space(
                self.as_ffi_ptr(),
                name_buf.as_ptr().cast(),
                base,
                size,
                arch_mmu_flags,
            )
        })
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_state_init(
    storage: &mut MaybeUninit<VmAddressRegionOrMappingState>,
    base: usize,
    size: usize,
    flags: u32,
    aspace: &VmAspace,
    parent: *mut VmAddressRegion,
    is_mapping: bool,
) {
    storage.write(VmAddressRegionOrMappingState::new(
        base,
        size,
        flags,
        aspace,
        parent,
        is_mapping,
        LifeCycleState::NotReady,
    ));
}

/// # Safety
///
/// `storage` is initialized and is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_destruct(
    storage: &mut MaybeUninit<VmAddressRegionOrMappingState>,
) {
    // SAFETY: `storage` is initialized and valid for drop.
    unsafe { storage.assume_init_drop() }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_base(this: &VmAddressRegionOrMapping) -> usize {
    this.base().0
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_size(this: &VmAddressRegionOrMapping) -> usize {
    this.size()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_flags(this: &VmAddressRegionOrMapping) -> u32 {
    this.flags()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_aspace(
    this: &VmAddressRegionOrMapping,
) -> &RefPtr<VmAspace> {
    this.aspace()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_is_mapping(
    this: &VmAddressRegionOrMapping,
) -> bool {
    this.is_mapping()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_ref_counted(
    this: &VmAddressRegionOrMapping,
) -> *const RefCounted {
    this.ref_count()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_or_mapping_assert_canary(this: &VmAddressRegionOrMapping) {
    this.state().canary.assert();
}

/// # Safety
///
/// The aspace lock is held, or no other reference to `this` exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_state_locked(
    this: &VmAddressRegionOrMapping,
) -> LifeCycleState {
    // SAFETY: The aspace lock is held by the caller, or no other reference to `this` exists.
    let token = unsafe { LockToken::new() };
    this.state_locked(&token)
}

/// # Safety
///
/// `this` is the base of a `VmMapping` whose object lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_state_locked_object(
    this: &VmAddressRegionOrMapping,
) -> LifeCycleState {
    // SAFETY: The object lock is held by the caller.
    let token = unsafe { LockToken::new() };
    this.state_locked_object(&token)
}

/// # Safety
///
/// The region lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_state_locked_region(
    this: &VmAddressRegionOrMapping,
) -> LifeCycleState {
    // SAFETY: The region lock is held by the caller.
    let token = unsafe { LockToken::new() };
    this.state_locked_region(&token)
}

/// # Safety
///
/// The region lock and the aspace lock are held, or no other reference to `this` exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_set_state_locked(
    this: &VmAddressRegionOrMapping,
    state: LifeCycleState,
) {
    // SAFETY: The region lock is held by the caller, or no other reference to `this` exists.
    let region_token = unsafe { LockToken::new() };
    // SAFETY: The aspace lock is held by the caller, or no other reference to `this` exists.
    let aspace_token = unsafe { LockToken::new() };
    this.set_state_locked(&region_token, &aspace_token, state);
}

/// # Safety
///
/// The aspace lock is held, or no other reference to `this` exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_memory_priority_locked(
    this: &VmAddressRegionOrMapping,
) -> MemoryPriority {
    // SAFETY: The aspace lock is held by the caller, or no other reference to `this` exists.
    let token = unsafe { LockToken::new() };
    this.memory_priority_locked(&token)
}

/// # Safety
///
/// The aspace lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_set_memory_priority_locked(
    this: &VmAddressRegionOrMapping,
    priority: MemoryPriority,
) {
    // SAFETY: The aspace lock is held by the caller.
    let mut token = unsafe { LockToken::new() };
    this.set_memory_priority_field_locked(&mut token, priority);
}

/// # Safety
///
/// The aspace lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_parent_locked(
    this: &VmAddressRegionOrMapping,
) -> *mut VmAddressRegion {
    // SAFETY: The aspace lock is held by the caller.
    unsafe { *this.state().parent.get(&LockToken::new()) }
}

/// # Safety
///
/// The aspace lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_or_mapping_set_parent_locked(
    this: &VmAddressRegionOrMapping,
    parent: *mut VmAddressRegion,
) {
    // SAFETY: The aspace lock is held by the caller.
    let mut token = unsafe { LockToken::new() };
    this.set_parent_locked(&mut token, parent);
}

/// # Safety
///
/// `name` points to a null-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_state_init(
    storage: &mut MaybeUninit<VmAddressRegionState>,
    name: *const c_char,
) {
    // SAFETY: `name` points to a null-terminated string.
    let name = unsafe { CStr::from_ptr(name) };
    storage.write(VmAddressRegionState::new(name));
}

/// # Safety
///
/// `storage` is initialized and is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_destruct(
    storage: &mut MaybeUninit<VmAddressRegionState>,
) {
    // SAFETY: `storage` is initialized and valid for drop.
    unsafe { storage.assume_init_drop() }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_name(this: &VmAddressRegion) -> *const c_char {
    this.state().name.as_ptr().cast()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_address_region_assert_canary(this: &VmAddressRegion) {
    this.state().canary.assert();
}
