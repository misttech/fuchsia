// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! 16-bit AP bootstrap region management.

use core::cell::{Cell, UnsafeCell};
use core::ffi::c_void;
use core::mem::{align_of, offset_of, size_of};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use debug::{ltracef, tracef};
use fbl::RefPtr;
use ksync::{LockPolicy, RawLock, RawMutexStorage};
use zx_status::Status;

use super::bootstrap16_ffi::{
    X86_BOOTSTRAP16_END, X86_BOOTSTRAP16_START, X86_SECONDARY_CPU_LONG_MODE_HIGH_ENTRY,
};
use crate::kernel::mp::SMP_MAX_CPUS;
use crate::kernel::thread::Thread;
use crate::kernel::types::PAddr;
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE,
};
use crate::vm::physmap::paddr_to_physmap;
use crate::vm::vm_aspace::{Type as VmAspaceType, VmAspace, vmm_flag};

const LOCAL_TRACE: u32 = 0;

/// Size of the 16-bit SMP bootstrap buffer in bytes (3 pages).
pub const X86_BOOTSTRAP16_BUFFER_SIZE: usize = 3 * page::SIZE;

const MB: usize = 1024 * 1024;
const CODE_64_SELECTOR: u32 = 0x10;

// LINT.IfChange
const BCD_PHYS_BOOTSTRAP_PML4_OFFSET: usize = 0;
const BCD_PHYS_KERNEL_PML4_OFFSET: usize = 4;
const BCD_PHYS_GDTR_OFFSET: usize = 10;
const BCD_PHYS_LM_ENTRY_OFFSET: usize = 16;
const BCD_LM_CS_OFFSET: usize = 20;
const BCD_VIRT_LM_HIGH_ENTRY_OFFSET: usize = 24;
const BCD_CPU_COUNTER_OFFSET: usize = 32;
const BCD_CPU_WAITING_OFFSET: usize = 36;
const BCD_PER_CPU_BASE_OFFSET: usize = 44;
// LINT.ThenChange(//zircon/kernel/arch/x86/include/arch/x86/bootstrap16.h)

/// Common 16-bit bootstrap header (`X86Bootstrap16Data`).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct X86Bootstrap16Data {
    /// Physical address of identity PML4.
    pub phys_bootstrap_pml4: u32,
    /// Physical address of the kernel PML4.
    pub phys_kernel_pml4: u32,
    /// Physical address of GDT, aligned such that it can be treated as an `LGDT` descriptor.
    pub padding1: u16,
    pub phys_gdtr_limit: u16,
    pub phys_gdtr_base: u32,

    /// Ordering of these two matter; they should be usable by `retfl`.
    /// Physical address of long mode entry point.
    pub phys_long_mode_entry: u32,
    /// 64-bit code segment to use.
    pub long_mode_cs: u32,

    /// The virtual address of the high-addressed entry point after the long mode entry point.
    pub virt_long_mode_high_entry: u64,
}

/// Per-CPU stack and initial `Thread` entry in [`X86ApBootstrapData`].
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub(super) struct X86ApPerCpuBootstrapData {
    /// Virtual address of the top of initial kstack.
    pub kstack_top: usize,
    /// Virtual address of initial `Thread`.
    pub thread: *mut Thread,
}

/// AP bootstrap configuration page (`X86ApBootstrapData`).
#[repr(C, packed)]
pub(super) struct X86ApBootstrapData {
    pub hdr: X86Bootstrap16Data,
    /// Counter for APs to use to determine which stack to take.
    pub cpu_id_counter: u32,
    /// Pointer to value to use to determine when APs are done with boot.
    pub cpu_waiting_mask: *const AtomicU32,
    /// Per-CPU data.
    pub per_cpu: [X86ApPerCpuBootstrapData; SMP_MAX_CPUS - 1],
}

