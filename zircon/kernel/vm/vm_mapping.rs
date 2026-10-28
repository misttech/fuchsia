// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::{
    ARCH_MMU_FLAG_CACHE_MASK, ARCH_MMU_FLAG_PERM_RWX_MASK, ARCH_MMU_FLAG_PERM_WRITE,
    ARCH_UNMAP_OPTION_ENLARGE, ARCH_UNMAP_OPTION_HARVEST, ARCH_UNMAP_OPTION_NONE, ArchMmuFlags,
};
use super::page_source::MultiPageRequest;
use super::vm_address_region::{
    LifeCycleState, MemoryPriority, VmAddressRegion, VmAddressRegionOrMapping, flag, region_list,
};
use super::vm_aspace::{VmAspace, VmAspaceLockClass, VmAspaceRegionLockClass};
use super::vm_object::{VmObject, VmObjectLockClass};
use crate::counters::define_kcounter;
use crate::kernel::types::VAddr;
use ::page as kernel_page;
use bitflags::bitflags;
use core::cell::{Cell, UnsafeCell};
use core::cmp::min;
use core::hint;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, Ordering};
use debug::{ltracef, tracef};
use fbl::{Canary, HasRefCount, Recyclable, RefCounted, RefPtr};
use ksync::{KCell, LockToken};
use pin_init::{PinInit, pin_data};
use vm_address_region_bindings as bindings;
use vm_constants_rs as constants;
use zr::{Opaque, pin_init_ffi, unsafe_pinned_drop_ffi};
use zx_status::Status;
use zx_types::zx_status_t;

define_kcounter!(VM_MAPPINGS_STATE_DEAD, "vm.aspace.mapping.state.dead", Sum);
define_kcounter!(VM_MAPPINGS_PROTECT_NO_WRITE, "vm.aspace.mapping.protect_without_write", Sum);

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

/// The MMU flags of a protection region and where it ends.
#[derive(Debug, Clone, Copy)]
pub struct FlagsRange {
    /// The MMU flags of the region.
    pub mmu_flags: ArchMmuFlags,
    /// The end of the region.
    pub region_top: u64,
}

bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct UnmapOptions: u8 {
        const ONLY_HAS_ZERO_PAGES = 1 << 0;
        const HARVEST = 1 << 1;
    }
}

zr::static_assert!(UnmapOptions::empty().bits() == bindings::VmMapping_UnmapOptions::kNone as u8);
zr::static_assert!(
    UnmapOptions::ONLY_HAS_ZERO_PAGES.bits()
        == bindings::VmMapping_UnmapOptions::OnlyHasZeroPages as u8
);
zr::static_assert!(UnmapOptions::HARVEST.bits() == bindings::VmMapping_UnmapOptions::Harvest as u8);

/// Whether `VmMapping::destroy_locked_object` unmaps the mapping from the arch aspace.
pub type DestroyUnmap = bindings::VmMapping_DestroyUnmap;
/// Whether `VmMapping::destroy_locked_object` removes the mapping from its parent's subregions.
pub type DestroyRemoveFromParent = bindings::VmMapping_DestroyRemoveFromParent;
/// Whether `VmMapping::activate_locked` inserts the mapping into its parent's subregions and the
/// VMO's mapping list.
pub type ActivateInsertRegions = bindings::VmMapping_ActivateInsertRegions;

/// A representation of the mapping of a VMO into the address space
#[derive(Recyclable)]
#[pin_data(PinnedDrop)]
#[repr(transparent)]
pub struct VmMapping {
    #[pin]
    raw: Opaque<bindings::VmMapping>,
}

unsafe_pinned_drop_ffi!(VmMapping, bindings::cpp_vm_mapping_destruct);

/// A B-Tree that records any transitions in protection. Each entry consists of a `usize` key and
/// an `ArchMmuFlags` value. A node (addr, flags) means that the range starting at `addr` has the
/// protections `flags`, up until the address of the next node in the tree, or `base() + size()` if
/// it is the last node.
#[repr(transparent)]
pub struct ProtectionRanges(Opaque<bindings::VmMappingProtectionRanges>);

impl ProtectionRanges {
    fn as_ffi_ptr(&self) -> *mut bindings::VmMappingProtectionRanges {
        self.0.get()
    }

    /// # Safety
    ///
    /// `raw` points to a live `ProtectionRanges` that outlives `'a` and is not mutated during `'a`.
    #[inline]
    unsafe fn from_raw_ref<'a>(raw: *const bindings::VmMappingProtectionRanges) -> &'a Self {
        // SAFETY: `raw` points to a live `ProtectionRanges` that outlives `'a`.
        unsafe { &*raw.cast::<Self>() }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        // SAFETY: `self` is live and not concurrently mutated.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_is_empty(self.as_ffi_ptr()) }
    }

    #[inline]
    fn begin(&self) -> ProtectionRangesCursor<'_> {
        // SAFETY: `self` is live and not mutated while the returned cursor is live, `out` is valid
        // for writes, and the closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursor::new(|out| {
                bindings::cpp_vm_mapping_protection_ranges_begin(self.as_ffi_ptr(), out.cast())
            })
        }
    }

    /// Returns a cursor to the first item whose key is strictly greater than `key`, or an end
    /// cursor (for which `get()` returns `None`) if no such item.
    #[inline]
    fn upper_bound(&self, key: usize) -> ProtectionRangesCursor<'_> {
        // SAFETY: `self` is live and not mutated while the returned cursor is live, `out` is valid
        // for writes, and the closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursor::new(|out| {
                bindings::cpp_vm_mapping_protection_ranges_upper_bound(
                    self.as_ffi_ptr(),
                    key,
                    out.cast(),
                )
            })
        }
    }
}

struct ProtectionRangesMut<'a> {
    ranges: NonNull<bindings::VmMappingProtectionRanges>,
    _ranges: PhantomData<&'a mut ProtectionRanges>,
}

impl Deref for ProtectionRangesMut<'_> {
    type Target = ProtectionRanges;

    #[inline]
    fn deref(&self) -> &ProtectionRanges {
        // SAFETY: `self.ranges` points to a live `ProtectionRanges` that outlives `self` and is
        // not mutated while `self` is borrowed.
        unsafe { ProtectionRanges::from_raw_ref(self.ranges.as_ptr()) }
    }
}

impl<'a> ProtectionRangesMut<'a> {
    /// # Safety
    ///
    /// `ranges` points to a live `ProtectionRanges` that outlives `'a` and that nothing else
    /// accesses during `'a`.
    #[inline]
    unsafe fn from_raw(ranges: *mut bindings::VmMappingProtectionRanges) -> Self {
        // SAFETY: `ranges` is not null.
        Self { ranges: unsafe { NonNull::new_unchecked(ranges) }, _ranges: PhantomData }
    }

    #[inline]
    fn new(ranges: Pin<&'a mut ProtectionRanges>) -> Self {
        // SAFETY: `ranges` is live for `'a` and exclusively borrowed.
        unsafe { Self::from_raw(ranges.as_ffi_ptr()) }
    }

    #[inline]
    fn as_ffi_ptr(&self) -> *mut bindings::VmMappingProtectionRanges {
        self.ranges.as_ptr()
    }

