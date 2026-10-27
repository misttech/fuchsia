// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The kernel crashlog: rendering of the crash report stored in persistent RAM, the panic buffer
//! that captures panic/assert messages for it, and the stash of the crashlog recovered from the
//! previous boot.

use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char, c_int, c_void};
use core::fmt::{self, Write as _};
use core::slice;

use affine::Round;
use bitflags::bitflags;
use boot_options::BootOptions;
use fbl::RefPtr;
use ksync::{KCell, declare_singleton_mutex};
use libc::stdio::{FILE, StringFile};
use zx_types::ZX_KOID_INVALID;

use crate::backtrace::Backtrace;
use crate::debuglog_rs::dlog_render_to_crashlog;
use crate::kernel::thread;
use crate::lockup_detector::{
    lockup_get_critical_section_oops_count, lockup_get_no_heartbeat_oops_count,
};
use crate::object::{ChannelDispatcher, HandleTableArena, RootJobObserver};
use crate::platform_rs::power::{
    ZirconCrashReason, platform_serial_is_suspended, platform_serial_suspended_at,
};
use crate::platform_rs::timer::{
    timer_current_mono_and_boot_ticks, timer_get_boot_ticks_offset, timer_get_mono_ticks_offset,
    timer_get_ticks_to_time_ratio, zx_msec,
};
use crate::top::boot_constants::{kernel_debug_ident, kernel_version_ident};
use crate::vm::pmm_checker::PmmChecker;
use crate::vm::pmm_node::PmmNode;
use crate::vm::vm_object::VmObject;

#[path = "panic_buffer.rs"]
mod panic_buffer;

#[cfg(ktest)]
#[path = "tests.rs"]
mod tests;

use panic_buffer::PANIC_BUFFER;

unsafe extern "C" {
    fn arch_render_crashlog_registers(target: *mut FILE, regs: *const CrashlogRegs);

    // Defined in //zircon/kernel/lib/io.
    #[allow(non_upper_case_globals)]
    static mut gStdoutNoPersist: FILE;
}

/// Size of the architecture's `crashlog_regs_t`.
#[cfg(target_arch = "aarch64")]
const CRASHLOG_REGS_SIZE: usize = 48;
/// Size of the architecture's `crashlog_regs_t`.
#[cfg(target_arch = "x86_64")]
const CRASHLOG_REGS_SIZE: usize = 40;
/// Size of the architecture's `crashlog_regs_t`.
#[cfg(target_arch = "riscv64")]
const CRASHLOG_REGS_SIZE: usize = 24;

/// Storage for the architecture's `crashlog_regs_t`.
///
/// The arch-independent crashlog code never looks inside the registers; they are written by the
/// architecture's exception handlers and rendered by `arch_render_crashlog_registers`.
#[repr(C, align(8))]
struct CrashlogRegs(zr::OpaqueBytes<CRASHLOG_REGS_SIZE>);

zr::static_assert!(core::mem::size_of::<CrashlogRegs>() == CRASHLOG_REGS_SIZE);
zr::static_assert!(core::mem::align_of::<CrashlogRegs>() == 8);

/// The C++ `crashlog_t`.
#[repr(C)]
struct Crashlog {
    regs: CrashlogRegs,
}

/// Holder for the global [`Crashlog`], which C++ exception handlers write directly through the
/// `g_crashlog` symbol.
///
/// This cannot be a `LazyInit`: the C++ code accesses the object by symbol name with no
/// initialization step, so it must be statically zero-initialized.
#[repr(transparent)]
struct CrashlogCell(UnsafeCell<Crashlog>);

// SAFETY: `CrashlogCell` only exposes its contents through the raw pointer returned by
// `regs_ptr()`, so sharing it is sound by itself; code that dereferences that pointer must not
// race with the C++ exception-path writes.
unsafe impl Sync for CrashlogCell {}

impl CrashlogCell {
    /// Returns a pointer to the global crashlog's registers.
    fn regs_ptr(&self) -> *const CrashlogRegs {
        // SAFETY: `self.0.get()` is a valid pointer to the `Crashlog`; this only computes a field
        // address without creating a reference.
        unsafe { &raw const (*self.0.get()).regs }
    }
}

