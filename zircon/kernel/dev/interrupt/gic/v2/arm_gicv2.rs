// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2012-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Driver for the ARM Generic Interrupt Controller, version 2.

mod gicv2m;
mod gicv2m_msi;
mod regs;

use crate::arch_rs::arm64::wfi;
use crate::arch_rs::{Iframe, curr_cpu_num, disable_ints, ints_disabled};
use crate::gic_common::{
    ARM_GIC_SGI_FLAG_NS, ARM_GIC_SGI_FLAG_TARGET_FILTER_MASK, GIC_BASE_SPI, register_sgi_handler,
};
use crate::kernel::mp::{MpIpi, mbx_generic_irq, mbx_interrupt_irq, mbx_reschedule_irq};
use crate::kernel::types::{
    BOOT_CPU_ID, PAddr, cpu_mask_t, cpu_num_t, cpu_num_to_mask, highest_cpu_set, lowest_cpu_set,
};
use crate::pdev_interrupt::{
    InterruptControllerOps, InterruptHandler, InterruptPolarity, InterruptTriggerMode,
    InterruptVector, MsiBlock, pdev_invoke_int_if_present, register_interrupt_controller,
};
use crate::root_resource_filter::root_resource_filter_add_deny_region;
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED_DEVICE,
};
use crate::vm::vm_aspace::VmAspace;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering};
use debug::{dprintf, ltracef, ltracef_level, tracef};
use kprint::{kprint, kprintln};
use page;
use regio::{MmioBank, MmioPtr, Offset, RwSafe};
#[cfg(ktest)]
use unittest as _;
use zbi::DcfgArmGicV2Driver;
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

// Keep the layout of the configuration struct shared with C++ in sync; the C++ side asserts
// the same values in <dev/interrupt/arm_gicv2_init.h>.
zr::static_assert!(core::mem::size_of::<DcfgArmGicV2Driver>() == 56);
zr::static_assert!(core::mem::align_of::<DcfgArmGicV2Driver>() == 8);

// Values read from the config.
static GIC_BASE: AtomicPtr<u32> = AtomicPtr::new(core::ptr::null_mut());
static GICD_OFFSET: AtomicU64 = AtomicU64::new(0);
static GICC_OFFSET: AtomicU64 = AtomicU64::new(0);
static GICH_OFFSET: AtomicU64 = AtomicU64::new(0);
static GICV_OFFSET: AtomicU64 = AtomicU64::new(0);

static MMIO_PHYS: AtomicU64 = AtomicU64::new(0);
static MSI_FRAME_PHYS: AtomicU64 = AtomicU64::new(0);
static IPI_BASE: AtomicU32 = AtomicU32::new(0);
static USE_MSI: AtomicBool = AtomicBool::new(false);

/// Size of the single mapping covering all of the GICv2 register apertures. `gic_bank` bounds
/// checks register offsets against it.
static GIC_MMIO_SIZE: AtomicU64 = AtomicU64::new(0);

const MAX_INT: u32 = 1024;
static MAX_IRQS: AtomicU32 = AtomicU32::new(0);

/// Offset of the distributor registers within the GICv2 mapping.
fn gicd_offset() -> usize {
    GICD_OFFSET.load(Ordering::Relaxed) as usize
}

/// Offset of the CPU interface registers within the GICv2 mapping.
fn gicc_offset() -> usize {
    GICC_OFFSET.load(Ordering::Relaxed) as usize
}

/// Returns a bank covering the whole GICv2 register mapping, accessed 32 bits at a time.
fn gic_bank() -> MmioBank<u32, RwSafe> {
    let base = GIC_BASE.load(Ordering::Relaxed);
    assert!(!base.is_null(), "GICv2 base pointer is null");
    // SAFETY: `GIC_BASE` is mapped in `rust_arm_gicv2_init_post_vm` before any register access
    // and the mapping remains valid for the lifetime of the kernel.
    let ptr = unsafe { MmioPtr::<u32, RwSafe>::new(base) };
    MmioBank::new(ptr, GIC_MMIO_SIZE.load(Ordering::Relaxed) as usize)
}

/// Reads one of the 32 bit GICv2 registers.
fn arm_gicv2_read32(reg: Offset<u32, RwSafe>) -> u32 {
    // SAFETY: `reg` names a register within the bounds of the GICv2 mapping.
    unsafe { gic_bank().at(reg) }.read()
}

