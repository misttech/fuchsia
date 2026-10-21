// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::{ARCH_MMU_FLAG_CACHE_MASK, ArchMmuFlags};
use super::page_source::MultiPageRequest;
use super::vm_address_region::{
    LifeCycleState, MemoryPriority, VmAddressRegionOrMapping, flag, region_list,
};
use super::vm_aspace::{VmAspaceLockClass, VmAspaceRegionLockClass};
use super::vm_object::{VmObject, VmObjectLockClass};
use crate::kernel::types::VAddr;
use ::page as kernel_page;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use debug::{ltracef, tracef};
use fbl::{Canary, HasRefCount, Recyclable, RefCounted, RefPtr};
use ksync::{KCell, LockToken};
use vm_address_region_bindings as bindings;
use vm_constants_rs as constants;
use zr::Opaque;
use zx_status::Status;
use zx_types::zx_status_t;

crate::counters::define_kcounter!(VM_MAPPINGS_STATE_DEAD, "vm.aspace.mapping.state.dead", Sum);

const LOCAL_TRACE: u32 = 0;

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

pub type FlagsRange = bindings::VmMapping_FlagsRange;

/// Whether `VmMapping::destroy_locked_object` unmaps the mapping from the arch aspace.
pub type DestroyUnmap = bindings::VmMapping_DestroyUnmap;
/// Whether `VmMapping::destroy_locked_object` removes the mapping from its parent's subregions.
pub type DestroyRemoveFromParent = bindings::VmMapping_DestroyRemoveFromParent;
/// Whether `VmMapping::activate_locked` inserts the mapping into its parent's subregions and the
/// VMO's mapping list.
pub type ActivateInsertRegions = bindings::VmMapping_ActivateInsertRegions;

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

    fn first_region_arch_mmu_flags_locked(
        &self,
        _aspace_token: &LockToken<'_, VmAspaceLockClass>,
    ) -> ArchMmuFlags {
        // SAFETY: The aspace lock is held.
        unsafe { *self.state().first_region_arch_mmu_flags.get() }
    }

    fn set_first_region_arch_mmu_flags_locked(
        &self,
        _aspace_token: &LockToken<'_, VmAspaceLockClass>,
        _object_token: &mut LockToken<'_, VmObjectLockClass>,
        flags: ArchMmuFlags,
    ) {
        // SAFETY: Both guarding locks are held.
        unsafe {
            *self.state().first_region_arch_mmu_flags.get() = flags;
        }
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

    #[inline]
    fn rest_protection_ranges_is_empty_locked(
        &self,
        _token: &LockToken<'_, VmAspaceLockClass>,
    ) -> bool {
        // SAFETY: `self` is live and the aspace lock is held.
        unsafe { bindings::cpp_vm_mapping_rest_protection_ranges_is_empty(self.as_ffi_ptr()) }
    }

    #[inline]
    fn rest_protection_ranges_clear_locked(
        &self,
        _token: &mut LockToken<'_, VmAspaceLockClass>,
        _object_token: &mut LockToken<'_, VmObjectLockClass>,
    ) {
        // SAFETY: `self` is live and the aspace and object locks are held.
        unsafe { bindings::cpp_vm_mapping_rest_protection_ranges_clear(self.as_ffi_ptr()) }
    }

    fn object_locked<'a>(
        &'a self,
        _token: &'a LockToken<'_, VmAspaceLockClass>,
    ) -> &'a Option<RefPtr<VmObject>> {
        // SAFETY: The aspace lock is held.
        unsafe { &*self.state().object.get() }
    }

    fn reset_object_locked(
        &self,
        _aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        _object_token: &mut LockToken<'_, VmObjectLockClass>,
    ) {
        // SAFETY: Both the aspace lock and the object lock are held.
        unsafe { *self.state().object.get() = None };
        self.state().object_reset.store(true, Ordering::Relaxed);
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

    /// Intended to be used from `VmEnumerator` callbacks where the `aspace` lock will be held.
    pub fn vmo_locked(&self, token: &LockToken<'_, VmAspaceLockClass>) -> Option<RefPtr<VmObject>> {
        self.object_locked(token).clone()
    }

    /// Returns the underlying VMO backing this mapping.
    pub fn vmo(&self) -> Option<RefPtr<VmObject>> {
        ksync::lock!(let guard = self.lock().lock());
        self.vmo_locked(guard.token())
    }

    /// Convenience wrapper for `vmo().decommit_range()` with the necessary
    /// offset modification and locking.
    pub fn decommit_range(&self, offset: usize, len: usize) -> Result<(), Status> {
        self.state().canary.assert();
        ltracef!(
            "{:p} [{:#x}+{:#x}], offset {:#x}, len {:#x}\n",
            self as *const _,
            self.base().0,
            self.size(),
            offset,
            len
        );

        ksync::lock!(let guard = self.lock().lock());
        let token = guard.token();
        if self.state_locked(token) != LifeCycleState::Alive {
            return Err(Status::BAD_STATE);
        }
        let end = offset.checked_add(len).ok_or(Status::OUT_OF_RANGE)?;
        if end > self.size() {
            return Err(Status::OUT_OF_RANGE);
        }
        // `VmObject::decommit_range` will typically call back into our instance's
        // `VmMapping::aspace_unmap_locked_object`.
        let object = self.object_locked(token).as_ref().unwrap();
        object.decommit_range(self.object_offset() + offset as u64, len as u64)
    }

    /// Takes a range relative to the vmo and converts it into a virtual address range
    /// relative to `aspace()`. Returns `Some((base, virtual_len))` if a non zero sized intersection
    /// was found, `None` otherwise.
    fn object_range_to_vaddr_range(
        &self,
        _token: &LockToken<'_, VmObjectLockClass>,
        offset: u64,
        len: u64,
    ) -> Option<(VAddr, u64)> {
        debug_assert!(kernel_page::is_aligned(offset as usize));
        debug_assert!(kernel_page::is_aligned(len as usize));

        // Zero sized ranges are considered to have no overlap.
        if len == 0 {
            return None;
        }

        // compute the intersection of the passed in vmo range and our mapping
        let (offset_new, virtual_len) = range_check::get_intersect_offset_len(
            self.object_offset(),
            self.size() as u64,
            offset,
            len,
        )?;

        debug_assert!(virtual_len > 0 && virtual_len <= usize::MAX as u64);
        debug_assert!(offset_new >= self.object_offset());

        ltracef!("intersection offset {:#x}, len {:#x}\n", offset_new, virtual_len);

        // make sure the `base` + `offset` is within our address space
        // should be, according to the range stored in `base()` + `size()`
        let (base, overflowed) =
            self.base().0.overflowing_add((offset_new - self.object_offset()) as usize);
        assert!(!overflowed);

        // make sure we're only operating within our window
        assert!(base >= self.base().0);
        assert!((base + virtual_len as usize - 1) <= (self.base().0 + self.size() - 1));

        Some((VAddr(base), virtual_len))
    }

    /// Returns the minimum of the requested map length, the size of the VMO or, if
    /// `FAULT_BEYOND_STREAM_SIZE` is set, the page containing the stream size. `map_range` can be
    /// trimmed to these lengths as it should not be considered an error to call `map_range` past
    /// the VMO size in a resizable VMO or past the page containing the stream size in a
    /// `FAULT_BEYOND_STREAM_SIZE` VMO.
    fn trimmed_object_range_locked(
        &self,
        aspace_token: &LockToken<'_, VmAspaceLockClass>,
        object_token: &LockToken<'_, VmObjectLockClass>,
        offset: u64,
        len: u64,
    ) -> u64 {
        let vmo_offset = self.object_offset() + offset;
        let object = self.object_locked(aspace_token).as_ref().unwrap();
        let vmo_size = object.size_locked(object_token);
        if vmo_offset >= vmo_size {
            return 0;
        }

        let mut trim_len = vmo_size - vmo_offset;

        if (self.flags() & flag::FAULT_BEYOND_STREAM_SIZE) != 0 {
            let paged = object.as_paged();
            debug_assert!(paged.is_some());
            // SAFETY: `paged` is `Some`.
            let paged = unsafe { paged.unwrap_unchecked() };
            debug_assert!(paged.lock_is_held());
            let stream_size_res = paged.saturating_stream_size_locked(object_token);
            // Creating a fault-beyond-stream-size mapping should have allocated a SSM.
            debug_assert!(stream_size_res.is_some());
            let stream_size = stream_size_res.unwrap();
            debug_assert!(stream_size <= vmo_size);
            if vmo_offset >= stream_size {
                return 0;
            }
            trim_len = stream_size - vmo_offset;
        }

        core::cmp::min(trim_len, len)
    }

    /// Enumerates any different protection ranges that exist inside this mapping. The virtual range
    /// specified by `base` and `size` must be within this mapping's base and size. The provided
    /// callback is called in virtual address order for each protection type. `Status::NEXT` and
    /// `Status::STOP` can be used to control iteration, with any other status becoming the return
    /// value of this method.
    pub fn enumerate_protection_ranges_locked<F>(
        &self,
        _token: &LockToken<'_, VmAspaceLockClass>,
        base: usize,
        size: usize,
        mut f: F,
    ) -> Result<(), Status>
    where
        F: FnMut(usize, usize, ArchMmuFlags) -> Result<(), Status>,
    {
        unsafe extern "C" fn callback_shim<
            F: FnMut(usize, usize, ArchMmuFlags) -> Result<(), Status>,
        >(
            ctx: *mut c_void,
            base: usize,
            len: usize,
            flags: bindings::arch_mmu_flags_t,
        ) -> zx_status_t {
            // SAFETY: `ctx` points to a valid `F` and is exclusively referenced for the call.
            let f = unsafe { &mut *ctx.cast::<F>() };
            Status::result_into_raw(f(base, len, flags))
        }

        let ctx_ptr: *mut c_void = (&raw mut f).cast();
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping` and `ctx_ptr` outlives the call.
        let status = unsafe {
            bindings::cpp_vm_mapping_enumerate_protection_ranges_locked(
                self.as_ffi_ptr(),
                base,
                size,
                Some(callback_shim::<F>),
                ctx_ptr,
            )
        };
        Status::ok(status)
    }

    pub(super) fn dump_locked(
        &self,
        _region_token: &LockToken<'_, VmAspaceRegionLockClass>,
        token: &LockToken<'_, VmAspaceLockClass>,
        depth: u32,
        verbose: bool,
    ) {
        self.state().canary.assert();
        for _ in 0..depth {
            kprint::kprint!("  ");
        }
        let object = self.object_locked(token).as_ref().unwrap();
        let mut vmo_name = [0u8; 32];
        object.get_name(&mut vmo_name);

        kprint::kprint!(
            "map {:p} [{:#x} {:#x}] sz {:#x} state {} mergeable {:s}\n",
            self as *const _,
            self.base().0,
            self.base().0 + self.size() - 1,
            self.size(),
            self.state_locked(token) as i32,
            if self.mergeable_locked(token) == Mergeable::Yes { "true" } else { "false" }
        );

        let _ = self.enumerate_protection_ranges_locked(
            token,
            self.base().0,
            self.size(),
            |base, len, mmu_flags| {
                for _ in 0..(depth + 1) {
                    kprint::kprint!("  ");
                }
                kprint::kprint!(" [{:#x} {:#x}] mmufl {:#x}\n", base, base + len - 1, mmu_flags);
                Err(Status::NEXT)
            },
        );

        for _ in 0..(depth + 1) {
            kprint::kprint!("  ");
        }
        let counts =
            object.get_attributed_memory_in_range(self.object_offset(), self.size() as u64);
        kprint::kprint!(
            "vmo {:p}/k{} off {:#x} bytes ({}/{}) ref {} '{:cs}'\n",
            object.as_raw(),
            object.user_id(),
            self.object_offset(),
            counts.uncompressed_bytes,
            counts.compressed_bytes,
            self.ref_count().ref_count_debug(),
            vmo_name.as_ptr()
        );
        if verbose {
            object.dump(depth + 1, false);
        }
    }

    /// # Safety
    ///
    /// No child of the parent has the same base as `self`, and `self` is kept alive by another
    /// owning reference for the duration of the call.
    unsafe fn activate(
        &self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        let object: *const VmObject =
            self.object_locked(aspace_token).as_deref().expect("object must be set");
        // SAFETY: `object` is kept alive by the object reference of `self`, which `activate_locked`
        // does not reset.
        let object = unsafe { &*object };
        ksync::lock!(let mut guard = object.lock_ref().lock());
        // SAFETY: No child of the parent has the same base as `self`, and `self` is kept alive by
        // another owning reference for the duration of the call.
        unsafe {
            self.activate_locked(
                region_token,
                aspace_token,
                guard.as_mut().token_mut(),
                ActivateInsertRegions::Yes,
            )
        }
    }

    /// Fully locked version of `activate` that can additionally control whether the region is
    /// installed into the parent subregion list and vmo mapping list or not. This control exists
    /// to facilitate the fine grained control needed for splitting, merging and replacing of
    /// mappings as when set to `No` this method is defined as never failing.
    ///
    /// # Safety
    ///
    /// If `insert_region` is `Yes`, no child of the parent has the same base as `self`, and `self`
    /// is kept alive by another owning reference for the duration of the call.
    unsafe fn activate_locked(
        &self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &mut LockToken<'_, VmObjectLockClass>,
        insert_region: ActivateInsertRegions,
    ) -> Result<(), Status> {
        debug_assert!(self.state_locked(aspace_token) == LifeCycleState::NotReady);
        let parent_ptr = self.parent_locked(aspace_token);
        debug_assert!(!parent_ptr.is_null());

        // SAFETY: `parent_ptr` is valid for reads.
        let parent = unsafe { &*parent_ptr };
        debug_assert!(parent.lock_is_held());
        debug_assert!(parent.region_lock_is_held());
        // If inserting into the regions attempt that first before modifying any state, as it can
        // fail.
        if insert_region == ActivateInsertRegions::Yes {
            // SAFETY: No child of `parent` has the same base as `self`.
            unsafe { region_list::insert_region(parent, region_token, aspace_token, self)? };

            let object = self.object_locked(aspace_token).as_ref().unwrap();
            if let Err(status) = object.add_mapping_locked(object_token, self) {
                // SAFETY: `self` is a child of `parent` and is kept alive by another owning
                // reference for the duration of the call.
                unsafe { region_list::remove_region(parent, region_token, aspace_token, self) };
                return Err(status);
            }
        }

        self.set_state_locked(region_token, aspace_token, LifeCycleState::Alive);

        // Now that we have added a mapping to the VMO it's cache policy becomes fixed, and we can
        // read it and augment our `arch_mmu_flags`.
        let object = self.object_locked(aspace_token).as_ref().unwrap();
        let cache_policy = object.get_mapping_cache_policy_locked(object_token);
        let mut arch_mmu_flags = self.first_region_arch_mmu_flags_locked(aspace_token);
        if (arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) != cache_policy {
            // Warn in the event that we somehow receive a VMO that has a cache
            // policy set while also holding cache policy flags within the arch
            // flags. The only path that should be able to achieve this is if
            // something in the kernel maps into their aspace incorrectly.
            if (arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) != 0 {
                tracef!(
                    "warning: mapping has conflicting cache policies: \
                     vmo {:#02x} arch_mmu_flags {:#02x}.\n",
                    cache_policy,
                    arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK
                );
                // Clear the existing cache policy and use the new one.
                arch_mmu_flags &= !ARCH_MMU_FLAG_CACHE_MASK;
            }
            // If we are changing the cache policy then this can only happen if this is a new
            // mapping region and not a new mapping occurring as a result of an unmap split. In the
            // case of a new mapping region we know there cannot yet be any protection ranges.
            debug_assert!(self.rest_protection_ranges_is_empty_locked(aspace_token));
            arch_mmu_flags |= cache_policy;
            self.set_first_region_arch_mmu_flags_locked(aspace_token, object_token, arch_mmu_flags);
        }
        Ok(())
    }

    /// Wrapper for `activate_locked` that does not insert into the parent region, and
    /// therefore cannot fail.
    fn activate_no_insert_locked(
        &self,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &mut LockToken<'_, VmObjectLockClass>,
    ) {
        // SAFETY: `insert_region` is `No`.
        let status = unsafe {
            self.activate_locked(
                region_token,
                aspace_token,
                object_token,
                ActivateInsertRegions::No,
            )
        };
        assert!(status.is_ok());
    }

    fn destroy_locked(
        this: &RefPtr<Self>,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        this.state().canary.assert();
        // Keep a `RefPtr` to the `object` so we know our lock remains valid.
        let object = this.object_locked(aspace_token).clone().expect("object must be set");
        ksync::lock!(let mut guard = object.lock_ref().lock());
        Self::destroy_locked_object(
            this,
            region_token,
            aspace_token,
            guard.as_mut().token_mut(),
            DestroyUnmap::Yes,
            DestroyRemoveFromParent::Yes,
        )
    }

    /// Internal fully locked version of `destroy`. Has controls to both skip the arch aspace
    /// unmapping as well as removal from the parent subregions list. These controls facilitate
    /// the fine grained control needed when splitting, merging and replacing mappings.
    /// If `unmap` is `No` then this method is defined to never fail. `remove_region` does not
    /// impact success or failure of the operation.
    fn destroy_locked_object(
        this: &RefPtr<Self>,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &mut LockToken<'_, VmObjectLockClass>,
        unmap: DestroyUnmap,
        remove_region: DestroyRemoveFromParent,
    ) -> Result<(), Status> {
        // The caller's `RefPtr` keeps us alive after we drop our last other reference in this
        // method (e.g. when removing it from the parent's subregions below).
        let this: &Self = this;

        // If this is the `last_fault` then clear it before removing from the VMAR tree.
        // Even if this destroy fails, it's always safe to clear `last_fault`, so we preference
        // doing it upfront for clarity.
        if core::ptr::eq(this.aspace().last_fault_locked(aspace_token), this) {
            this.aspace().clear_last_fault_locked(aspace_token);
        }

        // The vDSO code mapping can never be unmapped, not even
        // by VMAR destruction (except for process exit, of course).
        // TODO(mcgrathr): Turn this into a policy-driven process-fatal case
        // at some point.  teisenbe@ wants to eventually make `zx_vmar_destroy`
        // never fail.
        if core::ptr::eq(this.aspace().vdso_code_mapping_locked(aspace_token), this) {
            return Err(Status::ACCESS_DENIED);
        }

        if unmap == DestroyUnmap::Yes {
            // SAFETY: The virtual range of this mapping is no longer in use.
            unsafe {
                this.aspace().arch_aspace().unmap(
                    this.base().0,
                    this.size() / kernel_page::SIZE,
                    this.aspace().enlarge_arch_unmap(),
                )
            }?;
        }

        // Remove any priority.
        this.set_memory_priority_default_locked_object::<false>(aspace_token, object_token);

        this.rest_protection_ranges_clear_locked(aspace_token, object_token);
        let object = this.object_locked(aspace_token).as_ref().expect("object must be set");
        object.remove_mapping_locked(object_token, this);

        // Detach the region from the parent.
        let parent_ptr = this.parent_locked(aspace_token);
        if !parent_ptr.is_null() && remove_region == DestroyRemoveFromParent::Yes {
            // SAFETY: `parent_ptr` is valid for reads.
            let parent = unsafe { &*parent_ptr };
            debug_assert!(parent.lock_is_held());
            debug_assert!(parent.region_lock_is_held());
            // SAFETY: `this` is a child of `parent` and is kept alive by another owning
            // reference for the duration of the call.
            unsafe { region_list::remove_region(parent, region_token, aspace_token, this) };
        }

        // detach from any object we have mapped. Note that we are holding the `aspace.lock()`
        // so we will not race with other threads calling `vmo()`
        this.reset_object_locked(aspace_token, object_token);

        // mark ourself as dead
        this.set_parent_locked(aspace_token, core::ptr::null_mut());
        this.set_state_locked(region_token, aspace_token, LifeCycleState::Dead);
        VM_MAPPINGS_STATE_DEAD.add(1);
        Ok(())
    }

    fn destruct(&self) {
        self.state().canary.assert();
        ltracef!(
            "{:p} aspace {:p} base {:#x} size {:#x}\n",
            self as *const _,
            &**self.aspace(),
            self.base().0,
            self.size()
        );
        VM_MAPPINGS_STATE_DEAD.add(-1);
    }

    /// If `MemoryPriority::High`, then disable dynamic reclamation within this region. If
    /// `MemoryPriority::Default`, move towards allowing reclamation.
    ///
    /// When called with `SPLIT_ON_UNMAP = true`, we set the memory
    /// priority assuming the object has not been `activate`'d yet.
    fn set_memory_priority_locked<const SPLIT_ON_UNMAP: bool>(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        priority: MemoryPriority,
    ) {
        if SPLIT_ON_UNMAP {
            // all that's required to set our priority is to have the object and `aspace()` set up
            debug_assert!(
                self.state_locked(token) == LifeCycleState::NotReady
                    && self.object_locked(token).is_some()
            );
        } else {
            debug_assert!(self.state_locked(token) == LifeCycleState::Alive);
        }
        let to_high = priority == MemoryPriority::High;
        let delta: i64 = if to_high { 1 } else { -1 };
        if priority == self.memory_priority_locked(token) {
            return;
        }
        self.set_memory_priority_field_locked(token, priority);
        self.aspace().change_high_priority_count_locked(token, delta);
        let object = self.object_locked(token).as_deref().unwrap();
        if let Some(paged) = object.as_paged() {
            pin_init::stack_pin_init!(let pc = paged.make_priority_changer(delta));
            if priority == MemoryPriority::High {
                pc.as_mut().prepare_may_not_already_be_high_priority();
            }
            ksync::lock!(let guard = ksync::aliased_lock(object.lock_ref(), pc.lock()));
            let (_, cow_token) = guard.tokens();
            pc.as_mut().change_high_priority_count_locked(cow_token);
        }
    }

    /// Move towards allowing dynamic reclamation in the region. You may call this method with the
    /// `object` lock, in contrast to `set_memory_priority_locked`.
    ///
    /// When called with `set_memory_priority_default_locked_object::<true>`, we set the memory
    /// priority assuming the object has not been `activate`'d yet.
    fn set_memory_priority_default_locked_object<const SPLIT_ON_UNMAP: bool>(
        &self,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &LockToken<'_, VmObjectLockClass>,
    ) {
        if SPLIT_ON_UNMAP {
            // all that's required to set our priority is to have the object and `aspace()` set up
            debug_assert!(
                self.state_locked(aspace_token) == LifeCycleState::NotReady
                    && self.object_locked(aspace_token).is_some()
            );
        } else {
            debug_assert!(self.state_locked(aspace_token) == LifeCycleState::Alive);
        }
        if self.memory_priority_locked(aspace_token) == MemoryPriority::Default {
            return;
        }
        self.set_memory_priority_field_locked(aspace_token, MemoryPriority::Default);
        self.aspace().change_high_priority_count_locked(aspace_token, -1);
        if let Some(paged) = self.object_locked(aspace_token).as_deref().unwrap().as_paged() {
            pin_init::stack_pin_init!(let pc = paged.make_priority_changer(-1));
            debug_assert!(paged.cow_pages().lock_is_held()); // we have the object lock
            pc.as_mut().change_high_priority_count_locked(object_token);
        }
    }

    /// Marks the address region as high priority. Only call this method if the mapped
    /// `VmObject` is already high priority (for example if you have already called
    /// `set_memory_priority_locked` on a child).
    ///
    /// You may call this method with the object lock, in contrast to
    /// `set_memory_priority_locked`.
    ///
    /// When called with `set_memory_priority_high_already_positive_locked_object::<true>`, we set
    /// the memory priority assuming the object has not been `activate`'d yet.
    fn set_memory_priority_high_already_positive_locked_object<const SPLIT_ON_UNMAP: bool>(
        &self,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &LockToken<'_, VmObjectLockClass>,
    ) {
        if SPLIT_ON_UNMAP {
            // all that's required to set our priority is to have the object and `aspace()` set up
            debug_assert!(
                self.state_locked(aspace_token) == LifeCycleState::NotReady
                    && self.object_locked(aspace_token).is_some()
            );
        } else {
            debug_assert!(self.state_locked(aspace_token) == LifeCycleState::Alive);
        }
        if self.memory_priority_locked(aspace_token) == MemoryPriority::High {
            return;
        }
        self.set_memory_priority_field_locked(aspace_token, MemoryPriority::High);
        self.aspace().change_high_priority_count_locked(aspace_token, 1);
        if let Some(paged) = self.object_locked(aspace_token).as_deref().unwrap().as_paged() {
            pin_init::stack_pin_init!(let pc = paged.make_priority_changer(1));
            debug_assert!(paged.cow_pages().lock_is_held()); // we have the object lock
            pc.as_mut().prepare_is_already_high_priority_locked(object_token);
            pc.as_mut().change_high_priority_count_locked(object_token);
        }
    }

    /// Performs any actions necessary to apply a high memory priority over the given range. This
    /// method is always safe to call as it will internally check the memory priority status and
    /// skip if necessary, so the caller does not need to worry about races with a different memory
    /// priority being applied. As this may need to acquire the lock even to check the memory
    /// priority, if the caller knows they have not caused this to become high priority (i.e. they
    /// have called `set_memory_priority_locked` with `MemoryPriority::Default`), then calling this
    /// should be skipped for performance. Memory that needs to be committed for a high memory
    /// priority are user pager backed pages and any compressed or loaned pages. Anonymous pages and
    /// copy-on-write pages do not allocated / committed. This method has no return value as it is
    /// entirely best effort and no part of its operation is needed for correctness.
    fn commit_high_memory_priority(&self) {
        let vmo;
        let offset;
        let len;
        {
            ksync::lock!(let guard = self.lock().lock());
            if self.state_locked(guard.token()) != LifeCycleState::Alive
                || self.memory_priority_locked(guard.token()) != MemoryPriority::High
            {
                return;
            }
            vmo = self.vmo_locked(guard.token());
            offset = self.object_offset();
            len = self.size() as u64;
        }
        debug_assert!(vmo.is_some());
        let vmo = vmo.unwrap();
        vmo.commit_high_priority_pages(offset, len);
        // Ignore the return result of `map_range` as this is just best effort.
        let _ = self.map_range(offset as usize, len as usize, false, true);
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

    // Accessors for VMO-mapping state.
    // These can be read under either lock (both locks being held for writing), so we provide two
    // different accessors, one for each lock.
    /// Returns the architecture MMU flags and the top of the protection region containing `va`.
    pub fn arch_mmu_flags_range_locked(
        &self,
        _token: &LockToken<'_, VmAspaceLockClass>,
        va: usize,
    ) -> FlagsRange {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping` and the aspace lock is held.
        unsafe { bindings::cpp_vm_mapping_arch_mmu_flags_range_locked(self.as_ffi_ptr(), va) }
    }

    /// Returns the MMU flags for `offset` while holding the aspace lock.
    pub fn arch_mmu_flags_locked(
        &self,
        _token: &LockToken<'_, VmAspaceLockClass>,
        offset: VAddr,
    ) -> ArchMmuFlags {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping` and the aspace lock is held.
        unsafe { bindings::cpp_vm_mapping_arch_mmu_flags_locked(self.as_ffi_ptr(), offset.0) }
    }

    /// Resolves a page fault on this mapping while holding the aspace lock.
    pub fn page_fault_locked(
        &self,
        _token: &mut LockToken<'_, VmAspaceLockClass>,
        va: VAddr,
        pf_flags: u32,
        additional_pages: usize,
        page_request: Pin<&mut MultiPageRequest>,
    ) -> (Result<(), Status>, u32) {
        let mut mapped = 0u32;
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmMapping`, the aspace lock is held, and
        // `page_request` is a pinned `MultiPageRequest`.
        let status = unsafe {
            bindings::cpp_vm_mapping_page_fault_locked(
                self.as_ffi_ptr(),
                va.0,
                pf_flags,
                additional_pages,
                page_request.as_raw().cast(),
                &mut mapped,
            )
        };
        (Status::ok(status), mapped)
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
/// `this` is being destroyed and its state is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_destruct(this: &VmMapping) {
    this.destruct();
    // SAFETY: The state is initialized and is not used afterwards.
    unsafe {
        core::ptr::drop_in_place(
            bindings::cpp_vm_mapping_state(this.as_ffi_ptr()).cast::<VmMappingState>(),
        )
    };
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
    // SAFETY: The aspace lock is held by the caller.
    let aspace_token = unsafe { LockToken::new() };
    // SAFETY: The object lock is held by the caller.
    let mut object_token = unsafe { LockToken::new() };
    this.set_first_region_arch_mmu_flags_locked(&aspace_token, &mut object_token, flags);
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_vmo(this: &VmMapping) -> *mut bindings::VmObject {
    match this.vmo() {
        Some(vmo) => RefPtr::into_raw(vmo).cast_mut().cast(),
        None => core::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_decommit_range(
    this: &VmMapping,
    offset: usize,
    len: usize,
) -> zx_status_t {
    Status::result_into_raw(this.decommit_range(offset, len))
}

/// # Safety
///
/// The object lock is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_object_range_to_vaddr_range(
    this: &VmMapping,
    offset: u64,
    len: u64,
    base: &mut MaybeUninit<usize>,
    virtual_len: &mut MaybeUninit<u64>,
) -> bool {
    // SAFETY: The object lock is held by the caller.
    let token = unsafe { LockToken::new() };
    match this.object_range_to_vaddr_range(&token, offset, len) {
        Some((vaddr, vlen)) => {
            base.write(vaddr.0);
            virtual_len.write(vlen);
            true
        }
        None => false,
    }
}

/// # Safety
///
/// The aspace lock and the object lock are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_trimmed_object_range_locked(
    this: &VmMapping,
    offset: u64,
    len: u64,
) -> u64 {
    // SAFETY: The aspace lock is held by the caller.
    let aspace_token = unsafe { LockToken::new() };
    // SAFETY: The object lock is held by the caller.
    let object_token = unsafe { LockToken::new() };
    this.trimmed_object_range_locked(&aspace_token, &object_token, offset, len)
}

/// # Safety
///
/// The region lock and the aspace lock are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_dump_locked(this: &VmMapping, depth: u32, verbose: bool) {
    // SAFETY: The region lock is held by the caller.
    let region_token = unsafe { LockToken::new() };
    // SAFETY: The aspace lock is held by the caller.
    let token = unsafe { LockToken::new() };
    this.dump_locked(&region_token, &token, depth, verbose);
}

/// # Safety
///
/// The aspace lock is held and the object lock is not held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_set_mem_priority_locked(
    this: &VmMapping,
    priority: MemoryPriority,
) {
    // SAFETY: The aspace lock is held by the caller.
    let mut token = unsafe { LockToken::new() };
    this.set_memory_priority_locked::<false>(&mut token, priority);
}

