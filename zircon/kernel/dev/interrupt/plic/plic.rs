// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Ported from zircon/kernel/dev/interrupt/plic/plic.cc

use crate::arch_rs::riscv64::{boot_hart_id, curr_hart_id};
use crate::kernel::types::PAddr;
use crate::pdev_interrupt::{
    InterruptHandler, InterruptPolarity, InterruptTriggerMode, InterruptVector, MsiBlock,
    PdevInterruptOps, pdev_invoke_int_if_present, pdev_register_interrupts,
};
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED_DEVICE,
};
use crate::vm::vm_aspace::VmAspace;
use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use debug::{ltrace_entry, ltrace_exit, ltracef, ltracef_level};
use page;
use regio::{MmioBank, MmioPtr, Offset, RwSafe};
#[cfg(ktest)]
use unittest as _;
use zbi::DcfgRiscvPlicDriver;
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

static PLIC_BASE: AtomicPtr<u32> = AtomicPtr::new(core::ptr::null_mut());
static PLIC_SIZE: AtomicU32 = AtomicU32::new(0);
static PLIC_MAX_INT: AtomicU32 = AtomicU32::new(0);

// HACK: Temporary workaround for the SiFive HiFive Unleashed which has a
// different calculation formulas than QEMU-virt:
// #define SIFIVE_HIFIVE_UNLEASHED_HACK
const SIFIVE_HIFIVE_UNLEASHED_HACK: bool = false;

// TODO-rvbringup: have the offsets of each hart target be defined in ZBI from device tree
fn plic_hart_idx(hart: u32) -> u32 {
    if SIFIVE_HIFIVE_UNLEASHED_HACK {
        if hart != 0 { 2 * hart } else { !0 }
    } else {
        (2 * hart) + 1
    }
}

fn plic_bank() -> MmioBank<u32, RwSafe> {
    let base = PLIC_BASE.load(Ordering::Relaxed);
    assert!(!base.is_null(), "PLIC base pointer is null");
    let size = PLIC_SIZE.load(Ordering::Relaxed) as usize;
    // SAFETY: PLIC_BASE is mapped and initialized in plic_init_post_vm.
    // Memory mapping remains valid for the duration of the kernel's lifetime.
    let ptr = unsafe { MmioPtr::<u32, RwSafe>::new(base) };
    MmioBank::new(ptr, size)
}

fn plic_priority_offset(irq: u32) -> Offset<u32, RwSafe> {
    let word_offset = if SIFIVE_HIFIVE_UNLEASHED_HACK { irq as usize } else { 1 + irq as usize };
    Offset::new(word_offset * 4)
}

fn plic_enable_offset(irq: u32, hart: u32) -> Offset<u32, RwSafe> {
    let word_offset = 0x800 + (0x20 * plic_hart_idx(hart) as usize) + (irq as usize / 32);
    Offset::new(word_offset * 4)
}

fn plic_threshold_offset(hart: u32) -> Offset<u32, RwSafe> {
    let word_offset = 0x80000 + (0x400 * plic_hart_idx(hart) as usize);
    Offset::new(word_offset * 4)
}

fn plic_claim_complete_offset(hart: u32) -> Offset<u32, RwSafe> {
    let word_offset = 0x80001 + (0x400 * plic_hart_idx(hart) as usize);
    Offset::new(word_offset * 4)
}

extern "C" fn plic_is_valid_interrupt(vector: InterruptVector, _flags: u32) -> bool {
    vector.0 < PLIC_MAX_INT.load(Ordering::Relaxed)
}

extern "C" fn plic_get_base_vector() -> InterruptVector {
    InterruptVector(0)
}

extern "C" fn plic_get_max_vector() -> InterruptVector {
    InterruptVector(PLIC_MAX_INT.load(Ordering::Relaxed))
}

extern "C" fn plic_init_percpu_early() {}

fn plic_enable_vector(vector: u32, hart_id: u32) {
    let bank = plic_bank();
    let offset = plic_enable_offset(vector, hart_id);
    // SAFETY: offset is within bounds of the mapped PLIC MMIO bank.
    let reg = unsafe { bank.at(offset) };
    reg.modify(|val| *val |= 1 << (vector % 32));
}

fn plic_disable_vector(vector: u32, hart_id: u32) {
    let bank = plic_bank();
    let offset = plic_enable_offset(vector, hart_id);
    // SAFETY: offset is within bounds of the mapped PLIC MMIO bank.
    let reg = unsafe { bank.at(offset) };
    reg.modify(|val| *val &= !(1 << (vector % 32)));
}

