// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::ArchMmuFlags;
use super::vm_address_region::VmAddressRegionOrMapping;
use super::vm_aspace::VmAspaceLockClass;
use super::vm_object::{VmObject, VmObjectLockClass};
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use fbl::{Canary, HasRefCount, Recyclable, RefCounted, RefPtr};
use ksync::{KCell, LockToken};
use vm_address_region_bindings as bindings;
use vm_constants_rs as constants;
use zr::Opaque;
use zx_status::Status;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    No = 0,
    Yes = 1,
}

zr::static_assert!(
    core::mem::size_of::<Mergeable>() == core::mem::size_of::<bindings::VmMapping_Mergeable>()
);
zr::static_assert!(Mergeable::No as u8 == bindings::VmMapping_Mergeable::NO as u8);
zr::static_assert!(Mergeable::Yes as u8 == bindings::VmMapping_Mergeable::YES as u8);

/// A representation of the mapping of a VMO into the address space
#[repr(transparent)]
pub struct VmMapping {
    raw: Opaque<bindings::VmMapping>,
}

#[repr(C)]
pub struct VmMappingState {
    canary: Canary<{ fbl::magic(b"VMAP") }>,
    /// Whether this mapping may be merged with other adjacent mappings. A mergeable mapping is just
    /// a region that can be represented by any `VmMapping` object, not specifically this one.
    mergeable: KCell<Mergeable, VmAspaceLockClass>,
    /// TODO(https://fxbug.dev/42106188): Tracks whether this mapping has been transitioned into a
    /// private clone to allow for writes to safely be done without modifying a VMO that the mapping
    /// does not have permission to.
    private_clone: bool,
    /// Tracks whether the `object` has been reset (i.e. is `None`) or not. `object` is always
    /// `Some` at construction and only ever performs a single transition, which is to `None`, prior
    /// to destruction. The reset of `object`, and hence the modification of this value, happens
    /// under both the aspace lock and the `object` lock. See `page_fault` for usage of this.
    object_reset: AtomicBool,
    /// The protection flags of this mapping are tracked as a series of contiguous
    /// virtual address ranges. Since the base address of the mapping is known (`base`),
    /// we only need to store the flags for the first range, and then the start address
    /// and flags for any subsequent ranges where the protections change.
    ///
    /// `first_region_arch_mmu_flags` holds the MMU flags for the interval starting
    /// exactly at `base`. If the protection ranges are empty, these flags apply to the
    /// entire mapping: `[base, base + size)`.
    ///
    /// This can be read with either lock held, but requires both locks to write it.
    first_region_arch_mmu_flags: UnsafeCell<ArchMmuFlags>,
    /// pointer and region of the object we are mapping.
    /// The `object` cannot be immutable, as it gets reset, but logically it is a constant value
    /// from construction until it gets reset, and then it stays `None` until the mapping is
    /// destructed.
    object: UnsafeCell<Option<RefPtr<VmObject>>>,
    object_offset: u64,
    /// Pointer to a `CurrentlyFaulting` object if the mapping is presently handling a page fault.
    /// This is protected specifically by the `object` lock so that `aspace_unmap_locked_object` can
    /// inspect it.
    currently_faulting: KCell<*mut c_void, VmObjectLockClass>,
}

::zr::static_assert_size_and_align!(
    VmMappingState,
    constants::kVmMappingStateSize,
    constants::kVmMappingStateAlign,
);

impl VmMappingState {
    fn new(
        object: Option<RefPtr<VmObject>>,
        object_offset: u64,
        first_region_arch_mmu_flags: ArchMmuFlags,
        private_clone: bool,
        mergeable: Mergeable,
    ) -> Self {
        Self {
            canary: Canary::new(),
            mergeable: KCell::new(mergeable),
            private_clone,
            object_reset: AtomicBool::new(false),
            first_region_arch_mmu_flags: UnsafeCell::new(first_region_arch_mmu_flags),
            object: UnsafeCell::new(object),
            object_offset,
            currently_faulting: KCell::new(core::ptr::null_mut()),
        }
    }
}

impl HasRefCount for VmMapping {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        (**self).ref_count()
    }
}

// SAFETY: `recycle` releases the allocation exactly once when the last reference is dropped.
unsafe impl Recyclable for VmMapping {
    #[inline]
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: `ptr` points to a live `VmMapping` to which no reference remains.
        unsafe { bindings::cpp_vm_mapping_free(ptr.as_ptr().cast()) }
    }
}

