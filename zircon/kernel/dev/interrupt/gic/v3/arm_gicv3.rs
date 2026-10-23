// Copyright 2017 The Fuchsia Authors
// Copyright (c) 2017, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Driver for the ARM Generic Interrupt Controller, version 3 (and 4).

mod regs;

use crate::arch_rs::arm64::{
    cpu_num_to_mpidr, device_memory_barrier, isb, thread_memory_barrier, wfi,
};
use crate::arch_rs::cache::arch_clean_cache_range;
use crate::arch_rs::{Iframe, curr_cpu_num, disable_ints, ints_disabled, max_num_cpus};
use crate::dev_interrupt::{MAX_INTERRUPTS, MAX_MSI_IRQS};
use crate::gic_common::{ARM_GIC_SGI_FLAG_NS, GICV3, GICV4, register_sgi_handler};
use crate::kernel::mp::{
    MpIpi, SMP_MAX_CPUS, mbx_generic_irq, mbx_interrupt_irq, mbx_reschedule_irq,
};
use crate::kernel::types::{BOOT_CPU_ID, PAddr, cpu_mask_t, cpu_num_t, cpu_num_to_mask};
use crate::pdev_interrupt::{
    InterruptControllerOps, InterruptHandler, InterruptPolarity, InterruptTriggerMode,
    InterruptVector, MsiBlock, pdev_invoke_int_if_present, register_interrupt_controller,
};
use crate::root_resource_filter::root_resource_filter_add_deny_region;
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED_DEVICE,
};
use crate::vm::page::VmPageDoublyLinkedList;
use crate::vm::physmap::paddr_to_physmap;
use crate::vm::pmm;
use crate::vm::vm_aspace::VmAspace;
use core::ffi::c_void;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use debug::{dprintf, ltrace_entry, ltracef, ltracef_level};
use kprint::{kprint, kprintln};
use ksync::{KCell, LockToken, source_tag};
use lazy_init::LazyInit;
use libarch::arm64::MultiprocessorAffinityRegister;
use page;
use pin_init::stack_pin_init;
use pow2_range_allocator::Pow2RangeAllocator;
use regio::{AccessRestrictsTo, MmioBank, MmioPtr, Offset, Readable, RwSafe, RwUnsafe};
use zbi::DcfgArmGicV3Driver;
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

// Keep the layout of the configuration struct shared with C++ in sync; the C++ side asserts
// the same values in <dev/interrupt/arm_gicv3_init.h>.
zr::static_assert!(core::mem::size_of::<DcfgArmGicV3Driver>() == 48);
zr::static_assert!(core::mem::align_of::<DcfgArmGicV3Driver>() == 8);

/// Mask of all the affinity fields of an MPIDR value.
///
/// Mirrors `ARM64_MPIDR_MASK` from `zircon/kernel/arch/arm64/include/arch/arm64/mp.h`.
const ARM64_MPIDR_MASK: u64 = MultiprocessorAffinityRegister::AFF3_MASK
    | MultiprocessorAffinityRegister::AFF2_MASK
    | MultiprocessorAffinityRegister::AFF1_MASK
    | MultiprocessorAffinityRegister::AFF0_MASK;

// Base and stride of the mapped apertures.
static GICD_BASE: AtomicPtr<u32> = AtomicPtr::new(core::ptr::null_mut());
static GICR_BASE: AtomicPtr<u32> = AtomicPtr::new(core::ptr::null_mut());
static GITS_BASE: AtomicPtr<u32> = AtomicPtr::new(core::ptr::null_mut());
static GICR_STRIDE: AtomicU64 = AtomicU64::new(0);

static GICD_OFFSET: AtomicU64 = AtomicU64::new(0);
static GICR_OFFSET: AtomicU64 = AtomicU64::new(0);
static GIC_MMIO_PHYS: AtomicU64 = AtomicU64::new(0);
static IPI_BASE: AtomicU32 = AtomicU32::new(0);
static GIC_MAX_INT: AtomicU32 = AtomicU32::new(0);

const LPI_BASE: u32 = 8192;
const MAX_LPIS: u32 = 256;
const MAX_ITS_PCI_SLOTS: u32 = 32;
const ITS_TABLE_SIZE: usize = 0x10000; // 64 KiB
const ITS_TABLE_PAGES: usize = ITS_TABLE_SIZE / page::SIZE;
const ITS_TABLE_ALIGN_LOG2: u8 = 16; // 64 KiB alignment

const LPI_PROP_DEFAULT_PRIO: u8 = 0xa0;
const LPI_PROP_GROUP1: u8 = 1 << 1;
const LPI_PROP_ENABLED: u8 = 1 << 0;

const GITS_CMD_SYNC: u64 = 0x05;
const GITS_CMD_MAPD: u64 = 0x08;
const GITS_CMD_MAPC: u64 = 0x09;
const GITS_CMD_MAPTI: u64 = 0x0a;
const GITS_CMD_INV: u64 = 0x0c;

static GITS_PHYS: AtomicU64 = AtomicU64::new(0);
static ITS_ENABLED: AtomicBool = AtomicBool::new(false);
static LPI_PROP_PA: AtomicUsize = AtomicUsize::new(0);
static LPI_PROP_VA: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static LPI_PEND_PA: [AtomicUsize; SMP_MAX_CPUS] = [const { AtomicUsize::new(0) }; SMP_MAX_CPUS];
static ITS_CMDQ_VA: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static ITS_CPU0_RDBASE: AtomicU64 = AtomicU64::new(0);

ksync::declare_singleton_monitored_spinlock!(
    /// Serializes the ITS command queue and LPI configuration table updates.
    GicItsLock
);

/// The ITS command queue write offset and per-LPI mapped flags, guarded by `GicItsLock`.
/// `gic_init_its` takes the lock around each command it issues.
struct ItsState {
    cmdq_cwriter: u64,
    lpi_mapped: [bool; MAX_LPIS as usize],
}

static ITS_STATE: KCell<ItsState, GicItsLock> =
    KCell::new(ItsState { cmdq_cwriter: 0, lpi_mapped: [false; MAX_LPIS as usize] });

ksync::declare_singleton_spinlock!(
    /// Guards `LPI_HANDLERS`. The IRQ path holds it across the handler call, so replacing a
    /// handler waits for any call of it in flight, as the pdev table does.
    ///
    /// Lock order: `GicLpiHandlerLock` before anything a handler takes (its dispatcher, the
    /// `MsiAllocation`, and `GicItsLock` through `msi_mask_unmask`). It is never taken with
    /// `GicItsLock` held.
    ///
    /// A handler must never call `msi_register_handler` or `msi_free_block`, directly or
    /// indirectly: it would spin on this lock forever.
    GicLpiHandlerLock
);

/// The handler for each LPI, indexed from `LPI_BASE`.
static LPI_HANDLERS: KCell<[InterruptHandler; MAX_LPIS as usize], GicLpiHandlerLock> =
    KCell::new([InterruptHandler::DEFAULT; MAX_LPIS as usize]);

/// The LPI range allocator. Constructed in `rust_arm_gicv3_init_early`; `gic_init_its` sizes it
/// with `init`.
static LPI_ALLOC: LazyInit<Pow2RangeAllocator> = LazyInit::uninit();

/// Returns a bank covering the distributor registers, accessed 32 bits at a time.
fn gicd_bank() -> MmioBank<u32, RwSafe> {
    let base = GICD_BASE.load(Ordering::Relaxed);
    assert!(!base.is_null(), "GICv3 distributor base pointer is null");
    // SAFETY: `GICD_BASE` is mapped in `gic_init` before any register access and the mapping
    // remains valid for the lifetime of the kernel.
    let ptr = unsafe { MmioPtr::<u32, RwSafe>::new(base) };
    MmioBank::new(ptr, regs::GICD_REG_SIZE)
}

/// Returns a bank covering the distributor registers, accessed 64 bits at a time.
fn gicd_bank64() -> MmioBank<u64, RwSafe> {
    let base = GICD_BASE.load(Ordering::Relaxed);
    assert!(!base.is_null(), "GICv3 distributor base pointer is null");
    // SAFETY: As `gicd_bank`. The distributor base is page aligned and therefore suitably
    // aligned for 64 bit accesses.
    let ptr = unsafe { MmioPtr::<u64, RwSafe>::new(base.cast::<u64>()) };
    MmioBank::new(ptr, regs::GICD_REG_SIZE)
}

/// Returns a bank covering the redistributor registers of `cpu`.
fn gicr_bank(cpu: cpu_num_t) -> MmioBank<u32, RwSafe> {
    let base = GICR_BASE.load(Ordering::Relaxed);
    assert!(!base.is_null(), "GICv3 redistributor base pointer is null");
    debug_assert!(cpu < max_num_cpus());
    let stride = GICR_STRIDE.load(Ordering::Relaxed) as usize;
    // SAFETY: `GICR_BASE` is mapped in `gic_init` with room for `max_num_cpus()` strides, and
    // every caller passes either the current CPU or CPU 0, both below `max_num_cpus()`. The
    // mapping remains valid for the lifetime of the kernel.
    let ptr = unsafe { MmioPtr::<u32, RwSafe>::new(base.byte_add(stride * cpu as usize)) };
    MmioBank::new(ptr, stride)
}

/// Returns a bank covering the redistributor registers of `cpu`, accessed 64 bits at a time.
fn gicr_bank64(cpu: cpu_num_t) -> MmioBank<u64, RwSafe> {
    let base = GICR_BASE.load(Ordering::Relaxed);
    assert!(!base.is_null(), "GICv3 redistributor base pointer is null");
    debug_assert!(cpu < max_num_cpus());
    let stride = GICR_STRIDE.load(Ordering::Relaxed) as usize;
    // SAFETY: As `gicr_bank`. Each redistributor frame is 64KiB aligned and therefore suitably
    // aligned for 64 bit accesses.
    let ptr =
        unsafe { MmioPtr::<u64, RwSafe>::new(base.byte_add(stride * cpu as usize).cast::<u64>()) };
    MmioBank::new(ptr, stride)
}

