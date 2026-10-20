// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Ported from zircon/kernel/dev/interrupt/plic/plic.cc

use crate::arch_rs::Iframe;
use crate::arch_rs::riscv64::{arch_curr_cpu_num, boot_hart_id, curr_hart_id};
use crate::kernel::mp::MpIpi;
use crate::kernel::types::{PAddr, cpu_mask_t};
use crate::pdev_interrupt::{
    InterruptControllerOps, InterruptHandler, InterruptPolarity, InterruptTriggerMode,
    InterruptVector, MAX_INTERRUPTS, MsiBlock, pdev_invoke_int_if_present,
    register_interrupt_controller,
};
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED_DEVICE,
};
use crate::vm::vm_aspace::VmAspace;
use core::ffi::c_void;
use debug::{ltrace_entry, ltrace_exit, ltracef, ltracef_level};
use lazy_init::LazyInit;
use page;
use regio::{MmioBank, MmioPtr, Offset, RwSafe};
#[cfg(ktest)]
use unittest as _;
pub use zbi::DcfgRiscvPlicDriver;
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

static PLIC_BANK: LazyInit<MmioBank<u32, RwSafe>> = LazyInit::uninit();
static PLIC_MAX_INT: LazyInit<u32> = LazyInit::uninit();

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

fn plic_enable_vector(vector: u32, hart_id: u32) {
    let offset = plic_enable_offset(vector, hart_id);
    // SAFETY: offset is within bounds of the mapped PLIC MMIO bank.
    let reg = unsafe { PLIC_BANK.at(offset) };
    reg.modify(|val| *val |= 1 << (vector % 32));
}

fn plic_disable_vector(vector: u32, hart_id: u32) {
    let offset = plic_enable_offset(vector, hart_id);
    // SAFETY: offset is within bounds of the mapped PLIC MMIO bank.
    let reg = unsafe { PLIC_BANK.at(offset) };
    reg.modify(|val| *val &= !(1 << (vector % 32)));
}

/// The RISC-V platform-level interrupt controller driver.
struct Plic;

static PLIC: Plic = Plic;

impl InterruptControllerOps for Plic {
    // Enable and disable act on the boot hart's PLIC context only.  That is the whole
    // of the current policy: `set_affinity()` below is unimplemented, so every
    // interrupt is delivered to the boot hart, and there is no other context to keep in
    // sync.  Per-hart routing would change both of these and that function together.
    fn mask(&self, vector: InterruptVector) -> Result<(), Status> {
        ltracef!("vector {}\n", vector.0);
        if vector.0 >= *PLIC_MAX_INT {
            return Err(Status::INVALID_ARGS);
        }
        plic_disable_vector(vector.0, boot_hart_id());
        Ok(())
    }

    fn unmask(&self, vector: InterruptVector) -> Result<(), Status> {
        ltracef!("vector {}\n", vector.0);
        if vector.0 >= *PLIC_MAX_INT {
            return Err(Status::INVALID_ARGS);
        }
        plic_enable_vector(vector.0, boot_hart_id());
        Ok(())
    }

    fn deactivate(&self, vector: InterruptVector) -> Result<(), Status> {
        if vector.0 >= *PLIC_MAX_INT {
            return Err(Status::INVALID_ARGS);
        }
        // TODO-rvbringup: investigate what this would do
        panic!("PLIC deactivate unimplemented");
    }

    fn configure(
        &self,
        vector: InterruptVector,
        tm: InterruptTriggerMode,
        pol: InterruptPolarity,
    ) -> Result<(), Status> {
        ltracef!("vector {}, trigger mode {:?}, polarity {:?}\n", vector.0, tm, pol);
        if vector.0 >= *PLIC_MAX_INT {
            return Err(Status::INVALID_ARGS);
        }
        if pol != InterruptPolarity::High {
            return Err(Status::NOT_SUPPORTED);
        }
        Ok(())
    }

    fn config(
        &self,
        vector: InterruptVector,
    ) -> Result<(InterruptTriggerMode, InterruptPolarity), Status> {
        ltracef!("vector {}\n", vector.0);
        if vector.0 >= *PLIC_MAX_INT {
            return Err(Status::INVALID_ARGS);
        }
        // interrupt_trigger_mode::EDGE is 0, interrupt_polarity::HIGH is 0.
        Ok((InterruptTriggerMode::Edge, InterruptPolarity::High))
    }