extern "C" fn plic_mask_interrupt(vector: InterruptVector) -> Status {
    ltracef!("vector {}\n", vector.0);
    if vector.0 >= PLIC_MAX_INT.load(Ordering::Relaxed) {
        return Status::INVALID_ARGS;
    }
    plic_disable_vector(vector.0, boot_hart_id());
    Status::OK
}

extern "C" fn plic_unmask_interrupt(vector: InterruptVector) -> Status {
    ltracef!("vector {}\n", vector.0);
    if vector.0 >= PLIC_MAX_INT.load(Ordering::Relaxed) {
        return Status::INVALID_ARGS;
    }
    plic_enable_vector(vector.0, boot_hart_id());
    Status::OK
}

extern "C" fn plic_deactivate_interrupt(vector: InterruptVector) -> Status {
    if vector.0 >= PLIC_MAX_INT.load(Ordering::Relaxed) {
        return Status::INVALID_ARGS;
    }
    // TODO-rvbringup: investigate what this would do
    panic!("PLIC deactivate unimplemented");
}

extern "C" fn plic_configure_interrupt(
    vector: InterruptVector,
    tm: InterruptTriggerMode,
    pol: InterruptPolarity,
) -> Status {
    ltracef!("vector {}, trigger mode {:?}, polarity {:?}\n", vector.0, tm, pol);
    if vector.0 >= PLIC_MAX_INT.load(Ordering::Relaxed) {
        return Status::INVALID_ARGS;
    }
    if pol != InterruptPolarity::High {
        return Status::NOT_SUPPORTED;
    }
    Status::OK
}

extern "C" fn plic_get_interrupt_config(
    vector: InterruptVector,
    tm: *mut InterruptTriggerMode,
    pol: *mut InterruptPolarity,
) -> Status {
    ltracef!("vector {}\n", vector.0);
    if vector.0 >= PLIC_MAX_INT.load(Ordering::Relaxed) {
        return Status::INVALID_ARGS;
    }
    // SAFETY: Writing configuration constants back to pointers provided by C++ caller.
    // interrupt_trigger_mode::EDGE is 0, interrupt_polarity::HIGH is 0.
    unsafe {
        if !tm.is_null() {
            *tm = InterruptTriggerMode::Edge;
        }
        if !pol.is_null() {
            *pol = InterruptPolarity::High;
        }
    }
    Status::OK
}

extern "C" fn plic_set_affinity(_vector: InterruptVector, _mask: u32) -> Status {
    Status::NOT_SUPPORTED
}

extern "C" fn plic_remap_interrupt(vector: InterruptVector) -> InterruptVector {
    ltracef!("vector {}\n", vector.0);
    vector
}

extern "C" fn plic_handle_irq(_frame: *mut c_void) {
    // get the current vector
    let curr_hart_id = curr_hart_id();
    let boot_hart_id = boot_hart_id();
    assert_eq!(curr_hart_id, boot_hart_id, "PLIC interrupt handled on non-boot hart");

    let bank = plic_bank();
    let claim_offset = plic_claim_complete_offset(curr_hart_id);
    // SAFETY: claim_offset is within bounds of the mapped PLIC region.
    let claim_reg = unsafe { bank.at(claim_offset) };
    let vector = claim_reg.read();
    ltracef_level!(2, "vector {}\n", vector);

    if vector == 0 {
        // spurious
        return;
    }

    crate::kernel::stats::inc_interrupts();

    // SAFETY: Delivering interrupt and signaling EOI.
    unsafe {
        // deliver the interrupt
        pdev_invoke_int_if_present(InterruptVector(vector));
    }
    // EOI
    claim_reg.write(vector);
    ltracef_level!(2, "cpu {} exit\n", curr_hart_id);
}

extern "C" fn plic_send_ipi(_target: u32, _ipi: u32) -> Status {
    Status::NOT_SUPPORTED
}

extern "C" fn plic_init_percpu() {
    let curr_hart = curr_hart_id();
    ltracef!("hart {}\n", curr_hart);
    let max_int = PLIC_MAX_INT.load(Ordering::Relaxed);
    // mask all irqs on this cpu
    for i in 1..max_int {
        plic_disable_vector(i, curr_hart);
    }
}

extern "C" fn plic_shutdown() {
    panic!("PLIC shutdown unimplemented");
}

