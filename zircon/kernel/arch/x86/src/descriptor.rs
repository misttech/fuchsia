// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2009 Corey Tabaka
// Copyright (c) 2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicUsize, Ordering};

use super::x86::{
    PERCPU_CPU_NUM_OFFSET, PERCPU_DEFAULT_TSS_OFFSET, PERCPU_DIRECT_OFFSET,
    PERCPU_INTERRUPT_STACKS_NMI_OFFSET, PERCPU_KERNEL_SP_OFFSET, x86_lgdt, x86_ltr,
    x86_read_gs_offset64, x86_write_gs_offset64,
};
use crate::arch_rs::curr_cpu_num;
use crate::kernel::mp;
use crate::vm::arch_vm_aspace::{ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE};
use crate::vm::pmm::ALLOC_FLAG_ANY;
use crate::vm::vm_address_region::flag as vmar_flag;
use crate::vm::vm_aspace::VmAspace;
use crate::vm::vm_object_paged::VmObjectPaged;

const TSS_DESC_BUSY_BIT: u64 = 1u64 << 41;
const SEG_TYPE_TSS: u8 = 0x9;

const IO_BITMAP_BITS: usize = 65536;
const IO_BITMAP_BYTES: usize = IO_BITMAP_BITS / 8;
const TSS_BITMAP_OFFSET: usize = offset_of!(Tss64, tss_bitmap);

const INTERRUPT_STACK_SIZE: usize = 4096;

#[inline]
const fn tss_selector(i: u32) -> u16 {
    (0x38 + 16 * i) as u16
}

/// 64-bit Task State Segment (`tss_64_t` / `tss_t` in `arch/x86.h`).
#[repr(C, packed)]
struct Tss64 {
    rsvd0: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    rsvd1: u32,
    rsvd2: u32,
    ist1: u64,
    ist2: u64,
    ist3: u64,
    ist4: u64,
    ist5: u64,
    ist6: u64,
    ist7: u64,
    rsvd3: u32,
    rsvd4: u32,
    rsvd5: u16,
    iomap_base: u16,
    tss_bitmap: [u8; IO_BITMAP_BYTES + 1],
}

zr::static_assert!(TSS_BITMAP_OFFSET == 104);
zr::static_assert!(size_of::<Tss64>() == TSS_BITMAP_OFFSET + IO_BITMAP_BYTES + 1);
zr::static_assert!(core::mem::align_of::<Tss64>() == 1);
zr::static_assert!(PERCPU_KERNEL_SP_OFFSET == PERCPU_DEFAULT_TSS_OFFSET + offset_of!(Tss64, rsp0));

#[repr(C, packed)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TaskDesc {
    low: u64,
    high: u64,
}

zr::static_assert!(size_of::<TaskDesc>() == 16);
zr::static_assert!(core::mem::align_of::<TaskDesc>() == 1);

/// Packed GDTR descriptor structure for the `lgdt` instruction.
#[repr(C, packed)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Gdtr {
    limit: u16,
    address: usize,
}

zr::static_assert!(size_of::<Gdtr>() == 10);
zr::static_assert!(core::mem::align_of::<Gdtr>() == 1);

/// We create a new GDT after initialization is done and switch everyone to it.
///
/// Holds the address of the active GDT once [`gdt_setup`] has remapped it (`0` while still using
/// `_temp_gdt`). `AtomicUsize` with `Ordering::Relaxed` is sufficient because `GDT` is only
/// mutated once on the boot CPU in [`gdt_setup`] before secondary CPUs are brought online.
static GDT: AtomicUsize = AtomicUsize::new(0);

/// Returns the virtual address of the current Global Descriptor Table (`gdt_get`).
#[inline]
pub(super) fn gdt_get() -> usize {
    // `Relaxed` is sufficient because `GDT` is only written once on the boot CPU in `gdt_setup()`
    // before secondary CPUs are brought online.
    let val = GDT.load(Ordering::Relaxed);
    if val != 0 {
        val
    } else {
        (&raw mut super::descriptor_ffi::TEMP_GDT).cast::<u8>().expose_provenance()
    }
}

/// Loads `GDTR` with a `0xffff` limit and `base` address (`gdt_load`).
///
/// # Safety
///
/// The caller must ensure that `base` is a valid GDT virtual address that remains mapped while
/// loaded in `GDTR`.
#[inline]
pub(super) unsafe fn gdt_load(base: usize) {
    // During VM exit GDTR limit is always set to 0xffff and instead of
    // trying to maintain the limit aligned with the actual GDT size we
    // decided to just keep it 0xffff all the time and instead of relying
    // on the limit just map GDT in the way that accesses beyond GDT cause
    // page faults. This allows us to avoid calling LGDT on every VM exit.
    let gdtr = Gdtr { limit: 0xffff, address: base };
    // SAFETY: `gdtr` is a valid stack-allocated `Gdtr` pointing to `base`.
    unsafe { x86_lgdt((&raw const gdtr).cast()) };
}