/// Returns a bank covering the ITS registers, accessed 32 bits at a time.
fn gits_bank() -> MmioBank<u32, RwSafe> {
    let base = GITS_BASE.load(Ordering::Relaxed);
    assert!(!base.is_null(), "GICv3 ITS base pointer is null");
    // SAFETY: `GITS_BASE` is mapped in `gic_init_its` before any ITS register access and the
    // mapping remains valid for the lifetime of the kernel.
    let ptr = unsafe { MmioPtr::<u32, RwSafe>::new(base) };
    MmioBank::new(ptr, regs::GITS_REG_SIZE)
}

/// Returns a bank covering the ITS registers, accessed 64 bits at a time.
fn gits_bank64() -> MmioBank<u64, RwSafe> {
    let base = GITS_BASE.load(Ordering::Relaxed);
    assert!(!base.is_null(), "GICv3 ITS base pointer is null");
    // SAFETY: As `gits_bank`. The ITS base is page aligned and therefore suitably aligned for 64
    // bit accesses.
    let ptr = unsafe { MmioPtr::<u64, RwSafe>::new(base.cast::<u64>()) };
    MmioBank::new(ptr, regs::GITS_REG_SIZE)
}

/// Returns the virtual base address a bank was constructed over, for trace messages.
fn bank_base(bank: &MmioBank<u32, RwSafe>) -> usize {
    // SAFETY: Offset zero is within the bounds of any non-empty bank, and the resulting register
    // is only used for its address.
    let reg = unsafe { bank.at(Offset::<u32, RwSafe>::new(0)) };
    (*reg.io()).as_ptr().addr()
}

/// Reads a 32 bit register of `bank`.
fn read32(bank: &MmioBank<u32, RwSafe>, reg: Offset<u32, RwSafe>) -> u32 {
    // SAFETY: `reg` names a register within the bounds of `bank`, and every bank is built over a
    // mapping that remains valid for the lifetime of the kernel.
    unsafe { bank.at(reg) }.read()
}

/// Writes a 32 bit register of `bank`.
fn write32(bank: &MmioBank<u32, RwSafe>, reg: Offset<u32, RwSafe>, value: u32) {
    // SAFETY: As `read32`.
    unsafe { bank.at(reg) }.write(value);
}

/// Reads one of the 32 bit distributor registers.
fn gicd_read32(reg: Offset<u32, RwSafe>) -> u32 {
    read32(&gicd_bank(), reg)
}

/// Writes one of the 32 bit distributor registers.
fn gicd_write32(reg: Offset<u32, RwSafe>, value: u32) {
    write32(&gicd_bank(), reg, value)
}

/// Reads one of the 64 bit distributor registers.
fn gicd_read64(reg: Offset<u64, RwSafe>) -> u64 {
    // SAFETY: `reg` names a register within the bounds of the distributor mapping.
    unsafe { gicd_bank64().at(reg) }.read()
}

/// Writes one of the 64 bit distributor registers.
fn gicd_write64(reg: Offset<u64, RwSafe>, value: u64) {
    // SAFETY: As `gicd_read64`.
    unsafe { gicd_bank64().at(reg) }.write(value);
}

/// Reads one of the 32 bit redistributor registers of `cpu`.
fn gicr_read32(cpu: cpu_num_t, reg: Offset<u32, RwSafe>) -> u32 {
    read32(&gicr_bank(cpu), reg)
}

/// Writes one of the 32 bit redistributor registers of `cpu`.
fn gicr_write32(cpu: cpu_num_t, reg: Offset<u32, RwSafe>, value: u32) {
    write32(&gicr_bank(cpu), reg, value)
}

/// Reads one of the 64 bit redistributor registers of `cpu`.
fn gicr_read64<A: Readable>(cpu: cpu_num_t, reg: Offset<u64, A>) -> u64
where
    RwSafe: AccessRestrictsTo<A>,
{
    // SAFETY: `reg` names a register within the bounds of `cpu`'s redistributor frame.
    unsafe { gicr_bank64(cpu).at(reg) }.read()
}

/// Writes one of the 64 bit redistributor registers of `cpu`.
fn gicr_write64(cpu: cpu_num_t, reg: Offset<u64, RwSafe>, value: u64) {
    // SAFETY: As `gicr_read64`.
    unsafe { gicr_bank64(cpu).at(reg) }.write(value);
}

/// Writes one of the 64 bit redistributor registers of `cpu` that point the redistributor
/// at memory.
///
/// # Safety
///
/// Any memory `value` points the redistributor at must be reserved for it, aligned and large
/// enough for the table `value` describes, and stay allocated while LPIs are enabled.
unsafe fn gicr_write64_unsafe(cpu: cpu_num_t, reg: Offset<u64, RwUnsafe>, value: u64) {
    // SAFETY: `reg` names a register within the bounds of `cpu`'s redistributor frame, and
    // the caller vouches for `value`.
    unsafe { gicr_bank64(cpu).at(reg).write(value) };
}

/// Reads one of the 32 bit ITS registers.
fn gits_read32(reg: Offset<u32, RwSafe>) -> u32 {
    read32(&gits_bank(), reg)
}

/// Writes one of the 32 bit ITS registers.
fn gits_write32(reg: Offset<u32, RwSafe>, value: u32) {
    write32(&gits_bank(), reg, value)
}

/// Reads one of the 64 bit ITS registers.
fn gits_read64<A: Readable>(reg: Offset<u64, A>) -> u64
where
    RwSafe: AccessRestrictsTo<A>,
{
    // SAFETY: `reg` names a register within the bounds of the ITS mapping.
    unsafe { gits_bank64().at(reg) }.read()
}

/// Writes one of the 64 bit ITS registers.
fn gits_write64(reg: Offset<u64, RwSafe>, value: u64) {
    // SAFETY: As `gits_read64`.
    unsafe { gits_bank64().at(reg) }.write(value);
}

/// Writes one of the 64 bit ITS registers that point the ITS at memory.
///
/// # Safety
///
/// Any memory `value` points the ITS at must be reserved for it, aligned and large enough
/// for the table or queue `value` describes, and stay allocated while the ITS is enabled.
/// A `GITS_CWRITER` value must only cover commands that are complete in memory.
unsafe fn gits_write64_unsafe(reg: Offset<u64, RwUnsafe>, value: u64) {
    // SAFETY: `reg` names a register within the bounds of the ITS mapping, and the caller
    // vouches for `value`.
    unsafe { gits_bank64().at(reg).write(value) };
}

/// Spins until `(reg & mask) == expect`, returning `false` if it timed out.
fn gic_wait_for_mask(
    bank: &MmioBank<u32, RwSafe>,
    offset: Offset<u32, RwSafe>,
    mask: u32,
    expect: u32,
) -> bool {
    let mut count = 1000000;
    while (read32(bank, offset) & mask) != expect {
        count -= 1;
        if count == 0 {
            ltracef!(
                "arm_gicv3: wait timeout reg:{:#x}, val:{:#x}, mask:{:#x}\n",
                bank_base(bank) + offset.value,
                read32(bank, offset),
                mask
            );
            return false;
        }
    }
    true
}

/// Waits for the "register write pending" bit of GICD_CTLR to clear.
fn gicd_wait_for_rwp() {
    // Maintain the current log message for RWP timeouts.
    if !gic_wait_for_mask(&gicd_bank(), regs::gicd_ctlr(), regs::GICD_CTLR_RWP, 0) {
        ltracef!("arm_gicv3: rwp timeout {:#x}\n", gicd_read32(regs::gicd_ctlr()));
    }
}

/// Waits for the "register write pending" bit of `cpu`'s GICR_CTLR to clear.
///
/// Note that GICR_CTLR.RWP is not at the same position as GICD_CTLR.RWP (GICR_CTLR bit 31 is
/// UWP, upstream write pending).  Also note that GICR_CTLR.RWP only tracks writes to
/// GICR_ICENABLER0 and to certain GICR_CTLR fields (e.g. clearing EnableLPIs).
fn gicr_wait_for_rwp(cpu: cpu_num_t) {
    if !gic_wait_for_mask(&gicr_bank(cpu), regs::gicr_ctlr(), regs::GICR_CTLR_RWP, 0) {
        ltracef!("arm_gicv3: gicr rwp timeout {:#x}\n", gicr_read32(cpu, regs::gicr_ctlr()));
    }
}

fn gic_set_enable(vector: u32, enable: bool) {
    let reg = vector / 32;
    let mask = 1u32 << (vector % 32);
    if vector < 32 {
        let cpu_id = curr_cpu_num();
        let offset = if enable { regs::gicr_isenabler0() } else { regs::gicr_icenabler0() };
        gicr_write32(cpu_id, offset, mask);
        gicr_wait_for_rwp(cpu_id);
    } else {
        let offset = if enable { regs::gicd_isenabler(reg) } else { regs::gicd_icenabler(reg) };
        gicd_write32(offset, mask);
        gicd_wait_for_rwp();
    }
}