    fn set_affinity(&self, _vector: InterruptVector, _mask: cpu_mask_t) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }

    fn is_valid(&self, vector: InterruptVector, _flags: u32) -> bool {
        vector.0 < *PLIC_MAX_INT
    }

    fn base_vector(&self) -> InterruptVector {
        InterruptVector(0)
    }

    fn max_vector(&self) -> InterruptVector {
        InterruptVector(*PLIC_MAX_INT)
    }

    fn remap(&self, vector: InterruptVector) -> InterruptVector {
        ltracef!("vector {}\n", vector.0);
        vector
    }

    fn send_ipi(&self, _target: cpu_mask_t, _ipi: MpIpi) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }

    fn init_percpu_early(&self) {}

    fn init_percpu(&self) {
        let curr_hart = curr_hart_id();
        ltracef!("hart {}\n", curr_hart);
        let max_int = *PLIC_MAX_INT;
        // mask all irqs on this cpu
        for i in 1..max_int {
            plic_disable_vector(i, curr_hart);
        }
    }

    fn handle_irq(&self, _frame: &mut Iframe) {
        // get the current vector
        let curr_hart_id = curr_hart_id();
        let boot_hart_id = boot_hart_id();
        assert_eq!(curr_hart_id, boot_hart_id, "PLIC interrupt handled on non-boot hart");

        let claim_offset = plic_claim_complete_offset(curr_hart_id);
        // SAFETY: claim_offset is within bounds of the mapped PLIC region.
        let claim_reg = unsafe { PLIC_BANK.at(claim_offset) };
        let vector = claim_reg.read();
        ltracef_level!(2, "vector {}\n", vector);

        if vector == 0 {
            // spurious
            return;
        }

        crate::kernel::stats::inc_interrupts();

        // deliver the interrupt; the pdev table has MAX_INTERRUPTS slots
        if (vector as usize) < MAX_INTERRUPTS {
            // SAFETY: `vector` was checked to be less than `MAX_INTERRUPTS` just above.
            unsafe {
                pdev_invoke_int_if_present(InterruptVector(vector));
            }
        }
        // EOI
        claim_reg.write(vector);
        ltracef_level!(2, "cpu {} exit\n", curr_hart_id);
    }

    fn shutdown(&self) {
        panic!("PLIC shutdown unimplemented");
    }

    fn shutdown_cpu(&self) {
        // Nothing to be done here on the secondary cpus.
        assert!(arch_curr_cpu_num() != 0, "Shutdown called on boot CPU");
    }

    fn suspend_cpu(&self) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }

    fn resume_cpu(&self) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }

    fn msi_is_supported(&self) -> bool {
        false
    }

    fn msi_supports_masking(&self) -> bool {
        false
    }

    fn msi_mask_unmask(&self, _block: &MsiBlock, _msi_id: u32, _mask: bool) {
        panic!("PLIC MSI mask/unmask unimplemented");
    }

    fn msi_alloc_block(
        &self,
        _requested_irqs: u32,
        _can_target_64bit: bool,
        _is_msix: bool,
        _out_block: &mut MsiBlock,
    ) -> Result<(), Status> {
        panic!("PLIC MSI alloc block unimplemented");
    }

    fn msi_free_block(&self, _block: &mut MsiBlock) {
        panic!("PLIC MSI free block unimplemented");
    }

    fn msi_register_handler(&self, _block: &MsiBlock, _msi_id: u32, _handler: InterruptHandler) {
        panic!("PLIC MSI register handler unimplemented")
    }
}

unsafe extern "C" {
    fn root_resource_filter_add_deny_region(base: usize, size: usize, kind: u32);
}

/// Initializes the PLIC driver early in the boot sequence.
pub fn plic_init_early(_config: &DcfgRiscvPlicDriver) {}

/// Performs post-VM initialization for the PLIC driver, mapping the MMIO registers.
/// Called once during boot, when the VM system is ready.
pub fn plic_init_post_vm(config: &DcfgRiscvPlicDriver) {
    ltrace_entry!();
    assert!(config.num_irqs > 0);
    assert!(config.num_irqs as usize <= MAX_INTERRUPTS);

    let mut plic_base_void: *mut c_void = core::ptr::null_mut();
    let aspace = VmAspace::kernel_aspace();
    unsafe {
        aspace
            .alloc_physical(
                b"plic",
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
    let size = config.size_bytes as usize;
    // SAFETY: Called once during boot, serialized with respect to any other access.
    unsafe {
        PLIC_MAX_INT.init(num_irqs);
        let ptr = MmioPtr::<u32, RwSafe>::new(plic_base_void.cast::<u32>());
        PLIC_BANK.init(MmioBank::new(ptr, size));
    }

    let boot_hart = boot_hart_id();

    // mask all irqs and set their priority to 1
    for i in 1..num_irqs {
        plic_disable_vector(i, boot_hart);
        // SAFETY: plic_priority_offset is within bounds of the mapped PLIC MMIO bank.
        let priority_reg = unsafe { PLIC_BANK.at(plic_priority_offset(i)) };
        priority_reg.write(1);
    }

    // set global priority threshold to 0
    let threshold_reg = unsafe { PLIC_BANK.at(plic_threshold_offset(boot_hart)) };
    threshold_reg.write(0);

    // SAFETY: `plic_init_post_vm` runs once, from the post-VM driver handoff during
    // single-threaded boot, before secondary harts are started.
    unsafe {
        register_interrupt_controller(&PLIC);
    }
    ltrace_exit!();
}

/// Performs late initialization for the PLIC driver, registering deny regions.
/// Called once, after the post-VM initialization succeeded.
pub fn plic_init_late(config: &DcfgRiscvPlicDriver) {
    // Register the MMIO region we have already mapped after the fact to allow the resource
    // manager to initialize after the PostVM hook.
    unsafe {
        root_resource_filter_add_deny_region(
            config.mmio_phys as usize,
            config.size_bytes as usize,
            zx_types::ZX_RSRC_KIND_MMIO,
        );
    }
}

/// RISC-V PLIC driver kernel tests.
#[cfg(ktest)]
#[unittest::suite(name = "plic")]
mod tests {
    use unittest::{assert_eq, assert_err, assert_false, assert_ok, assert_true};

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
        let max = *PLIC_MAX_INT;
        if max > 0 {
            assert_true!(PLIC.is_valid(InterruptVector(max - 1), 0));
            assert_false!(PLIC.is_valid(InterruptVector(max), 0));
            assert_err!(PLIC.mask(InterruptVector(max)), Status::INVALID_ARGS);
        }
    }

    /// Test configuration polarity rejection contract.
    #[test]
    fn test_plic_configure_rejects_invalid_polarity() {
        let max = *PLIC_MAX_INT;
        if max > 1 {
            assert_ok!(PLIC.configure(
                InterruptVector(1),
                InterruptTriggerMode::Edge,
                InterruptPolarity::High
            ));
            assert_err!(
                PLIC.configure(
                    InterruptVector(1),
                    InterruptTriggerMode::Edge,
                    InterruptPolarity::Low
                ),
                Status::NOT_SUPPORTED
            );
        }
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