/// # Safety
///
/// The aspace lock is held and the object lock is not held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_set_mem_priority_locked_split(
    this: &VmMapping,
    priority: MemoryPriority,
) {
    // SAFETY: The aspace lock is held by the caller.
    let mut token = unsafe { LockToken::new() };
    this.set_memory_priority_locked::<true>(&mut token, priority);
}

/// # Safety
///
/// The aspace lock and the object lock are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_set_mem_priority_default_locked_object(this: &VmMapping) {
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    // SAFETY: The object lock is held by the caller.
    let object_token = unsafe { LockToken::new() };
    this.set_memory_priority_default_locked_object::<false>(&mut aspace_token, &object_token);
}

/// # Safety
///
/// The aspace lock and the object lock are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_set_mem_priority_default_locked_object_split(
    this: &VmMapping,
) {
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    // SAFETY: The object lock is held by the caller.
    let object_token = unsafe { LockToken::new() };
    this.set_memory_priority_default_locked_object::<true>(&mut aspace_token, &object_token);
}

/// # Safety
///
/// The aspace lock and the object lock are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_set_mem_priority_high_already_positive_locked_object(
    this: &VmMapping,
) {
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    // SAFETY: The object lock is held by the caller.
    let object_token = unsafe { LockToken::new() };
    this.set_memory_priority_high_already_positive_locked_object::<false>(
        &mut aspace_token,
        &object_token,
    );
}

