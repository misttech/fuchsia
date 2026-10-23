// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::{
    ArchMmuFlags, ArchUnmapOptions, ArchVmAspace, NonTerminalAction, TerminalAction,
};
use super::name::name_to_c_buf;
use super::vm_address_region::VmAddressRegion;
use super::vm_mapping::VmMapping;
use super::vm_object::VmObject;
use crate::kernel::thread::ThreadPtr;
use crate::kernel::types::PAddr;
use core::ffi::{CStr, c_void};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use fbl::RefPtr;
use ksync::{KMutex, LockClass, LockToken, RawCriticalMutex};
use lazy_init::{LazyInit, NoCheck};
use vm_aspace_bindings as bindings;
use zx_status::Status;

/// For region creation routines
pub mod vmm_flag {
    use super::bindings;

    /// allocate at specific address
    pub const VALLOC_SPECIFIC: u32 = bindings::VmAspace_VMM_FLAG_VALLOC_SPECIFIC;

    /// commit memory up front (no demand paging)
    pub const COMMIT: u32 = bindings::VmAspace_VMM_FLAG_COMMIT;
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    User = 0,
    Kernel = 1,
    /// You probably do not want to use `LowKernel`. It is primarily used for SMP bootstrap
    /// to allow mappings of very low memory using the standard VMM subsystem.
    LowKernel = 2,
    /// Used to construct an address space representing hypervisor guest memory.
    GuestPhysical = 3,
}

zr::static_assert!(Type::User as u32 == bindings::VmAspace_Type::User.0 as u32);
zr::static_assert!(Type::Kernel as u32 == bindings::VmAspace_Type::Kernel.0 as u32);
zr::static_assert!(Type::LowKernel as u32 == bindings::VmAspace_Type::LowKernel.0 as u32);
zr::static_assert!(Type::GuestPhysical as u32 == bindings::VmAspace_Type::GuestPhysical.0 as u32);

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareOpt {
    /// A normal independent address space initialized using [`ArchVmAspace::init`].
    None = 0,
    /// A restricted address space whose underlying [`ArchVmAspace`] will be initialized using
    /// `init_restricted`.
    Restricted = 1,
    /// A shared address space whose underlying [`ArchVmAspace`] will be initialized using
    /// `init_shared`.
    Shared = 2,
}

zr::static_assert!(ShareOpt::None as u32 == bindings::VmAspace_ShareOpt::None.0 as u32);
zr::static_assert!(ShareOpt::Restricted as u32 == bindings::VmAspace_ShareOpt::Restricted.0 as u32);
zr::static_assert!(ShareOpt::Shared as u32 == bindings::VmAspace_ShareOpt::Shared.0 as u32);

/// Primary lock that is used by all objects (VMARs and mappings) in the aspace hierarchy, and
/// serializes most modifications. Holding this lock is not sufficient to perform allocations or
/// deallocations (i.e. access the `subregions` field in VMARs and transition objects to/from
/// `Alive`). For this the `region_lock` must also be held.
#[derive(Debug, Copy, Clone)]
pub struct VmAspaceLockClass;

impl LockClass for VmAspaceLockClass {
    const ID: *mut c_void = core::ptr::null_mut();
}

/// Conceptually the `region_lock` represents the authority to manipulate the portions of the
/// address space that are allocated. This is implemented by requiring both `lock` and
/// `region_lock` to be held in order to modify the `subregions` list, or transition a
/// vmar/mapping to/from the `Alive` state. As a consequence, holding either `lock` OR
/// `region_lock` is sufficient to know that any presently `Alive` objects cannot change state,
/// and that the `subregions` can be safely iterated.
/// Where both must be held, the `region_lock` is to be acquired before `lock`.
#[derive(Debug, Copy, Clone)]
pub struct VmAspaceRegionLockClass;

impl LockClass for VmAspaceRegionLockClass {
    const ID: *mut c_void = core::ptr::null_mut();
}

pub type VmAspaceMutex = KMutex<VmAspaceLockClass, RawCriticalMutex>;

pub type VmAspaceRegionMutex = KMutex<VmAspaceRegionLockClass, RawCriticalMutex>;

#[repr(C)]
pub struct VmAspace {
    _facade: fbl::OpaqueRefCountedFacade,
}

