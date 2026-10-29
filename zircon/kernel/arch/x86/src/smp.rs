// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! x86 SMP initialization and AP bringup.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, Ordering};

use debug::{dprintf, tracef};
use kprint::kprintln;
use libarch::x86::thread_memory_barrier;
use zx_status::Status;

use super::apic::ApicInterruptDeliveryMode;
use super::bootstrap16::{X86ApBootstrapData, x86_bootstrap16_acquire, x86_bootstrap16_release};
use super::lapic::apic_send_ipi;
use super::smp_ffi::{
    X86_SECONDARY_CPU_LONG_MODE_ENTRY, lk_init_secondary_cpus, x86_allocate_ap_structures,
    x86_apic_id_to_cpu_num,
};
use crate::kernel::mp::{self, cpu_num_t};
use crate::kernel::scheduler;
use crate::kernel::thread::{self, Thread};
use crate::platform_rs::timer::DurationMono;

const MB: usize = 1024 * 1024;

/// Registers all of the CPUs in the system (`x86_init_smp`).
///
/// Must be called only once.
///
/// # Safety
///
/// The caller must ensure that `apic_ids` points to a live (not freed) allocation of `num_cpus`
/// valid APIC IDs (including the bootstrap processor) and that this function is called once during
/// single-threaded boot.
pub(super) unsafe fn x86_init_smp(apic_ids: *mut u32, num_cpus: u32) {
    assert!(num_cpus >= 1 && num_cpus <= u32::from(u8::MAX));
    // SAFETY: Caller guarantees `apic_ids` points to `num_cpus` valid APIC IDs.
    let status = unsafe { x86_allocate_ap_structures(apic_ids, num_cpus as u8) };
    if Status::ok(status).is_err() {
        tracef!("Failed to allocate structures for APs");
        return;
    }

    // SAFETY: Single-threaded SMP initialization after AP structures are allocated.
    unsafe { lk_init_secondary_cpus(num_cpus - 1) };
}