/// Redistributors for each PE need to be woken up before they will
/// distribute interrupts.
/// <https://developer.arm.com/documentation/198123/0302/Configuring-the-Arm-GIC>
fn gic_redistributor_sleep(sleep: bool) {
    let cpu = curr_cpu_num();
    debug_assert!(ints_disabled());

    // GICR_WAKER could be RW or RAZ/WI.  When GICD_CTLR.DS is 1, GICR_WAKER is
    // RW.  However, when GICD_CTLR.DS is 0, GICR_WAKER could be RW or RAZ/WI
    // depending on whether the access is Secure/Non-secure and FEAT_RME.
    //
    // Instead of checking those things we're going to take a shortcut.  In the
    // case we're writing a 1 to WAKER_PROCESSOR_SLEEP we'll read back GICR_WAKER
    // to determine if it's RW or RAZ/WI.  If the former, we'll
    // gic_wait_for_mask.  If the latter, we'll bail out.
    let mut waker = gicr_read32(cpu, regs::gicr_waker());
    if sleep {
        waker |= regs::WAKER_PROCESSOR_SLEEP;
    } else {
        waker &= !regs::WAKER_PROCESSOR_SLEEP;
    }
    gicr_write32(cpu, regs::gicr_waker(), waker);
    if sleep {
        let read_back = gicr_read32(cpu, regs::gicr_waker());
        if (read_back & regs::WAKER_PROCESSOR_SLEEP) == 0 {
            // Our write didn't take.  Must be RAZ/WI.  Don't bother waiting.
            return;
        }
    }

    let val = if sleep { regs::WAKER_CHILDREN_ASLEEP } else { 0 };
    gic_wait_for_mask(&gicr_bank(cpu), regs::gicr_waker(), regs::WAKER_CHILDREN_ASLEEP, val);
}

fn gic_init_percpu_lpi(cpu: cpu_num_t) {
    if LPI_PROP_VA.load(Ordering::Relaxed).is_null()
        || cpu as usize >= SMP_MAX_CPUS
        || LPI_PEND_PA[cpu as usize].load(Ordering::Relaxed) == 0
        || (gicr_read64(cpu, regs::gicr_typer()) & regs::GICR_TYPER_PLPIS) == 0
    {
        return;
    }
    let lpi_pend_pa = LPI_PEND_PA[cpu as usize].load(Ordering::Relaxed);

    // Ensure LPIs are disabled on this redistributor before programming PROPBASER/PENDBASER.
    // Note: GICR_CTLR.EnableLPIs may be WI once set; if it does not clear (e.g. on resume when
    // redistributor state was retained), keep the existing configuration.
    let gicr_ctlr = gicr_read32(cpu, regs::gicr_ctlr());
    if (gicr_ctlr & regs::GICR_CTLR_ENABLE_LPIS) != 0 {
        gicr_write32(cpu, regs::gicr_ctlr(), gicr_ctlr & !regs::GICR_CTLR_ENABLE_LPIS);
        gicr_wait_for_rwp(cpu);
        if (gicr_read32(cpu, regs::gicr_ctlr()) & regs::GICR_CTLR_ENABLE_LPIS) != 0 {
            return;
        }
    }

    // Ensure the pending table is zeroed in memory before setting GICR_PENDBASER_PTZ.
    let pend_va = paddr_to_physmap(PAddr(lpi_pend_pa)).0;
    // SAFETY: The pending table is an `ITS_TABLE_SIZE` run allocated by `gic_alloc_its_table`
    // that only this driver uses, mapped through the physmap, and LPIs are disabled on this
    // redistributor so the GIC is not accessing it. Cleaning it only needs it to be mapped.
    unsafe {
        core::ptr::write_bytes(
            core::ptr::with_exposed_provenance_mut::<u8>(pend_va),
            0,
            ITS_TABLE_SIZE,
        );
        arch_clean_cache_range(pend_va, ITS_TABLE_SIZE);
    }

    // Program LPI Configuration Table (IDbits = 15 -> 16-bit INTIDs 0..65535).
    let lpi_prop_pa = LPI_PROP_PA.load(Ordering::Relaxed) as u64;
    let mut propbaser = (lpi_prop_pa & regs::GICR_PROPBASER_ADDR_MASK)
        | regs::GICR_PROPBASER_SHAREABILITY_ISH
        | regs::GICR_PROPBASER_INNER_CACHE_WB
        | 15;
    // SAFETY: `lpi_prop_pa` is the `ITS_TABLE_SIZE` LPI configuration table from
    // `gic_alloc_its_table`, which is never freed and is large enough for the 16-bit INTIDs
    // programmed here.
    unsafe { gicr_write64_unsafe(cpu, regs::gicr_propbaser(), propbaser) };
    if (gicr_read64(cpu, regs::gicr_propbaser()) & regs::GICR_PROPBASER_SHAREABILITY_MASK) == 0 {
        propbaser = (propbaser
            & !(regs::GICR_PROPBASER_SHAREABILITY_MASK | regs::GICR_PROPBASER_INNER_CACHE_WB))
            | regs::GICR_PROPBASER_INNER_CACHE_NC;
        // SAFETY: As above.
        unsafe { gicr_write64_unsafe(cpu, regs::gicr_propbaser(), propbaser) };
    }

    // Program LPI Pending Table with PTZ (Pending Table Zero) set.
    let mut pendbaser = (lpi_pend_pa as u64 & regs::GICR_PENDBASER_ADDR_MASK)
        | regs::GICR_PENDBASER_SHAREABILITY_ISH
        | regs::GICR_PENDBASER_INNER_CACHE_WB
        | regs::GICR_PENDBASER_PTZ;
    // SAFETY: `lpi_pend_pa` is this CPU's `ITS_TABLE_SIZE` pending table from
    // `gic_alloc_its_table`, which is never freed, is large enough for 16-bit INTIDs and was
    // zeroed above as PTZ declares.
    unsafe { gicr_write64_unsafe(cpu, regs::gicr_pendbaser(), pendbaser) };
    if (gicr_read64(cpu, regs::gicr_pendbaser()) & regs::GICR_PENDBASER_SHAREABILITY_MASK) == 0 {
        pendbaser = (pendbaser
            & !(regs::GICR_PENDBASER_SHAREABILITY_MASK | regs::GICR_PENDBASER_INNER_CACHE_WB))
            | regs::GICR_PENDBASER_INNER_CACHE_NC;
        // SAFETY: As above.
        unsafe { gicr_write64_unsafe(cpu, regs::gicr_pendbaser(), pendbaser) };
    }

    // Enable LPIs on this redistributor.
    gicr_write32(
        cpu,
        regs::gicr_ctlr(),
        gicr_read32(cpu, regs::gicr_ctlr()) | regs::GICR_CTLR_ENABLE_LPIS,
    );
    device_memory_barrier();
}

/// Maps a physical region into the kernel address space, returning its virtual address.
fn map_region(phys_base: u64, size: usize, name: &[u8]) -> Result<*mut c_void, Status> {
    let mut ptr: *mut c_void = core::ptr::null_mut();
    let aspace = VmAspace::kernel_aspace();
    // SAFETY: `ptr` is a valid out parameter and the requested region is a device aperture
    // handed to us by the boot loader.
    unsafe {
        aspace.alloc_physical(
            name,
            size,
            &mut ptr,
            page::SHIFT as u8,
            PAddr(phys_base as usize),
            0,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_UNCACHED_DEVICE,
        )?;
    }
    Ok(ptr)
}

/// Allocates a size-aligned `ITS_TABLE_SIZE` table, fills it with `fill` and cleans it to memory,
/// returning its physical address, or 0 on failure.
fn gic_alloc_its_table(fill: u8) -> PAddr {
    stack_pin_init!(let pages = VmPageDoublyLinkedList::new());
    let Ok(pa) = pmm::alloc_contiguous(ITS_TABLE_PAGES, 0, ITS_TABLE_ALIGN_LOG2, pages.as_mut())
    else {
        return PAddr(0);
    };
    // The pages stay allocated to the GIC for the lifetime of the kernel.
    pages.as_mut().clear();
    let va = paddr_to_physmap(pa).0;
    // SAFETY: `pa` is a run of `ITS_TABLE_SIZE` bytes that was just allocated and that nothing
    // else references, mapped through the physmap. Cleaning it only needs it to be mapped.
    unsafe {
        core::ptr::write_bytes(
            core::ptr::with_exposed_provenance_mut::<u8>(va),
            fill,
            ITS_TABLE_SIZE,
        );
        arch_clean_cache_range(va, ITS_TABLE_SIZE);
    }
    pa
}

/// Posts a command to the ITS command queue. With `sync`, also posts a SYNC, hands the queue to
/// the ITS and waits for it to drain, returning `false` on a stall or a timeout.
fn gic_its_cmd(
    token: &mut LockToken<'_, GicItsLock>,
    dw0: u64,
    dw1: u64,
    dw2: u64,
    sync: bool,
) -> bool {
    let cmdq_va = ITS_CMDQ_VA.load(Ordering::Relaxed);
    if cmdq_va.is_null() {
        return false;
    }
    // SAFETY: `token` proves that `GicItsLock`, the singleton lock that guards `ITS_STATE`, is
    // held.
    let its = unsafe { ITS_STATE.get_mut(token) };
    let mut post = |d0: u64, d1: u64, d2: u64| {
        // SAFETY: `cmdq_cwriter` is a 32 byte aligned offset below `ITS_TABLE_SIZE` into the
        // command queue, which is mapped through the physmap and only written with
        // `GicItsLock` held. Cleaning the slot only needs it to be mapped.
        unsafe {
            let slot = cmdq_va.add(its.cmdq_cwriter as usize).cast::<u64>();
            slot.write(d0);
            slot.add(1).write(d1);
            slot.add(2).write(d2);
            slot.add(3).write(0);
            arch_clean_cache_range(slot.addr(), 32);
        }
        its.cmdq_cwriter = (its.cmdq_cwriter + 32) % ITS_TABLE_SIZE as u64;
    };
    post(dw0, dw1, dw2);
    if !sync {
        return true;
    }
    post(GITS_CMD_SYNC, 0, ITS_CPU0_RDBASE.load(Ordering::Relaxed) << 16);
    device_memory_barrier();
    // SAFETY: `post` wrote and cleaned every command below the new offset, and the barrier
    // above orders that before the ITS sees it.
    unsafe { gits_write64_unsafe(regs::gits_cwriter(), its.cmdq_cwriter) };
    for _ in 0..1000000 {
        let creadr = gits_read64(regs::gits_creadr());
        if (creadr & regs::GITS_CREADR_STALLED) != 0 {
            break;
        }
        if creadr == its.cmdq_cwriter {
            return true;
        }
    }
    dprintf!(
        INFO,
        "GICv3 ITS: command queue timeout/stall (CWRITER={:#x}, CREADR={:#x})\n",
        its.cmdq_cwriter,
        gits_read64(regs::gits_creadr())
    );
    false
}