impl Deref for VmMapping {
    type Target = VmAddressRegionOrMapping;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `VmAddressRegionOrMapping` is the first base of `VmMapping`.
        unsafe { &*core::ptr::from_ref(self).cast() }
    }
}

// SAFETY: `VmMapping` is not tied to the thread that created it.
unsafe impl Send for VmMapping {}
// SAFETY: Mutable state of `VmMapping` is only accessed with the aspace lock or the object lock
// held, or atomically.
unsafe impl Sync for VmMapping {}

impl VmMapping {
    fn as_ffi_ptr(&self) -> *mut bindings::VmMapping {
        self.raw.get()
    }

    fn state(&self) -> &VmMappingState {
        // SAFETY: `self` points to a live `VmMapping`.
        unsafe { &*bindings::cpp_vm_mapping_state(self.as_ffi_ptr()).cast() }
    }

    /// Marks a mapping as eligible for merging with adjacent compatible mappings.
    pub fn mark_mergeable(mapping: RefPtr<VmMapping>) {
        // SAFETY: `RefPtr::into_raw` transfers ownership of the reference to `ImportFromRawPtr`.
        unsafe {
            bindings::cpp_vm_mapping_mark_mergeable(RefPtr::into_raw(mapping).cast_mut().cast())
        }
    }

    /// Destroys this mapping, unmapping all pages and removing dependencies on the underlying VMO.
    pub fn destroy(&self) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        Status::ok(unsafe { bindings::cpp_vm_mapping_destroy(self.as_ffi_ptr()) })
    }

    /// Returns the offset into the underlying VMO for this mapping.
    pub fn object_offset(&self) -> u64 {
        self.state().object_offset
    }

    fn mergeable_locked(&self, token: &LockToken<'_, VmAspaceLockClass>) -> Mergeable {
        // SAFETY: The aspace lock is held.
        *unsafe { self.state().mergeable.get(token) }
    }

    fn set_mergeable_locked(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        mergeable: Mergeable,
    ) {
        // SAFETY: The aspace lock is held.
        *unsafe { self.state().mergeable.get_mut(token) } = mergeable;
    }

    fn currently_faulting(&self, token: &LockToken<'_, VmObjectLockClass>) -> *mut c_void {
        // SAFETY: The object lock is held.
        *unsafe { self.state().currently_faulting.get(token) }
    }

    fn set_currently_faulting(
        &self,
        token: &mut LockToken<'_, VmObjectLockClass>,
        faulting: *mut c_void,
    ) {
        // SAFETY: The object lock is held.
        *unsafe { self.state().currently_faulting.get_mut(token) } = faulting;
    }

    /// Convenience wrapper for vmo()->DecommitRange() with the necessary
    /// offset modification and locking.
    pub fn decommit_range(&self, offset: usize, len: usize) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        Status::ok(unsafe {
            bindings::cpp_vm_mapping_decommit_range(self.as_ffi_ptr(), offset, len)
        })
    }

    /// Map in pages from the underlying vm object, optionally committing pages as it goes.
    /// |ignore_existing| controls whether existing hardware mappings in the specified range should
    /// be ignored or treated as an error. |ignore_existing| should only be set to true for user
    /// mappings where populating mappings may already be racy with multiple threads, and where we
    /// are already tolerant of mappings being arbitrarily created and destroyed.
    pub fn map_range(
        &self,
        offset: usize,
        len: usize,
        commit: bool,
        ignore_existing: bool,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        Status::ok(unsafe {
            bindings::cpp_vm_mapping_map_range(
                self.as_ffi_ptr(),
                offset,
                len,
                commit,
                ignore_existing,
            )
        })
    }

    /// Unlocked convenience wrapper around unmap for testing.
    pub fn debug_unmap(&self, base: usize, size: usize) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        Status::ok(unsafe { bindings::cpp_vm_mapping_debug_unmap(self.as_ffi_ptr(), base, size) })
    }

    /// Unlocked convenience wrapper around protect for testing.
    pub fn debug_protect(
        &self,
        base: usize,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        Status::ok(unsafe {
            bindings::cpp_vm_mapping_debug_protect(
                self.as_ffi_ptr(),
                base,
                size,
                new_arch_mmu_flags,
            )
        })
    }

    /// Returns the underlying VMO backing this mapping.
    pub fn vmo(&self) -> Option<RefPtr<VmObject>> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`.
        let raw = unsafe { bindings::cpp_vm_mapping_vmo(self.as_ffi_ptr()) };
        // SAFETY: `raw` is null or an owned reference to a live `VmObject`.
        unsafe { RefPtr::try_from_raw(raw.cast()) }
    }

    /// Informs the mapping that a write is going to be performed to the backing VMO.
    ///
    /// If necessary, creates a private clone of the VMO and returns a new mapping.
    pub fn force_writable(&self) -> Result<RefPtr<VmMapping>, Status> {
        let mut out: *mut bindings::VmMapping = core::ptr::null_mut();
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping` and `out` is writable.
        let status =
            unsafe { bindings::cpp_vm_mapping_force_writable(self.as_ffi_ptr(), &mut out) };
        Status::ok(status)?;
        // SAFETY: `out` is null or an owned reference to a live `VmMapping`.
        Ok(unsafe {
            RefPtr::try_from_raw(out.cast()).expect("Should never be null with OK status")
        })
    }
}