/// Writes one of the 32 bit GICv2 registers.
fn arm_gicv2_write32(reg: Offset<u32, RwSafe>, value: u32) {
    // SAFETY: `reg` names a register within the bounds of the GICv2 mapping.
    unsafe { gic_bank().at(reg) }.write(value);
}

/// GIC v2 only allows 8 cpus.
const MAP_SIZE: usize = 8;

/// Maintains a map of logical cpu numbers, that the kernel uses internally, to
/// GIC cpu masks that are used by the ARM GIC system to specify interrupt
/// targets.
struct CpuMaskTranslator {
    /// Lookup gic_cpu_num by logical cpu number (not mask).
    ///
    /// Each CPU publishes its own entry from `gic_init_percpu` without holding any lock, so the
    /// entries are individually atomic.
    logical_to_gic: [AtomicU8; MAP_SIZE],
}

impl CpuMaskTranslator {
    const fn new() -> Self {
        Self { logical_to_gic: [const { AtomicU8::new(0) }; MAP_SIZE] }
    }

    fn logical_mask_to_gic(&self, logical: cpu_mask_t) -> cpu_mask_t {
        // Special case for only one cpu set since it is pretty common.
        if Self::only_one_cpu(logical) {
            return self.get_gic_mask(lowest_cpu_set(logical));
        }

        let mut out: cpu_mask_t = 0;
        for i in 0..MAP_SIZE as cpu_num_t {
            if ((logical >> i) & 1) != 0 {
                out |= self.get_gic_mask(i);
            }
        }
        out
    }

    fn get_gic_mask(&self, logical_id: cpu_num_t) -> cpu_mask_t {
        cpu_num_to_mask(
            self.logical_to_gic[logical_id as usize].load(Ordering::Relaxed) as cpu_num_t
        )
    }

    fn set_gic_id_for_logical_id(&self, logical_id: cpu_num_t, gic_id: cpu_num_t) {
        debug_assert!((logical_id as usize) < MAP_SIZE);
        debug_assert!(gic_id == (gic_id & 0xFF));
        self.logical_to_gic[logical_id as usize].store(gic_id as u8, Ordering::Relaxed);
    }

    fn only_one_cpu(mask: cpu_mask_t) -> bool {
        // In the general case mask & mask-1 would tell you if there is more than
        // one bit set, but we also account for the value being 0.
        mask != 0 && (mask & (mask - 1)) == 0
    }
}

static MASK_TRANSLATOR: CpuMaskTranslator = CpuMaskTranslator::new();

ksync::declare_singleton_spinlock!(
    /// Serializes the read-modify-write sequences the driver performs on the shared distributor
    /// registers. Acquired with the spinlock's default policy, which saves and restores interrupts.
    GicLock
);

/// Reads one of the first eight `GICD_ITARGETSR` registers, which are banked per CPU.
fn read_gicd_targetsr(target_reg: usize) -> u32 {
    arm_gicv2_read32(regs::gicd_itargetsr(target_reg as u32))
}

/// Determines the calling CPU's mask from the GIC's point of view.
///
/// Exposed for testing: tests pass a `fetch_gicd_targetsr_reg` that returns synthetic
/// `GICD_ITARGETSR` values.
fn gic_determine_local_mask(fetch_gicd_targetsr_reg: impl Fn(usize) -> u32) -> u8 {
    // The first 7 GICD_ITARGETSR registers only return values that apply to the
    // calling CPU. These are interrupt target registers so each byte is a cpu_mask
    // of what cpus are targeted by their cpu_mask from GIC's perspective. The
    // upside is that we can scan these target registers to determine what our
    // cpu_mask is from the GIC's perspective.
    for target_reg in 0..8 {
        let bytes = fetch_gicd_targetsr_reg(target_reg).to_le_bytes();
        for byte in bytes {
            if byte != 0 {
                return byte;
            }
        }
    }

    kprintln!("GICv2: unable to determine local GIC mask!");
    0
}

fn gic_set_enable(vector: u32, enable: bool) {
    let reg = vector / 32;
    let mask = 1u32 << (vector % 32);

    if enable {
        arm_gicv2_write32(regs::gicd_isenabler(reg), mask);
    } else {
        arm_gicv2_write32(regs::gicd_icenabler(reg), mask);
    }
}