extern "C" fn plic_shutdown_cpu() {
    // Nothing to be done here on the secondary cpus.
    debug_assert!(boot_hart_id() != curr_hart_id(), "Shutdown called on boot CPU");
}

extern "C" fn plic_suspend_cpu() -> Status {
    Status::NOT_SUPPORTED
}

extern "C" fn plic_resume_cpu() -> Status {
    Status::NOT_SUPPORTED
}

extern "C" fn plic_msi_is_supported() -> bool {
    false
}

extern "C" fn plic_msi_supports_masking() -> bool {
    false
}

extern "C" fn plic_msi_mask_unmask(_block: *const MsiBlock, _msi_id: u32, _mask: bool) {
    panic!("PLIC MSI mask/unmask unimplemented");
}

extern "C" fn plic_msi_alloc_block(
    _requested_irqs: u32,
    _can_target_64bit: bool,
    _is_msix: bool,
    _out_block: *mut MsiBlock,
) -> Status {
    panic!("PLIC MSI alloc block unimplemented");
}

extern "C" fn plic_msi_free_block(_block: *mut MsiBlock) {
    panic!("PLIC MSI free block unimplemented");
}

extern "C" fn plic_msi_register_handler(
    _block: *mut MsiBlock,
    _msi_id: u32,
    _handler: InterruptHandler,
) {
    panic!("PLIC MSI register handler unimplemented")
}

static PLIC_OPS: PdevInterruptOps = PdevInterruptOps {
    mask: plic_mask_interrupt,
    unmask: plic_unmask_interrupt,
    deactivate: plic_deactivate_interrupt,
    configure: plic_configure_interrupt,
    get_config: plic_get_interrupt_config,
    set_affinity: plic_set_affinity,
    is_valid: plic_is_valid_interrupt,
    get_base_vector: plic_get_base_vector,
    get_max_vector: plic_get_max_vector,
    remap: plic_remap_interrupt,
    send_ipi: plic_send_ipi,
    init_percpu_early: plic_init_percpu_early,
    init_percpu: plic_init_percpu,
    handle_irq: plic_handle_irq,
    shutdown: plic_shutdown,
    shutdown_cpu: plic_shutdown_cpu,
    suspend_cpu: plic_suspend_cpu,
    resume_cpu: plic_resume_cpu,
    msi_is_supported: plic_msi_is_supported,
    msi_supports_masking: plic_msi_supports_masking,
    msi_mask_unmask: plic_msi_mask_unmask,
    msi_alloc_block: plic_msi_alloc_block,
    msi_free_block: plic_msi_free_block,
    msi_register_handler: plic_msi_register_handler,
    get_status: None,
};

unsafe extern "C" {
    fn root_resource_filter_add_deny_region(base: usize, size: usize, kind: u32);
}

/// Initializes the PLIC driver early in the boot sequence.
///
/// # Safety
///
/// The caller must ensure that `_config` is a reference to a valid `DcfgRiscvPlicDriver`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plic_init_early(_config: &DcfgRiscvPlicDriver) {}

/// Performs post-VM initialization for the PLIC driver, mapping the MMIO registers.
///
/// # Safety
///
/// The caller must ensure that `config` is a reference to a valid `DcfgRiscvPlicDriver`,
/// and that this function is only called once during boot when the VM system is ready.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plic_init_post_vm(config: &DcfgRiscvPlicDriver) {
    ltrace_entry!();
    assert!(config.num_irqs > 0);

    let mut plic_base_void: *mut c_void = core::ptr::null_mut();
    let aspace = VmAspace::kernel_aspace();
    unsafe {
        aspace
            .alloc_physical(
                c"plic",
                config.size_bytes as usize,
                &mut plic_base_void,
                page::SHIFT as u8,
                PAddr(config.mmio_phys as usize),
                0,
                ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_UNCACHED_DEVICE,
            )
            .expect("Could not allocate PLIC mmio region");
    }

    let num_irqs = config.num_irqs;
    PLIC_MAX_INT.store(num_irqs, Ordering::Relaxed);
    PLIC_SIZE.store(config.size_bytes, Ordering::Relaxed);
    PLIC_BASE.store(plic_base_void as *mut u32, Ordering::Relaxed);

    let boot_hart = boot_hart_id();
    let bank = plic_bank();

    // mask all irqs and set their priority to 1
    for i in 1..num_irqs {
        plic_disable_vector(i, boot_hart);
        // SAFETY: plic_priority_offset is within bounds of the mapped PLIC MMIO bank.
        let priority_reg = unsafe { bank.at(plic_priority_offset(i)) };
        priority_reg.write(1);
    }

    // set global priority threshold to 0
    let threshold_reg = unsafe { bank.at(plic_threshold_offset(boot_hart)) };
    threshold_reg.write(0);

    // SAFETY: Registering the ops.
    unsafe {
        pdev_register_interrupts(&PLIC_OPS as *const PdevInterruptOps);
    }
    ltrace_exit!();
}