/// # Safety
///
/// `vmo` is null or an owned reference to a live `VmObject`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_state_init(
    storage: &mut MaybeUninit<VmMappingState>,
    vmo: *mut VmObject,
    vmo_offset: u64,
    first_mmu_flags: ArchMmuFlags,
    private_clone: bool,
    mergeable: Mergeable,
) {
    // SAFETY: `vmo` is null or an owned reference to a live `VmObject`.
    let object = unsafe { RefPtr::try_from_raw(vmo) };
    storage.write(VmMappingState::new(
        object,
        vmo_offset,
        first_mmu_flags,
        private_clone,
        mergeable,
    ));
}

/// # Safety
///
/// `storage` is initialized and is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_destruct(storage: &mut MaybeUninit<VmMappingState>) {
    // SAFETY: `storage` is initialized and valid for drop.
    unsafe { storage.assume_init_drop() }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_assert_canary(this: &VmMapping) {
    this.state().canary.assert();
}

zr::static_assert!(
    core::mem::size_of::<Option<RefPtr<VmObject>>>() == core::mem::size_of::<*const VmObject>()
);

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_object(this: &VmMapping) -> *const Option<RefPtr<VmObject>> {
    this.state().object.get()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_object_offset(this: &VmMapping) -> u64 {
    this.object_offset()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_private_clone(this: &VmMapping) -> bool {
    this.state().private_clone
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_object_reset(this: &VmMapping) -> bool {
    this.state().object_reset.load(Ordering::Relaxed)
}

/// # Safety
///
/// The aspace lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_mergeable_locked(this: &VmMapping) -> Mergeable {
    // SAFETY: The aspace lock is held by the caller.
    let token = unsafe { LockToken::new() };
    this.mergeable_locked(&token)
}

/// # Safety
///
/// The aspace lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_set_mergeable_locked(
    this: &VmMapping,
    mergeable: Mergeable,
) {
    // SAFETY: The aspace lock is held by the caller.
    let mut token = unsafe { LockToken::new() };
    this.set_mergeable_locked(&mut token, mergeable);
}

/// # Safety
///
/// The object lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_currently_faulting(this: &VmMapping) -> *mut c_void {
    // SAFETY: The object lock is held by the caller.
    let token = unsafe { LockToken::new() };
    this.currently_faulting(&token)
}

/// # Safety
///
/// The object lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_set_currently_faulting(
    this: &VmMapping,
    faulting: *mut c_void,
) {
    // SAFETY: The object lock is held by the caller.
    let mut token = unsafe { LockToken::new() };
    this.set_currently_faulting(&mut token, faulting);
}

/// # Safety
///
/// The aspace lock or the object lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_first_region_arch_mmu_flags(
    this: &VmMapping,
) -> ArchMmuFlags {
    // SAFETY: The aspace lock or the object lock is held by the caller.
    unsafe { *this.state().first_region_arch_mmu_flags.get() }
}

/// # Safety
///
/// The aspace lock and the object lock are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_set_first_region_arch_mmu_flags(
    this: &VmMapping,
    flags: ArchMmuFlags,
) {
    // SAFETY: Both the aspace lock and the object lock are held by the caller.
    unsafe { *this.state().first_region_arch_mmu_flags.get() = flags };
}

/// # Safety
///
/// The aspace lock and the object lock are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_reset_object_locked(this: &VmMapping) {
    // SAFETY: Both the aspace lock and the object lock are held by the caller.
    unsafe { *this.state().object.get() = None };
    this.state().object_reset.store(true, Ordering::Relaxed);
}
