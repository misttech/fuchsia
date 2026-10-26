// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::x86::feature::x86_get_microarch_config;
use crate::arch_rs::x86::mp::x86_force_halt_all_but_local_and_bsp;
use crate::arch_rs::x86::x86::{outp, x86_hlt};
use crate::arch_rs::{arch_mp_send_ipi, curr_cpu_num, disable_ints};
use crate::backtrace::Backtrace;
#[cfg(console_enabled)]
use crate::console::panic_shell_start;
use crate::debuglog_rs::{dlog_bluescreen_init, dlog_panic_start};
use crate::jtrace::{TraceBufferType, jtrace_dump};
use crate::kernel::mp::{MpIpi, MpIpiTarget, cpu_mask_t, cpu_num_to_mask, get_online_mask};
use crate::kernel::thread::current_get_backtrace;
use crate::kernel::types::AtomicCpuMask;
use crate::platform_rs::efi_bootbyte_set_reason;
use crate::platform_rs::power::{PlatformHaltAction, ZirconCrashReason};
use core::sync::atomic::{AtomicBool, Ordering};
use kprint::kprint;
use zx_status::Status;

use super::cmos_bootbyte::{RTC_BOOT_BOOTLOADER, RTC_BOOT_RECOVERY};
use super::keyboard::pc_keyboard_reboot;
use super::manufacturer;

/// The I/O port to write to for QEMU debug exit.
const QEMU_DEBUG_EXIT_PORT: u16 = 0xf4;

/// The return code that we should propagate to QEMU on isa-debug-exit.
/// This number must be non-zero and odd, since QEMU calculates the return
/// code as (val << 1) | 1 where "val" is the value written to 0xf4.
const QEMU_EXIT_CODE: u8 = 0x1f;
// QEMU exit code must be non-zero and odd.
zr::static_assert!(QEMU_EXIT_CODE != 0 && !QEMU_EXIT_CODE.is_multiple_of(2));

static HALTED_CPUS: AtomicCpuMask = AtomicCpuMask::new(0);

/// Records `reason` and then attempts to reset the system.
///
/// Not all microarch configs reset the system when recording the reboot reason.
/// Configs using `unknown_reboot_reason()` (e.g. `intel_default_config`, used for
/// unrecognized Intel models) only write the reason to CMOS and return, so fall
/// back to the generic reset mechanisms. The reason has already been recorded
/// at that point, so the next boot still honors it.
fn reboot_with_reason(reason: u64) {
    efi_bootbyte_set_reason(reason);
    if let Some(config) = x86_get_microarch_config() {
        if let Some(reboot_reason) = config.reboot_reason {
            // SAFETY: `reboot_reason` is a static microarch configuration callback.
            unsafe { reboot_reason(reason) };
        }
        // We fell through. Try normal reboot.
        if let Some(reboot_system) = config.reboot_system {
            // SAFETY: `reboot_system` is a static microarch configuration callback.
            unsafe { reboot_system() };
        }
    }
    // We fell through. Try rebooting via keyboard controller.
    pc_keyboard_reboot();
}

fn reboot() {
    // select the default reboot reason
    reboot_with_reason(0);
}

fn reboot_recovery() {
    reboot_with_reason(u64::from(RTC_BOOT_RECOVERY));
}

fn reboot_bootloader() {
    reboot_with_reason(u64::from(RTC_BOOT_BOOTLOADER));
}

fn halt_other_cpus() {
    static HALTED: AtomicBool = AtomicBool::new(false);

    if !HALTED.swap(true, Ordering::SeqCst) {
        // This function may have been called early in the boot process, before the
        // mp subsystem has been initialized or secondary CPUs have been brought
        // online.  To avoid calling into the mp subsystem before it has been
        // initialized, check the online mask.  If this CPU is the only one online,
        // then simply return.
        let targets: cpu_mask_t = get_online_mask() & !cpu_num_to_mask(curr_cpu_num());
        if targets == 0 {
            return;
        }

        // stop the other cpus
        kprint!("stopping other cpus\n");
        arch_mp_send_ipi(MpIpiTarget::AllButLocal, 0, MpIpi::Halt);

        // spin for a while
        // TODO: find a better way to spin at this low level
        for _ in 0..100000000 {
            if HALTED_CPUS.load(Ordering::SeqCst) == targets {
                break;
            }
            libarch::r#yield();
        }

        // Don't send an INIT IPI to the BSP, since that may cause the system to
        // reboot
        // SAFETY: Both callers (`platform_panic_start` and `platform_specific_halt`) are terminal
        // halt/panic paths that call `disable_ints()` before invoking `halt_other_cpus()`.
        unsafe { x86_force_halt_all_but_local_and_bsp() };
    }
}