/// Sets the enable bit of LPI `idx` in the LPI configuration table and invalidates any cached
/// copy of it. `token` proves `GicItsLock` is held.
fn gic_lpi_set_enable_locked(token: &mut LockToken<'_, GicItsLock>, idx: u32, enable: bool) {
    let lpi_prop_va = LPI_PROP_VA.load(Ordering::Relaxed);
    if lpi_prop_va.is_null() || idx >= MAX_LPIS {
        return;
    }
    let val = LPI_PROP_DEFAULT_PRIO | LPI_PROP_GROUP1 | if enable { LPI_PROP_ENABLED } else { 0 };
    // SAFETY: `idx < MAX_LPIS`, so the entry lies within the `ITS_TABLE_SIZE` configuration
    // table, which is reached through the physmap and only written with `GicItsLock` held.
    let entry = unsafe { lpi_prop_va.add(idx as usize) };
    // SAFETY: As above.
    if unsafe { entry.read() } == val {
        return;
    }
    // SAFETY: As above. Cleaning the entry only needs it to be mapped.
    unsafe {
        entry.write(val);
        arch_clean_cache_range(entry.addr(), 1);
    }
    device_memory_barrier();

    if (gicr_read64(0, regs::gicr_typer()) & regs::GICR_TYPER_DIRECT_LPI) != 0 {
        gicr_write64(0, regs::gicr_invlpir(), (LPI_BASE + idx) as u64);
        gic_wait_for_mask(&gicr_bank(0), regs::gicr_syncr(), 1, 0);
    }
    // SAFETY: `token` proves that `GicItsLock`, the singleton lock that guards `ITS_STATE`, is
    // held.
    if ITS_ENABLED.load(Ordering::Relaxed)
        && unsafe { ITS_STATE.get(token) }.lpi_mapped[idx as usize]
    {
        gic_its_cmd(token, GITS_CMD_INV, idx as u64, 0, true);
    }
}

fn gic_init_its(gicd_typer: u32) {
    let gits_phys = GITS_PHYS.load(Ordering::Relaxed);
    if (gicd_typer & regs::GICD_TYPER_LPIS) == 0 || gits_phys == 0 {
        return;
    }

    let gits_base = match map_region(gits_phys, regs::GITS_REG_SIZE, b"gits") {
        Ok(ptr) => ptr,
        Err(status) => {
            dprintf!(
                INFO,
                "GICv3: failed to map ITS registers at {:#x}: {}\n",
                gits_phys,
                status.into_raw()
            );
            return;
        }
    };
    GITS_BASE.store(gits_base.cast::<u32>(), Ordering::Relaxed);

    let gits_typer = gits_read64(regs::gits_typer());
    if (gits_typer & regs::GITS_TYPER_PLPIS) == 0 {
        dprintf!(
            INFO,
            "GICv3: ITS at {:#x} does not support physical LPIs (TYPER={:#x})\n",
            gits_phys,
            gits_typer
        );
        return;
    }

    // Allocate the shared 64 KiB LPI configuration table and per-CPU LPI pending tables.
    let lpi_prop_pa = gic_alloc_its_table(LPI_PROP_DEFAULT_PRIO | LPI_PROP_GROUP1);
    LPI_PROP_PA.store(lpi_prop_pa.0, Ordering::Relaxed);
    if lpi_prop_pa.0 == 0 {
        kprintln!("GICv3: failed to allocate LPI configuration table");
        return;
    }
    LPI_PROP_VA.store(
        core::ptr::with_exposed_provenance_mut::<u8>(paddr_to_physmap(lpi_prop_pa).0),
        Ordering::Relaxed,
    );

    let max_cpus = core::cmp::min(max_num_cpus() as usize, SMP_MAX_CPUS);
    for (cpu, lpi_pend_pa) in LPI_PEND_PA.iter().enumerate().take(max_cpus) {
        let pa = gic_alloc_its_table(0);
        lpi_pend_pa.store(pa.0, Ordering::Relaxed);
        if pa.0 == 0 {
            kprintln!("GICv3: failed to allocate LPI pending table for CPU {}", cpu);
            return;
        }
    }
    gic_init_percpu_lpi(0);

    // Disable ITS before programming BASER and CBASER registers.
    let gits_ctlr = gits_read32(regs::gits_ctlr());
    if (gits_ctlr & regs::GITS_CTLR_ENABLE) != 0 {
        gits_write32(regs::gits_ctlr(), gits_ctlr & !regs::GITS_CTLR_ENABLE);
        gic_wait_for_mask(
            &gits_bank(),
            regs::gits_ctlr(),
            regs::GITS_CTLR_QUIESCENT,
            regs::GITS_CTLR_QUIESCENT,
        );
    }

    // Allocate and program GITS_BASER(0..7) tables (Device and Interrupt Collection tables).
    for n in 0..8 {
        let mut baser = gits_read64(regs::gits_baser(n));
        if ((baser >> regs::GITS_BASER_TYPE_SHIFT) & regs::GITS_BASER_TYPE_MASK) == 0 {
            continue;
        }

        let table_pa = gic_alloc_its_table(0).0 as u64;
        if table_pa == 0 {
            kprintln!("GICv3: failed to allocate GITS_BASER({}) table", n);
            return;
        }

        // Probe supported Page_Size (preferring 64K) and Shareability (WARL fields).
        // SAFETY: Valid is clear and the ITS is disabled, so this only probes the WARL
        // fields; the ITS does not use the table.
        unsafe {
            gits_write64_unsafe(
                regs::gits_baser(n),
                regs::GITS_BASER_PAGE_SIZE_64K | regs::GITS_BASER_SHAREABILITY_ISH,
            )
        };
        baser = gits_read64(regs::gits_baser(n));

        let page_size_field = baser & regs::GITS_BASER_PAGE_SIZE_MASK;
        let mut num_pages: u64 = 16;
        let mut pa_field = table_pa & regs::GITS_BASER_ADDR_MASK_4K_16K;
        if page_size_field == regs::GITS_BASER_PAGE_SIZE_64K {
            num_pages = 1;
            pa_field = (table_pa & regs::GITS_BASER_ADDR_MASK_64K) | ((table_pa >> 36) & 0xf000);
        } else if page_size_field == regs::GITS_BASER_PAGE_SIZE_16K {
            num_pages = 4;
        }

        let mut cache_share = regs::GITS_BASER_INNER_CACHE_NC;
        if (baser & regs::GITS_BASER_SHAREABILITY_MASK) != 0 {
            cache_share = regs::GITS_BASER_SHAREABILITY_ISH | regs::GITS_BASER_INNER_CACHE_WB;
        }
        // SAFETY: `table_pa` is an `ITS_TABLE_SIZE` table from `gic_alloc_its_table`,
        // which is never freed, and `num_pages` pages of the chosen size span
        // `ITS_TABLE_SIZE`. The driver only maps device and collection IDs whose entries
        // fall inside it.
        unsafe {
            gits_write64_unsafe(
                regs::gits_baser(n),
                regs::GITS_BASER_VALID | cache_share | page_size_field | pa_field | (num_pages - 1),
            )
        };
    }

    // Allocate and program the 64 KiB ITS Command Queue (GITS_CBASER).
    let cmdq_pa = gic_alloc_its_table(0);
    if cmdq_pa.0 == 0 {
        kprintln!("GICv3: failed to allocate GITS command queue");
        return;
    }
    ITS_CMDQ_VA.store(
        core::ptr::with_exposed_provenance_mut::<u8>(paddr_to_physmap(cmdq_pa).0),
        Ordering::Relaxed,
    );

    // Issues an ITS command. The lock is uncontended this early; it is taken per command, rather
    // than across the loop below, because `gic_alloc_its_table` may not be called with it held.
    let its_cmd = |dw0: u64, dw1: u64, dw2: u64, sync: bool| -> bool {
        ksync::lock!(let mut guard = GicItsLock::lock(source_tag!()));
        gic_its_cmd(guard.as_mut().token_mut(), dw0, dw1, dw2, sync)
    };
    {
        ksync::lock!(let mut guard = GicItsLock::lock(source_tag!()));
        // SAFETY: `guard` holds `GicItsLock`, the singleton lock that guards `ITS_STATE`.
        unsafe { ITS_STATE.get_mut(guard.as_mut().token_mut()) }.cmdq_cwriter = 0;
    }

    let mut cbaser = regs::GITS_CBASER_VALID
        | regs::GITS_CBASER_INNER_CACHE_WB
        | regs::GITS_CBASER_SHAREABILITY_ISH
        | (cmdq_pa.0 as u64 & regs::GITS_CBASER_ADDR_MASK)
        | (ITS_TABLE_PAGES as u64 - 1);
    // SAFETY: `cmdq_pa` is the `ITS_TABLE_SIZE` command queue from `gic_alloc_its_table`,
    // which is never freed, and the size field programs `ITS_TABLE_PAGES` 4 KiB pages.
    unsafe { gits_write64_unsafe(regs::gits_cbaser(), cbaser) };
    if (gits_read64(regs::gits_cbaser()) & regs::GITS_CBASER_SHAREABILITY_MASK) == 0 {
        cbaser = (cbaser
            & !(regs::GITS_CBASER_SHAREABILITY_MASK | regs::GITS_CBASER_INNER_CACHE_WB))
            | regs::GITS_CBASER_INNER_CACHE_NC;
        // SAFETY: As above.
        unsafe { gits_write64_unsafe(regs::gits_cbaser(), cbaser) };
    }
    // SAFETY: Writing GITS_CBASER reset GITS_CREADR to 0, so this hands the ITS no commands.
    unsafe { gits_write64_unsafe(regs::gits_cwriter(), 0) };

    // Enable the ITS.
    gits_write32(regs::gits_ctlr(), regs::GITS_CTLR_ENABLE);
    device_memory_barrier();

    let its_cpu0_rdbase = if (gits_typer & regs::GITS_TYPER_PTA) != 0 {
        (GIC_MMIO_PHYS.load(Ordering::Relaxed) + GICR_OFFSET.load(Ordering::Relaxed)) >> 16
    } else {
        (gicr_read64(0, regs::gicr_typer()) >> 8) & 0xffff
    };
    ITS_CPU0_RDBASE.store(its_cpu0_rdbase, Ordering::Relaxed);

    // Map Collection 0 to CPU 0.
    if !its_cmd(GITS_CMD_MAPC, 0, (its_cpu0_rdbase << 16) | (1 << 63), true) {
        kprintln!("GICv3: failed to map ITS collection 0");
        return;
    }

    // Map PCI bus 0 slots 0..31 (8 event ID bits = 256 events per device).
    let itt_entry_size = ((gits_typer >> regs::GITS_TYPER_ITT_ENTRY_SIZE_SHIFT)
        & regs::GITS_TYPER_ITT_ENTRY_SIZE_MASK)
        + 1;
    let itt_size = (MAX_LPIS as u64 * itt_entry_size).next_multiple_of(256);
    let mut itt_pa: u64 = 0;
    let mut itt_rem: u64 = 0;
    for slot in 0..MAX_ITS_PCI_SLOTS {
        if itt_rem < itt_size {
            itt_pa = gic_alloc_its_table(0).0 as u64;
            if itt_pa == 0 {
                kprintln!("GICv3: failed to allocate ITS ITT pool");
                return;
            }
            itt_rem = ITS_TABLE_SIZE as u64;
        }
        if !its_cmd(
            GITS_CMD_MAPD | (((slot << 3) as u64) << 32),
            7,
            (itt_pa & 0x000f_ffff_ffff_ff00) | (1 << 63),
            slot + 1 == MAX_ITS_PCI_SLOTS,
        ) {
            kprintln!("GICv3: failed to map ITS device {:#x}", slot << 3);
            return;
        }
        itt_pa += itt_size;
        itt_rem -= itt_size;
    }

    // `get()` reaches the allocator's `init` past `LazyInit`'s own `init`.
    if LPI_ALLOC.get().init(MAX_LPIS).is_err() || LPI_ALLOC.add_range(0, MAX_LPIS).is_err() {
        kprintln!("GICv3: failed to init LPI range allocator");
        return;
    }

    ITS_ENABLED.store(true, Ordering::Relaxed);
    kprintln!("GICv3 ITS enabled at {:#x} (TYPER {:#x}, {} LPIs)", gits_phys, gits_typer, MAX_LPIS);
}