fn arm_gic_max_cpu() -> u32 {
    ((arm_gicv2_read32(regs::gicd_typer()) >> 5) & 0x7) + 1
}

fn arm_gic_init() -> Result<(), Status> {
    let typer = arm_gicv2_read32(regs::gicd_typer());
    let it_lines_number = typer & 0x1f;
    let max_irqs = (it_lines_number + 1) * 32;
    MAX_IRQS.store(max_irqs, Ordering::Relaxed);
    kprintln!(
        "GICv2 detected: max interrupts {}, max_cpus {}, TYPER {:#x}",
        max_irqs,
        arm_gic_max_cpu(),
        typer
    );
    debug_assert!(max_irqs <= MAX_INT);

    let mut i = 0;
    while i < max_irqs {
        arm_gicv2_write32(regs::gicd_icenabler(i / 32), !0);
        arm_gicv2_write32(regs::gicd_icpendr(i / 32), !0);
        i += 32;
    }

    if arm_gic_max_cpu() > 0 {
        // Set external interrupts to target cpu 0
        let mut i = 32;
        while i < max_irqs {
            arm_gicv2_write32(regs::gicd_itargetsr(i / 4), 0x01010101);
            i += 4;
        }
    }
    // Initialize all the SPIs to edge triggered
    for i in GIC_BASE_SPI..max_irqs {
        let _ = GICV2.configure(
            InterruptVector(i),
            InterruptTriggerMode::Edge,
            InterruptPolarity::High,
        );
    }

    arm_gicv2_write32(regs::gicd_ctlr(), 1); // enable GIC0

    GICV2.init_percpu_early();

    Ok(())
}

fn arm_gic_sgi(irq: u32, flags: u32, cpu_mask: u32) -> Result<(), Status> {
    let val = ((flags & ARM_GIC_SGI_FLAG_TARGET_FILTER_MASK) << 24)
        | ((cpu_mask & 0xff) << 16)
        | if (flags & ARM_GIC_SGI_FLAG_NS) != 0 { 1u32 << 15 } else { 0 }
        | (irq & 0xf);

    if irq >= 16 {
        return Err(Status::INVALID_ARGS);
    }

    ltracef!("GICD_SGIR: {:x}\n", val);

    arm_gicv2_write32(regs::gicd_sgir(), val);

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
    let ppi_mask = 0xffff0000u32;

    // GICD_ISENABLER0 is banked so it corresponds to *this* CPU's interface.
    (arm_gicv2_read32(regs::gicd_isenabler(0)) & ppi_mask) != 0
}

/// Returns true if any SPIs are enabled on the calling CPU.
fn is_spi_enabled() -> bool {
    debug_assert!(ints_disabled());

    // We're going to check four interrupts at a time.  Build a repeated mask for the current CPU.
    // Each byte in the mask is a CPU bit mask corresponding to CPU0..CPU7 (lsb..msb).
    let cpu_num = curr_cpu_num();
    debug_assert!(cpu_num < 8);
    let mask = 0x01010101u32 << cpu_num;

    let mut vector = GIC_BASE_SPI;
    while vector < MAX_IRQS.load(Ordering::Relaxed) {
        let reg = arm_gicv2_read32(regs::gicd_itargetsr(vector / 4));
        if (reg & mask) != 0 {
            return true;
        }
        vector += 4;
    }

    false
}

/// The ARM Generic Interrupt Controller v2 driver.
struct Gicv2;

static GICV2: Gicv2 = Gicv2;

impl InterruptControllerOps for Gicv2 {
    fn mask(&self, vector: InterruptVector) -> Result<(), Status> {
        ltracef!("vector {}\n", vector.0);
        if vector.0 >= MAX_IRQS.load(Ordering::Relaxed) {
            return Err(Status::INVALID_ARGS);
        }

        gic_set_enable(vector.0, false);

        Ok(())
    }

    fn unmask(&self, vector: InterruptVector) -> Result<(), Status> {
        ltracef!("vector {}\n", vector.0);
        if vector.0 >= MAX_IRQS.load(Ordering::Relaxed) {
            return Err(Status::INVALID_ARGS);
        }

        gic_set_enable(vector.0, true);

        Ok(())
    }