    #[inline]
    fn begin_mut(&mut self) -> ProtectionRangesCursorMut<'_> {
        let ranges = self.as_ffi_ptr();
        // SAFETY: `self` is live and exclusively accessed, `out` is valid for writes, and the
        // closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursorMut::new(self, |out| {
                bindings::cpp_vm_mapping_protection_ranges_begin(ranges, out.cast())
            })
        }
    }

    #[inline]
    fn end_mut(&mut self) -> ProtectionRangesCursorMut<'_> {
        let ranges = self.as_ffi_ptr();
        // SAFETY: `self` is live and exclusively accessed, `out` is valid for writes, and the
        // closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursorMut::new(self, |out| {
                bindings::cpp_vm_mapping_protection_ranges_end(ranges, out.cast())
            })
        }
    }

    /// Returns a cursor to the first item whose key is greater than or equal to `key`, or
    /// `end_mut()` if no such item.
    #[inline]
    fn lower_bound_mut(&mut self, key: usize) -> ProtectionRangesCursorMut<'_> {
        let ranges = self.as_ffi_ptr();
        // SAFETY: `self` is live and exclusively accessed, `out` is valid for writes, and the
        // closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursorMut::new(self, |out| {
                bindings::cpp_vm_mapping_protection_ranges_lower_bound(ranges, key, out.cast())
            })
        }
    }

    /// Returns a cursor to the item whose key is exactly `key`, or `end_mut()` if no such item.
    #[inline]
    fn find_mut(&mut self, key: usize) -> ProtectionRangesCursorMut<'_> {
        let ranges = self.as_ffi_ptr();
        // SAFETY: `self` is live and exclusively accessed, `out` is valid for writes, and the
        // closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursorMut::new(self, |out| {
                bindings::cpp_vm_mapping_protection_ranges_find(ranges, key, out.cast())
            })
        }
    }

    /// Inserts the provided value using the specified key. It is an error to insert a duplicate
    /// key and attempting to do so will either cause a runtime assertion failure or data structure
    /// corruption.
    ///
    /// If internal nodes needed to, but could not be, allocated then an end cursor (for which
    /// `get()` returns `None`) is returned, otherwise a cursor to the new item is returned. In all
    /// cases if an end cursor is returned the tree is left in an unmodified state.
    ///
    /// After an insertion all cursors, except the one returned, are stale and must not be used.
    ///
    /// # Safety
    ///
    /// `key` is not already present in `self`.
    #[inline]
    #[must_use]
    unsafe fn insert(&mut self, key: usize, flags: ArchMmuFlags) -> ProtectionRangesCursor<'_> {
        // SAFETY: `self` is live and exclusively accessed, `key` is not already present in it,
        // `out` is valid for writes, and the closure initializes `out` to a cursor into `self`.
        unsafe {
            ProtectionRangesCursor::new(|out| {
                bindings::cpp_vm_mapping_protection_ranges_insert(
                    self.as_ffi_ptr(),
                    key,
                    flags,
                    out.cast(),
                )
            })
        }
    }

    /// Clears all content from the tree, returning it to the empty state. All cursors are
    /// invalidated.
    #[inline]
    fn clear(&mut self) {
        // SAFETY: `self` is live and exclusively accessed.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_clear(self.as_ffi_ptr()) }
    }
}

struct ProtectionRangesCursor<'a> {
    raw: Opaque<bindings::VmMappingProtectionRangesCursor>,
    _ranges: PhantomData<&'a ProtectionRanges>,
}

zr::static_assert!(
    size_of::<bindings::VmMappingProtectionRangesCursor>()
        == constants::kVmMappingProtectionRangesCursorSize
);
zr::static_assert!(
    align_of::<bindings::VmMappingProtectionRangesCursor>()
        == constants::kVmMappingProtectionRangesCursorAlign
);

impl Clone for ProtectionRangesCursor<'_> {
    #[inline]
    fn clone(&self) -> Self {
        let raw = Opaque::uninit();
        // SAFETY: `self.raw` is initialized and valid for reads, `raw` is valid for writes, they
        // do not overlap, and the cursor type is trivially copyable.
        unsafe { ptr::copy_nonoverlapping(self.raw.get(), raw.get(), 1) };
        Self { raw, _ranges: PhantomData }
    }
}

impl<'a> ProtectionRangesCursor<'a> {
    /// # Safety
    ///
    /// `init` initializes the cursor it is passed to a cursor into ranges that outlive `'a` and
    /// are not mutated during `'a`.
    #[inline]
    unsafe fn new(init: impl FnOnce(*mut bindings::VmMappingProtectionRangesCursor)) -> Self {
        let raw = Opaque::uninit();
        init(raw.get());
        Self { raw, _ranges: PhantomData }
    }

    #[inline]
    fn get(&self) -> Option<(usize, ArchMmuFlags)> {
        if !self.is_valid() {
            return None;
        }
        let mut key = 0;
        let mut flags = 0;
        // SAFETY: `self.raw` is a valid cursor into live ranges that are not concurrently
        // mutated, and is not stale, and `key` and `flags` are valid for writes.
        unsafe {
            bindings::cpp_vm_mapping_protection_ranges_cursor_get(
                self.raw.get(),
                &mut key,
                &mut flags,
            )
        };
        Some((key, flags))
    }

    /// Returns whether the cursor is one that can be dereferenced, i.e. is not an end
    /// cursor or `begin()` of an empty tree. If the cursor is stale due to `insert` or `erase`
    /// having been called then the return value is undefined, and if iterator validation is
    /// enabled this method counts as an access and will trigger an error.
    #[inline]
    fn is_valid(&self) -> bool {
        // SAFETY: `self.raw` is a cursor into live ranges that are not concurrently mutated, and
        // is not stale.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_cursor_is_valid(self.raw.get()) }
    }

    #[inline]
    fn next(&mut self) {
        // SAFETY: `self.raw` is a cursor into live ranges that are not concurrently mutated, and
        // is not stale.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_cursor_next(self.raw.get()) }
    }

    /// # Safety
    ///
    /// `self` is not before the first item.
    #[inline]
    unsafe fn prev(&mut self) {
        // SAFETY: `self.raw` is a cursor into live ranges that are not concurrently mutated, is
        // not stale, and is not before the first item.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_cursor_prev(self.raw.get()) }
    }
}

struct ProtectionRangesCursorMut<'a> {
    ranges: NonNull<bindings::VmMappingProtectionRanges>,
    raw: Opaque<bindings::VmMappingProtectionRangesCursor>,
    _ranges: PhantomData<&'a mut ProtectionRanges>,
}

impl<'a> ProtectionRangesCursorMut<'a> {
    /// # Safety
    ///
    /// `init` initializes the cursor it is passed to a cursor into `ranges`.
    #[inline]
    unsafe fn new(
        ranges: &'a mut ProtectionRangesMut<'_>,
        init: impl FnOnce(*mut bindings::VmMappingProtectionRangesCursor),
    ) -> Self {
        let raw = Opaque::uninit();
        init(raw.get());
        Self { ranges: ranges.ranges, raw, _ranges: PhantomData }
    }

    #[inline]
    fn get(&self) -> Option<(usize, ArchMmuFlags)> {
        if !self.is_valid() {
            return None;
        }
        let mut key = 0;
        let mut flags = 0;
        // SAFETY: `self.raw` is a valid cursor into live ranges that are not concurrently
        // mutated, and is not stale, and `key` and `flags` are valid for writes.
        unsafe {
            bindings::cpp_vm_mapping_protection_ranges_cursor_get(
                self.raw.get(),
                &mut key,
                &mut flags,
            )
        };
        Some((key, flags))
    }

    #[inline]
    fn is_valid(&self) -> bool {
        // SAFETY: `self.raw` is a cursor into live ranges that are not concurrently mutated, and
        // is not stale.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_cursor_is_valid(self.raw.get()) }
    }

    /// # Safety
    ///
    /// `self` is not before the first item.
    #[inline]
    unsafe fn prev(&mut self) {
        // SAFETY: `self.raw` is a cursor into live ranges that are not concurrently mutated, is
        // not stale, and is not before the first item.
        unsafe { bindings::cpp_vm_mapping_protection_ranges_cursor_prev(self.raw.get()) }
    }

    /// Like `ProtectionRangesMut::insert`, but uses `self` as a hint of an item to insert near,
    /// and moves `self` to the cursor that `insert` would return. `self` can be to an invalid
    /// location (i.e. `end_mut()`).
    ///
    /// # Safety
    ///
    /// `key` is not already present in the ranges.
    #[inline]
    unsafe fn insert_hint(&mut self, key: usize, flags: ArchMmuFlags) {
        let raw: Opaque<bindings::VmMappingProtectionRangesCursor> = Opaque::uninit();
        // SAFETY: `self.ranges` is live and exclusively accessed, `self.raw` is a cursor into it
        // that is not stale, `key` is not already present in it, and `raw` is valid for writes.
        unsafe {
            bindings::cpp_vm_mapping_protection_ranges_insert_hint(
                self.ranges.as_ptr(),
                self.raw.get(),
                key,
                flags,
                raw.get().cast(),
            )
        };
        self.raw = raw;
    }

    /// Updates the flags of the item referenced by `self`. C++ `BTree::update` asserts that
    /// `self.is_valid()` is true. As this does not change the structure of the tree this does not
    /// invalidate any cursors.
    #[inline]
    fn update(&mut self, flags: ArchMmuFlags) {
        // SAFETY: `self.ranges` is live and exclusively accessed, and `self.raw` is a cursor into
        // it that is not stale.
        unsafe {
            bindings::cpp_vm_mapping_protection_ranges_update(
                self.ranges.as_ptr(),
                self.raw.get(),
                flags,
            )
        }
    }

    /// Removes the item at `self`, and moves `self` to the item that followed it.
    ///
    /// After an erase all cursors, except `self`, are stale and must not be used.
    ///
    /// # Safety
    ///
    /// `self.is_valid()` is true.
    #[inline]
    unsafe fn erase(&mut self) {
        let raw: Opaque<bindings::VmMappingProtectionRangesCursor> = Opaque::uninit();
        // SAFETY: `self.ranges` is live and exclusively accessed, `self.raw` is a valid cursor
        // into it that is not stale, and `raw` is valid for writes.
        unsafe {
            bindings::cpp_vm_mapping_protection_ranges_erase(
                self.ranges.as_ptr(),
                self.raw.get(),
                raw.get().cast(),
            )
        };
        self.raw = raw;
    }
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
    currently_faulting: KCell<Option<NonNull<CurrentlyFaulting<'static>>>, VmObjectLockClass>,
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
            currently_faulting: KCell::new(None),
        }
    }
}