impl fbl::HasRefCount for VmAspace {
    #[inline]
    fn ref_count(&self) -> &fbl::RefCounted {
        // SAFETY: `cpp_vm_aspace_get_ref_counted` returns a valid pointer to the C++
        // `fbl::RefCounted` subobject of `VmAspace`.
        unsafe {
            &*(bindings::cpp_vm_aspace_get_ref_counted(self.as_ffi_ptr()).cast::<fbl::RefCounted>())
        }
    }
}

// SAFETY: `VmAspace` represents a C++ `fbl::RefCounted` object.
unsafe impl fbl::Recyclable for VmAspace {
    #[cold]
    #[inline(never)]
    unsafe fn recycle(ptr: NonNull<Self>) {
        // SAFETY: Caller attests to preconditions.
        unsafe {
            bindings::cpp_vm_aspace_free(ptr.as_ptr().cast());
        }
    }
}

impl VmAspace {
    /// Creates an address space of the type specified in `type_` with name `name`.
    ///
    /// Although reference counted, the returned [`VmAspace`] must be explicitly destroyed via
    /// [`destroy`](Self::destroy).
    ///
    /// Returns `None` on failure (e.g. due to resource starvation).
    pub fn create(type_: Type, name: &[u8]) -> Option<RefPtr<VmAspace>> {
        let name_buf = name_to_c_buf(name);
        // SAFETY: `name_buf` is a NUL-terminated C string.
        unsafe {
            Self::import_from_raw_ptr(bindings::cpp_vm_aspace_create(
                bindings::VmAspace_Type(type_ as _),
                name_buf.as_ptr().cast(),
            ))
        }
    }

    pub(super) fn as_ffi_ptr(&self) -> *mut bindings::VmAspace {
        ptr::from_ref(self).cast_mut().cast()
    }

    /// # Safety
    ///
    /// `raw` must be null or a valid pointer to a `VmAspace` exported from C++ via
    /// `fbl::ExportToRawPtr` with an acquired reference count.
    unsafe fn import_from_raw_ptr(raw: *mut bindings::VmAspace) -> Option<RefPtr<Self>> {
        // SAFETY: The caller guarantees `raw` is null or points to a live `VmAspace` with an
        // acquired reference count.
        unsafe { RefPtr::try_from_raw(raw.cast::<Self>()) }
    }

    pub fn lock(&self) -> &VmAspaceMutex {
        // SAFETY: `core::ptr::from_ref(self)` points to a live `VmAspace`.
        unsafe {
            &*bindings::cpp_vm_aspace_lock(core::ptr::from_ref(self).cast()).cast::<VmAspaceMutex>()
        }
    }

    pub fn region_lock(&self) -> &VmAspaceRegionMutex {
        // SAFETY: `core::ptr::from_ref(self)` points to a live `VmAspace`.
        unsafe {
            &*bindings::cpp_vm_aspace_region_lock(core::ptr::from_ref(self).cast())
                .cast::<VmAspaceRegionMutex>()
        }
    }

    /// Creates an address space of the type specified in `type_` with name `name`.
    ///
    /// The returned aspace will start at `base` and span `size`.
    ///
    /// If `share_opt` is [`ShareOpt::Shared`], we're creating a shared address space, and the
    /// underlying [`ArchVmAspace`] will be initialized using the
    /// [`init_shared`](ArchVmAspace::init_shared) method instead of the normal
    /// [`init`](ArchVmAspace::init) method.
    ///
    /// If `share_opt` is [`ShareOpt::Restricted`], we're creating a restricted address space, and
    /// the underlying [`ArchVmAspace`] will be initialized using the
    /// [`init_restricted`](ArchVmAspace::init_restricted) method.
    ///
    /// Although reference counted, the returned [`VmAspace`] must be explicitly destroyed via
    /// [`destroy`](Self::destroy).
    ///
    /// Returns `None` on failure (e.g. due to resource starvation).
    pub fn create_with_opts(
        base: usize,
        size: usize,
        type_: Type,
        name: &[u8],
        share_opt: ShareOpt,
    ) -> Option<RefPtr<VmAspace>> {
        let name_buf = name_to_c_buf(name);
        // SAFETY: `name_buf` is a NUL-terminated C string.
        unsafe {
            Self::import_from_raw_ptr(bindings::cpp_vm_aspace_create_with_opts(
                base,
                size,
                bindings::VmAspace_Type(type_ as _),
                name_buf.as_ptr().cast(),
                bindings::VmAspace_ShareOpt(share_opt as _),
            ))
        }
    }