/// Performs late initialization for the PLIC driver, registering deny regions.
///
/// # Safety
///
/// The caller must ensure that `config` is a reference to a valid `DcfgRiscvPlicDriver`,
/// and that the driver has been initialized successfully in the post-VM phase.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plic_init_late(config: &DcfgRiscvPlicDriver) {
    // Register the MMIO region we have already mapped after the fact to allow the resource
    // manager to initialize after the PostVM hook.
    unsafe {
        root_resource_filter_add_deny_region(
            config.mmio_phys as usize,
            config.size_bytes as usize,
            0, // ZX_RSRC_KIND_MMIO
        );
    }
}

/// RISC-V PLIC driver kernel tests.
#[cfg(ktest)]
#[unittest::suite(name = "plic")]
mod tests {
    use unittest::{assert_eq, assert_false, assert_true};

    /// Test HART ID to PLIC context indexing mapping.
    #[test]
    fn test_plic_hart_to_context_index_mapping() {
        assert_eq!(plic_hart_idx(0), 1);
        assert_eq!(plic_hart_idx(1), 3);
        assert_eq!(plic_hart_idx(4), 9);
    }

    /// Test vector bounds enforcement against PLIC_MAX_INT.
    #[test]
    fn test_plic_vector_bounds_enforcement() {
        let orig_max = PLIC_MAX_INT.load(Ordering::Relaxed);
        PLIC_MAX_INT.store(64, Ordering::Relaxed);
        assert_true!(plic_is_valid_interrupt(InterruptVector(63), 0));
        assert_false!(plic_is_valid_interrupt(InterruptVector(64), 0));
        assert_eq!(
            plic_mask_interrupt(InterruptVector(64)).into_raw(),
            Status::INVALID_ARGS.into_raw()
        );
        PLIC_MAX_INT.store(orig_max, Ordering::Relaxed);
    }

    /// Test configuration polarity rejection contract.
    #[test]
    fn test_plic_configure_rejects_invalid_polarity() {
        let orig_max = PLIC_MAX_INT.load(Ordering::Relaxed);
        PLIC_MAX_INT.store(64, Ordering::Relaxed);
        assert_eq!(
            plic_configure_interrupt(
                InterruptVector(1),
                InterruptTriggerMode::Edge,
                InterruptPolarity::High
            )
            .into_raw(),
            Status::OK.into_raw()
        );
        assert_eq!(
            plic_configure_interrupt(
                InterruptVector(1),
                InterruptTriggerMode::Edge,
                InterruptPolarity::Low
            )
            .into_raw(),
            Status::NOT_SUPPORTED.into_raw()
        );
        PLIC_MAX_INT.store(orig_max, Ordering::Relaxed);
    }

    /// Test PLIC offset calculations and regio MmioBank operations.
    #[test]
    fn test_plic_regio_offsets_and_bank() {
        use super::*;

        // Verify offset calculations.
        assert_eq!(plic_priority_offset(1).value, 8);
        assert_eq!(plic_enable_offset(0, 0).value, 0x2000 + 0x80);
        assert_eq!(plic_threshold_offset(0).value, 0x200000 + 0x1000);
        assert_eq!(plic_claim_complete_offset(0).value, 0x200004 + 0x1000);

        // Test MmioBank operations using a placeholder buffer.
        let mut buffer = [0u32; 1024];
        let ptr = unsafe { MmioPtr::<u32, RwSafe>::new(buffer.as_mut_ptr()) };
        let bank = MmioBank::new(ptr, core::mem::size_of_val(&buffer));

        let offset = Offset::<u32, RwSafe>::new(16);
        let reg = unsafe { bank.at(offset) };

        reg.write(0x1234_5678);
        assert_eq!(reg.read(), 0x1234_5678);

        reg.modify(|val| *val |= 1);
        assert_eq!(reg.read(), 0x1234_5679);

        // Test accessing bank using PLIC priority offset.
        let priority_reg = unsafe { bank.at(plic_priority_offset(1)) };
        priority_reg.write(5);
        assert_eq!(priority_reg.read(), 5);
    }
}