zr::static_assert!(size_of::<X86Bootstrap16Data>() == 32);
zr::static_assert!(align_of::<X86Bootstrap16Data>() == 8);
zr::static_assert!(size_of::<X86ApPerCpuBootstrapData>() == 16);
zr::static_assert!(align_of::<X86ApPerCpuBootstrapData>() == 1);
zr::static_assert!(size_of::<X86ApBootstrapData>() == 44 + 16 * (SMP_MAX_CPUS - 1));
zr::static_assert!(align_of::<X86ApBootstrapData>() == 1);
zr::static_assert!(size_of::<X86ApBootstrapData>() <= page::SIZE);
zr::static_assert!(
    offset_of!(X86Bootstrap16Data, phys_bootstrap_pml4) == BCD_PHYS_BOOTSTRAP_PML4_OFFSET
);
zr::static_assert!(offset_of!(X86Bootstrap16Data, phys_kernel_pml4) == BCD_PHYS_KERNEL_PML4_OFFSET);
zr::static_assert!(offset_of!(X86Bootstrap16Data, phys_gdtr_limit) == BCD_PHYS_GDTR_OFFSET);
zr::static_assert!(offset_of!(X86Bootstrap16Data, phys_gdtr_base) == BCD_PHYS_GDTR_OFFSET + 2);
zr::static_assert!(
    offset_of!(X86Bootstrap16Data, phys_long_mode_entry) == BCD_PHYS_LM_ENTRY_OFFSET
);
zr::static_assert!(offset_of!(X86Bootstrap16Data, long_mode_cs) == BCD_LM_CS_OFFSET);
zr::static_assert!(
    offset_of!(X86Bootstrap16Data, virt_long_mode_high_entry) == BCD_VIRT_LM_HIGH_ENTRY_OFFSET
);
zr::static_assert!(offset_of!(X86ApBootstrapData, hdr) == BCD_PHYS_BOOTSTRAP_PML4_OFFSET);
zr::static_assert!(offset_of!(X86ApBootstrapData, cpu_id_counter) == BCD_CPU_COUNTER_OFFSET);
zr::static_assert!(offset_of!(X86ApBootstrapData, cpu_waiting_mask) == BCD_CPU_WAITING_OFFSET);
zr::static_assert!(offset_of!(X86ApBootstrapData, per_cpu) == BCD_PER_CPU_BASE_OFFSET);

/// Physical base address of the reserved 3-page low-memory 16-bit bootstrap buffer.
///
/// Initialized once during single-threaded early boot in [`x86_bootstrap16_init`] before secondary
/// CPU bringup calls [`x86_bootstrap16_acquire`], so `Ordering::Relaxed` is sufficient.
static BOOTSTRAP_PHYS_ADDR: AtomicUsize = AtomicUsize::new(usize::MAX);
static BOOTSTRAP_LOCK: RawMutexStorage = RawMutexStorage::INIT;

struct BootstrapAspaceCell(UnsafeCell<Option<RefPtr<VmAspace>>>);

// SAFETY: Access to `BootstrapAspaceCell` is serialized by `BOOTSTRAP_LOCK`.
unsafe impl Sync for BootstrapAspaceCell {}

/// The bootstrap address space is kept as a global variable in order to maintain ownership of low
/// mem PML4. If this aspace were released then the physical pages it holds would be returned to the
/// PMM and may be reallocated for other uses. Normally that's fine because we could always ask for
/// more pages from the PMM when we need them, but these pages are special in that are "low mem"
/// pages that exist in the first 4GB of the physical address space. If we were to release them they
/// may get reused for other purposes. Then if we need low mem pages in order to bootstrap a
/// new CPU, the PMM may not have any available and we'd be unable to do so.
static BOOTSTRAP_ASPACE: BootstrapAspaceCell = BootstrapAspaceCell(UnsafeCell::new(None));

/// Initializes the bootstrap16 subsystem by giving it pages to work with (`x86_bootstrap16_init`).
///
/// # Safety
///
/// - This function must be called at most once during single-threaded early boot before any calls
///   to [`x86_bootstrap16_acquire`].
/// - `bootstrap_base` must refer to [`X86_BOOTSTRAP16_BUFFER_SIZE`] bytes of RAM aligned on a page
///   boundary less than 1M that are reserved exclusively for the 16-bit bootstrap module.
pub unsafe fn x86_bootstrap16_init(bootstrap_base: PAddr) {
    debug_assert!(!page::is_page_rounded(BOOTSTRAP_PHYS_ADDR.load(Ordering::Relaxed)));
    debug_assert!(page::is_page_rounded(bootstrap_base.0));
    debug_assert!(bootstrap_base.0 <= MB - X86_BOOTSTRAP16_BUFFER_SIZE);
    BOOTSTRAP_PHYS_ADDR.store(bootstrap_base.0, Ordering::Relaxed);
}