/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized to the current CPU's `x86_percpu`
/// structure and that `percpu` and `tss` refer to live (not freed) `x86_percpu` and `default_tss`
/// objects owned by the current CPU.
#[inline]
unsafe fn tss_assign_ists(percpu: *mut u8, tss: *mut Tss64) {
    // SAFETY: `percpu` points to a valid `x86_percpu` and `tss` points to its `default_tss`.
    unsafe {
        (*tss).ist1 =
            percpu.add(PERCPU_INTERRUPT_STACKS_NMI_OFFSET + INTERRUPT_STACK_SIZE).addr() as u64;
        (*tss).ist2 =
            percpu.add(PERCPU_INTERRUPT_STACKS_NMI_OFFSET + 2 * INTERRUPT_STACK_SIZE).addr() as u64;
        (*tss).ist3 =
            percpu.add(PERCPU_INTERRUPT_STACKS_NMI_OFFSET + 3 * INTERRUPT_STACK_SIZE).addr() as u64;
    }
}

/// Initializes the current CPU's per-CPU Task State Segment (TSS) and loads the Task Register.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized for the current CPU and interrupts are
/// disabled.
pub(super) unsafe fn initialize_percpu_tss() {
    // SAFETY: Caller guarantees `%gs.base` is initialized for the current CPU.
    let percpu = unsafe {
        core::ptr::with_exposed_provenance_mut::<u8>(
            x86_read_gs_offset64::<PERCPU_DIRECT_OFFSET>() as usize
        )
    };
    // SAFETY: `PERCPU_CPU_NUM_OFFSET` (`0x58`) is the `u32` `cpu_num` field of `x86_percpu`.
    let cpu_num = unsafe { *percpu.add(PERCPU_CPU_NUM_OFFSET).cast::<u32>() };
    // SAFETY: `PERCPU_DEFAULT_TSS_OFFSET` (`0x70`) is the `default_tss` field of `x86_percpu`.
    let tss = unsafe { percpu.add(PERCPU_DEFAULT_TSS_OFFSET).cast::<Tss64>() };
    // SAFETY: `tss` points to the current CPU's exclusively accessed `default_tss`.
    unsafe { core::ptr::write_bytes(tss, 0, 1) };

    let sel = tss_selector(cpu_num);

    // Zeroed out TSS is okay for now.
    // SAFETY: `sel` is the current CPU's valid 16-byte TSS descriptor slot in the GDT.
    unsafe {
        set_global_desc_64(
            sel,
            tss.addr() as u64,
            (size_of::<Tss64>() - 1) as u32,
            1,
            0,
            0,
            SEG_TYPE_TSS,
            0,
            0,
        );
    }

    // SAFETY: `percpu` points to the current CPU's `x86_percpu` and `tss` points to its
    // `default_tss`.
    unsafe { tss_assign_ists(percpu, tss) };

    // SAFETY: `tss` points to the current CPU's exclusively accessed `default_tss`.
    unsafe {
        (*tss).iomap_base = TSS_BITMAP_OFFSET as u16;
        // Need to have an extra byte at the end of the bitmap because it will always potentially
        // read two bytes.
        (*tss).tss_bitmap[IO_BITMAP_BYTES] = 0xff;
    }

    // SAFETY: `%gs.base` and `default_tss` are initialized for the current CPU and interrupts are
    // disabled.
    unsafe { super::descriptor_ffi::x86_reset_tss_io_bitmap() };

    // SAFETY: `sel` was just populated above with a valid, non-busy 64-bit TSS descriptor.
    unsafe { x86_ltr(sel) };
}

/// Sets the kernel stack pointer (`rsp0`) in the current CPU's TSS (`x86_set_tss_sp`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` points to the current CPU's `x86_percpu` structure and
/// interrupts or preemption are disabled so the calling thread cannot migrate CPUs.
#[inline(always)]
pub(super) unsafe fn set_tss_sp(sp: usize) {
    // SAFETY: Caller guarantees `%gs.base` points to the current CPU's `x86_percpu` and
    // preemption/interrupts are disabled. `PERCPU_KERNEL_SP_OFFSET` (`0x74`) is the
    // offset of `default_tss.rsp0` in `x86_percpu`.
    unsafe { x86_write_gs_offset64::<PERCPU_KERNEL_SP_OFFSET>(sp as u64) };
}