impl HasRefCount for VmMapping {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        (**self).ref_count()
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
    /// private constructor, use `VmAddressRegion::create...()` instead
    #[expect(clippy::too_many_arguments)]
    fn new(
        parent: &VmAddressRegion,
        private_clone: bool,
        base: usize,
        size: usize,
        vmar_flags: u32,
        vmo: RefPtr<VmObject>,
        vmo_offset: u64,
        arch_mmu_flags: ArchMmuFlags,
        mergeable: Mergeable,
    ) -> impl PinInit<Self> {
        pin_init_ffi!(
            bindings::cpp_vm_mapping_construct,
            parent.as_ffi_ptr(),
            private_clone,
            base,
            size,
            vmar_flags,
            RefPtr::into_raw(vmo).cast_mut().cast(),
            vmo_offset,
            arch_mmu_flags,
            mergeable == Mergeable::Yes,
        )
    }

    /// private constructor, use `VmAddressRegion::create...()` instead
    #[expect(clippy::too_many_arguments)]
    fn new_with_ranges(
        parent: &VmAddressRegion,
        private_clone: bool,
        base: usize,
        size: usize,
        vmar_flags: u32,
        vmo: RefPtr<VmObject>,
        vmo_offset: u64,
        first_mmu_flags: ArchMmuFlags,
        ranges: Pin<&mut ProtectionRanges>,
        mergeable: Mergeable,
    ) -> impl PinInit<Self> {
        pin_init_ffi!(
            bindings::cpp_vm_mapping_construct_with_ranges,
            parent.as_ffi_ptr(),
            private_clone,
            base,
            size,
            vmar_flags,
            RefPtr::into_raw(vmo).cast_mut().cast(),
            vmo_offset,
            first_mmu_flags,
            ranges.as_ffi_ptr(),
            mergeable == Mergeable::Yes,
        )
    }

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