fn gic_init() -> Result<(), Status> {
    ltrace_entry!();

    debug_assert!(ints_disabled());

    let gic_mmio_phys = GIC_MMIO_PHYS.load(Ordering::Relaxed);
    let gicd_offset = GICD_OFFSET.load(Ordering::Relaxed);
    let gicr_offset = GICR_OFFSET.load(Ordering::Relaxed);

    // map in the distributor registers
    let gicd_base = match map_region(gic_mmio_phys + gicd_offset, regs::GICD_REG_SIZE, b"gicd") {
        Ok(ptr) => ptr,
        Err(status) => {
            kprint!("GICv3: failed to map distributor registers");
            return Err(status);
        }
    };
    GICD_BASE.store(gicd_base.cast::<u32>(), Ordering::Relaxed);
    dprintf!(
        SPEW,
        "GICv3: distributor mmio region [{:#x}, {:#x})\n",
        gicd_base as usize,
        gicd_base as usize + regs::GICD_REG_SIZE
    );

    let pidr2 = gicd_read32(regs::gicd_pidr2());
    let rev = (pidr2 >> regs::GICD_PIDR2_ARCHREV_SHIFT) & regs::GICD_PIDR2_ARCHREV_MASK;
    if rev != GICV3 && rev != GICV4 {
        return Err(Status::NOT_FOUND);
    }

    // Compute the stride of the redistributor registers.
    // Override what was passed in from the bootloader, since this is the truth based on the GIC
    // revision.
    // TODO: remove the gicr_stride variable passed in from bootloaders
    if rev == GICV3 {
        GICR_STRIDE.store(0x20000, Ordering::Relaxed);
    } else if rev == GICV4 {
        GICR_STRIDE.store(0x40000, Ordering::Relaxed);
    }
    let gicr_stride = GICR_STRIDE.load(Ordering::Relaxed);

    // map in the redistributor registers
    let redistributer_size = max_num_cpus() as usize * gicr_stride as usize;
    let gicr_base = match map_region(gic_mmio_phys + gicr_offset, redistributer_size, b"gicr") {
        Ok(ptr) => ptr,
        Err(status) => {
            kprint!("GICv3: failed to map redistributor registers");
            return Err(status);
        }
    };
    GICR_BASE.store(gicr_base.cast::<u32>(), Ordering::Relaxed);
    dprintf!(
        SPEW,
        "GICv3: redistributor mmio region [{:#x}, {:#x})\n",
        gicr_base as usize,
        gicr_base as usize + redistributer_size
    );

    let typer = gicd_read32(regs::gicd_typer());
    let gic_max_int = ((typer & 0x1f) + 1) * 32;
    GIC_MAX_INT.store(gic_max_int, Ordering::Relaxed);

    kprintln!("GICv3 detected: rev {}, max interrupts {}, TYPER {:#x}", rev, gic_max_int, typer);

    // disable the distributor
    gicd_write32(regs::gicd_ctlr(), 0);
    gicd_wait_for_rwp();
    isb();

    // distributor config: mask and clear all spis, set group 1.
    let mut i = 32;
    while i < gic_max_int {
        gicd_write32(regs::gicd_icenabler(i / 32), !0);
        gicd_write32(regs::gicd_icpendr(i / 32), !0);
        gicd_write32(regs::gicd_igroupr(i / 32), !0);
        gicd_write32(regs::gicd_igrpmodr(i / 32), 0);
        i += 32;
    }
    gicd_wait_for_rwp();

    // enable distributor with ARE, group 1 enable
    gicd_write32(
        regs::gicd_ctlr(),
        regs::CTLR_ENABLE_G0 | regs::CTLR_ENABLE_G1NS | regs::CTLR_ARE_S,
    );
    gicd_wait_for_rwp();

    // ensure we're running on cpu 0
    debug_assert!(curr_cpu_num() == 0);

    // set spi to target cpu 0 and its associated affinity. must do this after ARE enable
    let max_cpu = (typer >> 5) & 0x7;
    if max_cpu > 0 {
        for i in 32..gic_max_int {
            gicd_write64(regs::gicd_irouter(i), cpu_num_to_mpidr(0));
        }
    }

    GICV3_DRIVER.init_percpu_early();

    gic_init_its(typer);

    device_memory_barrier();
    isb();

    Ok(())
}

/// Extracts the AFF3, AFF2, and AFF1 fields out of a mpidr and formats them according to the
/// ICC_SGI1R register.
fn mpidr_aff_mask_to_sgir_mask(mpidr: u64) -> u64 {
    type Mpidr = MultiprocessorAffinityRegister;
    let mut mask = ((mpidr & Mpidr::AFF3_MASK) >> Mpidr::AFF3_SHIFT) << 48;
    mask |= ((mpidr & Mpidr::AFF2_MASK) >> Mpidr::AFF2_SHIFT) << 32;
    mask |= ((mpidr & Mpidr::AFF1_MASK) >> Mpidr::AFF1_SHIFT) << 16;
    mask
}

/// Sends a pending IPI for the AFF3-1 cluster we've been accumulating a mask for.
fn send_sgi_for_cluster(irq: u32, aff321: u64, aff0_mask: u64) {
    if aff0_mask != 0 {
        debug_assert!((aff0_mask & 0xffff) == aff0_mask);
        let sgi1r = (((irq & 0xf) as u64) << 24) | mpidr_aff_mask_to_sgir_mask(aff321) | aff0_mask;
        regs::write_sgi1r(sgi1r);
    }
}