/// Clears the busy bit in the 64-bit TSS descriptor at `sel` in the GDT (`x86_clear_tss_busy`).
///
/// # Safety
///
/// The caller must ensure that `sel` is a valid 64-bit TSS selector index within the active GDT
/// and that the caller has exclusive access to that TSS descriptor entry.
#[inline]
pub(super) unsafe fn clear_tss_busy(sel: u16) {
    let index = (sel >> 3) as usize;
    let desc = core::ptr::with_exposed_provenance_mut::<TaskDesc>(gdt_get() + index * 8);
    // SAFETY: Caller guarantees `sel` indexes a valid, exclusively accessed 16-byte task
    // descriptor in the GDT.
    unsafe {
        let low = &raw mut (*desc).low;
        low.write_unaligned(low.read_unaligned() & !TSS_DESC_BUSY_BIT);
    }
}

/// Fill in a descriptor in the GDT.
///
/// # Safety
///
/// The caller must ensure that `sel` is a valid 16-byte descriptor selector offset within the
/// active GDT and that the caller has exclusive access to that descriptor slot.
#[expect(clippy::too_many_arguments)]
unsafe fn set_global_desc_64(
    sel: u16,
    base: u64,
    limit: u32,
    present: u8,
    ring: u8,
    sys: u8,
    typ: u8,
    gran: u8,
    bits: u8,
) {
    /// 64-bit descriptor structure.
    #[repr(C, packed)]
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    struct SegDesc64 {
        limit_15_0: u16,
        base_15_0: u16,
        base_23_16: u8,
        type_s_dpl_p: u8,
        limit_19_16_flags: u8,
        base_31_24: u8,
        base_63_32: u32,
        reserved_sbz: u32,
    }

    zr::static_assert!(size_of::<SegDesc64>() == 16);
    zr::static_assert!(core::mem::align_of::<SegDesc64>() == 1);

    let entry = SegDesc64 {
        limit_15_0: (limit & 0x0000_ffff) as u16,
        base_15_0: (base & 0x0000_ffff) as u16,
        base_23_16: ((base & 0x00ff_0000) >> 16) as u8,
        type_s_dpl_p: (typ & 0x0f) // segment type
            | (u8::from(sys != 0) << 4) // system / non-system
            | ((ring & 0x03) << 5) // descriptor privilege level
            | (u8::from(present != 0) << 7), // present
        limit_19_16_flags: (((limit & 0x000f_0000) >> 16) as u8)
            | (u8::from(bits != 0) << 6) // 16 / 32 bit
            | (u8::from(gran != 0) << 7), // granularity
        base_31_24: ((base & 0xff00_0000) >> 24) as u8,
        base_63_32: (base >> 32) as u32,
        reserved_sbz: 0,
    };

    // copy it into the appropriate entry
    let index = (sel >> 3) as usize;

    // for x86_64 index is still in units of 8 bytes into the gdt table
    let g = core::ptr::with_exposed_provenance_mut::<SegDesc64>(gdt_get() + index * 8);
    // SAFETY: Caller guarantees `sel` indexes a valid 16-byte slot in the writable GDT.
    unsafe { g.write_unaligned(entry) };
}