    /// Creates a unified address space that consists of the given constituent address spaces.
    ///
    /// The passed in address spaces must meet the following criteria:
    /// 1. They must manage non-overlapping regions.
    /// 2. The shared [`VmAspace`] must have been created with the shared argument set to true.
    ///
    /// Although reference counted, the returned [`VmAspace`] must be explicitly destroyed via
    /// [`destroy`](Self::destroy). Note that it must be destroyed before the shared and
    /// restricted [`VmAspace`]s; destroying the constituent [`VmAspace`]s before destroying
    /// this one will trigger asserts.
    ///
    /// Returns `None` on failure (e.g. due to resource starvation).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `shared` and `restricted` are valid pointers to address
    /// spaces initialized as shared and restricted respectively.
    pub unsafe fn create_unified(
        shared: *mut VmAspace,
        restricted: *mut VmAspace,
        name: &[u8],
    ) -> Option<RefPtr<VmAspace>> {
        let name_buf = name_to_c_buf(name);
        // SAFETY: `shared` and `restricted` point to live `VmAspace`s and `name_buf` is a
        // NUL-terminated C string.
        unsafe {
            Self::import_from_raw_ptr(bindings::cpp_vm_aspace_create_unified(
                shared.cast(),
                restricted.cast(),
                name_buf.as_ptr().cast(),
            ))
        }
    }