fn arm_gic_sgi(irq: u32, flags: u32, cpu_mask: cpu_mask_t) -> Result<(), Status> {
    ltracef!("irq {}, flags {}, cpu_mask {:#x}\n", irq, flags, cpu_mask);

    if flags != ARM_GIC_SGI_FLAG_NS {
        return Err(Status::INVALID_ARGS);
    }

    if irq >= 16 {
        return Err(Status::INVALID_ARGS);
    }

    thread_memory_barrier();

    let mut cpu_mask = cpu_mask;
    let mut curr_aff321: u64 = 0; // Current AFF3-1 we're dealing with.
    let mut aff0_mask: u64 = 0; // 16 bit mask of the AFF0 we're accumulating.

    let mut cpu: cpu_num_t = 0;
    while cpu_mask != 0 && cpu < max_num_cpus() {
        type Mpidr = MultiprocessorAffinityRegister;
        let mpidr = cpu_num_to_mpidr(cpu);
        let aff321 = mpidr & (Mpidr::AFF3_MASK | Mpidr::AFF2_MASK | Mpidr::AFF1_MASK);
        let aff0 = mpidr & Mpidr::AFF0_MASK;

        // Without the RS field set, we can only deal with the first
        // 16 cpus within a single cluster.
        debug_assert!(aff0 < 16);

        if aff321 != curr_aff321 {
            // AFF3-1 has changed, see if we need to fire a pending IPI
            send_sgi_for_cluster(irq, curr_aff321, aff0_mask);
            curr_aff321 = aff321;
            aff0_mask = 0;
        }

        // This cpu is within the current aff mask we're looking at, accumulate.
        if (cpu_mask & (1u32 << cpu)) != 0 {
            cpu_mask &= !(1u32 << cpu);
            aff0_mask |= 1u64 << aff0;
        }

        cpu += 1;
    }

    // Fire any leftover accumulated mask.
    send_sgi_for_cluster(irq, curr_aff321, aff0_mask);

    Ok(())
}

fn arm_ipi_halt_handler() {
    ltracef!("cpu {}\n", curr_cpu_num());

    disable_ints();
    loop {
        wfi();
    }
}

/// Returns true if any PPIs are enabled on the calling CPU.
fn is_ppi_enabled() -> bool {
    debug_assert!(ints_disabled());

    // PPIs are 16-31.
    let mask = 0xffff0000u32;

    let cpu_num = curr_cpu_num();
    let reg = gicr_read32(cpu_num, regs::gicr_icenabler0());

    (reg & mask) != 0
}

/// Returns true if any SPIs are enabled on the calling CPU.
fn is_spi_enabled() -> bool {
    debug_assert!(ints_disabled());

    let cpu_num = curr_cpu_num();
    let mpidr = cpu_num_to_mpidr(cpu_num);
    let aff_mask = mpidr & ARM64_MPIDR_MASK;

    // Check each SPI to see if it's routed to this CPU and enabled.
    for i in 32..GIC_MAX_INT.load(Ordering::Relaxed) {
        let router = gicd_read64(regs::gicd_irouter(i));
        if (router & ARM64_MPIDR_MASK) == aff_mask {
            let reg_n = i / 32;
            let bit_n = 1u32 << (i % 32);
            if (gicd_read32(regs::gicd_isenabler(reg_n)) & bit_n) != 0 {
                return true;
            }
        }
    }

    false
}

/// The ARM Generic Interrupt Controller v3 (and v4) driver.
struct Gicv3;

static GICV3_DRIVER: Gicv3 = Gicv3;

impl InterruptControllerOps for Gicv3 {
    fn mask(&self, vector: InterruptVector) -> Result<(), Status> {
        ltracef!("vector {}\n", vector.0);

        if vector.0 >= GIC_MAX_INT.load(Ordering::Relaxed) {
            return Err(Status::INVALID_ARGS);
        }

        gic_set_enable(vector.0, false);

        Ok(())
    }

    fn unmask(&self, vector: InterruptVector) -> Result<(), Status> {
        ltracef!("vector {}\n", vector.0);

        if vector.0 >= GIC_MAX_INT.load(Ordering::Relaxed) {
            return Err(Status::INVALID_ARGS);
        }

        gic_set_enable(vector.0, true);

        Ok(())
    }

    fn deactivate(&self, vector: InterruptVector) -> Result<(), Status> {
        if vector.0 >= GIC_MAX_INT.load(Ordering::Relaxed) {
            return Err(Status::INVALID_ARGS);
        }

        let reg = 1u32 << (vector.0 % 32);
        gicd_write32(regs::gicd_icactiver(vector.0 / 32), reg);

        Ok(())
    }

    fn configure(
        &self,
        vector: InterruptVector,
        tm: InterruptTriggerMode,
        pol: InterruptPolarity,
    ) -> Result<(), Status> {
        ltracef!("vector {}, trigger mode {}, polarity {}\n", vector.0, tm.string(), pol.string());

        if vector.0 <= 15 || vector.0 >= GIC_MAX_INT.load(Ordering::Relaxed) {
            return Err(Status::INVALID_ARGS);
        }

        if pol != InterruptPolarity::High {
            // TODO: polarity should actually be configure through a GPIO controller
            return Err(Status::NOT_SUPPORTED);
        }

        // TODO(https://fxbug.dev/537888583): Unlike the GICv2 driver, this read-modify-write of
        // GICD_ICFGR is not serialized against concurrent callers.
        let reg = vector.0 / 16;
        let mask = 0x2u32 << ((vector.0 % 16) * 2);
        let mut val = gicd_read32(regs::gicd_icfgr(reg));
        if tm == InterruptTriggerMode::Edge {
            val |= mask;
        } else {
            val &= !mask;
        }
        gicd_write32(regs::gicd_icfgr(reg), val);

        let clear_reg = vector.0 / 32;
        let clear_mask = 1u32 << (vector.0 % 32);
        gicd_write32(regs::gicd_icpendr(clear_reg), clear_mask);

        Ok(())
    }

    fn config(
        &self,
        vector: InterruptVector,
    ) -> Result<(InterruptTriggerMode, InterruptPolarity), Status> {
        ltracef!("vector {}\n", vector.0);

        if vector.0 >= GIC_MAX_INT.load(Ordering::Relaxed) {
            return Err(Status::INVALID_ARGS);
        }

        Ok((InterruptTriggerMode::Edge, InterruptPolarity::High))
    }

    fn set_affinity(&self, vector: InterruptVector, mask: cpu_mask_t) -> Result<(), Status> {
        ltracef!("vector {}, mask {:#x}\n", vector.0, mask);

        // Don't complain if we are asked to set affinity for the boot CPU, this is
        // what we always do by default.
        if mask == cpu_num_to_mask(BOOT_CPU_ID) {
            return Ok(());
        }

        Err(Status::NOT_SUPPORTED)
    }

    fn is_valid(&self, vector: InterruptVector, _flags: u32) -> bool {
        vector.0 < GIC_MAX_INT.load(Ordering::Relaxed)
    }

    fn base_vector(&self) -> InterruptVector {
        // ARM Generic Interrupt Controller v3&4 chapter 2.2
        // INTIDs 0-15 are local CPU interrupts
        InterruptVector(16)
    }

    fn max_vector(&self) -> InterruptVector {
        InterruptVector(GIC_MAX_INT.load(Ordering::Relaxed))
    }

    fn remap(&self, vector: InterruptVector) -> InterruptVector {
        ltracef!("vector {}\n", vector.0);
        vector
    }

    fn send_ipi(&self, target: cpu_mask_t, ipi: MpIpi) -> Result<(), Status> {
        let gic_ipi_num = ipi as u32 + IPI_BASE.load(Ordering::Relaxed);

        // filter out targets outside of the range of cpus we care about
        let target = target & (((1u64 << max_num_cpus()) - 1) as cpu_mask_t);
        if target != 0 {
            ltracef!("target {:#x}, gic_ipi {}\n", target, gic_ipi_num);
            let _ = arm_gic_sgi(gic_ipi_num, ARM_GIC_SGI_FLAG_NS, target);
        }

        Ok(())
    }

    fn init_percpu_early(&self) {
        let cpu = curr_cpu_num();

        // wake up the redistributor
        gic_redistributor_sleep(false);

        // redistributer config: configure sgi/ppi as non-secure group 1.
        gicr_write32(cpu, regs::gicr_igroupr0(), !0);
        gicr_write32(cpu, regs::gicr_igrpmod0(), 0);
        gicr_wait_for_rwp(cpu);

        // redistributer config: clear and mask sgi/ppi.
        gicr_write32(cpu, regs::gicr_icenabler0(), 0xffffffff);
        gicr_write32(cpu, regs::gicr_icpendr0(), !0);
        gicr_wait_for_rwp(cpu);

        gic_init_percpu_lpi(cpu);

        // enable system register interface
        if !regs::read_sre() {
            regs::write_sre_enable();
            let sre = regs::read_sre();
            debug_assert!(sre);
        }

        // set priority threshold to max.
        regs::write_pmr(0xff);

        // enable group 1 interrupts.
        regs::write_igrpen(1);

        // Ensure the virtual CPU interface is disabled when running at EL2.
        if libarch::arm64::CURRENT_EL.read().el() >= 2 {
            regs::write_ich_hcr(0);
        }
    }

    fn init_percpu(&self) {
        crate::kernel::mp::set_curr_cpu_online(true);
        let ipi_base = IPI_BASE.load(Ordering::Relaxed);
        let _ = self.unmask(InterruptVector(MpIpi::Generic as u32 + ipi_base));
        let _ = self.unmask(InterruptVector(MpIpi::Reschedule as u32 + ipi_base));
        let _ = self.unmask(InterruptVector(MpIpi::Interrupt as u32 + ipi_base));
        let _ = self.unmask(InterruptVector(MpIpi::Halt as u32 + ipi_base));
    }