/// The global crashlog, exported to C++ as `g_crashlog`.
#[unsafe(export_name = "g_crashlog")]
static G_CRASHLOG: CrashlogCell = CrashlogCell(UnsafeCell::new(Crashlog {
    regs: CrashlogRegs(zr::OpaqueBytes::new([0; CRASHLOG_REGS_SIZE])),
}));

/// Holder for [`STDOUT_PANIC_BUFFER`], which hands out raw mutable [`FILE`] pointers for
/// `fprintf` without forming Rust references to the `FILE`.
#[repr(transparent)]
pub(crate) struct StdoutPanicBuffer(UnsafeCell<FILE>);

impl StdoutPanicBuffer {
    /// Returns a raw mutable pointer to the underlying [`FILE`].
    #[inline]
    pub(crate) const fn as_mut_ptr(&self) -> *mut FILE {
        self.0.get()
    }
}

// SAFETY: The `FILE` is never mutated after construction, its context pointer is null, and its
// write callback only touches the internally-synchronized panic buffer and console.
unsafe impl Sync for StdoutPanicBuffer {}

/// The `FILE` callback for [`STDOUT_PANIC_BUFFER`].
///
/// # Safety
///
/// If `len > 0`, `data` must be non-null and valid for reads of `len` bytes.
unsafe extern "C" fn stdout_panic_buffer_callback(
    _ptr: *mut c_void,
    data: *const c_char,
    len: usize,
) -> c_int {
    let bytes = if len == 0 {
        &[]
    } else {
        // SAFETY: `len > 0`, and the caller guarantees `data` is non-null and valid for reads of
        // `len` bytes.
        unsafe { slice::from_raw_parts(data.cast::<u8>(), len) }
    };
    stdout_panic_buffer_write(bytes)
}

/// Writes `bytes` to both stdout and the global [`PANIC_BUFFER`], exactly as writing to
/// [`STDOUT_PANIC_BUFFER`] does.
pub(crate) fn stdout_panic_buffer_write(bytes: &[u8]) -> c_int {
    PANIC_BUFFER.append(bytes);
    let stdout = &raw const gStdoutNoPersist;
    // SAFETY: `gStdoutNoPersist` is initialized once by a global constructor during `lk_main` and
    // never modified afterwards; before then its callback is null, which `FILE::write` handles.
    unsafe { (*stdout).write(bytes) }
}

/// A `FILE` that writes to both stdout and the global [`PANIC_BUFFER`], exported to C++ as
/// `stdout_panic_buffer`.
#[unsafe(export_name = "stdout_panic_buffer")]
pub(crate) static STDOUT_PANIC_BUFFER: StdoutPanicBuffer = StdoutPanicBuffer(UnsafeCell::new(
    // SAFETY: `stdout_panic_buffer_callback` ignores its context pointer and only requires that
    // `data` point to `len` readable bytes, which is the `FILE::new` contract. It is a plain
    // function, so it remains valid for the `'static` lifetime of this `FILE`.
    unsafe { FILE::new(Some(stdout_panic_buffer_callback), core::ptr::null_mut()) },
));

declare_singleton_mutex!(RecoveredCrashlogLock);

/// The stashed recovered crashlog.
// TODO(https://fxbug.dev/565808307): Remove this wrapper once the Rust `VmObject` binding is
// `Send + Sync`.
struct RecoveredCrashlog(Option<RefPtr<VmObject>>);

// SAFETY: `VmObject` is an internally synchronized, reference-counted kernel object that C++
// freely shares across threads; the Rust binding is just not marked `Send`/`Sync`. In addition,
// every access to `RECOVERED_CRASHLOG` goes through `RecoveredCrashlogLock`.
unsafe impl Send for RecoveredCrashlog {}

static RECOVERED_CRASHLOG: KCell<RecoveredCrashlog, RecoveredCrashlogLock> =
    KCell::new(RecoveredCrashlog(None));

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct RenderRegion: u32 {
        const BANNER            = 0x01;
        const DEBUG_INFO        = 0x02;
        const CRITICAL_COUNTERS = 0x04;
        const PANIC_BUFFER      = 0x08;
        const DLOG              = 0x10;
        const ROOT_JOB_CRITICAL = 0x20;
    }
}