    fn deactivate(&self, vector: InterruptVector) -> Result<(), Status> {
        if vector.0 >= MAX_IRQS.load(Ordering::Relaxed) {
            return Err(Status::INVALID_ARGS);
        }

        let reg = 1u32 << (vector.0 % 32);
        arm_gicv2_write32(regs::gicd_icactiver(vector.0 / 32), reg);

        Ok(())
    }

    fn configure(
        &self,
        vector: InterruptVector,
        tm: InterruptTriggerMode,
        pol: InterruptPolarity,
    ) -> Result<(), Status> {
        // Only configurable for SPI interrupts
        if (vector.0 >= MAX_IRQS.load(Ordering::Relaxed)) || (vector.0 < GIC_BASE_SPI) {
            return Err(Status::INVALID_ARGS);
        }

        if pol != InterruptPolarity::High {
            // TODO: polarity should actually be configure through a GPIO controller
            return Err(Status::NOT_SUPPORTED);
        }

        ksync::lock!(GicLock::lock());

        // type is encoded with two bits, MSB of the two determine type
        // 16 irqs encoded per ICFGR register
        let reg_ndx = vector.0 >> 4;
        let bit_shift = ((vector.0 & 0xf) << 1) + 1;
        let mut reg_val = arm_gicv2_read32(regs::gicd_icfgr(reg_ndx));
        if tm == InterruptTriggerMode::Edge {
            reg_val |= 1u32 << bit_shift;
        } else {
            reg_val &= !(1u32 << bit_shift);
        }
        arm_gicv2_write32(regs::gicd_icfgr(reg_ndx), reg_val);

        let clear_reg = vector.0 / 32;
        let clear_mask = 1u32 << (vector.0 % 32);
        arm_gicv2_write32(regs::gicd_icpendr(clear_reg), clear_mask);

        Ok(())
    }

    fn config(
        &self,
        vector: InterruptVector,
    ) -> Result<(InterruptTriggerMode, InterruptPolarity), Status> {
        if vector.0 >= MAX_IRQS.load(Ordering::Relaxed) {
            return Err(Status::INVALID_ARGS);
        }

        Ok((InterruptTriggerMode::Edge, InterruptPolarity::High))
    }

    fn set_affinity(&self, vector: InterruptVector, mask: cpu_mask_t) -> Result<(), Status> {
        ltracef!("vector {}, mask {:#x}\n", vector.0, mask);

        if vector.0 >= MAX_IRQS.load(Ordering::Relaxed) {
            return Err(Status::INVALID_ARGS);
        }

        // make sure the cpu mask is within range
        if highest_cpu_set(mask) > 7 {
            return Err(Status::INVALID_ARGS);
        }

        // The inner scope releases `GicLock` before the result is traced.
        let (old_targetsr, targetsr) = {
            ksync::lock!(GicLock::lock());

            // set the 4 bit field in the corresponding targetsr register with the
            // 8 bit cpu mask.
            let mut targetsr = arm_gicv2_read32(regs::gicd_itargetsr(vector.0 / 4));
            let old_targetsr = targetsr;
            targetsr &= !(0xffu32 << ((vector.0 % 4) * 8));
            targetsr |= mask << ((vector.0 % 4) * 8);
            arm_gicv2_write32(regs::gicd_itargetsr(vector.0 / 4), targetsr);

            (old_targetsr, targetsr)
        };

        ltracef!("setting TARGETSR old {:#x} new {:#x}\n", old_targetsr, targetsr);

        Ok(())
    }

    fn is_valid(&self, vector: InterruptVector, _flags: u32) -> bool {
        vector.0 < MAX_IRQS.load(Ordering::Relaxed)
    }

    fn base_vector(&self) -> InterruptVector {
        // ARM Generic Interrupt Controller v2 chapter 2.1
        // INTIDs 0-15 are local CPU interrupts
        InterruptVector(16)
    }

    fn max_vector(&self) -> InterruptVector {
        InterruptVector(MAX_IRQS.load(Ordering::Relaxed))
    }

    fn remap(&self, vector: InterruptVector) -> InterruptVector {
        vector
    }