/// Bring all of the specified APs up and hand them over to the kernel.
///
/// This function must not be called before `x86_init_smp`.
///
/// May be called by any running CPU. Due to requiring use of the very limited
/// low 1MB of memory, this function is not re-entrant. It should not be executed
/// more than once concurrently.
///
/// # Safety
///
/// The caller must ensure that `apic_ids` points to a live (not freed) allocation of at least
/// `count` valid `u32` elements (when `count > 0`), that `x86_init_smp` has already been called,
/// and that this function is called in a thread context where blocking (`thread::sleep_relative`)
/// is permitted.
pub(super) unsafe fn x86_bringup_aps(apic_ids: *const u32, count: u32) -> Result<(), Status> {
    static APS_STILL_BOOTING: AtomicU32 = AtomicU32::new(0);

    // if being asked to bring up 0 cpus, move on
    if count == 0 {
        return Ok(());
    }
    if (count as usize) > mp::SMP_MAX_CPUS - 1 {
        core::hint::cold_path();
        return Err(Status::INVALID_ARGS);
    }

    // SAFETY: Caller guarantees `apic_ids` points to `count` valid `u32` elements.
    let apic_ids_slice = unsafe { core::slice::from_raw_parts(apic_ids, count as usize) };

    // Sanity check the given ids
    let mut waiting_mask = 0u32;
    for &apic_id in apic_ids_slice {
        // SAFETY: `x86_apic_id_to_cpu_num` is a read-only lookup into the APIC ID table.
        let cpu = unsafe { x86_apic_id_to_cpu_num(apic_id) };
        debug_assert!(cpu > 0);
        if cpu <= 0 {
            core::hint::cold_path();
            return Err(Status::INVALID_ARGS);
        }
        if mp::is_cpu_online(cpu as cpu_num_t) {
            core::hint::cold_path();
            return Err(Status::BAD_STATE);
        }
        waiting_mask |= mp::cpu_num_to_mask(cpu as cpu_num_t);
    }

    let entry64 = core::ptr::addr_of!(X86_SECONDARY_CPU_LONG_MODE_ENTRY).addr();
    let (bootstrap_aperture, bootstrap_instr_ptr) = x86_bootstrap16_acquire(entry64)?;
    APS_STILL_BOOTING.store(waiting_mask, Ordering::SeqCst);
    let bootstrap_data = bootstrap_aperture.cast::<X86ApBootstrapData>();

    // SAFETY: `bootstrap_data` points to the page-aligned 2nd page of the mapped bootstrap
    // aperture.
    unsafe {
        addr_of_mut!((*bootstrap_data).cpu_id_counter).write_unaligned(0);
        addr_of_mut!((*bootstrap_data).cpu_waiting_mask)
            .write_unaligned(core::ptr::addr_of!(APS_STILL_BOOTING));
        // Zero the kstack list so if we have to bail, we can safely free the
        // resources.
        core::ptr::write_bytes(addr_of_mut!((*bootstrap_data).per_cpu), 0, 1);
    }

    let mut cleanup_all = zr::defer(move || {
        for i in 0..(count as usize) {
            // SAFETY: `bootstrap_data` is mapped and `per_cpu` was zero-initialized above.
            unsafe {
                let t = addr_of_mut!((*bootstrap_data).per_cpu[i].thread).read_unaligned();
                Thread::delete(t);
                addr_of_mut!((*bootstrap_data).per_cpu[i].thread)
                    .write_unaligned(core::ptr::null_mut());
            }
        }
        // SAFETY: `bootstrap_aperture` was acquired above and no AP was started yet.
        unsafe { x86_bootstrap16_release(bootstrap_aperture) };
    });

    // Allocate kstacks and threads for all processors
    for i in 0..(count as usize) {
        let thread = Thread::create_empty().ok_or(Status::NO_MEMORY)?.as_ptr();
        // SAFETY: `i < count <= SMP_MAX_CPUS - 1`, and `bootstrap_data` is writable. Storing
        // `thread` immediately ensures `cleanup_all` will delete it if `stack_init` fails.
        unsafe {
            addr_of_mut!((*bootstrap_data).per_cpu[i].thread).write_unaligned(thread);
        }
        // SAFETY: `thread` was just allocated by `Thread::create_empty`.
        unsafe { Thread::stack_init(thread)? };
        // SAFETY: `thread`'s stack was just initialized above.
        let kstack_top = unsafe { thread::get_stack_top(thread) };
        // SAFETY: `i < count <= SMP_MAX_CPUS - 1`, and `bootstrap_data` is writable.
        unsafe {
            addr_of_mut!((*bootstrap_data).per_cpu[i].kstack_top).write_unaligned(kstack_top);
        }
    }
    cleanup_all.cancel();

    // Memory fence to ensure all writes to the bootstrap region are
    // visible on the APs when they come up
    thread_memory_barrier();

    dprintf!(INFO, "booting apic ids: ");
    for &apic_id in apic_ids_slice {
        dprintf!(INFO, "{:#x} ", apic_id);
        apic_send_ipi(0, apic_id, ApicInterruptDeliveryMode::Init);
    }
    dprintf!(INFO, "\n");

    // Wait 10 ms and then send the startup signals
    let _ = thread::sleep_relative(DurationMono::from_millis(10));

    // Actually send the startups
    debug_assert!(bootstrap_instr_ptr.0 < MB && page::is_page_rounded(bootstrap_instr_ptr.0));
    let vec = (bootstrap_instr_ptr.0 >> page::SHIFT) as u8;
    // Try up to two times per CPU, as Intel 3A recommends.
    for _tries in 0..2 {
        for &apic_id in apic_ids_slice {
            // This will cause the APs to begin executing at
            // |bootstrap_instr_ptr| in physical memory.
            apic_send_ipi(vec, apic_id, ApicInterruptDeliveryMode::Startup);
        }

        if APS_STILL_BOOTING.load(Ordering::SeqCst) == 0 {
            break;
        }
        // Wait 1ms for cores to boot.  The docs recommend 200us between STARTUP
        // IPIs.
        let _ = thread::sleep_relative(DurationMono::from_millis(1));
    }

    // The docs recommend waiting 200us for cores to boot.  We do a bit more
    // work before the cores report in, so wait longer (up to 1 second).
    let mut tries_left = 200;
    while APS_STILL_BOOTING.load(Ordering::SeqCst) != 0 && tries_left > 0 {
        let _ = thread::sleep_relative(DurationMono::from_millis(5));
        tries_left -= 1;
    }

    let mut failed_aps = APS_STILL_BOOTING.swap(0, Ordering::SeqCst);
    if failed_aps != 0 {
        core::hint::cold_path();
        kprintln!("Failed to boot CPUs: mask {:x}", failed_aps);
        for (i, &apic_id) in apic_ids_slice.iter().enumerate() {
            // SAFETY: `x86_apic_id_to_cpu_num` is a read-only lookup into the APIC ID table.
            let cpu = unsafe { x86_apic_id_to_cpu_num(apic_id) };
            let mask = mp::cpu_num_to_mask(cpu as cpu_num_t);
            if (failed_aps & mask) == 0 {
                continue;
            }

            // Shut the failed AP down
            apic_send_ipi(0, apic_id, ApicInterruptDeliveryMode::Init);

            // It shouldn't have been possible for it to have been in the scheduler...
            assert!(!scheduler::peek_is_active(cpu as cpu_num_t));

            // Make sure the CPU is not marked online
            // SAFETY: `cpu` is a valid CPU number (`> 0` and mapped from `apic_id`) that failed to
            // boot, is verified inactive in the scheduler above, and never enabled interrupts.
            unsafe { mp::set_cpu_offline(cpu as cpu_num_t) };

            // Free the failed AP's thread, it was cancelled before it could use it.
            // SAFETY: The failed AP was shut down and `APS_STILL_BOOTING` was swapped to 0 before
            // the AP could claim its `Thread`.
            unsafe {
                let t = addr_of_mut!((*bootstrap_data).per_cpu[i].thread).read_unaligned();
                Thread::delete(t);
                addr_of_mut!((*bootstrap_data).per_cpu[i].thread)
                    .write_unaligned(core::ptr::null_mut());
            }

            failed_aps &= !mask;
        }
        debug_assert!(failed_aps == 0);

        return Err(Status::TIMED_OUT);
    }

    // Now that everything is booted, cleanup temporary structures, but keep the threads and stacks.
    // SAFETY: All APs have booted and transitioned off the bootstrap aperture; if any AP failed to
    // boot (`failed_aps != 0`), we returned early above without calling `x86_bootstrap16_release`
    // so a timed-out AP cannot have the bootstrap region reused underneath it.
    unsafe { x86_bootstrap16_release(bootstrap_aperture) };
    Ok(())
}