fn map_reason_to_regions(reason: Option<ZirconCrashReason>) -> RenderRegion {
    match reason {
        Some(ZirconCrashReason::NoCrash) => RenderRegion::empty(),

        Some(ZirconCrashReason::Oom) => {
            RenderRegion::BANNER | RenderRegion::CRITICAL_COUNTERS | RenderRegion::DLOG
        }

        Some(ZirconCrashReason::UserspaceRootJobTermination) => {
            RenderRegion::BANNER
                | RenderRegion::CRITICAL_COUNTERS
                | RenderRegion::DLOG
                | RenderRegion::ROOT_JOB_CRITICAL
        }

        Some(ZirconCrashReason::Panic | ZirconCrashReason::SoftwareWatchdog) => {
            RenderRegion::BANNER
                | RenderRegion::DEBUG_INFO
                | RenderRegion::CRITICAL_COUNTERS
                | RenderRegion::PANIC_BUFFER
                | RenderRegion::DLOG
        }

        _ => RenderRegion::BANNER,
    }
}

/// Formats like C's `%#x`, which (unlike Rust's `{:#x}`) prints zero without the `0x` prefix.
struct CAltHex(u64);

impl fmt::Display for CAltHex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 { f.write_str("0") } else { write!(f, "{:#x}", self.0) }
    }
}