/// Signals that this CPU is in its halt loop.
pub fn platform_halt_cpu() {
    // Signal that this CPU is in its halt loop
    HALTED_CPUS.fetch_or(cpu_num_to_mask(curr_cpu_num()), Ordering::SeqCst);
}

/// FFI trampoline for [`platform_halt_cpu`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_halt_cpu() {
    platform_halt_cpu();
}

/// Returns true iff this platform supports [`platform_suspend_cpu`].
pub fn platform_supports_suspend_cpu() -> bool {
    false
}

/// FFI trampoline for [`platform_supports_suspend_cpu`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_supports_suspend_cpu() -> bool {
    platform_supports_suspend_cpu()
}

/// Suspends the calling CPU.
pub fn platform_suspend_cpu(_allow_domain: bool) -> Result<(), Status> {
    Err(Status::NOT_SUPPORTED)
}

/// FFI trampoline for [`platform_suspend_cpu`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_suspend_cpu(allow_domain: bool) -> Result<(), Status> {
    platform_suspend_cpu(allow_domain)
}

/// Returns true iff this platform supports [`platform_suspend_system`].
pub fn platform_supports_suspend_system() -> bool {
    false
}

/// FFI trampoline for [`platform_supports_suspend_system`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_supports_suspend_system() -> bool {
    platform_supports_suspend_system()
}

/// Suspends the entire system from the boot CPU.
pub fn platform_suspend_system() -> Result<(), Status> {
    Err(Status::NOT_SUPPORTED)
}

/// FFI trampoline for [`platform_suspend_system`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_suspend_system() -> Result<(), Status> {
    platform_suspend_system()
}

// TODO(https://fxbug.dev/42180675): Refactor platform_panic_start.
/// Informs the system that a panic message is about to be printed and that
/// `platform_halt` will be called shortly. Stops other CPUs if `halt_others` is
/// true.
pub fn platform_panic_start(halt_others: bool) {
    disable_ints();
    dlog_panic_start();

    static PANIC_STARTED: AtomicBool = AtomicBool::new(false);
    if !PANIC_STARTED.swap(true, Ordering::SeqCst) {
        dlog_bluescreen_init();
        // Attempt to dump the current debug trace buffer, if we have one.
        jtrace_dump(TraceBufferType::Current);
    }

    if halt_others {
        halt_other_cpus();
    }
}

/// FFI trampoline for [`platform_panic_start`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_panic_start(halt_others: bool) {
    platform_panic_start(halt_others);
}

/// Platform-specific actions to be taken in a halt situation.
pub fn platform_specific_halt(
    suggested_action: PlatformHaltAction,
    reason: ZirconCrashReason,
    halt_on_panic: bool,
) -> ! {
    kprint!(
        "platform_halt suggested_action {} reason {}\n",
        suggested_action as i32,
        reason as i32
    );

    disable_ints();

    match suggested_action {
        PlatformHaltAction::Shutdown => {
            if c"QEMU" == manufacturer() {
                // SAFETY: Writing to the QEMU debug exit port on a QEMU virtual machine to power
                // off the guest.
                unsafe {
                    outp(QEMU_DEBUG_EXIT_PORT, QEMU_EXIT_CODE >> 1);
                }
            }
            kprint!("Power off failed, halting\n");
        }
        PlatformHaltAction::Reboot => {
            kprint!("Rebooting...\n");
            reboot();
            kprint!("Reboot failed, halting\n");
        }
        PlatformHaltAction::Halt => {
            kprint!("Halting...\n");
            halt_other_cpus();
        }
        PlatformHaltAction::RebootBootloader => {
            kprint!("Rebooting ... To Boot Loader\n");
            reboot_bootloader();
            // We fell through.
            kprint!("platform_halt: Unsupported halt reason {}\n", suggested_action as i32);
        }
        PlatformHaltAction::RebootRecovery => {
            kprint!("Rebooting ... To Recovery\n");
            reboot_recovery();
            // We fell through.
            kprint!("platform_halt: Unsupported halt reason {}\n", suggested_action as i32);
        }
    }

    if reason == ZirconCrashReason::Panic {
        let mut bt = Backtrace::new();
        current_get_backtrace(&mut bt);
        bt.print();
    }

    if !halt_on_panic {
        kprint!("Rebooting...\n");
        reboot();
    }

    kprint!("Halted\n");

    #[cfg(console_enabled)]
    panic_shell_start();

    loop {
        x86_hlt();
    }
}

/// FFI trampoline for [`platform_specific_halt`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_specific_halt(
    suggested_action: PlatformHaltAction,
    reason: ZirconCrashReason,
    halt_on_panic: bool,
) -> ! {
    platform_specific_halt(suggested_action, reason, halt_on_panic)
}
