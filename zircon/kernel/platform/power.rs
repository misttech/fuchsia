// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2012 Google, Inc.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_ALWAYS, CMD_FLAG_PANIC, CmdArgs, static_command};
use crate::pdev_watchdog::{hw_watchdog_pet, hw_watchdog_set_enabled};
use crate::persistent_debuglog_rs::persistent_dlog_invalidate;
use boot_options::BootOptions;
#[cfg(console_enabled)]
use core::ffi::{CStr, c_int};
use core::ffi::{c_char, c_void};
use core::ptr::{NonNull, slice_from_raw_parts_mut};
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

unsafe extern "C" {
    fn cpp_platform_specific_halt(
        suggested_action: PlatformHaltAction,
        reason: ZirconCrashReason,
        halt_on_panic: bool,
    ) -> !;
    fn cpp_platform_panic_start();
    fn cpp_platform_crashlog_get() -> NonNull<c_void>;
    fn cpp_platform_crashlog_enable_crashlog_uptime_updates(
        crashlog: NonNull<c_void>,
        enabled: bool,
    );
    fn cpp_platform_crashlog_get_render_target(
        crashlog: NonNull<c_void>,
        out_len: &mut usize,
    ) -> *mut c_char;
    fn cpp_platform_crashlog_finalize(
        crashlog: NonNull<c_void>,
        reason: ZirconCrashReason,
        amt: usize,
    );
    fn cpp_crashlog_to_string(
        target_ptr: *mut c_char,
        target_len: usize,
        reason: ZirconCrashReason,
    ) -> usize;
    fn cpp_jtrace_invalidate();
    fn cpp_platform_serial_is_suspended() -> bool;
}

#[inline]
fn platform_serial_is_suspended() -> bool {
    // SAFETY: `platform_serial_is_suspended` is safe to call from any context, including panic.
    unsafe { cpp_platform_serial_is_suspended() }
}

struct PlatformCrashlog;

struct PlatformCrashlogInterface(NonNull<c_void>);

impl PlatformCrashlog {
    #[inline]
    fn get() -> PlatformCrashlogInterface {
        // SAFETY: `PlatformCrashlog::Get()` always returns a reference to a static-lifetime
        // `PlatformCrashlog::Interface` implementation.
        PlatformCrashlogInterface(unsafe { cpp_platform_crashlog_get() })
    }
}

impl PlatformCrashlogInterface {
    #[inline]
    fn enable_crashlog_uptime_updates(&self, enabled: bool) {
        // SAFETY: `self.0` is a valid `PlatformCrashlog::Interface*` returned by
        // `PlatformCrashlog::Get()`.
        unsafe { cpp_platform_crashlog_enable_crashlog_uptime_updates(self.0, enabled) }
    }

    #[inline]
    fn get_render_target(&self) -> *mut [c_char] {
        let mut len = 0;
        // SAFETY: `self.0` is a valid `PlatformCrashlog::Interface*` returned by
        // `PlatformCrashlog::Get()`, and `&mut len` is a valid writable `usize`.
        let ptr = unsafe { cpp_platform_crashlog_get_render_target(self.0, &mut len) };
        slice_from_raw_parts_mut(ptr, len)
    }

    #[inline]
    fn finalize(&self, reason: ZirconCrashReason, amt: usize) {
        // SAFETY: `self.0` is a valid `PlatformCrashlog::Interface*` returned by
        // `PlatformCrashlog::Get()`.
        unsafe { cpp_platform_crashlog_finalize(self.0, reason, amt) }
    }
}

/// # Safety
///
/// If `target.len() > 0`, the caller must ensure that `target.cast::<c_char>()` is non-null
/// and valid for writes of `target.len()` bytes for the duration of this call, with no
/// concurrent accesses to that memory range.
#[inline]
unsafe fn crashlog_to_string(target: *mut [c_char], reason: ZirconCrashReason) -> usize {
    // SAFETY: Discharged by the caller per this function's `# Safety` contract.
    unsafe { cpp_crashlog_to_string(target.cast::<c_char>(), target.len(), reason) }
}

#[inline]
fn jtrace_invalidate() {
    // SAFETY: Invalidates the global jtrace buffer (or is a no-op if jtrace is disabled).
    unsafe { cpp_jtrace_invalidate() }
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
            // `target.len() > 0`, so it is non-null and valid for writes of `target.len()` bytes
            // during `platform_halt`.
            rendered_crashlog_len = unsafe { crashlog_to_string(target, reason) };
        }
        crashlog.finalize(reason, rendered_crashlog_len);
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