/// Serialize the crashlog to string into `target`. If `reason` is OOM, then a
/// different preamble will be used, and the backtrace will not be included.
pub(crate) fn crashlog_to_string(target: &mut [u8], reason: u32) -> usize {
    let mut outfile = StringFile::new(target);
    let known_reason = ZirconCrashReason::try_from(reason).ok();
    let regions = map_reason_to_regions(known_reason);

    if regions.contains(RenderRegion::BANNER) {
        let reason_str = match known_reason {
            Some(ZirconCrashReason::NoCrash) => "NO CRASH",

            Some(ZirconCrashReason::Oom) => "OOM",

            Some(ZirconCrashReason::Panic) => "KERNEL PANIC",

            Some(ZirconCrashReason::SoftwareWatchdog) => "SW WATCHDOG",

            Some(ZirconCrashReason::UserspaceRootJobTermination) => {
                "USERSPACE ROOT JOB TERMINATION"
            }

            _ => "UNKNOWN",
        };
        let _ = write!(outfile, "ZIRCON REBOOT REASON ({reason_str})\n\n");

        let ticks = timer_current_mono_and_boot_ticks();
        let transform = timer_get_ticks_to_time_ratio();

        let _ = write!(
            outfile,
            "UPTIME (ms)\n{}\n\n",
            transform.scale::<{ Round::DOWN }>(ticks.boot_now) / zx_msec(1)
        );
        let _ = write!(
            outfile,
            "RUNTIME (ms)\n{}\n\n",
            transform.scale::<{ Round::DOWN }>(ticks.mono_now) / zx_msec(1)
        );
        let _ = write!(
            outfile,
            "TIMER INFO (ticks) counter:{} boot: {} mono: {}\n\n",
            // `boot_now = current_ticks + boot_ticks_offset`. The offset is negative.
            CAltHex(ticks.boot_now.wrapping_sub(timer_get_boot_ticks_offset()) as u64),
            CAltHex(timer_get_boot_ticks_offset() as u64),
            CAltHex(timer_get_mono_ticks_offset() as u64)
        );

        // If usermode had suspended the debug UART (or was in the process of
        // suspending it) when this crashlog was generated, none of the kernel's
        // recent output (including any output related to this crashlog) made it out
        // of the serial port.  Make a note of that so that whoever reads this
        // crashlog knows why.
        if platform_serial_is_suspended() {
            let _ = write!(
                outfile,
                "DEBUG UART SUSPENDED (or suspending, since boot time {}ms); \
                 kernel output since then was not written to the serial console\n\n",
                platform_serial_suspended_at().0 / zx_msec(1),
            );
        }

        outfile.write(if regions.contains(RenderRegion::DEBUG_INFO) {
            // Include full symbolizer markup context for DebugInfo.
            kernel_debug_ident()
        } else {
            // Otherwise, the human-readable prefix of the full text.
            kernel_version_ident()
        });
    }

    if regions.contains(RenderRegion::ROOT_JOB_CRITICAL) {
        let proc_koid = RootJobObserver::get_critical_process_koid();
        let proc_name_chars = RootJobObserver::get_critical_process_name();
        let proc_name = CStr::from_bytes_until_nul(&proc_name_chars)
            .map_or(&proc_name_chars[..], CStr::to_bytes);
        if proc_koid != ZX_KOID_INVALID {
            outfile.write(b"ROOT JOB TERMINATED BY CRITICAL PROCESS DEATH: ");
            outfile.write(proc_name);
            let _ = writeln!(outfile, " ({proc_koid})");
        }
    }

    if regions.contains(RenderRegion::DEBUG_INFO) {
        // The presence of "REGISTERS" is load-bearing; we have crashlog tests that
        // that explicitly parse for it.
        // LINT.IfChange
        let _ = writeln!(outfile, "REGISTERS");
        // LINT.ThenChange(
        //     //src/tests/kernel_crashlog/kernel_crashlog_test.go,
        //     //zircon/kernel/lib/crashlog/src/tests.rs
        // )

        // Note: it is the architecture specific implementation which is responsible
        // for determining if the crashlog registers are valid, and what (if
        // anything) to print.
        outfile.with_file(|file| {
            // SAFETY: `file.as_mut_ptr()` points to a valid `FILE` for the duration of the call,
            // and `regs_ptr()` points to the global crashlog registers, whose `iframe` is null or
            // points to the live `iframe_t` of the crashing CPU.
            unsafe { arch_render_crashlog_registers(file.as_mut_ptr(), G_CRASHLOG.regs_ptr()) }
        });

        let _ = writeln!(outfile, "BACKTRACE (up to 16 calls)");
        let mut bt = Backtrace::new();
        thread::current_get_backtrace(&mut bt);
        outfile.with_file(|file| {
            // SAFETY: `file.as_mut_ptr()` points to a valid `FILE` for the duration of the call.
            unsafe { bt.print_without_version(file.as_mut_ptr()) }
        });
        let _ = writeln!(outfile);
    }

    if regions.contains(RenderRegion::CRITICAL_COUNTERS) {
        // Include counters for critical events.
        let _ = writeln!(
            outfile,
            "counters: haf={} paf={} pvf={} lcs={} lhb={} cf={} ",
            HandleTableArena::get_alloc_failed_count(),
            PmmNode::get_alloc_failed_count(),
            PmmChecker::get_validation_failed_count(),
            lockup_get_critical_section_oops_count(),
            lockup_get_no_heartbeat_oops_count(),
            ChannelDispatcher::get_channel_full_count()
        );
    }

    if regions.contains(RenderRegion::PANIC_BUFFER) {
        // Include as much of the contents of the panic buffer as we can, if it is
        // not empty.
        //
        // The panic buffer is one of the last thing we print.  Space is limited so
        // if the panic/assert message was long we may not be able to include the
        // whole thing.  That's OK.  The panic buffer is a "nice to have" and we've
        // already printed the primary diagnostics (register dump and backtrace).
        PANIC_BUFFER.with_contents(|contents| {
            if contents.is_empty() {
                let _ = writeln!(outfile, "panic buffer: empty");
            } else {
                // The contents end at the first NUL, if any.
                let contents =
                    CStr::from_bytes_until_nul(contents).map_or(contents, CStr::to_bytes);
                outfile.write(b"panic buffer: ");
                outfile.write(contents);
                outfile.write(b"\n");
            }
        });
    }

    if regions.contains(RenderRegion::DLOG) {
        // Finally, if we have been configured to do so, render as much of the
        // recent debug log as we can fit into the crashlog memory.

        // This banner is load-bearing server-side as it's used to extract Dlog dump.
        // Please notify //src/developer/forensics/OWNERS upon changing.
        // LINT.IfChange
        const HEADER: &[u8] = b"\n--- BEGIN DLOG DUMP ---\n";
        const FOOTER: &[u8] = b"\n--- END DLOG DUMP ---\n";
        // LINT.ThenChange(//src/developer/forensics/feedback/reboot_log/reboot_log.cc)

        outfile.write(HEADER);
        if BootOptions::get().render_dlog_to_crashlog {
            let available_region = outfile.available_region();
            let payload_len = available_region.len().saturating_sub(FOOTER.len());
            let payload_region = &mut available_region[..payload_len];

            let rendered = dlog_render_to_crashlog(payload_region);
            outfile.skip(rendered);
        } else {
            let _ = writeln!(outfile, "DLOG -> Crashlog disabled");
        }
        outfile.write(FOOTER);
    }

    outfile.used_region().len()
}

