// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_char;
use core::fmt::{self, Write};
use core::panic::PanicInfo;

use crate::platform_rs::power::{
    PlatformHaltAction, ZirconCrashReason, platform_halt, platform_panic_start,
};

/// Printed before the panic message, matching the [`super::debug::panic_start`] banner.
const PANIC_HEADER: &str = "\n*** KERNEL PANIC (Rust):\n*** ";

/// Written after the panic message: once to end it, and once more to separate it
/// from the stack trace, matching [`super::debug::panic_finish`].
const NEWLINE: &str = "\n";

unsafe extern "C" {
    fn cpp_crashlog_panic_write(data: *const c_char, len: usize);
}

/// A `core::fmt::Write` sink for the crashlog's `stdout_panic_buffer`, which
/// sends everything to both the console and the persistent panic buffer.
///
/// Formatted output is streamed straight through piece by piece, so the panic
/// handler needs no local buffer and never truncates. This matters: the handler
/// runs on whatever stack the panicking thread had left, which for a kernel
/// thread is 8 KiB total and may be nearly exhausted -- a stack overflow
/// arrives here too.
struct PanicWriter;

impl Write for PanicWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        // SAFETY: `text` is a valid, initialized slice of `text.len()` bytes that
        // outlives the call, and the callee copies the bytes out without
        // retaining the pointer.
        unsafe { cpp_crashlog_panic_write(text.as_ptr().cast::<c_char>(), text.len()) };
        Ok(())
    }
}

#[panic_handler]
fn rust_panic(info: &PanicInfo<'_>) -> ! {
    platform_panic_start();

    let mut writer = PanicWriter;

    // The results below are ignored because `PanicWriter::write_str()` never
    // fails.
    let _ = writer.write_str(PANIC_HEADER);
    // `PanicMessage`'s `Display` writes a literal message directly and renders a
    // formatted one (integer overflow, bounds checks, `assert_eq!`, ...)
    // piecewise into `writer`.
    let message = info.message();
    let _ = match info.location() {
        Some(location) => write!(writer, "{}:{}: {message}", location.file(), location.line()),
        None => write!(writer, "{message}"),
    };

    // End the panic message, then add one more newline between the panic
    // message and the stack trace. Rust panic messages conventionally have no
    // trailing newline, so this isn't checked for.
    let _ = writer.write_str(NEWLINE);
    let _ = writer.write_str(NEWLINE);

    platform_halt(PlatformHaltAction::Halt, ZirconCrashReason::Panic)
}