    fn send_ipi(&self, logical_target: cpu_mask_t, ipi: MpIpi) -> Result<(), Status> {
        let target = MASK_TRANSLATOR.logical_mask_to_gic(logical_target);

        let gic_ipi_num = ipi as u32 + IPI_BASE.load(Ordering::Relaxed);

        if target != 0 {
            ltracef!("target {:#x}, gic_ipi {}\n", target, gic_ipi_num);
            let _ = arm_gic_sgi(gic_ipi_num, ARM_GIC_SGI_FLAG_NS, target);
        }

        Ok(())
    }

    fn init_percpu_early(&self) {
        arm_gicv2_write32(regs::gicc_ctlr(), 0x1); // EnableGrp1
        arm_gicv2_write32(regs::gicc_pmr(), 0xff); // unmask interrupts at all priority levels
    }

    fn init_percpu(&self) {
        crate::kernel::mp::set_curr_cpu_online(true);
        let ipi_base = IPI_BASE.load(Ordering::Relaxed);
        let _ = self.unmask(InterruptVector(MpIpi::Generic as u32 + ipi_base));
        let _ = self.unmask(InterruptVector(MpIpi::Reschedule as u32 + ipi_base));
        let _ = self.unmask(InterruptVector(MpIpi::Interrupt as u32 + ipi_base));
        let _ = self.unmask(InterruptVector(MpIpi::Halt as u32 + ipi_base));

        let logical_num = curr_cpu_num();

        match crate::topology::get_system_topology().processor_by_logical_id(logical_num) {
            Ok(node) => {
                let arm64_info = match *node.entity() {
                    zbi::TopologyEntity::Processor(zbi::TopologyProcessor {
                        architecture_info: zbi::TopologyArchitectureInfo::Arm64(info),
                        ..
                    }) => Some(info),
                    _ => None,
                };
                debug_assert!(arm64_info.is_some());
                if let Some(info) = arm64_info {
                    let gic_id = cpu_num_t::from(info.gic_id);
                    MASK_TRANSLATOR.set_gic_id_for_logical_id(logical_num, gic_id);
                }
            }
            Err(status) => kprintln!(
                "arm_gicv2: unable to get logical processor {} in topology, status: {}",
                logical_num,
                status.into_raw()
            ),
        }

        // Lookup the gic_mask for this processor via GIC registers and compare it
        // to what we were told by the bootloader, warn if there is a mismatch.
        let gic_mask = gic_determine_local_mask(read_gicd_targetsr) as cpu_mask_t;
        let assigned_gic_mask = MASK_TRANSLATOR.get_gic_mask(logical_num);
        if gic_mask != assigned_gic_mask {
            kprint!(
                "arm_gicv2: WARNING assigned gic_id of {} does not match processor's gic_id of {}.\
Successful operation is unlikely!",
                assigned_gic_mask,
                gic_mask
            );
        }
        tracef!(
            "logical_cpu_mask: {} programmatic_gic_mask: {} assigned_gic_mask: {}\n",
            cpu_num_to_mask(curr_cpu_num()),
            gic_mask,
            assigned_gic_mask
        );
    }

    fn handle_irq(&self, frame: &mut Iframe) {
        // get the current vector
        let iar = arm_gicv2_read32(regs::gicc_iar());
        let vector = iar & 0x3ff;

        if vector >= 0x3fe {
            // spurious
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

        // deliver the interrupt
        // SAFETY: The `vector >= 0x3fe` check above caps `vector` at 0x3fd, which is less than
        // `MAX_INTERRUPTS` (1024), so the handler table index is in range.
        unsafe {
            pdev_invoke_int_if_present(InterruptVector(vector));
        }
        arm_gicv2_write32(regs::gicc_eoir(), iar);

        ltracef_level!(2, "cpu {} exit\n", curr_cpu_num());
    }

    fn shutdown(&self) {
        // Turn off all GIC0 interrupts at the distributor.
        arm_gicv2_write32(regs::gicd_ctlr(), 0);
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

        // Turn off interrupts at the CPU interface.
        arm_gicv2_write32(regs::gicc_ctlr(), 0);
    }

    fn suspend_cpu(&self) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }

    fn resume_cpu(&self) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }

    fn msi_is_supported(&self) -> bool {
        gicv2m_msi::arm_gicv2m_msi_is_supported()
    }

    fn msi_supports_masking(&self) -> bool {
        gicv2m_msi::arm_gicv2m_msi_supports_masking()
    }

    fn msi_mask_unmask(&self, block: &MsiBlock, msi_id: u32, mask: bool) {
        gicv2m_msi::arm_gicv2m_msi_mask_unmask(block, msi_id, mask);
    }

    fn msi_alloc_block(
        &self,
        requested_irqs: u32,
        can_target_64bit: bool,
        is_msix: bool,
        out_block: &mut MsiBlock,
    ) -> Result<(), Status> {
        gicv2m_msi::arm_gicv2m_msi_alloc_block(requested_irqs, can_target_64bit, is_msix, out_block)
    }

    fn msi_free_block(&self, block: &mut MsiBlock) {
        gicv2m_msi::arm_gicv2m_msi_free_block(block);
    }

    fn msi_register_handler(&self, block: &MsiBlock, msi_id: u32, handler: InterruptHandler) {
        gicv2m_msi::arm_gicv2m_msi_register_handler(block, msi_id, handler);
    }

    fn status(&self, vector: InterruptVector) -> Result<(bool, bool), Status> {
        // TODO(https://fxbug.dev/537888583): Unlike every other entry point of this driver, the
        // bound checked here is the architectural maximum rather than the number of interrupts the
        // distributor reported.
        if vector.0 >= MAX_INT {
            return Err(Status::INVALID_ARGS);
        }

        let reg = vector.0 / 32;
        let mask = 1u32 << (vector.0 % 32);
        let enabled = (arm_gicv2_read32(regs::gicd_isenabler(reg)) & mask) != 0;
        let pending = (arm_gicv2_read32(regs::gicd_ispendr(reg)) & mask) != 0;

        Ok((pending, enabled))
    }
}

unsafe extern "C" {
    fn gicv2_hw_interface_register();
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

// Init hooks

/// Early (single-threaded) initialization of the GICv2 driver.
///
/// # Safety
///
/// Must only be called once during early single-threaded boot, before multiple CPUs are active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arm_gicv2_init_early(config: &DcfgArmGicV2Driver) {
    assert!(config.mmio_phys != 0);

    // SAFETY: Called once during early single-threaded boot, before the MSI allocators can be
    // reached by any other code.
    unsafe {
        gicv2m_msi::init_allocators();
    }

    MMIO_PHYS.store(config.mmio_phys, Ordering::Relaxed);
    MSI_FRAME_PHYS.store(config.msi_frame_phys, Ordering::Relaxed);
    GICD_OFFSET.store(config.gicd_offset, Ordering::Relaxed);
    GICC_OFFSET.store(config.gicc_offset, Ordering::Relaxed);
    GICH_OFFSET.store(config.gich_offset, Ordering::Relaxed);
    GICV_OFFSET.store(config.gicv_offset, Ordering::Relaxed);
    IPI_BASE.store(config.ipi_base, Ordering::Relaxed);
    USE_MSI.store(config.use_msi != 0, Ordering::Relaxed);
}