/// Upon success, returns a pointer to the virtual address of the bootstrap data, and the physical
/// address of the first instruction that should be executed in 16-bit mode.
///
/// If this function returns success, `x86_bootstrap16_release()` must be called
/// later, to allow the bootstrap16 module to be reused.
pub(super) fn x86_bootstrap16_acquire(entry64: usize) -> Result<(*mut c_void, PAddr), Status> {
    let bootstrap_phys_addr = BOOTSTRAP_PHYS_ADDR.load(Ordering::Relaxed);
    // Make sure x86_bootstrap16_init has been called, and bail early if not.
    if !page::is_page_rounded(bootstrap_phys_addr) {
        core::hint::cold_path();
        return Err(Status::BAD_STATE);
    }

    // This routine assumes that the bootstrap buffer is 3 pages long.
    zr::static_assert!(X86_BOOTSTRAP16_BUFFER_SIZE == 3 * page::SIZE);

    ltracef!("bootstrap_phys_addr {:#x}\n", bootstrap_phys_addr);

    let start_addr = core::ptr::addr_of!(X86_BOOTSTRAP16_START).addr();
    let end_addr = core::ptr::addr_of!(X86_BOOTSTRAP16_END).addr();
    // Make sure the entrypoint code is in the bootstrap code that will be loaded.
    if entry64 < start_addr || entry64 >= end_addr {
        core::hint::cold_path();
        return Err(Status::INVALID_ARGS);
    }

    let kernel_aspace = VmAspace::kernel_aspace();
    let bootstrap_virt_addr_cell = Cell::new(core::ptr::null_mut::<c_void>());

    // Ensure only one caller is using the bootstrap region.
    // SAFETY: `BOOTSTRAP_LOCK` is a statically initialized `RawMutexStorage` (`Mutex`) whose
    // ownership spans from `x86_bootstrap16_acquire` to `x86_bootstrap16_release`.
    unsafe {
        <RawMutexStorage as RawLock>::DefaultPolicy::acquire(&BOOTSTRAP_LOCK, &mut (), ());
    }

    // Clean up the kernel address space on the way out. The bootstrap address space does not need
    // to be cleaned up since it is kept as a global variable.
    let mut cleanup = zr::defer(|| {
        let virt_addr = bootstrap_virt_addr_cell.get();
        if !virt_addr.is_null() {
            // SAFETY: `virt_addr` was allocated from `kernel_aspace` in `x86_bootstrap16_acquire`
            // and is being cleaned up on error exit.
            let _ = unsafe { kernel_aspace.free_region(virt_addr.addr()) };
        }
        // SAFETY: `BOOTSTRAP_LOCK` was acquired on the current thread above.
        unsafe {
            <RawMutexStorage as RawLock>::DefaultPolicy::release(&BOOTSTRAP_LOCK, &mut ());
        }
    });

    // SAFETY: `BOOTSTRAP_LOCK` is held exclusively by the current thread.
    let has_bootstrap_aspace = unsafe { (*BOOTSTRAP_ASPACE.0.get()).is_some() };
    if !has_bootstrap_aspace {
        let new_aspace =
            VmAspace::create(VmAspaceType::LowKernel, b"bootstrap16").ok_or(Status::NO_MEMORY)?;

        // Bootstrap aspace needs 3 regions mapped:
        // 1) The bootstrap region (identity mapped) which contains:
        // 1.a) A copy of the bootstrap code.
        // 1.b) A copy of the GDT used temporarily to bounce.
        // These next two come implicitly from the shared kernel aspace:
        // 2) The kernel's version of the bootstrap code page (matched mapping)
        // 3) The page containing the aps_still_booting counter (matched mapping)
        let mut vaddr = core::ptr::without_provenance_mut(bootstrap_phys_addr);
        // SAFETY: `vaddr` is a valid local pointer and `bootstrap_phys_addr` is the reserved
        // low-memory bootstrap physical region.
        let res = unsafe {
            new_aspace.alloc_physical(
                b"bootstrap_mapping",
                X86_BOOTSTRAP16_BUFFER_SIZE,
                &mut vaddr,
                page::SHIFT as u8,
                PAddr(bootstrap_phys_addr),
                vmm_flag::VALLOC_SPECIFIC,
                ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_PERM_EXECUTE,
            )
        };
        if let Err(status) = res {
            tracef!("Failed to create wakeup bootstrap aspace\n");
            let _ = new_aspace.destroy();
            return Err(status);
        }
        // SAFETY: `BOOTSTRAP_LOCK` is held exclusively by the current thread, and no other
        // reference to `BOOTSTRAP_ASPACE` is live.
        unsafe {
            *BOOTSTRAP_ASPACE.0.get() = Some(new_aspace);
        }
    }
    // SAFETY: `BOOTSTRAP_LOCK` is held exclusively by the current thread, and `BOOTSTRAP_ASPACE`
    // was verified or populated with `Some` above.
    let bootstrap_aspace: &RefPtr<VmAspace> =
        unsafe { (*BOOTSTRAP_ASPACE.0.get()).as_ref().unwrap() };

    // Map the AP bootstrap page and a low mem data page to configure
    // the AP processors with.
    let mut bootstrap_virt_ptr: *mut c_void = core::ptr::null_mut();
    // SAFETY: `bootstrap_virt_ptr` is a valid local pointer and `bootstrap_phys_addr` is the
    // reserved bootstrap physical region.
    let res = unsafe {
        kernel_aspace.alloc_physical(
            b"bootstrap16_aperture",
            X86_BOOTSTRAP16_BUFFER_SIZE,
            &mut bootstrap_virt_ptr,    // requested virtual address
            page::SHIFT as u8,          // alignment log2
            PAddr(bootstrap_phys_addr), // physical address
            0,                          // vmm flags
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE, // arch mmu flags
        )
    };
    if let Err(status) = res {
        tracef!("could not allocate AP bootstrap page: {}\n", status.into_raw());
        return Err(status);
    }
    debug_assert!(!bootstrap_virt_ptr.is_null());
    bootstrap_virt_addr_cell.set(bootstrap_virt_ptr);
    let bootstrap_virt_u8 = bootstrap_virt_ptr.cast::<u8>();

    // Copy the bootstrap code and _temp_gdt to the bootstrap buffer. Compute where the offsets are
    // going to be up front.
    let bootstrap_code_len = end_addr - start_addr;
    assert!(bootstrap_code_len <= page::SIZE - 8);
    let temp_gdt_offset = (bootstrap_code_len + 7) & !7;

    let temp_gdt_start = core::ptr::addr_of!(super::descriptor_ffi::TEMP_GDT).cast::<u8>();
    let temp_gdt_end = core::ptr::addr_of!(super::descriptor_ffi::TEMP_GDT_END).cast::<u8>();
    let temp_gdt_len = temp_gdt_end.addr() - temp_gdt_start.addr();
    assert!(temp_gdt_len > 0 && temp_gdt_len < u16::MAX as usize);

    // Make sure the bootstrap code + gdt (aligned to 8 bytes) fits within the first page.
    assert!(temp_gdt_offset + temp_gdt_len < page::SIZE);

    // SAFETY: `temp_gdt_offset < page::SIZE` is within the 3-page `bootstrap_virt_u8` allocation.
    let temp_gdt_virt_addr = unsafe { bootstrap_virt_u8.add(temp_gdt_offset) };

    // Copy the bootstrap code in.
    // SAFETY: `bootstrap_virt_u8` is mapped read-write for 3 pages and `X86_BOOTSTRAP16_START`
    // points to `bootstrap_code_len` bytes of `.text.bootstrap16`.
    unsafe {
        core::ptr::copy_nonoverlapping(
            core::ptr::addr_of!(X86_BOOTSTRAP16_START),
            bootstrap_virt_u8,
            bootstrap_code_len,
        );
    }
    ltracef!(
        "bootstrap code virt {:p} phys {:#x} len {:#x}\n",
        bootstrap_virt_u8,
        bootstrap_phys_addr,
        bootstrap_code_len
    );

    // Copy _temp_gdt to just after the code, aligned to an 8 byte boundary. This is to avoid
    // any issues with the kernel being loaded > 4GB.
    // SAFETY: `temp_gdt_virt_addr` + `temp_gdt_len` fits within the first page of
    // `bootstrap_virt_u8` and `temp_gdt_start` points to `temp_gdt_len` bytes in `gdt.S`.
    unsafe {
        core::ptr::copy_nonoverlapping(temp_gdt_start, temp_gdt_virt_addr, temp_gdt_len);
    }
    let temp_gdt_phys_addr = bootstrap_phys_addr + temp_gdt_offset;
    ltracef!(
        "temp_gdt virt {:p} phys {:#x} len {:#x}\n",
        temp_gdt_virt_addr,
        temp_gdt_phys_addr,
        temp_gdt_len
    );
    debug_assert!(temp_gdt_phys_addr < u32::MAX as usize);

    // Configuration data shared with the APs to get them to 64-bit mode stored in the 2nd page
    // of the bootstrap buffer.
    // SAFETY: The 2nd page (`page::SIZE`) is within the 3-page `bootstrap_virt_u8` allocation.
    let bootstrap_data = unsafe { bootstrap_virt_u8.add(page::SIZE).cast::<X86Bootstrap16Data>() };

    let long_mode_entry = bootstrap_phys_addr + (entry64 - start_addr);
    assert!(long_mode_entry <= u32::MAX as usize);

    // Carve out the 3rd page of the bootstrap physical buffer to hold a copy of the top level
    // page table for the bootstrapping code to use temporarily. Copy the contents of the bootstrap
    // aspace's top level PML4 to this page to make sure it's located in low (<4GB) memory. This
    // is needed when bootstrapping from 32bit to 64bit since the CR3 register is only 32bits wide
    // at the time you have to load it.
    let phys_bootstrap_pml4 = bootstrap_phys_addr + 2 * page::SIZE;
    let bootstrap_aspace_pml4 = bootstrap_aspace.arch_aspace().arch_table_phys();
    // SAFETY: The 3rd page (`2 * page::SIZE`) is within the 3-page `bootstrap_virt_u8` allocation.
    let phys_bootstrap_pml4_virt = unsafe { bootstrap_virt_u8.add(2 * page::SIZE) };
    let bootstrap_aspace_pml4_virt =
        core::ptr::with_exposed_provenance::<u8>(paddr_to_physmap(bootstrap_aspace_pml4).0);
    ltracef!(
        "phys_bootstrap_pml4 {:p} ({:#x}), bootstrap_aspace_pml4 {:p} ({:#x})\n",
        phys_bootstrap_pml4_virt,
        phys_bootstrap_pml4,
        bootstrap_aspace_pml4_virt,
        bootstrap_aspace_pml4.0
    );
    debug_assert!(!phys_bootstrap_pml4_virt.is_null() && !bootstrap_aspace_pml4_virt.is_null());
    // SAFETY: `phys_bootstrap_pml4_virt` is the 3rd page of the mapped `bootstrap_virt_u8`
    // aperture and `bootstrap_aspace_pml4_virt` is the physmap address of `bootstrap_aspace`'s
    // PML4 page.
    unsafe {
        core::ptr::copy_nonoverlapping(
            bootstrap_aspace_pml4_virt,
            phys_bootstrap_pml4_virt,
            page::SIZE,
        );
    }

    let phys_kernel_pml4 = kernel_aspace.arch_aspace().arch_table_phys().0;
    assert!(phys_kernel_pml4 <= u32::MAX as usize);

    let virt_long_mode_high_entry =
        core::ptr::addr_of!(X86_SECONDARY_CPU_LONG_MODE_HIGH_ENTRY).addr() as u64;

    // SAFETY: `bootstrap_data` points to the 2nd page of the mapped `bootstrap_virt_u8`
    // aperture, which is page-aligned and writable.
    unsafe {
        bootstrap_data.write(X86Bootstrap16Data {
            phys_bootstrap_pml4: phys_bootstrap_pml4 as u32,
            phys_kernel_pml4: phys_kernel_pml4 as u32,
            padding1: 0,
            phys_gdtr_limit: (temp_gdt_len - 1) as u16,
            phys_gdtr_base: temp_gdt_phys_addr as u32,
            phys_long_mode_entry: long_mode_entry as u32,
            long_mode_cs: CODE_64_SELECTOR,
            virt_long_mode_high_entry,
        });
    }

    // Cancel the deferred cleanup, since we're returning the new aspace and
    // region.
    // NOTE: Since we cancel the cleanup, we are not releasing `BOOTSTRAP_LOCK`.
    // This is released in `x86_bootstrap16_release()` when the caller is done
    // with the bootstrap region.
    cleanup.cancel();

    Ok((bootstrap_data.cast(), PAddr(bootstrap_phys_addr)))
}

/// Releases the bootstrap16 aperture and lock (`x86_bootstrap16_release`).
///
/// # Safety
///
/// The caller must ensure that `bootstrap_aperture` is a non-null pointer returned by a prior
/// successful call to [`x86_bootstrap16_acquire`] on the current thread and has not yet been
/// released.
pub(super) unsafe fn x86_bootstrap16_release(bootstrap_aperture: *mut c_void) {
    debug_assert!(!bootstrap_aperture.is_null());
    debug_assert!(BOOTSTRAP_LOCK.is_held());
    let kernel_aspace = VmAspace::kernel_aspace();
    let addr = bootstrap_aperture.addr() - page::SIZE;
    // SAFETY: `addr` is the base virtual address of the 3-page `bootstrap16_aperture` region
    // allocated in `x86_bootstrap16_acquire`.
    let _ = unsafe { kernel_aspace.free_region(addr) };

    // SAFETY: `BOOTSTRAP_LOCK` was acquired in `x86_bootstrap16_acquire` on the current thread.
    unsafe {
        <RawMutexStorage as RawLock>::DefaultPolicy::release(&BOOTSTRAP_LOCK, &mut ());
    }
}