    fn first_region_arch_mmu_flags_locked_object(
        &self,
        _object_token: &LockToken<'_, VmObjectLockClass>,
    ) -> ArchMmuFlags {
        // SAFETY: The object lock is held.
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
    fn rest_protection_ranges_locked<'a>(
        &'a self,
        _token: &'a LockToken<'_, VmAspaceLockClass>,
    ) -> &'a ProtectionRanges {
        // SAFETY: `self` is live, and the returned pointer points to a live `ProtectionRanges` that
        // outlives `'a` and is not mutated during `'a`.
        unsafe {
            ProtectionRanges::from_raw_ref(bindings::cpp_vm_mapping_rest_protection_ranges_const(
                self.as_ffi_ptr(),
            ))
        }
    }

    #[inline]
    fn rest_protection_ranges_locked_object<'a>(
        &'a self,
        _token: &'a LockToken<'_, VmObjectLockClass>,
    ) -> &'a ProtectionRanges {
        // SAFETY: `self` is live, and the returned pointer points to a live `ProtectionRanges` that
        // outlives `'a` and is not mutated during `'a`.
        unsafe {
            ProtectionRanges::from_raw_ref(bindings::cpp_vm_mapping_rest_protection_ranges_const(
                self.as_ffi_ptr(),
            ))
        }
    }

    #[inline]
    fn rest_protection_ranges_mut_locked<'a>(
        &'a self,
        _aspace_token: &'a mut LockToken<'_, VmAspaceLockClass>,
        _object_token: &'a mut LockToken<'_, VmObjectLockClass>,
    ) -> ProtectionRangesMut<'a> {
        // SAFETY: `self` is live, and the returned pointer points to a live `ProtectionRanges` that
        // outlives `'a` and that nothing else accesses during `'a`.
        unsafe {
            ProtectionRangesMut::from_raw(bindings::cpp_vm_mapping_rest_protection_ranges(
                self.as_ffi_ptr(),
            ))
        }
    }

    /// # Safety
    ///
    /// `other` is not `self`.
    #[inline]
    unsafe fn rest_protection_ranges_mut_and_other_locked<'a>(
        &'a self,
        other: &'a VmMapping,
        _aspace_token: &'a mut LockToken<'_, VmAspaceLockClass>,
        _object_token: &'a mut LockToken<'_, VmObjectLockClass>,
    ) -> (ProtectionRangesMut<'a>, &'a ProtectionRanges) {
        // SAFETY: `self` is live, and the returned pointer points to a live `ProtectionRanges` that
        // outlives `'a` and that nothing else accesses during `'a`.
        let ranges = unsafe {
            ProtectionRangesMut::from_raw(bindings::cpp_vm_mapping_rest_protection_ranges(
                self.as_ffi_ptr(),
            ))
        };
        // SAFETY: `other` is live, and the returned pointer points to a live `ProtectionRanges`
        // that outlives `'a` and is not mutated during `'a`.
        let other_ranges = unsafe {
            ProtectionRanges::from_raw_ref(bindings::cpp_vm_mapping_rest_protection_ranges_const(
                other.as_ffi_ptr(),
            ))
        };
        (ranges, other_ranges)
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

    fn currently_faulting<'s>(
        &'s self,
        token: &LockToken<'_, VmObjectLockClass>,
    ) -> Option<NonNull<CurrentlyFaulting<'s>>> {
        // SAFETY: The object lock is held.
        let faulting = *unsafe { self.state().currently_faulting.get(token) };
        faulting.map(NonNull::cast::<CurrentlyFaulting<'s>>)
    }

    /// # Safety
    ///
    /// `faulting` is `None`, or points to a `CurrentlyFaulting` for this mapping that stays live
    /// and is not moved until `currently_faulting` is next set. Until then, the object lock stays
    /// held, no `&mut` to it exists, and it is not written except through its `Cell` fields.
    unsafe fn set_currently_faulting(
        &self,
        token: &mut LockToken<'_, VmObjectLockClass>,
        faulting: Option<NonNull<CurrentlyFaulting<'_>>>,
    ) {
        // SAFETY: The object lock is held.
        *unsafe { self.state().currently_faulting.get_mut(token) } =
            faulting.map(NonNull::cast::<CurrentlyFaulting<'static>>);
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

    /// Unmap any pages that map the passed in vmo range from the arch aspace.
    /// May not intersect with this range.
    /// If `UnmapOptions::ONLY_HAS_ZERO_PAGES` is set in `options` then the caller is asserting that
    /// it knows that any mappings in the region will only be for the shared zero page.
    ///
    /// # Safety
    ///
    /// This mapping is in its VMO's mapping list, `offset` and `len` are page aligned, and the
    /// pages that map `[offset, offset + len)` of the VMO in this mapping may be unmapped. If
    /// `UnmapOptions::ONLY_HAS_ZERO_PAGES` is set in `options`, any mappings of that range only
    /// map the shared zero page.
    pub(super) unsafe fn aspace_unmap_locked_object(
        &self,
        token: &LockToken<'_, VmObjectLockClass>,
        offset: u64,
        len: u64,
        options: UnmapOptions,
    ) {
        self.state().canary.assert();

        // NOTE: must be acquired with the vmo lock held, but doesn't need to take
        // the address space lock, since it will not manipulate its location in the
        // vmar tree. However, it must be held in the `Alive` state across this call.
        //
        // Avoids a race with `destroy_locked` since it removes ourself from the VMO's
        // mapping list with the VMO lock held before dropping this state to `Dead`. The
        // VMO can't call back to us once we're out of their list.
        debug_assert!(self.state_locked_object(token) == LifeCycleState::Alive);

        // The object pointer itself is not accessed in this method, and we do not hold the correct
        // lock for it, but we know the object lock is held and so therefore the pointer is valid
        // and will not be modified. Therefore it's correct to read it here for the purposes of an
        // assert, but cannot be expressed nicely with regular annotations.
        // SAFETY: The object lock is held.
        debug_assert!(unsafe { (*self.state().object.get()).is_some() });

        // In the case of unmapping known instances of the zero page check if this range intersects
        // with an in progress fault. If it does we can skip the unmap with the knowledge that the
        // mapping will be updated later. This is safe since the zero page is, by definition, only
        // mapped read only, and is never modified so delaying the update of the mapping cannot
        // cause either any users to see incorrect data, or users to be able to modify an old
        // mapping.
        if options.contains(UnmapOptions::ONLY_HAS_ZERO_PAGES)
            && let Some(currently_faulting) = self.currently_faulting(token)
        {
            // SAFETY: `currently_faulting` points to a live `CurrentlyFaulting`, to which no `&mut`
            // exists and which is not written except through its `Cell` fields while this
            // reference is live.
            let currently_faulting = unsafe { currently_faulting.as_ref() };
            if currently_faulting.unmap_range(offset, len) {
                return;
            }
        }

        ltracef!(
            "region {:p} obj_offset {:#x} size {}, offset {:#x} len {:#x}\n",
            self,
            self.object_offset(),
            self.size(),
            offset,
            len
        );

        // See if there's an intersect.
        let Some((base, new_len)) = self.object_range_to_vaddr_range(token, offset, len) else {
            return;
        };

        // If this is a kernel mapping then we should not be removing mappings out of the arch
        // aspace, unless this mapping has explicitly opted out of this check.
        let aspace = self.aspace();
        debug_assert!(
            aspace.is_user()
                || aspace.is_guest_physical()
                || (self.flags() & flag::DEBUG_DYNAMIC_KERNEL_MAPPING) != 0
        );

        let mut aspace_op = aspace.enlarge_arch_unmap();
        if options.contains(UnmapOptions::HARVEST) {
            aspace_op |= ARCH_UNMAP_OPTION_HARVEST;
        }

        // SAFETY: `[base, base + new_len)` is part of this mapping and may be unmapped.
        let status = unsafe {
            aspace.arch_aspace().unmap(base.0, new_len as usize / kernel_page::SIZE, aspace_op)
        };
        assert!(status.is_ok());
    }

    /// Removes any writeable mappings for the passed in vmo range from the arch aspace.
    /// May fall back to unmapping pages from the arch aspace if necessary.
    ///
    /// # Safety
    ///
    /// This mapping is in its VMO's mapping list, `offset` and `len` are page aligned, and the
    /// pages that map `[offset, offset + len)` of the VMO in this mapping may have write permission
    /// removed, and may be unmapped.
    pub(super) unsafe fn aspace_remove_write_locked_object(
        &self,
        token: &LockToken<'_, VmObjectLockClass>,
        offset: u64,
        len: u64,
    ) {
        ltracef!(
            "region {:p} obj_offset {:#x} size {}, offset {:#x} len {:#x}\n",
            self,
            self.object_offset(),
            self.size(),
            offset,
            len
        );

        self.state().canary.assert();

        // NOTE: must be acquired with the vmo lock held, but doesn't need to take
        // the address space lock, since it will not manipulate its location in the
        // vmar tree. However, it must be held in the `Alive` state across this call.
        //
        // Avoids a race with `destroy_locked` since it removes ourself from the VMO's
        // mapping list with the VMO lock held before dropping this state to `Dead`. The
        // VMO can't call back to us once we're out of their list.
        debug_assert!(self.state_locked_object(token) == LifeCycleState::Alive);

        // The object pointer itself is not accessed in this method, and we do not hold the correct
        // lock for it, but we know the object lock is held and so therefore the pointer is valid
        // and will not be modified. Therefore it's correct to read it here for the purposes of an
        // assert, but cannot be expressed nicely with regular annotations.
        // SAFETY: The object lock is held.
        debug_assert!(unsafe { (*self.state().object.get()).is_some() });

        // If this doesn't support writing then nothing to be done, as we know we have no write
        // mappings.
        if (self.flags() & flag::CAN_MAP_WRITE) == 0 {
            return;
        }

        // See if there's an intersect.
        let Some((base, new_len)) = self.object_range_to_vaddr_range(token, offset, len) else {
            return;
        };

        // If this is a kernel mapping then we should not modify mappings in the arch aspace,
        // unless this mapping has explicitly opted out of this check.
        if cfg!(debug_assertions)
            && !(self.aspace().is_user()
                || self.aspace().is_guest_physical()
                || (self.flags() & flag::DEBUG_DYNAMIC_KERNEL_MAPPING) != 0)
        {
            debug_assert_aspace_remove_write_failed(self, offset, len);
        }

        let status = self.enumerate_protection_ranges_locked_object(
            token,
            base.0,
            new_len as usize,
            |region_base, region_len, mut mmu_flags| {
                // If this range doesn't currently support being writable then we can skip.
                if (mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) == 0 {
                    return Err(Status::NEXT);
                }

                // Build new mmu flags without writing.
                mmu_flags &= !ARCH_MMU_FLAG_PERM_WRITE;

                // SAFETY: `region_base` and `region_len` are page aligned, and
                // `[region_base, region_base + region_len)` is part of this mapping, its
                // permissions may be changed to `mmu_flags`, and it may be unmapped.
                let result = unsafe {
                    Self::protect_or_unmap(self.aspace(), region_base, region_len, mmu_flags)
                };
                if result.is_ok() {
                    return Err(Status::NEXT);
                }
                result
            },
        );
        assert!(status.is_ok());
    }

    /// Checks if this is a kernel mapping within the given VMO range, which would be an error to be
    /// unpinning.
    pub(super) fn aspace_debug_unpin_locked_object(
        &self,
        token: &LockToken<'_, VmObjectLockClass>,
        offset: u64,
        len: u64,
    ) {
        ltracef!(
            "region {:p} obj_offset {:#x} size {}, offset {:#x} len {:#x}\n",
            self,
            self.object_offset(),
            self.size(),
            offset,
            len
        );

        self.state().canary.assert();

        // NOTE: must be acquired with the vmo lock held, but doesn't need to take
        // the address space lock, since it will not manipulate its location in the
        // vmar tree. However, it must be held in the `Alive` state across this call.
        //
        // Avoids a race with `destroy_locked` since it removes ourself from the VMO's
        // mapping list with the VMO lock held before dropping this state to `Dead`. The
        // VMO can't call back to us once we're out of their list.
        debug_assert!(self.state_locked_object(token) == LifeCycleState::Alive);

        // See if there's an intersect.
        if self.object_range_to_vaddr_range(token, offset, len).is_none() {
            return;
        }

        // This unpin is not allowed for kernel mappings, unless the mapping has specifically opted
        // out of this debug check due to it performing its own dynamic management.
        debug_assert!(
            self.aspace().is_user()
                || self.aspace().is_guest_physical()
                || (self.flags() & flag::DEBUG_DYNAMIC_KERNEL_MAPPING) != 0
        );
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
        token: &LockToken<'_, VmAspaceLockClass>,
        base: usize,
        size: usize,
        func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(usize, usize, ArchMmuFlags) -> Result<(), Status>,
    {
        self.enumerate_protection_ranges(
            || self.is_alive_locked(token),
            || self.rest_protection_ranges_locked(token),
            || self.first_region_arch_mmu_flags_locked(token),
            base,
            size,
            func,
        )
    }

    fn enumerate_protection_ranges_locked_object<F>(
        &self,
        token: &LockToken<'_, VmObjectLockClass>,
        base: usize,
        size: usize,
        func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(usize, usize, ArchMmuFlags) -> Result<(), Status>,
    {
        self.enumerate_protection_ranges(
            || self.is_alive_locked_object(token),
            || self.rest_protection_ranges_locked_object(token),
            || self.first_region_arch_mmu_flags_locked_object(token),
            base,
            size,
            func,
        )
    }

    #[inline(always)]
    fn enumerate_protection_ranges<'r, F>(
        &self,
        is_alive: impl FnOnce() -> bool,
        rest_protection_ranges: impl FnOnce() -> &'r ProtectionRanges,
        first_region_arch_mmu_flags: impl FnOnce() -> ArchMmuFlags,
        base: usize,
        size: usize,
        mut func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(usize, usize, ArchMmuFlags) -> Result<(), Status>,
    {
        debug_assert!(self.is_in_range(VAddr(base), size));
        // If the mapping is no longer alive, then return early since there's nothing to enumerate.
        if !is_alive() {
            hint::cold_path();
            return Ok(());
        }

        let end = base.wrapping_add(size);

        // Find the first transition strictly after `base`.
        let mut it = rest_protection_ranges().upper_bound(base);

        // `it` now represents the end point of the first range, so look backwards to determine the
        // start flags.
        let mut prev = it.clone();
        // SAFETY: `prev` comes from `upper_bound`, so it is not before the first item.
        unsafe { prev.prev() };
        let mut flags = match prev.get() {
            Some((_, flags)) => flags,
            None => first_region_arch_mmu_flags(),
        };
        let mut range_start = base;

        loop {
            let current = it.get();
            // The current range ends at either `end` or the next transition point, whichever is
            // earlier.
            let range_end = match current {
                Some((key, _)) => min(key, end),
                None => end,
            };
            debug_assert!(range_start < range_end);

            let result = func(range_start, range_end - range_start, flags);
            if result != Err(Status::NEXT) {
                if result == Err(Status::STOP) {
                    return Ok(());
                }
                return result;
            }

            // If we've reached the end of the requested range, or there are no more transitions,
            // stop. Placing this check here allows the range generation above to also generate the
            // 'trailing' range for the last transition point without needing an extra invocation of
            // `func`, which helps inlining.
            let Some((key, next_flags)) = current else {
                break;
            };
            if key >= end {
                break;
            }

            // Move to the next transition point.
            range_start = range_end;
            flags = next_flags;
            it.next();
        }
        Ok(())
    }

    #[inline]
    fn flags_range_at_addr_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        va: usize,
    ) -> FlagsRange {
        let rest_protection_ranges = self.rest_protection_ranges_locked(token);
        if rest_protection_ranges.is_empty() {
            return FlagsRange {
                mmu_flags: self.first_region_arch_mmu_flags_locked(token),
                region_top: self.base().0.wrapping_add(self.size()) as u64,
            };
        }
        core::hint::cold_path();
        // Find the first transition strictly after `va`.
        let mut it = rest_protection_ranges.upper_bound(va);
        // `it` represents the end of the range that includes `va`
        let top = match it.get() {
            Some((key, _)) => key,
            None => self.base().0.wrapping_add(self.size()),
        };
        // Now go backwards to find the start of the range that includes `va`, which tells us the
        // flags.
        // SAFETY: `it` comes from `upper_bound`, so it is not before the first item.
        unsafe { it.prev() };
        let flags = match it.get() {
            Some((_, flags)) => flags,
            None => self.first_region_arch_mmu_flags_locked(token),
        };
        FlagsRange { mmu_flags: flags, region_top: top as u64 }
    }

    /// Helper for protect and unmap.
    ///
    /// # Safety
    ///
    /// `base` and `size` are page aligned, and `[base, base + size)` is part of a mapping in
    /// `aspace` whose permissions may be changed to `new_arch_mmu_flags`, and which may be
    /// unmapped.
    unsafe fn protect_or_unmap(
        aspace: &VmAspace,
        base: usize,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        debug_assert!(kernel_page::is_aligned(base) && kernel_page::is_aligned(size));
        // This can never be used to set a WRITE permission since it does not ask the underlying VMO
        // to perform the copy-on-write step. The underlying VMO might also support dirty tracking,
        // which requires write permission faults in order to track pages as dirty when written.
        assert!((new_arch_mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) == 0);
        // If not removing all permissions do the protect, otherwise skip straight to unmapping the
        // entire region.
        if (new_arch_mmu_flags & ARCH_MMU_FLAG_PERM_RWX_MASK) != 0 {
            // SAFETY: The permissions of `[base, base + size)` may be changed to
            // `new_arch_mmu_flags`.
            let status = unsafe {
                aspace.arch_aspace().protect(
                    base,
                    size / kernel_page::SIZE,
                    new_arch_mmu_flags,
                    if aspace.can_enlarge_arch_unmap() {
                        ARCH_UNMAP_OPTION_ENLARGE
                    } else {
                        ARCH_UNMAP_OPTION_NONE
                    },
                )
            };
            // If the unmap failed and we are allowed to unmap extra portions of the aspace then
            // fall through and unmap, otherwise return with whatever the status is.
            if status.is_ok() {
                return status;
            }
            hint::cold_path();
            if !aspace.can_enlarge_arch_unmap() {
                return status;
            }
        }

        // SAFETY: `[base, base + size)` may be unmapped.
        unsafe {
            aspace.arch_aspace().unmap(base, size / kernel_page::SIZE, aspace.enlarge_arch_unmap())
        }
    }

    /// Copies protection ranges for the given sub-range into the returned first flags and
    /// `out_ranges`. The sub-range `[base, base + size)` must be within
    /// `[self.base(), self.base() + self.size())`.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not alive.
    ///
    /// # Safety
    ///
    /// `out_ranges` is empty.
    unsafe fn copy_protection_ranges_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        base: usize,
        size: usize,
        out_ranges: Pin<&mut ProtectionRanges>,
    ) -> Result<ArchMmuFlags, Status> {
        debug_assert!(size != 0 && self.is_in_range(VAddr(base), size));
        let mut out_ranges = ProtectionRangesMut::new(out_ranges);
        let mut first_flags = None;
        let out_first_flags = &mut first_flags;
        self.enumerate_protection_ranges_locked(token, base, size, move |b, _s, flags| {
            if b == base {
                *out_first_flags = Some(flags);
            } else {
                // SAFETY: `b` is not present in `out_ranges`.
                if !unsafe { out_ranges.insert(b, flags) }.is_valid() {
                    hint::cold_path();
                    return Err(Status::NO_MEMORY);
                }
            }
            Err(Status::NEXT)
        })?;
        Ok(first_flags.expect("first_flags must be set"))
    }

    /// Merges a copy of the protection ranges of `right` into this mapping.
    /// Assumes `right` is immediately to the right of this mapping.
    ///
    /// # Safety
    ///
    /// `right` is not `self`, and `right.base()` and the keys of the protection ranges of `right`
    /// are distinct from each other and absent from the protection ranges of `self`.
    unsafe fn merge_protection_ranges_locked(
        &self,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &mut LockToken<'_, VmObjectLockClass>,
        right: &VmMapping,
    ) -> Result<(), Status> {
        debug_assert!(right.base().0 == self.base().0 + self.size());
        let first_flags = self.first_region_arch_mmu_flags_locked(aspace_token);
        let right_first_flags = right.first_region_arch_mmu_flags_locked(aspace_token);
        // SAFETY: `right` is not `self`.
        let (mut rest_protection_ranges, right_rest_protection_ranges) = unsafe {
            self.rest_protection_ranges_mut_and_other_locked(right, aspace_token, object_token)
        };
        // Lookup the flags for the end of the range.
        let mut it = rest_protection_ranges.end_mut();
        // SAFETY: `it` is not before the first item.
        unsafe { it.prev() };
        let last_flags = match it.get() {
            Some((_, flags)) => flags,
            None => first_flags,
        };
        if last_flags != right_first_flags {
            // SAFETY: `right.base()` is not present in `rest_protection_ranges`.
            unsafe { it.insert_hint(right.base().0, right_first_flags) };
            if !it.is_valid() {
                hint::cold_path();
                return Err(Status::NO_MEMORY);
            }
        }
        let mut right_it = right_rest_protection_ranges.begin();
        while let Some((key, value)) = right_it.get() {
            // SAFETY: `key` is not present in `rest_protection_ranges`.
            unsafe { it.insert_hint(key, value) };
            if !it.is_valid() {
                hint::cold_path();
                return Err(Status::NO_MEMORY);
            }
            right_it.next();
        }
        Ok(())
    }

    /// Removes any protection range transitions within the range `[base, end)`.
    fn clear_protection_range_transitions_locked(
        &self,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &mut LockToken<'_, VmObjectLockClass>,
        base: usize,
        end: usize,
    ) {
        let mut rest_protection_ranges =
            self.rest_protection_ranges_mut_locked(aspace_token, object_token);
        let mut it = rest_protection_ranges.lower_bound_mut(base);
        while it.get().is_some_and(|(key, _)| key < end) {
            // SAFETY: `it` is valid.
            unsafe { it.erase() };
        }
    }

    /// Removes all protection range transitions at or below `split_addr`, returning the flags
    /// that are active at `split_addr`.
    fn remove_after_split_locked(
        &self,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        object_token: &mut LockToken<'_, VmObjectLockClass>,
        split_addr: usize,
    ) -> ArchMmuFlags {
        let mut flags = self.first_region_arch_mmu_flags_locked(aspace_token);
        let mut rest_protection_ranges =
            self.rest_protection_ranges_mut_locked(aspace_token, object_token);
        let mut it = rest_protection_ranges.begin_mut();
        while let Some((key, value)) = it.get() {
            if key > split_addr {
                break;
            }
            flags = value;
            // SAFETY: `it` is valid.
            unsafe { it.erase() };
        }
        flags
    }

    /// Change access permissions for this mapping.  It is an error to specify a
    /// caching mode in the flags.  This will persist the caching mode the
    /// mapping was created with.  If a subrange of the mapping is specified, the
    /// mapping may be split.
    ///
    /// # Safety
    ///
    /// `size` is not zero, `base` and `size` are page aligned, `[base, base + size)` is within
    /// this mapping and may have its permissions changed and be unmapped, and `new_arch_mmu_flags`
    /// has no cache flags and is valid for this mapping.
    unsafe fn protect_locked(
        &self,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
        base: usize,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        // Assert a few things that should already have been checked by the caller.
        debug_assert!(size != 0 && kernel_page::is_aligned(base) && kernel_page::is_aligned(size));
        debug_assert!((new_arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) == 0);
        debug_assert!(self.is_valid_mapping_flags(new_arch_mmu_flags));

        let object = self
            .object_locked(aspace_token)
            .as_ref()
            .map(RefPtr::as_ptr)
            .expect("object must be set");
        // SAFETY: `self` holds a reference to `object` that is only dropped with the aspace lock
        // held, and `aspace_token` holds the aspace lock until this function returns.
        let object = unsafe { object.as_ref_unchecked() };
        // grab the lock for the vmo
        ksync::lock!(let mut guard = object.lock_ref().lock());
        let object_token = guard.as_mut().token_mut();

        // Persist our current caching mode. Every protect region will have the same caching mode so
        // we can acquire this from any region.
        let new_arch_mmu_flags = new_arch_mmu_flags
            | (self.first_region_arch_mmu_flags_locked(aspace_token) & ARCH_MMU_FLAG_CACHE_MASK);

        // This will get called by `enumerate_protection_ranges_locked` below for every existing
        // unique protection range that gets changed and allows us to fine tune the protect action
        // based on the previous flags.
        let protect_callback = move |base: usize, size: usize, old_arch_mmu_flags: ArchMmuFlags| {
            // Perform an early return if the new and old flags are the same, as there's nothing to
            // be done.
            if new_arch_mmu_flags == old_arch_mmu_flags {
                return Err(Status::NEXT);
            }

            let mut flags = new_arch_mmu_flags;
            // Check if the new flags have the write permission. This is problematic as we cannot
            // just change any existing hardware mappings to have the write permission, as any
            // individual mapping may be the result of a read fault and still need to have a
            // copy-on-write step performed. This could also map a dirty tracked VMO which requires
            // write permission faults to track pages as dirty when written.
            if (new_arch_mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) != 0 {
                // Whatever happens, we're not going to be protecting the arch aspace to have write
                // mappings, so this has to be a user aspace so that we can lazily take write faults
                // in the future.
                assert!(self.aspace().is_user() || self.aspace().is_guest_physical());
                flags &= !ARCH_MMU_FLAG_PERM_WRITE;
                VM_MAPPINGS_PROTECT_NO_WRITE.add(1);
                // If the new flags without write permission are the same as the old flags, then
                // skip the protect step since it will be a no-op.
                if flags == old_arch_mmu_flags {
                    return Err(Status::NEXT);
                }
            }

            // SAFETY: `base` and `size` are page aligned, and `[base, base + size)` is part of a
            // mapping in `self.aspace()` whose permissions may be changed to `flags`, and which
            // may be unmapped.
            let status = unsafe { Self::protect_or_unmap(self.aspace(), base, size, flags) };
            // If the protect failed then we do not have sufficient information left to rollback in
            // order to return an error, nor can we claim success, so require the protect to have
            // succeeded to continue.
            assert!(status.is_ok());
            Err(Status::NEXT)
        };

        // To handle the protection change, we need to ensure that the start and end of the
        // requested range are represented as boundary nodes in the protection ranges B-tree if
        // they are not already. This ensures that any change in protections for the sub-range does
        // not unintentionally leak into adjacent sub-ranges.

        // Efficiently calculate the flags before, at, and after the requested range using
        // minimal BTree searches.
        let mut flags_before = self.first_region_arch_mmu_flags_locked(aspace_token);
        let mut flags_at_base = self.first_region_arch_mmu_flags_locked(aspace_token);
        let mut flags_end = self.first_region_arch_mmu_flags_locked(aspace_token);
        let mut rest_protection_ranges =
            self.rest_protection_ranges_mut_locked(aspace_token, object_token);
        let mut base_exists = false;

        let mut it_base = rest_protection_ranges.upper_bound(base);
        // SAFETY: `it_base` is not before the first item.
        unsafe { it_base.prev() };
        if let Some((addr, flags)) = it_base.get() {
            if addr == base {
                base_exists = true;
                flags_at_base = flags;
                // SAFETY: `it_base` is not before the first item.
                unsafe { it_base.prev() };
                if let Some((_prev_addr, prev_flags)) = it_base.get() {
                    flags_before = prev_flags;
                }
            } else {
                flags_at_base = flags;
                flags_before = flags_at_base;
            }
        }

        let mut end_exists = false;

        let mut it_end = rest_protection_ranges.upper_bound(base + size);
        // SAFETY: `it_end` is not before the first item.
        unsafe { it_end.prev() };
        if let Some((addr, flags)) = it_end.get() {
            if addr == base + size {
                end_exists = true;
                flags_end = flags;
            } else {
                flags_end = flags;
            }
        }

        // We only need a boundary node at `base` if it's not the start of the mapping and the
        // preceding flags are different from the new ones.
        let need_base = base > self.base().0 && flags_before != new_arch_mmu_flags;
        let insert_base = need_base && !base_exists;

        // Similarly, we need a boundary node at `base + size` if it's not the end of the mapping
        // and the new flags for the range are different from the existing flags that follow it.
        let need_end = base + size < self.base().0 + self.size() && new_arch_mmu_flags != flags_end;
        let insert_end = need_end && !end_exists;

        // Perform any required insertions first. By inserting the *old* flags initially, we make
        // the insertion a structural change only (a no-op for effective permissions). This allows
        // us to handle OOM errors before we begin any non-reversible mutations.
        if insert_base {
            // SAFETY: `base` is not present.
            let it = unsafe { rest_protection_ranges.insert(base, flags_at_base) };
            if !it.is_valid() {
                hint::cold_path();
                return Err(Status::NO_MEMORY);
            }
        }
        if insert_end {
            // SAFETY: `base + size` is not present.
            let it = unsafe { rest_protection_ranges.insert(base + size, flags_end) };
            if !it.is_valid() {
                hint::cold_path();
                if insert_base {
                    // SAFETY: `base` is present in `rest_protection_ranges`.
                    unsafe { rest_protection_ranges.find_mut(base).erase() };
                }
                return Err(Status::NO_MEMORY);
            }
        }

        // Now that all structural nodes are guaranteed to exist, we can safely perform the
        // architectural protection changes. Calling this after successful insertions ensures
        // that we don't have to rollback MMU changes if an allocation fails.
        let _ = self.enumerate_protection_ranges_locked(aspace_token, base, size, protect_callback);

        // Update the protection flags for the start of the range.
        if base == self.base().0 {
            self.set_first_region_arch_mmu_flags_locked(
                aspace_token,
                object_token,
                new_arch_mmu_flags,
            );
        } else if need_base {
            let mut rest_protection_ranges =
                self.rest_protection_ranges_mut_locked(aspace_token, object_token);
            rest_protection_ranges.find_mut(base).update(new_arch_mmu_flags);
        } else {
            let mut rest_protection_ranges =
                self.rest_protection_ranges_mut_locked(aspace_token, object_token);
            // If the boundary node is no longer needed (e.g., flags now match), remove it.
            let mut it = rest_protection_ranges.find_mut(base);
            if it.is_valid() {
                // SAFETY: `it` is valid.
                unsafe { it.erase() };
            }
        }

        // If a boundary node at `base + size` is not needed, remove it to keep the tree minimal.
        if !need_end {
            let mut rest_protection_ranges =
                self.rest_protection_ranges_mut_locked(aspace_token, object_token);
            let mut it = rest_protection_ranges.find_mut(base + size);
            if it.is_valid() {
                // SAFETY: `it` is valid.
                unsafe { it.erase() };
            }
        }

        // Finally, erase any internal boundary nodes that were entirely within the protected
        // range, as the entire range `[base, base + size)` now has uniform protections.
        self.clear_protection_range_transitions_locked(
            aspace_token,
            object_token,
            base + 1,
            base + size,
        );

        Ok(())
    }

    #[inline(never)]
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
    pub(super) unsafe fn activate(
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
            debug_assert!(self.rest_protection_ranges_locked(aspace_token).is_empty());
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

    pub(super) fn destroy_locked(
        this: RefPtr<Self>,
        region_token: &mut LockToken<'_, VmAspaceRegionLockClass>,
        aspace_token: &mut LockToken<'_, VmAspaceLockClass>,
    ) -> Result<(), Status> {
        this.state().canary.assert();
        // Keep a `RefPtr` to the `object` so we know our lock remains valid.
        let object = this.object_locked(aspace_token).clone().expect("object must be set");
        ksync::lock!(let mut guard = object.lock_ref().lock());
        Self::destroy_locked_object(
            &this,
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

        this.rest_protection_ranges_mut_locked(aspace_token, object_token).clear();
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
    pub(super) fn commit_high_memory_priority(&self) {
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

    /// Unlocked convenience wrapper of `protect_locked` for testing.
    ///
    /// # Safety
    ///
    /// `size` is not zero, `base` and `size` are page aligned, `[base, base + size)` is within
    /// this mapping and may have its permissions changed and be unmapped, and `new_arch_mmu_flags`
    /// has no cache flags and is valid for this mapping.
    pub unsafe fn debug_protect(
        &self,
        base: usize,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        ksync::lock!(let mut guard = self.lock().lock());
        // SAFETY: `size` is not zero, `base` and `size` are page aligned, `[base, base + size)` is
        // within this mapping and may have its permissions changed and be unmapped, and
        // `new_arch_mmu_flags` has no cache flags and is valid for this mapping.
        unsafe { self.protect_locked(guard.as_mut().token_mut(), base, size, new_arch_mmu_flags) }
    }

    // Accessors for VMO-mapping state.
    // These can be read under either lock (both locks being held for writing), so we provide two
    // different accessors, one for each lock.
    /// Returns the architecture MMU flags and the top of the protection region containing `va`.
    pub fn arch_mmu_flags_range_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        va: usize,
    ) -> FlagsRange {
        self.flags_range_at_addr_locked(token, va)
    }

    /// Returns the MMU flags for `offset` while holding the aspace lock.
    pub fn arch_mmu_flags_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        offset: VAddr,
    ) -> ArchMmuFlags {
        self.flags_range_at_addr_locked(token, offset.0).mmu_flags
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

#[cold]
#[inline(never)]
fn debug_assert_aspace_remove_write_failed(mapping: &VmMapping, offset: u64, len: u64) -> ! {
    panic!(
        "region {:p} obj_offset {:#x} size {}, offset {:#x} len {:#x}",
        mapping,
        mapping.object_offset(),
        mapping.size(),
        offset,
        len
    );
}

/// Helper class for managing the logic of skipping certain unmap operations for in progress faults.
/// This is expected to be stack allocated under the object lock and the object lock must not be
/// dropped over its lifetime.
/// Creating this object creates a contract where the caller will either update the mapping for this
/// location and call `mapping_updated`, or this object will automatically unmap the location if
/// necessary.
#[repr(C)]
struct CurrentlyFaulting<'a> {
    /// Reference back to the original mapping.
    mapping: &'a VmMapping,
    /// The offset, in object space, of the page fault.
    object_offset: u64,
    len: u64,
    state: Cell<CurrentlyFaultingState>,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum CurrentlyFaultingState {
    NoUnmapNeeded,
    UnmapSkipped,
    Completed,
}

::zr::static_assert_size_and_align!(
    CurrentlyFaulting<'static>,
    constants::kVmMappingCurrentlyFaultingSize,
    constants::kVmMappingCurrentlyFaultingAlign,
);

impl<'a> CurrentlyFaulting<'a> {
    fn new(mapping: &'a VmMapping, object_offset: u64, len: u64) -> Self {
        Self {
            mapping,
            object_offset,
            len,
            state: Cell::new(CurrentlyFaultingState::NoUnmapNeeded),
        }
    }

    /// # Safety
    ///
    /// `object_offset` and `len` are page aligned, `[object_offset, object_offset + len)` is in the
    /// range of the VMO that the mapping maps, and the mapping has no other `CurrentlyFaulting`.
    /// `end` is called on `self` before the object lock is released, `self` is deallocated or
    /// moved, or `'a` ends. Until then, the pages that map the range in the mapping may be
    /// unmapped, no `&mut` to `self` exists, and it is not written except through its `Cell`
    /// fields.
    unsafe fn begin(&self, token: &mut LockToken<'_, VmObjectLockClass>) {
        debug_assert!(self.mapping.currently_faulting(token).is_none());
        // SAFETY: `self` is a `CurrentlyFaulting` for its mapping that stays live and is not moved
        // until `end` resets `currently_faulting`. Until then, the object lock stays held, no
        // `&mut` to `self` exists, and it is not written except through its `Cell` fields.
        unsafe { self.mapping.set_currently_faulting(token, Some(NonNull::from(self))) };
    }

    /// Called to say that the given range needs to be unmapped. This returns true if updating the
    /// range will be handled by the faulting thread and that the unmap can therefore be skipped.
    /// Returns false if the caller should unmap themselves.
    fn unmap_range(&self, object_offset: u64, len: u64) -> bool {
        debug_assert!(self.state.get() != CurrentlyFaultingState::Completed);
        if range_check::intersects_offset_len(object_offset, len, self.object_offset, self.len) {
            self.state.set(CurrentlyFaultingState::UnmapSkipped);
            return true;
        }
        false
    }

    /// Called to indicate that the mapping for the fault location has been updated successfully.
    /// This acts to cancel the unmap that would otherwise happen when this object goes out of
    /// scope.
    fn mapping_updated(&self) {
        self.state.set(CurrentlyFaultingState::Completed);
    }

    /// # Safety
    ///
    /// `begin` was called on `self`, `end` has not been called on it since, and the ongoing
    /// preconditions of `begin` still hold.
    unsafe fn end(&self, token: &mut LockToken<'_, VmObjectLockClass>) {
        // If the caller did not call `mapping_updated`, and an unmap was skipped, then we must
        // unmap the range ourselves. We only do the unmap here if a prior unmap was skipped to
        // avoid needless unmaps due to transient errors such as needing to wait on a page request.
        if self.state.get() == CurrentlyFaultingState::UnmapSkipped {
            // SAFETY: `begin` requires that `self.object_offset` and `self.len` are page aligned,
            // `[self.object_offset, self.object_offset + self.len)` is in the range of the VMO
            // that `self.mapping` maps, and the pages that map that range in `self.mapping` may be
            // unmapped until now.
            unsafe { self.unmap_skipped(token) };
        }
        // SAFETY: `faulting` is `None`.
        unsafe { self.mapping.set_currently_faulting(token, None) };
    }

    /// # Safety
    ///
    /// `self.object_offset` and `self.len` are page aligned,
    /// `[self.object_offset, self.object_offset + self.len)` is in the range of the VMO that
    /// `self.mapping` maps, and the pages that map that range in `self.mapping` may be unmapped.
    #[cold]
    #[inline(never)]
    unsafe fn unmap_skipped(&self, token: &LockToken<'_, VmObjectLockClass>) {
        let (base, new_len) =
            self.mapping.object_range_to_vaddr_range(token, self.object_offset, self.len).unwrap();
        assert!(new_len == self.len);
        let aspace = self.mapping.aspace();
        // SAFETY: `[base, base + new_len)` maps the range of the VMO given to `new`, whose pages
        // may be unmapped.
        let status = unsafe {
            aspace.arch_aspace().unmap(
                base.0,
                new_len as usize / kernel_page::SIZE,
                aspace.enlarge_arch_unmap(),
            )
        };
        assert!(status.is_ok());
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
/// `storage` is aligned and valid for writes of a `CurrentlyFaulting`. `object_offset` and `len`
/// are page aligned, `[object_offset, object_offset + len)` is in the range of the VMO that
/// `mapping` maps, and `mapping` has no other `CurrentlyFaulting`. The object lock is held.
/// `storage` is passed to `rust_vm_mapping_currently_faulting_end` before the object lock is
/// released, `storage` is deallocated or moved, or `mapping`'s lifetime ends. Until then, the pages
/// that map the range in `mapping` may be unmapped, and `storage` is not accessed except by passing
/// it to `rust_vm_mapping_currently_faulting_mapping_updated` and
/// `rust_vm_mapping_currently_faulting_end`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_mapping_currently_faulting_begin<'a>(
    storage: *mut CurrentlyFaulting<'a>,
    mapping: &'a VmMapping,
    object_offset: u64,
    len: u64,
) {
    // SAFETY: `storage` is aligned and valid for writes of a `CurrentlyFaulting`.
    unsafe { storage.write(CurrentlyFaulting::new(mapping, object_offset, len)) };
    // SAFETY: The object lock is held by the caller.
    let mut token = unsafe { LockToken::new() };
    // SAFETY: `storage` holds a `CurrentlyFaulting`. `object_offset` and `len` are page aligned,
    // the range is in the range of the VMO that `mapping` maps, and `mapping` has no other
    // `CurrentlyFaulting`. `rust_vm_mapping_currently_faulting_end` calls `end` on `storage` before
    // the object lock is released, `storage` is deallocated or moved, or `'a` ends. Until then, the
    // pages that map the range in `mapping` may be unmapped, and `storage` is only accessed through
    // shared references, which do not write it except through its `Cell` fields.
    unsafe { (*storage).begin(&mut token) };
}

/// # Safety
///
/// `storage` holds a `CurrentlyFaulting` initialized by `rust_vm_mapping_currently_faulting_begin`
/// and not yet passed to `rust_vm_mapping_currently_faulting_end`, and the ongoing preconditions of
/// `rust_vm_mapping_currently_faulting_begin` still hold. The object lock is held.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_mapping_currently_faulting_mapping_updated(
    storage: &CurrentlyFaulting<'_>,
) {
    storage.mapping_updated();
}

/// # Safety
///
/// `storage` holds a `CurrentlyFaulting` initialized by `rust_vm_mapping_currently_faulting_begin`
/// and not yet passed to `rust_vm_mapping_currently_faulting_end`, and the ongoing preconditions of
/// `rust_vm_mapping_currently_faulting_begin` still hold. The object lock is held.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_mapping_currently_faulting_end(storage: &CurrentlyFaulting<'_>) {
    // SAFETY: The object lock is held by the caller.
    let mut token = unsafe { LockToken::new() };
    // SAFETY: `storage` was passed to `rust_vm_mapping_currently_faulting_begin`, which called
    // `begin` on it, and has not been passed to `rust_vm_mapping_currently_faulting_end` since.
    // The preconditions of `rust_vm_mapping_currently_faulting_begin`, and so the ongoing
    // preconditions of `begin`, still hold.
    unsafe { storage.end(&mut token) };
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

/// # Safety
///
/// `size` is not zero, `base` and `size` are page aligned, `[base, base + size)` is within `this`
/// and may have its permissions changed and be unmapped, and `new_arch_mmu_flags` has no cache
/// flags and is valid for `this`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_debug_protect(
    this: &VmMapping,
    base: usize,
    size: usize,
    new_arch_mmu_flags: ArchMmuFlags,
) -> zx_status_t {
    // SAFETY: `size` is not zero, `base` and `size` are page aligned, `[base, base + size)` is
    // within `this` and may have its permissions changed and be unmapped, and
    // `new_arch_mmu_flags` has no cache flags and is valid for `this`.
    Status::result_into_raw(unsafe { this.debug_protect(base, size, new_arch_mmu_flags) })
}

/// # Safety
///
/// The aspace lock is held. `size` is not zero, `base` and `size` are page aligned,
/// `[base, base + size)` is within `this` and may have its permissions changed and be unmapped,
/// and `new_arch_mmu_flags` has no cache flags and is valid for `this`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_protect_locked(
    this: &VmMapping,
    base: usize,
    size: usize,
    new_arch_mmu_flags: ArchMmuFlags,
) -> zx_status_t {
    // SAFETY: The aspace lock is held by the caller.
    let mut token = unsafe { LockToken::new() };
    // SAFETY: `size` is not zero, `base` and `size` are page aligned, `[base, base + size)` is
    // within `this` and may have its permissions changed and be unmapped, and
    // `new_arch_mmu_flags` has no cache flags and is valid for `this`.
    Status::result_into_raw(unsafe {
        this.protect_locked(&mut token, base, size, new_arch_mmu_flags)
    })
}

/// # Safety
///
/// The object lock is held, `this` is in its VMO's mapping list, `offset` and `len` are page
/// aligned, and the pages that map `[offset, offset + len)` of the VMO in `this` may be unmapped.
/// If `UnmapOptions::ONLY_HAS_ZERO_PAGES` is set in `options`, any mappings of that range only map
/// the shared zero page.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_aspace_unmap_locked_object(
    this: &VmMapping,
    offset: u64,
    len: u64,
    options: UnmapOptions,
) {
    // SAFETY: The object lock is held by the caller.
    let token = unsafe { LockToken::new() };
    // SAFETY: `this` is in its VMO's mapping list, `offset` and `len` are page aligned, and the
    // pages that map `[offset, offset + len)` of the VMO in `this` may be unmapped. If
    // `UnmapOptions::ONLY_HAS_ZERO_PAGES` is set in `options`, any mappings of that range only map
    // the shared zero page.
    unsafe { this.aspace_unmap_locked_object(&token, offset, len, options) };
}

/// # Safety
///
/// The object lock is held, `this` is in its VMO's mapping list, `offset` and `len` are page
/// aligned, and the pages that map `[offset, offset + len)` of the VMO in `this` may have write
/// permission removed, and may be unmapped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_aspace_remove_write_locked_object(
    this: &VmMapping,
    offset: u64,
    len: u64,
) {
    // SAFETY: The object lock is held by the caller.
    let token = unsafe { LockToken::new() };
    // SAFETY: `this` is in its VMO's mapping list, `offset` and `len` are page aligned, and the
    // pages that map `[offset, offset + len)` of the VMO in `this` may have write permission
    // removed, and may be unmapped.
    unsafe { this.aspace_remove_write_locked_object(&token, offset, len) };
}

/// # Safety
///
/// The object lock is held, and `this` is in its VMO's mapping list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_aspace_debug_unpin_locked_object(
    this: &VmMapping,
    offset: u64,
    len: u64,
) {
    // SAFETY: The object lock is held by the caller.
    let token = unsafe { LockToken::new() };
    this.aspace_debug_unpin_locked_object(&token, offset, len);
}

