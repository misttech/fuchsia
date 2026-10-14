// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::halt_token::HaltToken;
use super::power::{PlatformHaltAction, ZirconCrashReason, platform_halt};
use super::timer::{DurationMono, InstantMono, current_mono_time};
use crate::clt_tag;
use crate::debuglog_rs::dlog_shutdown;
use crate::kernel::deadline::{Deadline, TimerSlack};
use crate::kernel::mp::{
    cpu_num_to_mask, get_online_mask as mp_get_online_mask, unplug_cpu_mask as mp_unplug_cpu_mask,
    wait_for_all_cpus_ready as mp_wait_for_all_cpus_ready,
};
use crate::kernel::thread::{self, ThreadLockGuard};
use crate::kernel::types::BOOT_CPU_ID;
use crate::vm::pmm::pmm_checker_check_all_free_pages;
use kprint::kprintln;
use zx_status::Status;
use zx_types::zx_status_t;

/// Gracefully halt and perform `action`.
///
/// This function attempts to acquire the halt token. If successful, it will perform `action` or
/// panic if the system cannot be successfully halted before `panic_deadline` is reached.
///
/// If the halt token cannot be acquired, this function will block forever.
pub fn platform_graceful_halt_helper(
    action: PlatformHaltAction,
    reason: ZirconCrashReason,
    panic_deadline: InstantMono,
) -> ! {
    if !HaltToken::get().take() {
        kprintln!(
            "platform_graceful_halt_helper: halt/reboot already in progress; sleeping forever"
        );
        let _ = thread::sleep(InstantMono::INFINITE);
    }

    kprintln!(
        "platform_graceful_halt_helper: action={} reason={} panic_deadline={} current_time={}",
        action as u32,
        reason as u32,
        panic_deadline.0,
        current_mono_time().0
    );

    // Migrate to the boot CPU before shutting down the secondary CPUs.  Note that
    // this action also hard-pins our thread to the boot CPU, so we don't need to
    // worry about migration after this.
    // SAFETY: `BOOT_CPU_ID` is active, and `platform_graceful_halt_helper` is called in normal
    // thread context with no spinlocks or `ChainLockTransaction` held.
    unsafe { thread::migrate_to_cpu(BOOT_CPU_ID) };
    kprintln!("platform_graceful_halt_helper: Migrated thread to boot CPU.");

    let status = platform_halt_secondary_cpus(panic_deadline);
    assert!(
        status.is_ok(),
        "platform_halt_secondary_cpus failed: {}\n",
        Status::result_into_raw(status)
    );
    kprintln!("platform_graceful_halt_helper: Halted secondary CPUs.");

    // If we're in a debug build, check the PMM's free list for any latent corruption.
    if !cfg!(lk_debuglevel = "0") {
        pmm_checker_check_all_free_pages();
    }

    // Delay shutdown of debuglog to ensure log messages emitted by above calls will be written.
    kprintln!("platform_graceful_halt_helper: Shutting down dlog.");
    let status = dlog_shutdown(panic_deadline);
    assert!(status.is_ok(), "dlog_shutdown failed: {}\n", Status::result_into_raw(status));

    kprintln!("platform_graceful_halt_helper: Calling platform_halt.");
    platform_halt(action, reason);
    #[allow(unreachable_code)]
    {
        panic!("ERROR: failed to halt the platform\n");
    }
}

/// Gracefully halt secondary (non-boot) CPUs.
///
/// While the mechanism used is platform dependent, this function attempts to shut them down
/// gracefully so that secondary CPUs aren't holding any kernel locks.
///
/// Returns an error if all secondary CPU could not be not successfully shutdown before `deadline`
/// is reached.
///
/// This function must be called from the primary (boot) CPU.
pub fn platform_halt_secondary_cpus(deadline: InstantMono) -> Result<(), Status> {
    // Ensure the current thread is pinned to the boot CPU.
    {
        let current_thread = thread::current_get();
        // SAFETY: `current_thread` is a valid, live `Thread`, this CPU is not in a chain lock
        // transaction, and `guard` is dropped at the end of this scope without blocking.
        ksync::lock!(
            let guard = unsafe {
                ThreadLockGuard::lock(current_thread, clt_tag!("platform_halt_secondary_cpus"))
            }
        );
        debug_assert!(guard.scheduler_state().hard_affinity() == cpu_num_to_mask(BOOT_CPU_ID));
    }

    // It's possible that the one or more secondary CPUs is still in the process
    // of booting up.  Wait for all the started CPUs to check-in before shutting
    // them down.
    //
    // The deadline here should be long enough that the secondary CPUs have had
    // ample time to startup but small enough that we don't hold up the shutdown
    // process "too long".
    let status = mp_wait_for_all_cpus_ready(Deadline::after_mono(
        DurationMono::from_seconds(5),
        TimerSlack::none(),
    ));
    if let Err(err) = status {
        kprintln!("failed to wait for cpus to start: {}", err.into_raw());
    }

    // "Unplug" online secondary CPUs before halting them.
    let primary = cpu_num_to_mask(BOOT_CPU_ID);
    let mask = mp_get_online_mask() & !primary;
    let status = mp_unplug_cpu_mask(mask, deadline.0);
    if status.is_ok() {
        return Ok(());
    }

    // mp_unplug_cpu_mask failed.  Perhaps another thread was trying to shutdown the secondary CPUs.
    // If the primary CPU is the only one left, then we've done our job.
    if status == Err(Status::BAD_STATE) && mp_get_online_mask() == primary {
        return Ok(());
    }

    status
}

/// FFI trampoline for [`platform_graceful_halt_helper`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_graceful_halt_helper(
    action: PlatformHaltAction,
    reason: ZirconCrashReason,
    panic_deadline: InstantMono,
) -> ! {
    platform_graceful_halt_helper(action, reason, panic_deadline)
}

/// FFI trampoline for [`platform_halt_secondary_cpus`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_halt_secondary_cpus(deadline: InstantMono) -> zx_status_t {
    Status::result_into_raw(platform_halt_secondary_cpus(deadline))
}
