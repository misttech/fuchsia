// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2012 Google, Inc.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_ALWAYS, CMD_FLAG_PANIC, CmdArgs, static_command};
use crate::jtrace::jtrace_invalidate;
use crate::pdev_watchdog::{hw_watchdog_pet, hw_watchdog_set_enabled};
use crate::persistent_debuglog_rs::persistent_dlog_invalidate;
use crate::platform_rs::crashlog::PlatformCrashlog;
use crate::platform_rs::timer::InstantBoot;
use boot_options::BootOptions;
#[cfg(console_enabled)]
use core::ffi::{CStr, c_int};
use core::slice;
#[cfg(console_enabled)]
use debug::dprintf;

/// Action to perform when halting the platform.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformHaltAction {
    /// Spin forever.
    Halt = 0,

    /// Reset the CPU.
    Reboot = 1,

    /// Reboot into the bootloader.
    RebootBootloader = 2,

    /// Reboot into the recovery partition.
    RebootRecovery = 3,

    /// Shutdown and power off.
    Shutdown = 4,
}

/// Software reason for a recent crash or reboot.
///
/// TODO(https://fxbug.dev/534467534): For now, these values are copied from
/// `zircon/system/public/zircon/boot/crash-reason.h`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZirconCrashReason {
    /// 0 is reserved for "Invalid".  It will never be used by a functioning
    /// crash-logger.
    Invalid = 0,

    /// "Unknown" indicates that the system does not know the reason for a recent
    /// crash.  The primary use of this reason is to be something which can be left
    /// in the crashlog in case the system spontaneously reboots without a chance to
    /// gracefully finalize the log, perhaps because of something like a hardware
    /// watchdog timer.
    Unknown = 1,

    /// "No Crash" indicates that the system deliberately rebooted in an
    /// orderly fashion.  No crash occurred.
    NoCrash = 2,

    /// "OOM" indicates a crash triggered by the system because of an unrecoverable
    /// out-of-memory situation.
    Oom = 3,

    /// "Panic" indicates a crash triggered by the system because of an unrecoverable
    /// kernel panic situation.
    Panic = 4,

    /// "Software watchdog" indicates a crash triggered by a kernel level software
    /// watchdog construct.  Note that this is distinct from a hardware based WDT.
    /// If the system reboots because of a hardware watchdog, it will have no chance
    /// to record the reboot reason, and the crashlog will indicate "unknown".  The
    /// HW reboot reason may be known, but only if the bootloader reports it to us.
    SoftwareWatchdog = 5,

    /// "Userspace root job termination" indicates a crash triggered by the system
    /// because it detected the termination of the userspace root job, most likely
    /// because one of its critical processes crashed.
    UserspaceRootJobTermination = 6,
}

impl TryFrom<u32> for ZirconCrashReason {
    type Error = u32;

    /// Converts a raw `zircon_crash_reason_t` value, returning it as the error if it does not name
    /// a known reason.
    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        match raw {
            x if x == Self::Invalid as u32 => Ok(Self::Invalid),
            x if x == Self::Unknown as u32 => Ok(Self::Unknown),
            x if x == Self::NoCrash as u32 => Ok(Self::NoCrash),
            x if x == Self::Oom as u32 => Ok(Self::Oom),
            x if x == Self::Panic as u32 => Ok(Self::Panic),
            x if x == Self::SoftwareWatchdog as u32 => Ok(Self::SoftwareWatchdog),
            x if x == Self::UserspaceRootJobTermination as u32 => {
                Ok(Self::UserspaceRootJobTermination)
            }
            _ => Err(raw),
        }
    }
}

unsafe extern "C" {
    fn cpp_platform_specific_halt(
        suggested_action: PlatformHaltAction,
        reason: ZirconCrashReason,
        halt_on_panic: bool,
    ) -> !;
    fn cpp_platform_panic_start();
    fn cpp_platform_serial_is_suspended() -> bool;
    fn cpp_platform_serial_suspended_at() -> InstantBoot;
}

/// Returns whether kernel access to the debug UART has been suspended (or a suspend operation is
/// in progress) via `platform_serial_suspend`.
#[inline]
pub fn platform_serial_is_suspended() -> bool {
    // SAFETY: `platform_serial_is_suspended` is safe to call from any context, including panic.
    unsafe { cpp_platform_serial_is_suspended() }
}

/// Returns the boot time at which the debug UART was most recently suspended via
/// `platform_serial_suspend`. The value is only meaningful when [`platform_serial_is_suspended`]
/// returns `true`.
#[inline]
pub fn platform_serial_suspended_at() -> InstantBoot {
    // SAFETY: `platform_serial_suspended_at` is safe to call from any context, including panic.
    unsafe { cpp_platform_serial_suspended_at() }
}

#[inline]
fn platform_specific_halt(
    suggested_action: PlatformHaltAction,
    reason: ZirconCrashReason,
    halt_on_panic: bool,
) -> ! {
    // SAFETY: `cpp_platform_specific_halt` halts or reboots the system and never returns.
    unsafe { cpp_platform_specific_halt(suggested_action, reason, halt_on_panic) }
}