/// # Safety
///
/// The aspace lock is held, and `out_ranges` is empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_copy_protection_ranges_locked(
    this: &VmMapping,
    base: usize,
    size: usize,
    out_first_flags: &mut MaybeUninit<ArchMmuFlags>,
    out_ranges: Pin<&mut ProtectionRanges>,
) -> zx_status_t {
    // SAFETY: The aspace lock is held by the caller.
    let token = unsafe { LockToken::new() };
    // SAFETY: `out_ranges` is empty.
    let result = unsafe { this.copy_protection_ranges_locked(&token, base, size, out_ranges) };
    Status::result_into_raw(result.map(|first_flags| {
        out_first_flags.write(first_flags);
    }))
}

/// # Safety
///
/// The aspace lock and the object lock of `this` and of `right` are held, `right` is not
/// `this`, and `right.base()` and the keys of the protection ranges of `right` are distinct from
/// each other and absent from the protection ranges of `this`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_merge_protection_ranges_locked(
    this: &VmMapping,
    right: &VmMapping,
) -> zx_status_t {
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    // SAFETY: The object lock is held by the caller.
    let mut object_token = unsafe { LockToken::new() };
    // SAFETY: `right` is not `this`, and `right.base()` and the keys of the protection ranges of
    // `right` are distinct from each other and absent from the protection ranges of `this`.
    Status::result_into_raw(unsafe {
        this.merge_protection_ranges_locked(&mut aspace_token, &mut object_token, right)
    })
}

