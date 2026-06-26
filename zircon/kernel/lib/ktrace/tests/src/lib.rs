// Copyright 2026 The Fuchsia Authors
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![no_std]

use ktrace_rs::KTrace;

/// Test-only FFI helper to write a single word record from Rust.
///
/// # Safety
///
/// This must be called with interrupts disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zircon_ktrace_test_rust_write(header: u64, val: u64) -> i32 {
    let ktrace = KTrace::get_instance();
    // SAFETY: The caller guarantees interrupts are disabled.
    if let Ok(mut res) = unsafe { ktrace.reserve(header) } {
        let _ = res.write_word(val);
        let _ = res.commit();
        0
    } else {
        -1
    }
}
