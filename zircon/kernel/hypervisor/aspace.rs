// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Guest physical address space management (`<hypervisor/aspace.h>`).

use core::ffi::c_void;
use core::mem::{align_of, size_of};
use core::ptr::with_exposed_provenance_mut;
use fbl::RefPtr;
use ksync::LockToken;
use range_check::in_range_max;
use zr::static_assert;
use zx_status::Status;
use zx_types::{zx_gpaddr_t, zx_vaddr_t};

use crate::kernel::types::{PAddr, VAddr};
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_CACHED, ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ,
    ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED_DEVICE, ArchMmuFlags, ArchVmAspace,
};
use crate::vm::fault::flag as pf_flag;
use crate::vm::page_source::MultiPageRequest;
use crate::vm::pinned_vm_object::PinnedVmObject;
use crate::vm::vm_address_region::{VmAddressRegion, VmAddressRegionOpChildren, flag as vmar_flag};
use crate::vm::vm_aspace::{Type, VmAspace, VmAspaceLockClass};
use crate::vm::vm_mapping::VmMapping;
use crate::vm::vm_object_physical::VmObjectPhysical;

const INTERRUPT_MMU_FLAGS: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;
const GUEST_MMU_FLAGS: ArchMmuFlags =
    ARCH_MMU_FLAG_CACHED | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

/// RAII object that holds a mapping of guest physical address space to the host
/// kernel virtual address space. Can be used to map a frequently accessed
/// portion of guest physical memory for faster access.
#[repr(C)]
#[derive(Default)]
pub struct GuestPtr {
    mapping: Option<RefPtr<VmMapping>>,
    pinned_vmo: PinnedVmObject,
    offset: zx_vaddr_t,
}

static_assert!(size_of::<GuestPtr>() == 40);
static_assert!(align_of::<GuestPtr>() == 8);

impl GuestPtr {
    /// Creates a new `GuestPtr` from an active host `mapping`, `pinned_vmo`, and intra-page
    /// `offset`.
    pub fn new(mapping: RefPtr<VmMapping>, pinned_vmo: PinnedVmObject, offset: zx_vaddr_t) -> Self {
        Self { mapping: Some(mapping), pinned_vmo, offset }
    }

    /// Destroys the host mapping and unpins the underlying VMO pages.
    pub fn reset(&mut self) {
        if let Some(mapping) = self.mapping.take() {
            let _ = mapping.destroy();
        }
        self.pinned_vmo = PinnedVmObject::default();
    }

    /// Returns a raw pointer to the mapped guest memory if `type_size` fits within the remaining
    /// mapped size from `offset`, or null otherwise.
    pub(super) fn as_raw_ptr(&self, type_size: usize) -> *mut c_void {
        let mapping = self.mapping.as_ref().expect("GuestPtr is not initialized");
        if self.offset + type_size > mapping.size() {
            return core::ptr::null_mut();
        }
        with_exposed_provenance_mut(mapping.base().0 + self.offset)
    }

    /// Returns a typed pointer to the mapped guest memory, or null if `size_of::<T>()` exceeds
    /// the remaining mapped size from `offset`.
    pub fn as_ptr<T>(&self) -> *mut T {
        self.as_raw_ptr(size_of::<T>()).cast::<T>()
    }
}

impl Drop for GuestPtr {
    fn drop(&mut self) {
        self.reset();
    }
}

/// Manages the guest physical address space (`VmAspace::Type::GuestPhysical`) for a hypervisor
/// guest.
#[repr(C)]
#[derive(Default)]
pub struct GuestPhysicalAspace {
    physical_aspace: Option<RefPtr<VmAspace>>,
}

static_assert!(size_of::<GuestPhysicalAspace>() == size_of::<usize>());
static_assert!(align_of::<GuestPhysicalAspace>() == align_of::<usize>());