/// # Safety
///
/// The aspace lock and the object lock are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_set_mem_priority_high_already_positive_locked_object_split(
    this: &VmMapping,
) {
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    // SAFETY: The object lock is held by the caller.
    let object_token = unsafe { LockToken::new() };
    this.set_memory_priority_high_already_positive_locked_object::<true>(
        &mut aspace_token,
        &object_token,
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_commit_high_memory_priority(this: &VmMapping) {
    this.commit_high_memory_priority();
}

/// # Safety
///
/// The region lock and the aspace lock are held and the object lock is not held, the children of
/// the parent are not being iterated, no child of the parent has the same base as `this`, and
/// `this` is kept alive by another owning reference for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_activate(this: &VmMapping) -> zx_status_t {
    // SAFETY: The region lock is held by the caller.
    let mut region_token = unsafe { LockToken::new() };
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    // SAFETY: No child of the parent has the same base as `this`, and `this` is kept alive by
    // another owning reference for the duration of the call.
    Status::result_into_raw(unsafe { this.activate(&mut region_token, &mut aspace_token) })
}

/// # Safety
///
/// The region lock, the aspace lock and the object lock are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_activate_no_insert_locked(this: &VmMapping) {
    // SAFETY: The region lock is held by the caller.
    let mut region_token = unsafe { LockToken::new() };
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    // SAFETY: The object lock is held by the caller.
    let mut object_token = unsafe { LockToken::new() };
    this.activate_no_insert_locked(&mut region_token, &mut aspace_token, &mut object_token);
}

