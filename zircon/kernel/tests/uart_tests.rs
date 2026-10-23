// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! UART blocking and non-blocking transmit console test command.

use core::ffi::c_int;
use kprint::{kformat, kprintln};
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};

use crate::console::CmdArgs;
use crate::debuglog_rs::{dlog_serial_write, serial_write};

unsafe extern "C" {
    fn cpp_global_prng_draw(buffer: *mut u8, len: usize);
}

/// Shared scratch buffer for formatting UART test lines.
///
/// Guarded by a `ksync::KMutex` so the 1,048-byte scratch buffer can remain in static storage off
/// the 8 KB kernel stack without `static mut` aliasing undefined behavior if `k uart_tests` is
/// invoked concurrently.
#[ksync::guarded]
struct UartTestBuffer {
    /// Mutex serializing access to `buf`.
    #[mutex]
    lock: ksync::KMutex,
    /// Scratch line buffer.
    #[guarded_by(lock)]
    buf: [u8; 1048],
}

/// Static UART test line buffer, guarded by `UartTestBuffer::lock` to keep the 1,048-byte buffer
/// off the kernel stack while preventing concurrent `static mut` aliasing.
static BUF: UartTestBuffer = UartTestBuffer {
    lock: ksync::KMutex::new(ksync::RawMutex::const_init(
        <UartTestBufferLockClass as ksync::LockClass>::ID,
    )),
    buf: ksync::KCell::new([0u8; 1048]),
};

fn uart_blocking_print_test() {
    ksync::lock!(let mut guard = BUF.lock_lock());
    let buf = guard.as_mut().buf_mut();
    for count in 0..5 {
        let line = kformat!(
            buf,
            "Blocking Test Count {} (FIRST LINE)\n\
             AND THIS SHOULD BE THE SECOND LINE Count {}\n",
            count,
            count
        );
        dlog_serial_write(line);
    }
}

fn uart_nonblocking_print_test() {
    ksync::lock!(let mut guard = BUF.lock_lock());
    let buf = guard.as_mut().buf_mut();
    for count in 0..5 {
        let line = kformat!(
            buf,
            "NON-Blocking Test Count {} (FIRST LINE)\n\
             AND THIS SHOULD BE THE SECOND LINE Count {}\n",
            count,
            count
        );
        serial_write(line);
    }
}

fn uart_print_lots_of_lines(block: bool) {
    const TOTAL_LINES: usize = 1024;
    const MAX_SINGLE_LINE_LEN: i32 = 128;
    const MIN_SINGLE_LINE_LEN: i32 = 80;

    let mut seed = [0u8; 32];
    // SAFETY: `cpp_global_prng_draw` writes `seed.len()` bytes into `seed`.
    unsafe {
        cpp_global_prng_draw(seed.as_mut_ptr(), seed.len());
    }
    let mut rng = SmallRng::from_seed(seed);

    ksync::lock!(let mut guard = BUF.lock_lock());
    let buf = guard.as_mut().buf_mut();
    for count in 0..TOTAL_LINES {
        let len = if block {
            kformat!(&mut buf[..1024], "BLOCK LINE: {} ", count).len()
        } else {
            kformat!(&mut buf[..1024], "NON-BLOCK LINE: {} ", count).len()
        };
        // Pick a random line length between 80 and 128
        let mut i = rng.random_range(MIN_SINGLE_LINE_LEN..MAX_SINGLE_LINE_LEN);
        i -= len as i32; // Less what we've already snprintf'ed above
        let mut s = len;
        while i > 0 {
            i -= 1;
            if rng.random_ratio(1, 2) {
                buf[s] = b'a' + rng.random_range(0u8..26);
            } else {
                buf[s] = b'A' + rng.random_range(0u8..26);
            }
            s += 1;
        }
        buf[s] = b'\n';
        s += 1;
        if block {
            dlog_serial_write(&buf[..s]);
        } else {
            serial_write(&buf[..s]);
        }
    }
}

/// # Safety
///
/// `_argv` is not dereferenced; `unsafe` only to match the console command callback signature.
pub(super) unsafe extern "C" fn uart_tests(
    _argc: c_int,
    _argv: *const CmdArgs,
    _flags: u32,
) -> c_int {
    // Print out a few short lines, to test '\n' behavior
    uart_blocking_print_test();
    uart_nonblocking_print_test();
    // Print a few long lines to test bulk output,
    // both block and non-blocking
    let mut i = 100;

    while i > 0 {
        i -= 1;
        uart_print_lots_of_lines(true);
        uart_print_lots_of_lines(false);
        kprintln!("Printed Count = {}", 100 - i);
    }

    0
}