    /// Destroys this address space.
    ///
    /// `destroy()` does not free this object, but rather allows it to be freed when the last
    /// retaining `RefPtr` is destroyed.
    pub fn destroy(&self) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        Status::ok(unsafe { bindings::cpp_vm_aspace_destroy(self.as_ffi_ptr()) })
    }

    /// Renames this address space.
    pub fn rename(&self, name: &[u8]) {
        let name_buf = name_to_c_buf(name);
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace` and `name_buf` is a
        // NUL-terminated C string.
        unsafe { bindings::cpp_vm_aspace_rename(self.as_ffi_ptr(), name_buf.as_ptr().cast()) }
    }

    pub fn base(&self) -> usize {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { bindings::cpp_vm_aspace_base(self.as_ffi_ptr()) }
    }

    pub fn size(&self) -> usize {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { bindings::cpp_vm_aspace_size(self.as_ffi_ptr()) }
    }

    /// # Safety
    ///
    /// The caller must ensure no concurrent `rename` occurs for the duration of use.
    pub unsafe fn name_ptr(&self) -> &CStr {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { CStr::from_ptr(bindings::cpp_vm_aspace_name(self.as_ffi_ptr())) }
    }

    /// Returns a reference to the architecturally specific part of the address space
    /// (`ArchVmAspace`). This is internally locked and does not need to be guarded by `lock_`.
    pub fn arch_aspace(&self) -> &ArchVmAspace {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { &*bindings::cpp_vm_aspace_arch_aspace(self.as_ffi_ptr()).cast() }
    }

    pub fn is_user(&self) -> bool {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { bindings::cpp_vm_aspace_is_user(self.as_ffi_ptr()) }
    }

    /// Encodes the idea that we can always unmap from user aspaces.
    pub(super) fn enlarge_arch_unmap(&self) -> ArchUnmapOptions {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { bindings::cpp_vm_aspace_enlarge_arch_unmap(self.as_ffi_ptr()) }
    }

    /// Keep a cache of the `VmMapping` of the last page fault that occurred. On a page fault this
    /// can be checked to see if it matches more quickly than walking the full vmar tree. Mappings
    /// that are stored here must be in the `Alive` state, implying that they are in the VMAR tree.
    /// It is then the responsibility of the `VmMapping` to remove itself from here should it
    /// transition out of `Alive`, and remove itself from the VMAR tree. A raw pointer is stored
    /// here since the `VmMapping` must be alive and in tree anyway and if it were a `RefPtr` we
    /// would not be able to handle being the one to drop the last ref and perform destruction.
    pub(super) fn last_fault_locked(
        &self,
        _token: &LockToken<'_, VmAspaceLockClass>,
    ) -> *const VmMapping {
        // SAFETY: The aspace lock is held, proven by `_token`.
        unsafe { bindings::cpp_vm_aspace_last_fault_locked(self.as_ffi_ptr()).cast() }
    }

    pub(super) fn clear_last_fault_locked(&self, _token: &mut LockToken<'_, VmAspaceLockClass>) {
        // SAFETY: The aspace lock is held, proven by `_token`.
        unsafe {
            bindings::cpp_vm_aspace_clear_last_fault_locked(self.as_ffi_ptr());
        }
    }

    pub(super) fn vdso_code_mapping_locked(
        &self,
        _token: &LockToken<'_, VmAspaceLockClass>,
    ) -> *const VmMapping {
        // SAFETY: The aspace lock is held, proven by `_token`.
        unsafe { bindings::cpp_vm_aspace_vdso_code_mapping_locked(self.as_ffi_ptr()).cast() }
    }

    pub fn is_aslr_enabled(&self) -> bool {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { bindings::cpp_vm_aspace_is_aslr_enabled(self.as_ffi_ptr()) }
    }

    /// Returns true if this address space has been destroyed.
    pub fn is_destroyed(&self) -> bool {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { bindings::cpp_vm_aspace_is_destroyed(self.as_ffi_ptr()) }
    }

    /// Returns the singleton kernel address space.
    pub fn kernel_aspace() -> &'static VmAspace {
        // SAFETY: The kernel aspace is a live, static `VmAspace`.
        unsafe { &*(bindings::cpp_vm_aspace_kernel_aspace() as *const VmAspace) }
    }

    /// Returns the root address region (`RootVmar`) for this address space.
    pub fn root_vmar(&self) -> Option<RefPtr<VmAddressRegion>> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { RefPtr::try_from_raw(bindings::cpp_vm_aspace_root_vmar(self.as_ffi_ptr()).cast()) }
    }

    /// Returns the root address region (`RootVmarLocked`) while holding the aspace lock.
    pub fn root_vmar_locked(
        &self,
        _token: &LockToken<'_, VmAspaceLockClass>,
    ) -> Option<RefPtr<VmAddressRegion>> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace` and the aspace lock is held.
        unsafe {
            RefPtr::try_from_raw(bindings::cpp_vm_aspace_root_vmar_locked(self.as_ffi_ptr()).cast())
        }
    }

    /// Sets the per-thread address space pointer to this address space.
    pub fn attach_to_thread(&self, thread: ThreadPtr) {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace` and `thread.as_raw()` points to
        // a live `Thread`.
        unsafe {
            bindings::cpp_vm_aspace_attach_to_thread(self.as_ffi_ptr(), thread.as_raw().cast())
        }
    }

    pub fn dump(&self, verbose: bool) {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { bindings::cpp_vm_aspace_dump(self.as_ffi_ptr(), verbose) }
    }

    pub fn drop_all_user_page_tables() {
        // SAFETY: Dropping all user page tables has no safety preconditions.
        unsafe { bindings::cpp_vm_aspace_drop_all_user_page_tables() }
    }

    pub fn drop_user_page_tables(&self) {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { bindings::cpp_vm_aspace_drop_user_page_tables(self.as_ffi_ptr()) }
    }

    pub fn dump_all_aspaces(verbose: bool) {
        // SAFETY: Dumping all address spaces has no safety preconditions.
        unsafe { bindings::cpp_vm_aspace_dump_all_aspaces(verbose) }
    }

    /// Harvests all accessed information across all user mappings and updates any page age
    /// information for terminal mappings, and potentially harvests page tables depending on the
    /// passed in action.
    ///
    /// This requires holding `aspaces_list_lock_` over the entire duration and
    /// whilst not a commonly used lock this function should still only be called infrequently to
    /// avoid monopolizing the lock.
    pub fn harvest_all_user_accessed_bits(
        non_terminal_action: NonTerminalAction,
        terminal_action: TerminalAction,
    ) {
        // SAFETY: Harvesting user accessed bits has no safety preconditions.
        unsafe {
            bindings::cpp_vm_aspace_harvest_all_user_accessed_bits(
                non_terminal_action as _,
                terminal_action as _,
            )
        }
    }

    /// Generates a soft fault against this address space.
    ///
    /// This is similar to `page_fault` except:
    /// * This address space may not currently be active and this does not have to be called from
    ///   the hardware exception handler.
    /// * May be invoked spuriously in situations where the hardware mappings would have prevented a
    ///   real `page_fault` from occurring.
    ///
    /// May block on page requests and must be called without locks held.
    pub fn soft_fault(&self, va: usize, flags: u32) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        Status::ok(unsafe { bindings::cpp_vm_aspace_soft_fault(self.as_ffi_ptr(), va, flags) })
    }

    /// Similar to `soft_fault`, but additionally takes a length indicating that the range of
    /// `[va, va+len)` is expected to be accessed with `flags` after resolving this fault. The
    /// address space can take this range as a hint to attempt to preemptively avoid future faults.
    ///
    /// There are no alignment restrictions on `va` or `len`, although it is assumed that `len` is
    /// greater than zero.
    pub fn soft_fault_in_range(&self, va: usize, flags: u32, len: usize) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        Status::ok(unsafe {
            bindings::cpp_vm_aspace_soft_fault_in_range(self.as_ffi_ptr(), va, flags, len)
        })
    }

    /// Generates an accessed flag fault against this address space.
    ///
    /// This is a specialized version of `soft_fault` that will only resolve a potential missing
    /// access flag and nothing else.
    pub fn accessed_fault(&self, va: usize) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        Status::ok(unsafe { bindings::cpp_vm_aspace_accessed_fault(self.as_ffi_ptr(), va) })
    }

    /// Page fault routine.
    ///
    /// Should only be called by the hypervisor or by `Thread::Current::Fault`.
    pub fn page_fault(&self, va: usize, flags: u32) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        Status::ok(unsafe { bindings::cpp_vm_aspace_page_fault(self.as_ffi_ptr(), va, flags) })
    }

    /// Legacy function to assist in the transition to VMARs.
    ///
    /// Assumes a flat VMAR structure in which all VMOs are mapped as children of the root.
    /// Will assert if used on user address spaces.
    ///
    /// # Safety
    ///
    /// The caller must ensure `ptr` points to a valid memory location that can hold the allocated
    /// address or specific starting address.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn alloc_physical(
        &self,
        name: &[u8],
        size: usize,
        ptr: *mut *mut c_void,
        align_pow2: u8,
        paddr: PAddr,
        vmm_flags: u32,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        let name_buf = name_to_c_buf(name);
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`, `name_buf` is a NUL-terminated
        // C string, and `ptr` is valid for writes.
        Status::ok(unsafe {
            bindings::cpp_vm_aspace_alloc_physical(
                self.as_ffi_ptr(),
                name_buf.as_ptr().cast(),
                size,
                ptr,
                align_pow2,
                paddr.0,
                vmm_flags,
                arch_mmu_flags,
            )
        })
    }

    /// Legacy function to assist in the transition to VMARs.
    ///
    /// Assumes a flat VMAR structure in which all VMOs are mapped as children of the root.
    /// Will assert if used on user address spaces.
    ///
    /// # Safety
    ///
    /// The caller must ensure `ptr` points to a valid memory location that can hold the allocated
    /// address or specific starting address.
    pub unsafe fn alloc_contiguous(
        &self,
        name: &[u8],
        size: usize,
        ptr: *mut *mut c_void,
        align_pow2: u8,
        vmm_flags: u32,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        let name_buf = name_to_c_buf(name);
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`, `name_buf` is a NUL-terminated
        // C string, and `ptr` is valid for writes.
        Status::ok(unsafe {
            bindings::cpp_vm_aspace_alloc_contiguous(
                self.as_ffi_ptr(),
                name_buf.as_ptr().cast(),
                size,
                ptr,
                align_pow2,
                vmm_flags,
                arch_mmu_flags,
            )
        })
    }

    /// Legacy function to assist in the transition to VMARs.
    ///
    /// Assumes a flat VMAR structure in which all VMOs are mapped as children of the root.
    /// Will assert if used on user address spaces.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the virtual address range being freed is no longer in use.
    pub unsafe fn free_region(&self, va: usize) -> Result<(), Status> {
        // SAFETY: `self.as_ffi_ptr() to a live `VmAspace`.
        Status::ok(unsafe { bindings::cpp_vm_aspace_free_region(self.as_ffi_ptr(), va) })
    }

    /// Internal use function for mapping VMOs.  Do not use.  This is exposed in
    /// the public API purely for tests.
    ///
    /// # Safety
    ///
    /// The caller must ensure that creating a mapping with the specified range and flags is sound.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn map_object_internal(
        &self,
        vmo: RefPtr<VmObject>,
        name: &[u8],
        offset: u64,
        size: usize,
        align_pow2: u8,
        vmm_flags: u32,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<*mut c_void, Status> {
        let mut ptr = core::ptr::null_mut();
        let name_buf = name_to_c_buf(name);
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`, `name_buf` is a NUL-terminated
        // C string, and `ptr` is valid for writes.
        Status::ok(unsafe {
            bindings::cpp_vm_aspace_map_object_internal(
                self.as_ffi_ptr(),
                VmObject::cast_raw(RefPtr::into_raw(vmo).cast_mut()),
                name_buf.as_ptr().cast(),
                offset,
                size,
                &mut ptr,
                align_pow2,
                vmm_flags,
                arch_mmu_flags,
            )
        })?;
        Ok(ptr)
    }

    /// Returns the vDSO base address for this address space.
    pub fn vdso_base_address(&self) -> usize {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { bindings::cpp_vm_aspace_vdso_base_address(self.as_ffi_ptr()) }
    }

    /// Returns the vDSO code address for this address space.
    pub fn vdso_code_address(&self) -> usize {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { bindings::cpp_vm_aspace_vdso_code_address(self.as_ffi_ptr()) }
    }

    /// Returns whether this address space is currently set to be a high memory priority.
    pub fn is_high_memory_priority(&self) -> bool {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        unsafe { bindings::cpp_vm_aspace_is_high_memory_priority(self.as_ffi_ptr()) }
    }

    /// Increments or decrements the priority count of this aspace. The high priority count is used
    /// to control active page table reclamation, and applies to the whole aspace. The count is
    /// never allowed to go negative and so callers must only subtract what they have already added.
    /// Further, callers are required to remove any additions before the aspace is destroyed.
    pub(super) fn change_high_priority_count_locked(
        &self,
        _token: &mut LockToken<'_, VmAspaceLockClass>,
        delta: i64,
    ) {
        // SAFETY: `self` is a live `VmAspace` and the aspace lock is held, proven by `_token`.
        unsafe {
            bindings::cpp_vm_aspace_change_high_priority_count_locked(self.as_ffi_ptr(), delta)
        }
    }

    /// Finds the memory mapping for `vaddr` in this address space.
    pub fn find_mapping(&self, vaddr: usize) -> Option<RefPtr<VmMapping>> {
        // SAFETY: `self.as_ffi_ptr()` points to a live `VmAspace`.
        let ptr = unsafe { bindings::cpp_vm_aspace_find_mapping(self.as_ffi_ptr(), vaddr) };
        // SAFETY: `ptr` is null or points to a live `VmMapping` with an acquired refcount.
        unsafe { RefPtr::try_from_raw(ptr.cast()) }
    }
}

static KERNEL_ROOT_VMAR: LazyInit<VmAddressRegion, NoCheck> = LazyInit::uninit();

/// # Safety
///
/// `kernel_aspace` is the kernel aspace. Must be called at most once, before any other use of
/// the kernel root region. The last reference to the returned region must never be released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_address_region_kernel_root_init(
    kernel_aspace: &VmAspace,
) -> &'static VmAddressRegion {
    // SAFETY: `kernel_aspace` is the kernel aspace, and this is its only root region.
    let init = unsafe { VmAddressRegion::new_kernel_root(kernel_aspace) };
    // SAFETY: `KERNEL_ROOT_VMAR` is initialized at most once, before any other use.
    let Ok(()) = unsafe { Pin::static_ref(&KERNEL_ROOT_VMAR).init_pin(init) };
    // SAFETY: `KERNEL_ROOT_VMAR` is initialized.
    unsafe { KERNEL_ROOT_VMAR.get_unchecked() }
}