/// # Safety
///
/// `this` points to a live `VmMapping`. The region lock and the aspace lock are held and the object
/// lock is not held, and the children of the parent are not being iterated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_destroy_locked(this: *const VmMapping) -> zx_status_t {
    // SAFETY: `this` points to a live `VmMapping`.
    let this = RefPtr::from_ref(unsafe { &*this });
    // SAFETY: The region lock is held by the caller.
    let mut region_token = unsafe { LockToken::new() };
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    Status::result_into_raw(VmMapping::destroy_locked(&this, &mut region_token, &mut aspace_token))
}

/// # Safety
///
/// `this` points to a live `VmMapping`. The region lock, the aspace lock and the object lock are
/// held, and the children of the parent are not being iterated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_destroy_locked_object(
    this: *const VmMapping,
    unmap: DestroyUnmap,
    remove_region: DestroyRemoveFromParent,
) -> zx_status_t {
    // SAFETY: `this` points to a live `VmMapping`.
    let this = RefPtr::from_ref(unsafe { &*this });
    // SAFETY: The region lock is held by the caller.
    let mut region_token = unsafe { LockToken::new() };
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    // SAFETY: The object lock is held by the caller.
    let mut object_token = unsafe { LockToken::new() };
    Status::result_into_raw(VmMapping::destroy_locked_object(
        &this,
        &mut region_token,
        &mut aspace_token,
        &mut object_token,
        unmap,
        remove_region,
    ))
}