    fn handle_irq(&self, frame: &mut Iframe) {
        // ICC_IAR1_EL1 returns a 24-bit INTID in bits [23:0]:
        //   0..15:                SGIs
        //   16..31:               PPIs
        //   32..1019 (0x3fb):     SPIs
        //   1020..1023 (0x3fc..0x3ff): Special / spurious INTIDs
        //   8192+ (LPI_BASE+):    LPIs
        let iar = regs::read_iar();
        let vector = iar & 0xffffff;

        ltracef_level!(2, "iar {:#x}, vector {}\n", iar, vector);

        // Ignore architectural special/spurious INTIDs (1020..1023) without masking
        // out LPIs (INTID >= 8192). Special INTIDs must not be written to EOIR.
        if (0x3fc..=0x3ff).contains(&vector) {
            return;
        }

        // tracking external hardware irqs in this variable
        if vector >= 32 {
            crate::kernel::stats::inc_interrupts();
        }

        let _trace = crate::ktrace_rs::cpu_begin_scope!("kernel:irq", "irq", "irq #" => vector);

        ltracef_level!(
            2,
            "iar {:#x} cpu {} currthread {:p} vector {} pc {:#x}\n",
            iar,
            curr_cpu_num(),
            crate::kernel::thread::current_get(),
            vector,
            frame.elr
        );

        // Deliver LPIs (INTID >= 8192) via the ITS LPI handler table, and
        // SGIs/PPIs/SPIs via the standard pdev interrupt table.
        if vector >= LPI_BASE {
            let idx = vector - LPI_BASE;
            if idx < MAX_LPIS {
                ksync::lock!(
                    let guard =
                        GicLpiHandlerLock::lock_policy::<ksync::NoIrqSavePolicy>()
                );
                // SAFETY: `guard` holds `GicLpiHandlerLock`, which guards `LPI_HANDLERS`.
                let handler = unsafe { LPI_HANDLERS.get(guard.token()) }[idx as usize];
                if handler.present() {
                    handler.invoke();
                }
            }
        } else if (vector as usize) < MAX_INTERRUPTS {
            // INTIDs 1024..8191 are reserved or belong to the GICv3.1 extended SPI/PPI ranges,
            // which this driver never enables, and have no slot in the pdev table.
            // SAFETY: `vector` was checked to be less than `MAX_INTERRUPTS` just above.
            unsafe {
                pdev_invoke_int_if_present(InterruptVector(vector));
            }
        }
        regs::write_eoir(vector);

        ltracef_level!(2, "cpu {} exit\n", curr_cpu_num());
    }

    fn shutdown(&self) {
        // Turn off all GIC0 interrupts at the distributor.
        gicd_write32(regs::gicd_ctlr(), 0);
    }

    fn shutdown_cpu(&self) {
        debug_assert!(ints_disabled());

        // If we're running on a secondary CPU there's a good chance this CPU will be powered off
        // shortly (PSCI_CPU_OFF).  Sending an interrupt to a CPU that's been powered off may result
        // in an "erronerous state" (see Power State Coordination Interface (PSCI) System Software
        // on ARM specification, 5.5.2).  So before we shutdown the GIC, make sure we've
        // migrated/disabled any and all peripheral interrupts targeted at this CPU (PPIs and SPIs).
        //
        // Note, we don't perform these checks on the boot CPU because we don't call PSCI_CPU_OFF on
        // the boot CPU, and we likely still have PPIs and SPIs targeting the boot CPU.
        debug_assert!(curr_cpu_num() == BOOT_CPU_ID || !is_ppi_enabled());
        debug_assert!(curr_cpu_num() == BOOT_CPU_ID || !is_spi_enabled());
        // TODO(maniscalco): If/when we start using LPIs, make sure none are targeted at this CPU.

        // Disable group 1 interrupts at the CPU interface.
        regs::write_igrpen(0);

        // Mask and clear all SGIs and PPIs at the redistributor.
        //
        // Once the PE is marked asleep (below, or by firmware during PSCI CPU_OFF if GICR_WAKER is
        // not writable from here), any interrupt that is both enabled and pending at its
        // redistributor causes the redistributor to assert a wake request to the power controller,
        // which can prevent the CPU's power domain (and thus the cluster or the system) from
        // powering down.  The IPI SGIs are still enabled at this point, and an IPI may have arrived
        // after this CPU disabled interrupts (e.g. a reschedule IPI sent while it was going
        // offline), leaving it pending.  PPIs should already be disabled on a secondary CPU, but
        // that is only checked by the debug assertions above, so mask them too.
        //
        // What matters is that the mask has taken effect before the PE is marked asleep, so wait
        // for it.  An interrupt arriving afterwards is left pending but masked, which is harmless.
        // Clearing any pending state is not strictly necessary, but ensures nothing is left pending
        // across power off.
        //
        // Masking and clearing here cannot cause a lost wakeup:
        //
        // * This CPU will never handle another interrupt before it is powered off.  It has already
        //   disabled interrupts and group 1 at its CPU interface (above), and PSCI CPU_OFF does not
        //   return.  So any interrupt pending here, or arriving later, would go unhandled
        //   regardless.
        //
        // * Nothing depends on this CPU handling such an interrupt.  It is already offline (see
        //   mp_set_curr_cpu_online(false) in mp_unplug_current_cpu) and inactive, so mp_reschedule
        //   no longer targets it, its unpinned threads have been migrated, and mp_sync_exec stops
        //   waiting for (and dequeues its tasks from) CPUs that go offline.  IPIs may still be sent
        //   to an offline CPU (e.g. by mp_interrupt), but they have no work to do there.
        //
        // * An offline CPU is brought back online by PSCI CPU_ON, not by an interrupt.  When it
        //   comes back online, init_percpu_early masks and clears all SGIs and PPIs anyway (as it
        //   always has), and then init_percpu marks the CPU online and unmasks the IPI SGIs.
        //   Unmasking does not clear pending state, so an IPI sent once the CPU is marked online
        //   but before the IPI SGIs are unmasked is still delivered.
        //
        // * This function is only called on secondary CPUs going offline (the boot CPU is never
        //   taken offline).  Suspending a CPU, which must remain wakeable by interrupts, uses
        //   suspend_cpu, which masks nothing.
        //
        // Nor can the mask be lost in a way that matters.  It only needs to hold while this CPU is
        // offline, after which init_percpu_early reinitializes the redistributor, and it is
        // reapplied every time the CPU goes offline (e.g. at the start of every system suspend).
        // Preserving redistributor state while the CPU is powered off, including across a system
        // suspend, is up to the hardware and firmware, as it is for all other GIC state.
        //
        // Note that Secure SGIs and PPIs cannot be masked from here; masking those is up to
        // firmware.
        let cpu = curr_cpu_num();
        gicr_write32(cpu, regs::gicr_icenabler0(), !0);
        gicr_wait_for_rwp(cpu);
        gicr_write32(cpu, regs::gicr_icpendr0(), !0);

        // Mark the PE as offline. This will keep the redistributor from routing
        // interrupts and for any interrupts targeting it, trigger a wake-request to
        // the power controller.
        gic_redistributor_sleep(true);
    }

    fn suspend_cpu(&self) -> Result<(), Status> {
        debug_assert!(ints_disabled());

        gic_redistributor_sleep(true);

        Ok(())
    }

    fn resume_cpu(&self) -> Result<(), Status> {
        self.init_percpu_early();
        self.init_percpu();
        Ok(())
    }

    fn msi_is_supported(&self) -> bool {
        ITS_ENABLED.load(Ordering::Relaxed)
    }

    fn msi_supports_masking(&self) -> bool {
        ITS_ENABLED.load(Ordering::Relaxed)
    }

    fn msi_mask_unmask(&self, block: &MsiBlock, msi_id: u32, mask: bool) {
        debug_assert!(block.allocated);
        debug_assert!(msi_id < block.num_irq);
        let lpi_id = block.base_irq_id + msi_id;
        debug_assert!((LPI_BASE..LPI_BASE + MAX_LPIS).contains(&lpi_id));
        let idx = lpi_id - LPI_BASE;

        ksync::lock!(let mut guard = GicItsLock::lock(source_tag!()));
        gic_lpi_set_enable_locked(guard.as_mut().token_mut(), idx, !mask);
    }

    fn msi_alloc_block(
        &self,
        requested_irqs: u32,
        can_target_64bit: bool,
        _is_msix: bool,
        out_block: &mut MsiBlock,
    ) -> Result<(), Status> {
        if out_block.allocated {
            return Err(Status::BAD_STATE);
        }
        if !ITS_ENABLED.load(Ordering::Relaxed) {
            return Err(Status::NOT_SUPPORTED);
        }
        if requested_irqs == 0 || requested_irqs > MAX_MSI_IRQS as u32 {
            return Err(Status::INVALID_ARGS);
        }

        let doorbell = GITS_PHYS.load(Ordering::Relaxed) + regs::GITS_TRANSLATER_OFFSET;
        let is_32bit = (doorbell >> 32) == 0;
        if !can_target_64bit && !is_32bit {
            return Err(Status::NOT_SUPPORTED);
        }

        // Round up to the power-of-two range size the allocator hands out.
        let alloc_size = requested_irqs.next_power_of_two();
        let alloc_start = LPI_ALLOC.allocate_range(alloc_size)?;

        {
            ksync::lock!(let mut guard = GicItsLock::lock(source_tag!()));
            let token = guard.as_mut().token_mut();
            for i in 0..alloc_size {
                let idx = alloc_start + i;
                // SAFETY: `token` proves that `GicItsLock`, the singleton lock that guards
                // `ITS_STATE`, is held.
                if !unsafe { ITS_STATE.get(token) }.lpi_mapped[idx as usize] {
                    for slot in 0..MAX_ITS_PCI_SLOTS {
                        gic_its_cmd(
                            token,
                            GITS_CMD_MAPTI | (((slot << 3) as u64) << 32),
                            idx as u64 | (((LPI_BASE + idx) as u64) << 32),
                            0,
                            false,
                        );
                    }
                    // SAFETY: As above.
                    unsafe { ITS_STATE.get_mut(token) }.lpi_mapped[idx as usize] = true;
                }
                gic_lpi_set_enable_locked(token, idx, true);
            }
        }

        out_block.is_32bit = is_32bit;
        out_block.base_irq_id = LPI_BASE + alloc_start;
        out_block.num_irq = alloc_size;
        out_block.tgt_addr = doorbell;
        out_block.tgt_data = alloc_start;
        out_block.allocated = true;
        Ok(())
    }