impl GuestPhysicalAspace {
    /// Creates a new `GuestPhysicalAspace`.
    pub fn create() -> Result<Self, Status> {
        let physical_aspace =
            VmAspace::create(Type::GuestPhysical, c"guest_physical").ok_or(Status::NO_MEMORY)?;
        Ok(Self { physical_aspace: Some(physical_aspace) })
    }

    #[inline]
    fn physical_aspace(&self) -> &RefPtr<VmAspace> {
        self.physical_aspace.as_ref().expect("GuestPhysicalAspace is not initialized")
    }

    /// Returns the size of the guest physical address space in bytes.
    pub fn size(&self) -> usize {
        self.physical_aspace().size()
    }

    /// Returns a reference to the underlying `ArchVmAspace`.
    pub fn arch_aspace(&self) -> &ArchVmAspace {
        self.physical_aspace().arch_aspace()
    }

    /// Returns the root `VmAddressRegion` of the guest physical address space.
    pub fn root_vmar(&self) -> RefPtr<VmAddressRegion> {
        self.physical_aspace().root_vmar().expect("GuestPhysicalAspace has no root VMAR")
    }

    /// Returns true if `guest_paddr` is backed by a `VmMapping` in this guest physical address
    /// space.
    pub fn is_mapped(&self, guest_paddr: zx_gpaddr_t) -> bool {
        ksync::lock!(let guard = self.physical_aspace().lock().lock());
        self.find_mapping(guard.token(), guest_paddr).is_some()
    }

    /// Maps the physical interrupt controller range `[host_paddr, host_paddr + len)` at
    /// `guest_paddr` in the guest physical address space.
    pub fn map_interrupt_controller(
        &mut self,
        guest_paddr: zx_gpaddr_t,
        host_paddr: PAddr,
        len: usize,
    ) -> Result<(), Status> {
        let vmo = VmObjectPhysical::create(host_paddr, len)?;
        // SAFETY: `vmo` was just created and has no mappings or children yet.
        unsafe {
            vmo.set_mapping_cache_policy(ARCH_MMU_FLAG_UNCACHED_DEVICE)?;
        }
        let vmo_size = vmo.size() as usize;

        // The root VMAR will maintain a reference to the VmMapping internally so
        // we don't need to maintain a long-lived reference to the mapping here.
        let mapping_result = self.root_vmar().create_vm_mapping(
            guest_paddr,
            vmo_size,
            /* align_pow2*/ 0,
            vmar_flag::SPECIFIC,
            VmObjectPhysical::into_vm_object(vmo),
            /* vmo_offset */ 0,
            INTERRUPT_MMU_FLAGS,
            b"guest_interrupt_vmo",
        )?;

        // Write mapping to page table.
        if let Err(status) = mapping_result.mapping.map_range(0, vmo_size, true, false) {
            let _ = mapping_result.mapping.destroy();
            return Err(status);
        }

        Ok(())
    }

    /// Unmaps `[guest_paddr, guest_paddr + len)` from the deepest containing VMAR in the guest
    /// physical address space.
    pub fn unmap_range(&mut self, guest_paddr: zx_gpaddr_t, len: usize) -> Result<(), Status> {
        // Walk from the root down through any child VMARs for this range so we can do an unmap.
        let mut vmar = self.root_vmar();
        while let Some(next) = vmar.find_region(VAddr(guest_paddr)) {
            let Some(next_vmar) = next.as_vm_address_region() else {
                break;
            };
            vmar = next_vmar;
        }
        // SAFETY: Unmaps a guest physical address range rather than active host kernel memory.
        unsafe { vmar.unmap(VAddr(guest_paddr), len, VmAddressRegionOpChildren::No) }
    }