/// Post-VM initialization of the GICv2 driver, mapping its own MMIO registers and registering
/// with the pdev interrupt layer.
///
/// # Safety
///
/// Must only be called once during boot, after the VM system is ready and before secondary CPUs
/// are brought up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arm_gicv2_init_post_vm(config: &DcfgArmGicV2Driver) {
    // Compute the total size of the mmio region by finding the farthest offset and adding its size,
    // assuming all of the space between is part of the same mmio interface.
    let mut max_offset: u64 = 0;
    let mut mmio_size: usize = 0;

    let gicv2_apertures = [
        (GICD_OFFSET.load(Ordering::Relaxed), regs::GICD_REG_SIZE),
        (GICC_OFFSET.load(Ordering::Relaxed), regs::GICC_REG_SIZE),
        (GICH_OFFSET.load(Ordering::Relaxed), regs::GICH_REG_SIZE),
        (GICV_OFFSET.load(Ordering::Relaxed), regs::GICV_REG_SIZE),
    ];
    // TODO(https://fxbug.dev/537888583): This tracks the aperture with the farthest offset rather
    // than the one that ends last, so an aperture placed last but sized small yields an mmio_size
    // that does not cover the end of every aperture. The `MmioBank` built over this size asserts
    // that each offset lies inside it, so an access past the end of a short mmio_size panics.
    for (offset, len) in gicv2_apertures {
        if offset > max_offset {
            max_offset = offset;
            mmio_size = offset as usize + len;
        }
    }

    // Map the mmio region.
    let mmio_phys = MMIO_PHYS.load(Ordering::Relaxed);
    let gic_base = match map_region(mmio_phys, mmio_size, b"gicv2") {
        Ok(ptr) => ptr,
        Err(status) => panic!("Could not allocate GICv2 mmio region: {}\n", status.into_raw()),
    };
    GIC_BASE.store(gic_base.cast::<u32>(), Ordering::Relaxed);
    GIC_MMIO_SIZE.store(mmio_size as u64, Ordering::Relaxed);
    dprintf!(
        SPEW,
        "GICv2: mmio region [{:#x}, {:#x})\n",
        gic_base.addr(),
        gic_base.addr() + mmio_size
    );
    assert!(!gic_base.is_null());

    if arm_gic_init().is_err() {
        if config.optional != 0 {
            // failed to detect gic v2 but it's marked optional. continue
            return;
        }
        kprintln!("GICv2: failed to detect GICv2, interrupts will be broken");
        return;
    }

    dprintf!(
        SPEW,
        "GICv2 (ID {:#x}), IPI base {}, MMIO phys {:#x}, GICD offset {:#x}, GICC offset {:#x}\n\
         \tGICH offset {:#x}, GICV offset {:#x}\n",
        arm_gicv2_read32(regs::gicc_iidr()),
        IPI_BASE.load(Ordering::Relaxed),
        mmio_phys,
        GICD_OFFSET.load(Ordering::Relaxed),
        GICC_OFFSET.load(Ordering::Relaxed),
        GICH_OFFSET.load(Ordering::Relaxed),
        GICV_OFFSET.load(Ordering::Relaxed)
    );

    // pass the list of physical and virtual addresses for the GICv2m register apertures
    let msi_frame_phys = MSI_FRAME_PHYS.load(Ordering::Relaxed);
    if USE_MSI.load(Ordering::Relaxed) && msi_frame_phys != 0 {
        // map in the gicv2m aperture
        let frame_base = match map_region(msi_frame_phys, regs::GICV2M_FRAME_REG_SIZE, b"gicv2m") {
            Ok(ptr) => ptr,
            Err(status) => {
                panic!("Could not allocate GICv2m mmio region: {}\n", status.into_raw())
            }
        };
        dprintf!(
            SPEW,
            "GICv2m: mmio region [{:#x}, {:#x})\n",
            frame_base.addr(),
            frame_base.addr() + regs::GICV2M_FRAME_REG_SIZE
        );

        // `arm_gicv2m_init` copies the frames into its own storage, so these may live on the
        // stack.
        let reg_frames = [msi_frame_phys];
        let reg_frames_virt = [frame_base.cast::<u32>()];
        assert!(!reg_frames_virt[0].is_null());
        // SAFETY: `frame_base` is the base of the `GICV2M_FRAME_REG_SIZE` device mapping of
        // `msi_frame_phys` made just above, which is never unmapped.
        unsafe {
            gicv2m::arm_gicv2m_init(&reg_frames, &reg_frames_virt);
        }
    }
    // SAFETY: This function's contract has it called once during boot, before secondary CPUs are
    // brought up, so nothing calls `get_driver()` concurrently.
    unsafe {
        register_interrupt_controller(&GICV2);
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

    // SAFETY: Registers the hypervisor's GICv2 hardware interface ops, which is safe to call
    // once the GIC has been initialized.
    unsafe {
        gicv2_hw_interface_register();
    }
}