/// Common platform halt path. This handles some tasks we always want to make
/// sure we handle before dropping into the common platform specific halt
/// routine.
pub fn platform_halt(suggested_action: PlatformHaltAction, reason: ZirconCrashReason) -> ! {
    // Disable the automatic uptime updating.  We are going to attempt to
    // deliberately halt the system, and we don't want the crashlog to indicate a
    // spontaneous reboot.
    PlatformCrashlog::get().enable_crashlog_uptime_updates(false);

    // We are haling on purpose.  Disable the watchdog (if we have one, and if we
    // can) if we plan to halt instead of instigate a reboot.  If we are going to
    // try to actually reboot, pet the dog one last time to give ourselves the
    // maximum amount of time to arrange our graceful reboot.
    let mut halt_on_panic = BootOptions::get().halt_on_panic;

    // If we are crashing while usermode has suspended the debug UART, there is no
    // way for anyone to interact with the panic shell, and we cannot print
    // anything to the UART.  Rather than silently hanging, reboot so that the
    // crashlog (which will note that the UART was suspended) can be recovered.
    if halt_on_panic && reason != ZirconCrashReason::NoCrash && platform_serial_is_suspended() {
        halt_on_panic = false;
    }

    if halt_on_panic {
        // SAFETY: Safe to call after early boot; dispatches to the registered static watchdog
        // driver or the no-op default driver.
        let _ = unsafe { hw_watchdog_set_enabled(false) };
    } else {
        // SAFETY: Safe to call after early boot; dispatches to the registered static watchdog
        // driver or the no-op default driver.
        unsafe { hw_watchdog_pet() };
    }

    // Render and stash our crashlog.  crashlog_to_string will decide what should
    // be in the crashlog payload based on the provided crash reason.
    {
        let mut rendered_crashlog_len = 0;
        let crashlog = PlatformCrashlog::get();
        let target = crashlog.get_render_target();
        if target.len() > 0 {
            // SAFETY: `target` was returned by `crashlog.get_render_target()` with
            // `target.len() > 0`, so by `crashlog::Interface`'s safety contract it is non-null,
            // valid for writes of `target.len()` bytes, and unaliased during `platform_halt`.
            let target = unsafe { slice::from_raw_parts_mut(target.cast::<u8>(), target.len()) };
            rendered_crashlog_len = crate::crashlog::crashlog_to_string(target, reason as u32);
        }
        // SAFETY: Called during single-threaded platform halt after
        // `enable_crashlog_uptime_updates(false)` and after rendering to `target` has completed.
        unsafe {
            crashlog.finalize(reason, rendered_crashlog_len);
        }
    }

    // This is a graceful reboot, invalidate the persistent dlog (if we have one)
    // and persistent trace buffer (if we have one) so that we don't attempt to
    // recover it during a reboot.
    if reason == ZirconCrashReason::NoCrash {
        persistent_dlog_invalidate();
        jtrace_invalidate();
    }

    // Finally, fall into the platform specific halt handler.
    platform_specific_halt(suggested_action, reason, halt_on_panic);
}

/// FFI trampoline for [`platform_halt`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_halt(
    suggested_action: PlatformHaltAction,
    reason: ZirconCrashReason,
) -> ! {
    platform_halt(suggested_action, reason)
}

/// Prepares the platform to print a panic: stops the other CPUs and switches the
/// console to a mode that works without them.
#[inline]
pub fn platform_panic_start() {
    // SAFETY: no preconditions; safe to call from any CPU at any time.
    unsafe { cpp_platform_panic_start() }
}

/// # Safety
///
/// If `argc > 1`, `argv` must be valid for at least 2 elements and `argv[1].arg_str` must be a
/// valid NUL-terminated C string.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_reboot(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
    let is_panic_shell = (flags & CMD_FLAG_PANIC) != 0;

    // If we are already panicking, don't repeat the first half of `platform_halt`. Instead,
    // just finish the reboot.
    if is_panic_shell {
        platform_specific_halt(
            PlatformHaltAction::Reboot,
            ZirconCrashReason::Panic,
            /*halt_on_panic=*/ false,
        );
        // unreachable
    }

    // SAFETY: The console dispatcher guarantees `argv` is valid for `argc` elements when `argc > 1`
    // and that `arg_str` points to a valid NUL-terminated string.
    if argc > 1 && unsafe { CStr::from_ptr((*argv.add(1)).arg_str) } == c"bootloader" {
        dprintf!(INFO, "rebooting to the bootloader . . .\n");
        platform_halt(PlatformHaltAction::RebootBootloader, ZirconCrashReason::NoCrash);
        // unreachable
    }
    platform_halt(PlatformHaltAction::Reboot, ZirconCrashReason::NoCrash);
}

#[cfg(console_enabled)]
static_command!(
    CMD_REBOOT,
    c"reboot".as_ptr(),
    c"reboot the system".as_ptr(),
    cmd_reboot,
    CMD_AVAIL_ALWAYS
);
