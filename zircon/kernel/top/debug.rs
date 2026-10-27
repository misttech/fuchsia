// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{VaList, c_char, c_int, c_void};
use core::fmt::{self, Display, Formatter};
use core::mem::size_of_val;

use libc::stdio::{fprintf, vfprintf};
use libc::string::strlen;
use zerocopy::IntoBytes;

use crate::crashlog::stdout_panic_buffer;
use crate::hw_rng::hw_rng_get_entropy;
use crate::platform_rs::power::{
    PlatformHaltAction, ZirconCrashReason, platform_halt, platform_panic_start,
};
use crate::platform_rs::timer::{DurationBoot, InstantBoot, current_boot_time, zx_usec};

/// Return the program counter of the calling function.
macro_rules! get_caller {
    () => {{
        // SAFETY: `llvm.returnaddress` with level `0` has no preconditions and returns the current
        // function's return address directly.
        #[allow(unused_unsafe)]
        unsafe {
            $crate::top::debug_ffi::return_address(0)
        }
    }};
}

/// Return the address of the current stack frame.
macro_rules! get_frame {
    () => {{
        // SAFETY: `llvm.frameaddress` with level `0` has no preconditions and returns the current
        // function's frame pointer directly.
        #[allow(unused_unsafe)]
        unsafe {
            $crate::top::debug_ffi::frame_address(0)
        }
    }};
}

/// Start a system panic, and print a header message.
///
/// Calls should be followed by:
///
///   * Calling [`fprintf`] on [`stdout_panic_buffer`] with the reason for the panic, followed by a
///     newline.
///
///   * A call to [`panic_finish`].
#[inline(always)]
fn panic_start(pc: *mut c_void, frame: *mut c_void) {
    platform_panic_start();

    // SAFETY: `stdout_panic_buffer` is a valid `FILE`, and the `%p` specifiers match the two
    // `c_void` pointer arguments `(pc, frame)`.
    unsafe {
        fprintf(
            stdout_panic_buffer.as_mut_ptr(),
            c"\n*** KERNEL PANIC (caller pc: %p, stack frame: %p):\n*** ".as_ptr(),
            pc,
            frame,
        );
    }
}

/// Finish a system panic.
///
/// This function will not return, but will perform an action such as rebooting the system or
/// dropping the system into a debug shell.
///
/// Marked `#[inline(always)]` to avoid an additional stack frame from appearing in the backtrace.
#[inline(always)]
fn panic_finish() -> ! {
    // Add a newline between the panic message and the stack trace.
    // SAFETY: `stdout_panic_buffer` is a valid `FILE`, and the format string contains no format
    // specifiers.
    unsafe {
        fprintf(stdout_panic_buffer.as_mut_ptr(), c"\n".as_ptr());
    }

    platform_halt(PlatformHaltAction::Halt, ZirconCrashReason::Panic)
}

/// Determine if the given string ends with the given character.
///
/// # Safety
///
/// `str` must be non-null and point to a valid NUL-terminated C string.
unsafe fn ends_with(str: *const c_char, x: c_char) -> bool {
    // SAFETY: The caller guarantees `str` points to a valid NUL-terminated C string.
    let len = unsafe { strlen(str) };
    // SAFETY: `len > 0` implies `len - 1 < len`, so `str.add(len - 1)` is in bounds of `str` and
    // valid for reads.
    len > 0 && unsafe { *str.add(len - 1) } == x
}

/// Formats a `VaList` panic message and halts the system.
///
/// # Safety
///
/// `fmt` must be non-null and point to a valid NUL-terminated C format string whose format
/// specifiers match the arguments in `ap`.
pub(super) unsafe extern "C" fn vpanic(
    pc: *mut c_void,
    frame: *mut c_void,
    fmt: *const c_char,
    ap: VaList<'_>,
) -> ! {
    panic_start(pc, frame);

    // Print the user message.
    // SAFETY: `stdout_panic_buffer` is a valid `FILE`, and the caller guarantees `fmt` is a valid
    // NUL-terminated format string matching `ap`.
    unsafe {
        vfprintf(stdout_panic_buffer.as_mut_ptr(), fmt, ap);
    }

    // Add a newline to the end of the panic message if it was missing.
    // SAFETY: The caller guarantees `fmt` points to a valid NUL-terminated C string.
    if !unsafe { ends_with(fmt, b'\n' as c_char) } {
        // SAFETY: `stdout_panic_buffer` is a valid `FILE`, and the format string contains no
        // format specifiers.
        unsafe {
            fprintf(stdout_panic_buffer.as_mut_ptr(), c"\n".as_ptr());
        }
    }

    panic_finish()
}

/// Spin the CPU for a period of (short) time.
#[unsafe(no_mangle)]
pub extern "C" fn spin(usecs: u32) {
    let start: InstantBoot = current_boot_time();

    let nsecs = DurationBoot(zx_usec(i64::from(usecs)));
    while current_boot_time() - start < nsecs {}
}