/// Allocates, populates, and maps the post-boot Global Descriptor Table (`gdt_setup`).
///
/// # Safety
///
/// The caller must ensure that this function is called once on CPU 0 while only CPU 0 is online
/// and after the kernel address space and VMM are initialized.
pub(super) unsafe fn gdt_setup() {
    debug_assert_eq!(curr_cpu_num(), 0);
    debug_assert_eq!(mp::get_online_mask(), 1);
    // Max GDT size is limited to 64K and we reserve the whole 64K area, but we map
    // just enough read/write to store GDT and leave the rest mapped read-only
    // so all write accesses beyond GDT last page are going to cause page fault.
    //
    // Why don't we just set a proper limit value? That's because during VM exit
    // on x86 architecture GDT limit is always set to 0xFFFF (see Intel SDM, Volume
    // 3, 27.5.2 Loading Host Segment and Descriptor-Table Registers) and therefore
    // requiring the hypervisor to restore GDT limit after VM exit using LGDT
    // instruction which is a serializing instruction (see Intel SDM, Volume 3, 8.3
    // Serializing Instructions).
    let vmar_flags =
        vmar_flag::CAN_MAP_SPECIFIC | vmar_flag::CAN_MAP_READ | vmar_flag::CAN_MAP_WRITE;

    let temp_gdt_start = (&raw const super::descriptor_ffi::TEMP_GDT).addr();
    let temp_gdt_end = (&raw const super::descriptor_ffi::TEMP_GDT_END).addr();
    let gdt_real_size = temp_gdt_end - temp_gdt_start;
    assert!(gdt_real_size <= super::descriptor_ffi::TEMP_GDT_SIZE);
    const GDT_SIZE: usize = 0x10000;

    let vmo = VmObjectPaged::create(ALLOC_FLAG_ANY, VmObjectPaged::ALWAYS_PINNED, GDT_SIZE as u64)
        .expect("VmObjectPaged::create failed for GDT");

    // Copy the temporary gdt.
    // SAFETY: `_temp_gdt` to `_temp_gdt_end` is a valid static byte range in `gdt.S` of length
    // `gdt_real_size <= TEMP_GDT_SIZE`, and no other CPU or thread is mutating `_temp_gdt`.
    let temp_gdt_slice = unsafe {
        core::slice::from_raw_parts(
            (&raw const super::descriptor_ffi::TEMP_GDT).cast::<u8>(),
            gdt_real_size,
        )
    };
    assert_eq!(vmo.write(0, temp_gdt_slice), Ok(()));

    let vmar = VmAspace::kernel_aspace()
        .root_vmar()
        .expect("kernel root_vmar")
        .create_sub_vmar(0, GDT_SIZE, page::SHIFT as u8, vmar_flags, b"gdt_vmar")
        .expect("create_sub_vmar failed for gdt_vmar");

    // Can only create a protection boundary at page boundaries, so round up the gdt to the next
    // page boundary for mapping and protection purposes.
    let gdt_real_map_size = page::round_up(gdt_real_size);

    // Create the read/write real gdt mapping.
    let mapping_result = vmar
        .create_vm_mapping(
            0,
            gdt_real_map_size,
            page::SHIFT as u8,
            vmar_flag::SPECIFIC,
            VmObjectPaged::into_vm_object(vmo.clone()),
            0,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
            b"gdt-rw",
        )
        .expect("create_vm_mapping failed for gdt-rw");

    assert_eq!(mapping_result.mapping.map_range(0, gdt_real_map_size, false, false), Ok(()));

    // Create the read-only mapping to the maximal GDT size.
    let gdt_ro_size = GDT_SIZE - gdt_real_map_size;
    let ro_mapping_result = vmar
        .create_vm_mapping(
            gdt_real_map_size,
            gdt_ro_size,
            page::SHIFT as u8,
            vmar_flag::SPECIFIC,
            VmObjectPaged::into_vm_object(vmo),
            gdt_real_map_size as u64,
            ARCH_MMU_FLAG_PERM_READ,
            b"gdt-ro",
        )
        .expect("create_vm_mapping failed for gdt-ro");

    assert_eq!(ro_mapping_result.mapping.map_range(0, gdt_ro_size, false, false), Ok(()));

    // `Relaxed` is sufficient because `gdt_setup` runs on CPU 0 before SMP bringup.
    GDT.store(mapping_result.base, Ordering::Relaxed);
    // SAFETY: `gdt_get()` returns the newly mapped kernel GDT address.
    unsafe { gdt_load(gdt_get()) };
}

/// GDT and TSS descriptor tests.
#[cfg(ktest)]
#[unittest::suite(name = "descriptor_tests")]
#[expect(unused_imports)]
mod descriptor_tests {
    use crate::arch_rs::InterruptDisableGuard;

    /// Verifies clearing the TSS busy bit and reloading `TR` on the current CPU's GDT TSS entry.
    #[test]
    fn test_clear_tss_busy_and_reload_tr() {
        let _irqd = InterruptDisableGuard::new();
        let sel = tss_selector(curr_cpu_num());
        let index = (sel >> 3) as usize;
        let desc = core::ptr::with_exposed_provenance_mut::<TaskDesc>(gdt_get() + index * 8);

        // SAFETY: `sel` indexes the calling CPU's active 16-byte TSS descriptor in the GDT, and
        // interrupts are disabled so the thread cannot migrate CPUs.
        unsafe {
            let initial = desc.read_unaligned();
            assert_ne!(initial.low & TSS_DESC_BUSY_BIT, 0);

            clear_tss_busy(sel);
            let cleared = desc.read_unaligned();
            assert_eq!(
                cleared,
                TaskDesc { low: initial.low & !TSS_DESC_BUSY_BIT, high: initial.high }
            );

            x86_ltr(sel);
            let reloaded = desc.read_unaligned();
            assert_eq!(reloaded, initial);
        }
    }
}