/// # Safety
///
/// The aspace lock and the object lock are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_clear_protection_range_transitions_locked(
    this: &VmMapping,
    base: usize,
    end: usize,
) {
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    // SAFETY: The object lock is held by the caller.
    let mut object_token = unsafe { LockToken::new() };
    this.clear_protection_range_transitions_locked(&mut aspace_token, &mut object_token, base, end);
}

/// # Safety
///
/// The aspace lock and the object lock are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_remove_after_split_locked(
    this: &VmMapping,
    split_addr: usize,
) -> ArchMmuFlags {
    // SAFETY: The aspace lock is held by the caller.
    let mut aspace_token = unsafe { LockToken::new() };
    // SAFETY: The object lock is held by the caller.
    let mut object_token = unsafe { LockToken::new() };
    this.remove_after_split_locked(&mut aspace_token, &mut object_token, split_addr)
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_create(
    parent: &VmAddressRegion,
    private_clone: bool,
    base: usize,
    size: usize,
    vmar_flags: u32,
    vmo: RefPtr<VmObject>,
    vmo_offset: u64,
    arch_mmu_flags: ArchMmuFlags,
    mergeable: Mergeable,
) -> Option<RefPtr<VmMapping>> {
    let init = VmMapping::new(
        parent,
        private_clone,
        base,
        size,
        vmar_flags,
        vmo,
        vmo_offset,
        arch_mmu_flags,
        mergeable,
    );
    RefPtr::pin_init(init).ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_mapping_create_with_ranges(
    parent: &VmAddressRegion,
    private_clone: bool,
    base: usize,
    size: usize,
    vmar_flags: u32,
    vmo: RefPtr<VmObject>,
    vmo_offset: u64,
    first_mmu_flags: ArchMmuFlags,
    ranges: Pin<&mut ProtectionRanges>,
    mergeable: Mergeable,
) -> Option<RefPtr<VmMapping>> {
    let init = VmMapping::new_with_ranges(
        parent,
        private_clone,
        base,
        size,
        vmar_flags,
        vmo,
        vmo_offset,
        first_mmu_flags,
        ranges,
        mergeable,
    );
    RefPtr::pin_init(init).ok()
}

/// # Safety
///
/// `this` is the last reference to a mapping allocated with `kalloc::Box`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_mapping_recycle(this: NonNull<VmMapping>) {
    // SAFETY: `this` is the last reference to a mapping allocated with `kalloc::Box`.
    unsafe { VmMapping::recycle(this) }
}