/// Busy-waits for at least `usecs` microseconds without blocking.
///
/// This does not yield the CPU, so it is usable with interrupts or preemption disabled. Prefer
/// [`crate::kernel::thread::sleep_relative`] wherever blocking is acceptable.
#[inline]
pub fn spin_usecs(usecs: u32) {
    spin(usecs);
}

/// Systemwide halt with a formatted panic message.
///
/// # Safety
///
/// `fmt` must be non-null and point to a valid NUL-terminated C format string whose format
/// specifiers match the variadic arguments in `ap`.
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn panic(fmt: *const c_char, ap: ...) -> ! {
    // SAFETY: The caller guarantees `fmt` is a valid NUL-terminated format string matching `ap`.
    unsafe { vpanic(get_caller!(), get_frame!(), fmt, ap) }
}

/// Called when an assert with a formatted message fails.
///
/// # Safety
///
/// `file` and `expression` must each be either null or point to a valid NUL-terminated C string.
/// `fmt` must be non-null and point to a valid NUL-terminated C format string whose format
/// specifiers match the variadic arguments in `ap`.
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn assert_fail_msg(
    file: *const c_char,
    line: c_int,
    expression: *const c_char,
    fmt: *const c_char,
    ap: ...
) -> ! {
    panic_start(get_caller!(), get_frame!());

    // Print the user message.
    // SAFETY: `stdout_panic_buffer` is a valid `FILE`, the format specifiers match `(file, line,
    // expression)`, and the caller guarantees `file` and `expression` are each null or valid C
    // strings.
    unsafe {
        fprintf(
            stdout_panic_buffer.as_mut_ptr(),
            c"ASSERT FAILED at (%s:%d): %s\n".as_ptr(),
            file,
            line,
            expression,
        );
    }
    // SAFETY: `stdout_panic_buffer` is a valid `FILE`, and the caller guarantees `fmt` is a valid
    // NUL-terminated format string matching `ap`.
    unsafe {
        vfprintf(stdout_panic_buffer.as_mut_ptr(), fmt, ap);
    }

    // Add a newline to the end of the panic message if it was missing.
    // SAFETY: The caller guarantees `fmt` points to a valid NUL-terminated C string.
    if !unsafe { ends_with(fmt, b'\n' as c_char) } {
        // SAFETY: `stdout_panic_buffer` is a valid `FILE`, and the format string contains no
        // format specifiers.
        unsafe {
            fprintf(stdout_panic_buffer.as_mut_ptr(), c"\n".as_ptr());
        }
    }

    panic_finish()
}

/// Called when an assert fails.
///
/// # Safety
///
/// `file` and `expression` must each be either null or point to a valid NUL-terminated C string.
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn assert_fail(
    file: *const c_char,
    line: c_int,
    expression: *const c_char,
) -> ! {
    panic_start(get_caller!(), get_frame!());
    // SAFETY: `stdout_panic_buffer` is a valid `FILE`, the format specifiers match `(file, line,
    // expression)`, and the caller guarantees `file` and `expression` are each null or valid C
    // strings.
    unsafe {
        fprintf(
            stdout_panic_buffer.as_mut_ptr(),
            c"ASSERT FAILED at (%s:%d): %s\n".as_ptr(),
            file,
            line,
            expression,
        );
    }
    panic_finish()
}

/// Chooses a stack guard value from hardware entropy if available.
#[unsafe(no_mangle)]
#[sanitize(safestack = "off")]
pub extern "C" fn choose_stack_guard() -> usize {
    let mut guard: usize = 0;
    if hw_rng_get_entropy(guard.as_mut_bytes()) != size_of_val(&guard) {
        // We can't get a random value, so use a randomish value.
        guard = 0xdeadbeef00ff00ff ^ (&raw const guard).addr();
    }
    guard
}

/// One chunk of the filler line in the `k crash rust_panic` message.
const CRASH_PANIC_FILLER_CHUNK: &str =
    "................................................................";

/// Number of filler chunks. Together they make the message several hundred bytes
/// long, while the whole panic output still fits in the 2048-byte crashlog panic
/// buffer.
const CRASH_PANIC_FILLER_CHUNK_COUNT: usize = 8;

/// Renders the filler line piecewise, so the panic message is formatted rather
/// than a literal and has to be streamed through the panic handler.
struct CrashPanicFiller;

impl Display for CrashPanicFiller {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        for _ in 0..CRASH_PANIC_FILLER_CHUNK_COUNT {
            formatter.write_str(CRASH_PANIC_FILLER_CHUNK)?;
        }
        Ok(())
    }
}

/// Deliberately panics through the Rust panic handler. Backs `k crash rust_panic`.
///
/// The message ends with a marker on its own line, which tests use to check that
/// a long formatted message reaches the console and crashlog untruncated.
#[unsafe(no_mangle)]
pub extern "C" fn rust_debug_crash_panic() -> ! {
    panic!("rust_panic test message:\n{CrashPanicFiller}\nend of rust_panic test message");
}