    /// Resolves a guest physical page fault at `guest_paddr`.
    pub fn page_fault(&mut self, guest_paddr: zx_gpaddr_t) -> Result<(), Status> {
        pin_init::stack_pin_init!(let page_request = MultiPageRequest::new());

        let guest_paddr = page::round_down(guest_paddr);

        loop {
            let status = {
                ksync::lock!(let mut guard = self.physical_aspace().lock().lock());
                let mapping =
                    self.find_mapping(guard.token(), guest_paddr).ok_or(Status::NOT_FOUND)?;

                // In order to avoid re-faulting if the guest changes how it accesses
                // guest physical memory, and to avoid the need for invalidation of the
                // guest physical address space on x86 (through the use of INVEPT), we
                // fault the page with the maximum allowable permissions of the mapping.
                let mmu_flags = mapping.arch_mmu_flags_locked(guard.token(), VAddr(guest_paddr));
                let mut pf_flags = pf_flag::GUEST | pf_flag::HW_FAULT;
                if (mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) != 0 {
                    pf_flags |= pf_flag::WRITE;
                }
                if (mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 {
                    pf_flags |= pf_flag::INSTRUCTION;
                }
                let (fault_status, _) = mapping.page_fault_locked(
                    guard.as_mut().token_mut(),
                    VAddr(guest_paddr),
                    pf_flags,
                    0,
                    page_request.as_mut(),
                );
                fault_status
            };

            if status == Err(Status::SHOULD_WAIT) {
                page_request.as_mut().wait(true)?;
            } else {
                return status;
            }
        }
    }

    /// Creates a pinned host kernel mapping (`GuestPtr`) for `[guest_paddr, guest_paddr + len)`.
    pub fn create_guest_ptr(
        &mut self,
        guest_paddr: zx_gpaddr_t,
        len: usize,
        name: &[u8],
    ) -> Result<GuestPtr, Status> {
        let begin = page::round_down(guest_paddr);
        let end = guest_paddr.wrapping_add(len).wrapping_add(page::MASK) & !page::MASK;
        let mapping_len = end.wrapping_sub(begin);
        if begin > end || !in_range_max(begin as u64, mapping_len as u64, self.size() as u64) {
            return Err(Status::INVALID_ARGS);
        }

        let (intra_mapping_offset, mapping_object_offset, vmo) = {
            ksync::lock!(let guard = self.physical_aspace().lock().lock());
            let guest_mapping = self.find_mapping(guard.token(), begin).ok_or(Status::NOT_FOUND)?;
            let intra_mapping_offset = (begin - guest_mapping.base().0) as u64;
            if !in_range_max(intra_mapping_offset, mapping_len as u64, guest_mapping.size() as u64)
            {
                // The address range is not contained within a single mapping.
                return Err(Status::OUT_OF_RANGE);
            }
            let mapping_object_offset = guest_mapping.object_offset();
            let vmo =
                guest_mapping.vmo_locked(guard.token()).expect("live mapping must have a VMO");
            (intra_mapping_offset, mapping_object_offset, vmo)
        };

        // Pin the range of the guest VMO to ensure the user cannot manipulate it to cause our
        // kernel mapping to become invalidate and generate faults.
        let pinned_vmo = PinnedVmObject::create(
            vmo.clone(),
            mapping_object_offset + intra_mapping_offset,
            mapping_len as u64,
            true,
        )?;

        let kernel_vmar =
            VmAspace::kernel_aspace().root_vmar().expect("kernel aspace has no root VMAR");
        let host_mapping_result = kernel_vmar.create_vm_mapping(
            /* mapping_offset */ 0,
            mapping_len,
            /* align_pow2 */ 0,
            /* vmar_flags */ 0,
            vmo,
            mapping_object_offset + intra_mapping_offset,
            GUEST_MMU_FLAGS,
            name,
        )?;
        // Pre-populate the page tables so there's no need for kernel page faults.
        host_mapping_result.mapping.map_range(0, mapping_len, true, false)?;

        Ok(GuestPtr::new(host_mapping_result.mapping, pinned_vmo, guest_paddr - begin))
    }

    fn find_mapping(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        guest_paddr: zx_gpaddr_t,
    ) -> Option<RefPtr<VmMapping>> {
        let mut region = self.physical_aspace().root_vmar_locked(token)?;
        while let Some(next) = region.find_region_locked(token, VAddr(guest_paddr)) {
            if let Some(mapping) = next.as_vm_mapping() {
                return Some(mapping);
            }
            region = next.as_vm_address_region()?;
        }
        None
    }
}

impl Drop for GuestPhysicalAspace {
    fn drop(&mut self) {
        if let Some(physical_aspace) = self.physical_aspace.take() {
            // VmAspace maintains a circular reference with it's root VMAR. We need to
            // destroy the VmAspace in order to break that reference and allow the
            // VmAspace to be destructed.
            let _ = physical_aspace.destroy();
        }
    }
}

// Note: `hypervisor_unittest.cc` exercises `GuestPhysicalAspace` on both `x86_64` and `aarch64`
// via the C++ facade; `aspace_rust` is currently gated on `x86_64` because `aarch64` requires
// `arm64_get_boot_el()`, `arm64_mmu_features.vhe`, and `ArchVmAspace::arch_set_asid`, which are
// not yet exposed in Rust.
/// Unit tests for `GuestPhysicalAspace` and `GuestPtr`.
#[cfg(all(ktest, target_arch = "x86_64"))]
#[unittest::suite(name = "aspace_rust")]
mod tests {
    use crate::vm::pmm;
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm_object_paged::VmObjectPaged;
    use unittest::{expect_eq, expect_err, expect_false, expect_ok, expect_true};
    use zx_types::{
        ZX_CACHE_POLICY_UNCACHED, ZX_CACHE_POLICY_UNCACHED_DEVICE, ZX_CACHE_POLICY_WRITE_COMBINING,
    };