    fn msi_free_block(&self, block: &mut MsiBlock) {
        debug_assert!(block.allocated);
        debug_assert!(
            block.base_irq_id >= LPI_BASE
                && block.base_irq_id + block.num_irq <= LPI_BASE + MAX_LPIS
        );

        let alloc_start = block.base_irq_id - LPI_BASE;
        let range = alloc_start as usize..(alloc_start + block.num_irq) as usize;
        {
            ksync::lock!(let mut guard = GicItsLock::lock(source_tag!()));
            let token = guard.as_mut().token_mut();
            for idx in range.clone() {
                gic_lpi_set_enable_locked(token, idx as u32, false);
            }
        }
        {
            ksync::lock!(let mut guard = GicLpiHandlerLock::lock());
            // SAFETY: `guard` holds `GicLpiHandlerLock`, which guards `LPI_HANDLERS`.
            let handlers = unsafe { LPI_HANDLERS.get_mut(guard.as_mut().token_mut()) };
            handlers[range].fill(InterruptHandler::DEFAULT);
        }

        LPI_ALLOC.free_range(alloc_start, block.num_irq);
        *block = MsiBlock::default();
    }

    fn msi_register_handler(&self, block: &MsiBlock, msi_id: u32, handler: InterruptHandler) {
        debug_assert!(block.allocated);
        debug_assert!(msi_id < block.num_irq);
        let lpi_id = block.base_irq_id + msi_id;
        debug_assert!((LPI_BASE..LPI_BASE + MAX_LPIS).contains(&lpi_id));
        let idx = lpi_id - LPI_BASE;

        // Replacing a handler waits for any call of the old one in flight. This doesn't rely on
        // the enable state: `msi_alloc_block` enables the whole block, and the IRQ path only EOIs
        // an LPI with no handler.
        if !handler.present() {
            ksync::lock!(let mut guard = GicItsLock::lock(source_tag!()));
            gic_lpi_set_enable_locked(guard.as_mut().token_mut(), idx, false);
        }
        {
            ksync::lock!(let mut guard = GicLpiHandlerLock::lock());
            // SAFETY: `guard` holds `GicLpiHandlerLock`, which guards `LPI_HANDLERS`.
            let handlers = unsafe { LPI_HANDLERS.get_mut(guard.as_mut().token_mut()) };
            handlers[idx as usize] = handler;
        }
        if handler.present() {
            ksync::lock!(let mut guard = GicItsLock::lock(source_tag!()));
            gic_lpi_set_enable_locked(guard.as_mut().token_mut(), idx, true);
        }
    }

    fn status(&self, vector: InterruptVector) -> Result<(bool, bool), Status> {
        if vector.0 >= GIC_MAX_INT.load(Ordering::Relaxed) {
            return Err(Status::INVALID_ARGS);
        }

        let reg = vector.0 / 32;
        let mask = 1u32 << (vector.0 % 32);
        let (enabled, pending) = if vector.0 < 32 {
            let cpu_id = curr_cpu_num();
            (
                (gicr_read32(cpu_id, regs::gicr_isenabler0()) & mask) != 0,
                (gicr_read32(cpu_id, regs::gicr_ispendr0()) & mask) != 0,
            )
        } else {
            (
                (gicd_read32(regs::gicd_isenabler(reg)) & mask) != 0,
                (gicd_read32(regs::gicd_ispendr(reg)) & mask) != 0,
            )
        };

        Ok((pending, enabled))
    }
}

unsafe extern "C" {
    fn gicv3_hw_interface_register();
}

extern "C" fn sgi_generic_handler(_: *mut c_void) {
    mbx_generic_irq();
}

extern "C" fn sgi_reschedule_handler(_: *mut c_void) {
    mbx_reschedule_irq();
}

extern "C" fn sgi_interrupt_handler(_: *mut c_void) {
    mbx_interrupt_irq();
}

extern "C" fn sgi_halt_handler(_: *mut c_void) {
    arm_ipi_halt_handler();
}

/// Early (single-threaded) initialization of the GICv3 driver.
///
/// # Safety
///
/// Must only be called once during early single-threaded boot, before multiple CPUs are active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arm_gicv3_init_early(config: &DcfgArmGicV3Driver) {
    assert!(config.mmio_phys != 0);

    GIC_MMIO_PHYS.store(config.mmio_phys, Ordering::Relaxed);
    GICD_OFFSET.store(config.gicd_offset, Ordering::Relaxed);
    GICR_OFFSET.store(config.gicr_offset, Ordering::Relaxed);
    GICR_STRIDE.store(config.gicr_stride, Ordering::Relaxed);
    IPI_BASE.store(config.ipi_base, Ordering::Relaxed);
    if config.its_phys != 0 {
        GITS_PHYS.store(config.its_phys, Ordering::Relaxed);
    } else if config.gicr_offset >= 0xa0000 {
        GITS_PHYS.store(config.mmio_phys + 0x80000, Ordering::Relaxed);
    }

    // SAFETY: This function's contract makes this the only access to `LPI_ALLOC` so far, so
    // construction cannot race with any use of the allocator.
    unsafe {
        let Ok(()) = Pin::static_ref(&LPI_ALLOC).init_pin(Pow2RangeAllocator::new());
    }
}

/// Post-VM initialization of the GICv3 driver, mapping its own MMIO registers and registering
/// with the pdev interrupt layer.
///
/// # Safety
///
/// Must only be called once during boot, after the VM system is ready and before secondary CPUs
/// are brought up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arm_gicv3_init_post_vm(config: &DcfgArmGicV3Driver) {
    if gic_init().is_err() {
        if config.optional != 0 {
            // failed to detect gic v3 but it's marked optional. continue
            return;
        }
        kprintln!("GICv3: failed to detect GICv3, interrupts will be broken");
        return;
    }

    dprintf!(
        SPEW,
        "GICv3: IPI base {}, MMIO phys {:#x}, GICD offset {:#x}, GICR offset/stride {:#x}/{:#x}\n",
        IPI_BASE.load(Ordering::Relaxed),
        GIC_MMIO_PHYS.load(Ordering::Relaxed),
        GICD_OFFSET.load(Ordering::Relaxed),
        GICR_OFFSET.load(Ordering::Relaxed),
        GICR_STRIDE.load(Ordering::Relaxed)
    );

    // SAFETY: This function's contract has it called once during boot, before secondary CPUs are
    // brought up, so nothing calls `get_driver()` concurrently.
    unsafe {
        register_interrupt_controller(&GICV3_DRIVER);
    }

    let ipi_base = IPI_BASE.load(Ordering::Relaxed);
    // SAFETY: The interrupt controller was registered just above.
    let status = unsafe {
        register_sgi_handler(
            MpIpi::Generic as u32 + ipi_base,
            InterruptHandler { cookie: core::ptr::null_mut(), r#fn: Some(sgi_generic_handler) },
        )
    };
    debug_assert!(status.is_ok());
    // SAFETY: As above.
    let status = unsafe {
        register_sgi_handler(
            MpIpi::Reschedule as u32 + ipi_base,
            InterruptHandler { cookie: core::ptr::null_mut(), r#fn: Some(sgi_reschedule_handler) },
        )
    };
    debug_assert!(status.is_ok());
    // SAFETY: As above.
    let status = unsafe {
        register_sgi_handler(
            MpIpi::Interrupt as u32 + ipi_base,
            InterruptHandler { cookie: core::ptr::null_mut(), r#fn: Some(sgi_interrupt_handler) },
        )
    };
    debug_assert!(status.is_ok());
    // SAFETY: As above.
    let status = unsafe {
        register_sgi_handler(
            MpIpi::Halt as u32 + ipi_base,
            InterruptHandler { cookie: core::ptr::null_mut(), r#fn: Some(sgi_halt_handler) },
        )
    };
    debug_assert!(status.is_ok());

    // SAFETY: Registers the hypervisor's GICv3 hardware interface ops, which is safe to call
    // once the GIC has been initialized.
    unsafe {
        gicv3_hw_interface_register();
    }
}

/// Late initialization of the GICv3 driver, placing its registers on the MMIO deny list.
///
/// # Safety
///
/// Must only be called after `rust_arm_gicv3_init_early`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arm_gicv3_init_late(_config: &DcfgArmGicV3Driver) {
    let gic_mmio_phys = GIC_MMIO_PHYS.load(Ordering::Relaxed);
    assert!(gic_mmio_phys != 0);

    // Place the physical address of the GICv3 registers on the MMIO deny list.
    // Users will not be able to create MMIO resources which permit mapping of the
    // GIC registers, even if they have access to the root resource.
    //
    // Unlike GICv2, only the distributor and re-distributor registers are memory
    // mapped. There is one block of distributor registers for the system, and
    // one block of redistributor registers for each CPU.
    let gicd_offset = GICD_OFFSET.load(Ordering::Relaxed);
    let gicr_offset = GICR_OFFSET.load(Ordering::Relaxed);
    let gicr_stride = GICR_STRIDE.load(Ordering::Relaxed);
    root_resource_filter_add_deny_region(
        (gic_mmio_phys + gicd_offset) as usize,
        regs::GICD_REG_SIZE,
        zx_types::ZX_RSRC_KIND_MMIO,
    );
    for i in 0..max_num_cpus() as u64 {
        root_resource_filter_add_deny_region(
            (gic_mmio_phys + gicr_offset + (gicr_stride * i)) as usize,
            gicr_stride as usize,
            zx_types::ZX_RSRC_KIND_MMIO,
        );
    }
    if !GITS_BASE.load(Ordering::Relaxed).is_null() {
        root_resource_filter_add_deny_region(
            GITS_PHYS.load(Ordering::Relaxed) as usize,
            regs::GITS_REG_SIZE,
            zx_types::ZX_RSRC_KIND_MMIO,
        );
    }
}