/// Late initialization of the GICv2 driver, placing its registers on the MMIO deny list.
///
/// # Safety
///
/// Must only be called after `rust_arm_gicv2_init_early`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arm_gicv2_init_late(_config: &DcfgArmGicV2Driver) {
    let mmio_phys = MMIO_PHYS.load(Ordering::Relaxed);
    assert!(mmio_phys != 0);

    // Place the physical address of the GICv2 registers on the MMIO deny list.
    // Users will not be able to create MMIO resources which permit mapping of the
    // GIC registers, even if they have access to the root resource.
    let gich_offset = GICH_OFFSET.load(Ordering::Relaxed);
    let gicv_offset = GICV_OFFSET.load(Ordering::Relaxed);
    let msi_frame_phys = MSI_FRAME_PHYS.load(Ordering::Relaxed);
    root_resource_filter_add_deny_region(
        (mmio_phys + GICC_OFFSET.load(Ordering::Relaxed)) as usize,
        regs::GICC_REG_SIZE,
        zx_types::ZX_RSRC_KIND_MMIO,
    );
    root_resource_filter_add_deny_region(
        (mmio_phys + GICD_OFFSET.load(Ordering::Relaxed)) as usize,
        regs::GICD_REG_SIZE,
        zx_types::ZX_RSRC_KIND_MMIO,
    );
    if gich_offset != 0 {
        root_resource_filter_add_deny_region(
            (mmio_phys + gich_offset) as usize,
            regs::GICH_REG_SIZE,
            zx_types::ZX_RSRC_KIND_MMIO,
        );
    }
    if gicv_offset != 0 {
        root_resource_filter_add_deny_region(
            (mmio_phys + gicv_offset) as usize,
            regs::GICV_REG_SIZE,
            zx_types::ZX_RSRC_KIND_MMIO,
        );
    }
    if msi_frame_phys != 0 {
        root_resource_filter_add_deny_region(
            msi_frame_phys as usize,
            regs::GICV2M_FRAME_REG_SIZE,
            zx_types::ZX_RSRC_KIND_MMIO,
        );
    }
}

/// Returns the virtual address of the GICV (virtual CPU interface) aperture, or null if the
/// platform does not implement it.
#[unsafe(no_mangle)]
pub extern "C" fn rust_arm_gicv2_gicv_address() -> *mut c_void {
    let gicv_offset = GICV_OFFSET.load(Ordering::Relaxed);
    if gicv_offset == 0 {
        return core::ptr::null_mut();
    }
    GIC_BASE.load(Ordering::Relaxed).wrapping_byte_add(gicv_offset as usize).cast()
}

/// Returns the virtual address of the GICH (virtual interface control) aperture.
#[unsafe(no_mangle)]
pub extern "C" fn rust_arm_gicv2_gich_address() -> *mut c_void {
    let gich_offset = GICH_OFFSET.load(Ordering::Relaxed);
    GIC_BASE.load(Ordering::Relaxed).wrapping_byte_add(gich_offset as usize).cast()
}

/// ARM GICv2 driver kernel tests.
#[cfg(ktest)]
#[unittest::suite(name = "gicv2")]
mod tests {
    use unittest::assert_eq;

    /// Set and lookup translations.
    #[test]
    fn test_gic_translator_translate() {
        let translator = CpuMaskTranslator::new();

        translator.set_gic_id_for_logical_id(0, 7);
        assert_eq!(0b10000000u32, translator.get_gic_mask(0));

        translator.set_gic_id_for_logical_id(1, 2);
        assert_eq!(0b00000100u32, translator.get_gic_mask(1));

        translator.set_gic_id_for_logical_id(2, 0);
        assert_eq!(0b00000001u32, translator.get_gic_mask(2));
    }

    /// Translate full cpu masks.
    #[test]
    fn test_gic_translator_translate_mask() {
        let translator = CpuMaskTranslator::new();

        translator.set_gic_id_for_logical_id(0, 7);
        translator.set_gic_id_for_logical_id(1, 2);
        translator.set_gic_id_for_logical_id(2, 0);

        assert_eq!(0b10000101u32, translator.logical_mask_to_gic(0b00000111));
        assert_eq!(0b10000000u32, translator.logical_mask_to_gic(0b00000001));
    }

    /// Determine local mask from target registers.
    #[test]
    fn test_determine_local_mask() {
        {
            // From second reg in first target.
            let targets: [u32; 8] = [
                0x00800000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
                0x00000000,
            ];

            assert_eq!(
                0b10000000u8,
                gic_determine_local_mask(|target| {
                    debug_assert!(target < 8);
                    targets[target]
                })
            );
        }

        {
            // From last reg in last target.
            let targets: [u32; 8] = [
                0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
                0x00000080,
            ];

            assert_eq!(
                0b10000000u8,
                gic_determine_local_mask(|target| {
                    debug_assert!(target < 8);
                    targets[target]
                })
            );
        }
    }
}