    const MMU_FLAGS: ArchMmuFlags =
        ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_PERM_EXECUTE;

    fn create_vmo(vmo_size: usize) -> Result<RefPtr<VmObjectPaged>, Status> {
        VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, vmo_size as u64)
    }

    fn commit_vmo(vmo: &VmObjectPaged) -> Result<(), Status> {
        vmo.commit_range(0, vmo.size())
    }

    fn create_mapping(
        vmar: &VmAddressRegion,
        vmo: RefPtr<VmObjectPaged>,
        addr: zx_gpaddr_t,
        mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        let vmo_size = vmo.size() as usize;
        vmar.create_vm_mapping(
            addr,
            vmo_size,
            0,
            vmar_flag::SPECIFIC,
            VmObjectPaged::into_vm_object(vmo),
            0,
            mmu_flags,
            b"vmo",
        )
        .map(|_| ())
    }

    fn create_sub_vmar(
        vmar: &VmAddressRegion,
        offset: usize,
        size: usize,
    ) -> Result<RefPtr<VmAddressRegion>, Status> {
        vmar.create_sub_vmar(offset, size, 0, vmar.flags() | vmar_flag::SPECIFIC, b"vmar")
    }

    /// Verifies that `unmap_range` unmaps a mapped page in the guest physical address space.
    #[test]
    fn guest_physical_aspace_unmap_range() {
        // Setup.
        let mut gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");
        let vmo = create_vmo(page::SIZE).expect("Failed to create VMO");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS));

        // Unmap page.
        expect_ok!(gpa.unmap_range(0, page::SIZE));

        // Verify `is_mapped` for unmapped address fails.
        expect_false!(gpa.is_mapped(0));
    }

    /// Verifies that `unmap_range` succeeds when the range is outside any existing mapping.
    #[test]
    fn guest_physical_aspace_unmap_range_outside_of_mapping() {
        // Setup.
        let mut gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");
        let vmo = create_vmo(page::SIZE).expect("Failed to create VMO");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS));

        // Unmap page.
        expect_ok!(gpa.unmap_range(page::SIZE * 8, page::SIZE));
    }

    /// Verifies that `unmap_range` unmaps ranges spanning multiple mappings.
    #[test]
    fn guest_physical_aspace_unmap_range_multiple_mappings() {
        // Setup.
        let mut gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");

        let vmo1 = create_vmo(page::SIZE * 2).expect("Failed to create VMO");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo1, 0, MMU_FLAGS));

        let vmo2 = create_vmo(page::SIZE * 2).expect("Failed to create VMO");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo2, page::SIZE * 3, MMU_FLAGS));

        // Unmap pages.
        expect_ok!(gpa.unmap_range(page::SIZE, page::SIZE * 3));

        // Verify `is_mapped` for unmapped addresses fails.
        for addr in (page::SIZE..page::SIZE * 4).step_by(page::SIZE) {
            expect_false!(gpa.is_mapped(addr));
        }

        // Verify `is_mapped` for mapped addresses succeeds.
        expect_true!(gpa.is_mapped(0));
        expect_true!(gpa.is_mapped(page::SIZE * 4));
    }

    /// Verifies that `unmap_range` rejects ranges intersecting sub-VMARs.
    #[test]
    fn guest_physical_aspace_unmap_range_sub_region() {
        // Setup.
        let mut gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");
        let root_vmar = gpa.root_vmar();
        // To test partial unmapping within sub-VMAR:
        // Sub-VMAR from [0, page::SIZE * 2).
        // Map within sub-VMAR from [page::SIZE, page::SIZE * 2).
        let sub_vmar1 =
            create_sub_vmar(&root_vmar, 0, page::SIZE * 2).expect("Failed to create sub-VMAR");
        expect_true!(sub_vmar1.has_parent());
        let vmo1 = create_vmo(page::SIZE).expect("Failed to create VMO");
        expect_ok!(create_mapping(&sub_vmar1, vmo1, page::SIZE, MMU_FLAGS));
        // To test destroying of sub-VMAR:
        // Sub-VMAR from [page::SIZE * 2, page::SIZE * 3).
        // Map within sub-VMAR from [0, page::SIZE).
        let sub_vmar2 = create_sub_vmar(&root_vmar, page::SIZE * 2, page::SIZE)
            .expect("Failed to create sub-VMAR");
        expect_true!(sub_vmar2.has_parent());
        let vmo2 = create_vmo(page::SIZE).expect("Failed to create VMO");
        expect_ok!(create_mapping(&sub_vmar2, vmo2, 0, MMU_FLAGS));
        // To test partial unmapping within root-VMAR:
        // Map within root-VMAR from [page::SIZE * 3, page::SIZE * 5).
        let vmo3 = create_vmo(page::SIZE * 2).expect("Failed to create VMO");
        expect_ok!(create_mapping(&root_vmar, vmo3, page::SIZE * 3, MMU_FLAGS));

        // Cannot unmap spanning across two different sub-vmars.
        expect_err!(gpa.unmap_range(page::SIZE, page::SIZE * 3), Status::INVALID_ARGS);

        // Verify `is_mapped` for mapped addresses succeeds.
        expect_true!(gpa.is_mapped(page::SIZE * 4));

        // Verify that sub-VMARs still have a parent.
        expect_true!(sub_vmar1.has_parent());
        expect_true!(sub_vmar2.has_parent());
    }

    /// Verifies that multiple mappings of a single VMO at different offsets work properly.
    #[test]
    fn guest_physical_address_space_single_vmo_multiple_mappings() {
        let _scanner_disable = AutoVmScannerDisable::new();

        // Setup.
        let gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");
        let vmo = create_vmo(page::SIZE * 4).expect("Failed to create VMO");

        // Map a single page of this four page VMO at offset 0x1000 and offset 0x3000.
        expect_ok!(
            gpa.root_vmar()
                .create_vm_mapping(
                    page::SIZE,
                    page::SIZE,
                    0,
                    vmar_flag::SPECIFIC,
                    VmObjectPaged::into_vm_object(vmo.clone()),
                    page::SIZE as u64,
                    MMU_FLAGS,
                    b"vmo",
                )
                .map(|_| ())
        );
        expect_ok!(
            gpa.root_vmar()
                .create_vm_mapping(
                    page::SIZE * 3,
                    page::SIZE,
                    0,
                    vmar_flag::SPECIFIC,
                    VmObjectPaged::into_vm_object(vmo.clone()),
                    (page::SIZE * 3) as u64,
                    MMU_FLAGS,
                    b"vmo",
                )
                .map(|_| ())
        );

        expect_ok!(commit_vmo(&vmo));

        // No mapping at 0x0 or 0x2000.
        expect_false!(gpa.is_mapped(0));
        expect_false!(gpa.is_mapped(page::SIZE * 2));

        // There is a mapping at 0x1000 and 0x3000.
        expect_true!(gpa.is_mapped(page::SIZE));
        expect_true!(gpa.is_mapped(page::SIZE * 3));
    }

    /// Verifies that `page_fault` faults in pages across mappings with different permission flags.
    #[test]
    fn guest_physical_aspace_page_fault() {
        // Setup.
        let mut gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");
        let vmo = create_vmo(page::SIZE).expect("Failed to create VMO");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo.clone(), 0, MMU_FLAGS));
        expect_ok!(create_mapping(
            &gpa.root_vmar(),
            vmo.clone(),
            page::SIZE,
            ARCH_MMU_FLAG_PERM_READ
        ));
        expect_ok!(create_mapping(
            &gpa.root_vmar(),
            vmo.clone(),
            page::SIZE * 2,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
        ));
        expect_ok!(create_mapping(
            &gpa.root_vmar(),
            vmo,
            page::SIZE * 3,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE,
        ));

        // Fault in each page.
        for addr in (0..page::SIZE * 4).step_by(page::SIZE) {
            expect_ok!(gpa.page_fault(addr));
        }
    }

    /// Verifies that `map_interrupt_controller` maps a physical page into the guest address space.
    #[test]
    fn guest_physical_aspace_map_interrupt_controller() {
        // Setup.
        let mut gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");
        let vmo = create_vmo(page::SIZE).expect("Failed to create VMO");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS));

        // Allocate a page to use as the interrupt controller.
        let (vm_page, paddr) = pmm::alloc_page(0).expect("Unable to allocate a page");

        // Map interrupt controller page in an arbitrary location.
        const GICV_ADDRESS: zx_gpaddr_t = 0x800001000;
        expect_ok!(gpa.map_interrupt_controller(GICV_ADDRESS, paddr, page::SIZE));

        // Cleanup: must destroy `gpa` (which unmaps the physical page) before freeing `vm_page`.
        drop(gpa);
        // SAFETY: `vm_page` was allocated above and is no longer mapped.
        unsafe {
            pmm::free_page(vm_page);
        }
    }

    /// Verifies mapping a VMO with `ZX_CACHE_POLICY_UNCACHED` into a `GuestPhysicalAspace`.
    #[test]
    fn guest_physical_aspace_uncached() {
        // Setup.
        let vmo = create_vmo(page::SIZE).expect("Failed to create VMO");
        // SAFETY: `vmo` was just created and has no mappings or children.
        expect_ok!(unsafe {
            vmo.set_mapping_cache_policy(ZX_CACHE_POLICY_UNCACHED as ArchMmuFlags)
        });

        let gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS));
    }

    /// Verifies mapping a VMO with `ZX_CACHE_POLICY_UNCACHED_DEVICE` into a `GuestPhysicalAspace`.
    #[test]
    fn guest_physical_aspace_uncached_device() {
        // Setup.
        let vmo = create_vmo(page::SIZE).expect("Failed to create VMO");
        // SAFETY: `vmo` was just created and has no mappings or children.
        expect_ok!(unsafe {
            vmo.set_mapping_cache_policy(ZX_CACHE_POLICY_UNCACHED_DEVICE as ArchMmuFlags)
        });

        let gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS));
    }

    /// Verifies mapping a VMO with `ZX_CACHE_POLICY_WRITE_COMBINING` into a `GuestPhysicalAspace`.
    #[test]
    fn guest_physical_aspace_write_combining() {
        // Setup.
        let vmo = create_vmo(page::SIZE).expect("Failed to create VMO");
        // SAFETY: `vmo` was just created and has no mappings or children.
        expect_ok!(unsafe {
            vmo.set_mapping_cache_policy(ZX_CACHE_POLICY_WRITE_COMBINING as ArchMmuFlags)
        });

        let gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS));
    }

    /// Verifies `VmAddressRegion::protect` on a mapping in a `GuestPhysicalAspace`.
    #[test]
    fn guest_physical_aspace_protect() {
        // Setup.
        let vmo = create_vmo(page::SIZE).expect("Failed to create VMO");

        let gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, 0, MMU_FLAGS));

        expect_ok!(gpa.root_vmar().protect(
            VAddr(0),
            page::SIZE,
            ARCH_MMU_FLAG_PERM_WRITE,
            VmAddressRegionOpChildren::Yes,
        ));
    }

    /// Verifies `ArchVmAspace::query` across mappings with various permission flags.
    #[test]
    fn guest_physical_aspace_query() {
        // Disable the VM scanner to ensure pages stay committed into the VMO, and hence stay
        // mapped into the aspace before we query.
        let _scanner_disable = AutoVmScannerDisable::new();

        // This test is arch independent so be conservative with the permission and assume that
        // read is needed for any other permission.
        const MMU_FLAG_TESTS: [ArchMmuFlags; 4] = [
            ARCH_MMU_FLAG_PERM_READ,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_PERM_EXECUTE,
        ];

        let vmo = create_vmo(page::SIZE).expect("Failed to create VMO");
        let gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");

        for flags in MMU_FLAG_TESTS {
            let mapping_result = gpa
                .root_vmar()
                .create_vm_mapping(
                    0,
                    page::SIZE,
                    0,
                    vmar_flag::SPECIFIC,
                    VmObjectPaged::into_vm_object(vmo.clone()),
                    0,
                    flags,
                    b"vmo",
                )
                .expect("Failed to create mapping");
            expect_ok!(mapping_result.mapping.map_range(0, page::SIZE, true, false));

            let (_, query_flags) = gpa.arch_aspace().query(0).expect("Failed to query");
            expect_eq!(flags, query_flags);

            // Cleanup the mapping for next iteration.
            expect_ok!(mapping_result.mapping.destroy());
        }
    }

    /// Verifies that `create_guest_ptr` maps and pins a guest region accessible from the host.
    #[test]
    fn guest_physical_aspace_create_guest_ptr() {
        let mut gpa = GuestPhysicalAspace::create().expect("Failed to create GuestPhysicalAspace");
        let vmo = create_vmo(page::SIZE * 2).expect("Failed to create VMO");
        expect_ok!(create_mapping(&gpa.root_vmar(), vmo, page::SIZE, MMU_FLAGS));

        let mut guest_ptr = gpa
            .create_guest_ptr(page::SIZE + 64, size_of::<u64>(), b"test-guest-ptr")
            .expect("Failed to create GuestPtr");
        let ptr = guest_ptr.as_ptr::<u64>();
        expect_true!(!ptr.is_null());
        // SAFETY: `ptr` points to a mapped, pinned `u64` at `page::SIZE + 64`.
        unsafe {
            *ptr = 0xdeadbeef_cafebabe;
            expect_eq!(0xdeadbeef_cafebabe, *ptr);
        }
        // Requesting a type larger than the mapped page must return null.
        expect_true!(guest_ptr.as_ptr::<[u8; page::SIZE]>().is_null());
        guest_ptr.reset();
    }
}