/// Stash the recovered crashlog for later retrieval by the `k crashlog dump` command.
pub(crate) fn crashlog_stash(crashlog: Option<RefPtr<VmObject>>) {
    ksync::lock!(let mut guard = RecoveredCrashlogLock::lock());
    let token = guard.as_mut().token_mut();
    // SAFETY: `token` proves that the singleton lock guarding `RECOVERED_CRASHLOG` is held.
    unsafe { RECOVERED_CRASHLOG.get_mut(token) }.0 = crashlog;
}

#[cfg(console_enabled)]
mod console {
    use core::ffi::{CStr, c_int};
    use core::slice;

    use fbl::RefPtr;
    use kprint::kprintln;
    use zx_status::Status;

    use super::{RECOVERED_CRASHLOG, RecoveredCrashlogLock};
    use crate::console::{CMD_AVAIL_ALWAYS, CmdArgs, static_command};
    use crate::vm::vm_object::VmObject;

    /// Returns the previously stashed recovered crashlog, or `None`.
    fn crashlog_get_stashed() -> Option<RefPtr<VmObject>> {
        ksync::lock!(let guard = RecoveredCrashlogLock::lock());
        // SAFETY: `guard.token()` proves that the singleton lock guarding `RECOVERED_CRASHLOG` is
        // held.
        unsafe { RECOVERED_CRASHLOG.get(guard.token()) }.0.clone()
    }

    fn print_recovered_crashlog() {
        let Some(crashlog) = crashlog_get_stashed() else {
            kprintln!("no recovered crashlog");
            return;
        };

        // Allocate a temporary buffer to read the VMO's contents into. The extra byte keeps the
        // buffer a real heap allocation (not a dangling pointer) even when the crashlog is empty,
        // since `read` requires a kernel address.
        let crashlog_size = crashlog.size() as usize;
        let buffer_size = crashlog_size + 1;
        let Ok(mut buffer) = kalloc::Box::<[u8]>::try_new_uninit_slice(buffer_size) else {
            kprintln!("error: failed to allocate {:u} for crashlog", buffer_size);
            return;
        };

        let data = match crashlog.read(0, &mut buffer[..crashlog_size]) {
            Ok(data) => data,
            Err(status) => {
                kprintln!(
                    "error: failed to read from recovered crashlog vmo: {:d}",
                    status.into_raw()
                );
                return;
            }
        };
        // The contents end at the first NUL, if any.
        let contents = CStr::from_bytes_until_nul(data).map_or(&data[..], CStr::to_bytes);

        kprintln!("recovered crashlog follows...");
        kprintln!("{:s}", contents);
        kprintln!("... end of recovered crashlog");
    }

    /// Console command handler for `crashlog`.
    ///
    /// # Safety
    ///
    /// `argv` must point to a valid array of at least `argc` [`CmdArgs`] elements, where each
    /// element's `arg_str` is a valid null-terminated C string.
    unsafe extern "C" fn cmd_crashlog(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return Status::INTERNAL.into_raw();
        }
        // SAFETY: `argc > 0` and `argv` is non-null; the caller guarantees `argv` is valid for
        // `argc` elements.
        let argv = unsafe { slice::from_raw_parts(argv, argc as usize) };
        'usage: {
            if argc < 2 {
                kprintln!("not enough arguments");
                break 'usage;
            }

            if argv[1].as_str() == "dump" {
                print_recovered_crashlog();
            } else {
                kprintln!("unknown command");
                break 'usage;
            }

            return zx_types::ZX_OK;
        }
        kprintln!("usage:");
        kprintln!(
            "{:cs} dump                              : dump the recovered crashlog",
            argv[0].arg_str
        );
        Status::INTERNAL.into_raw()
    }

    static_command!(
        CMD_CRASHLOG,
        c"crashlog".as_ptr(),
        c"crashlog".as_ptr(),
        cmd_crashlog,
        CMD_AVAIL_ALWAYS
    );
}
