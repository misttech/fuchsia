// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Spinning thread console command (`k spinner`).

use core::ffi::{c_int, c_void};
use kprint::kprintln;
use zx_status::Status;

use crate::console::CmdArgs;
use crate::kernel::thread::create_with_priority;

extern "C" fn spinner_thread(_arg: *mut c_void) -> i32 {
    loop {
        // SAFETY: Empty inline assembly has no side effects and prevents loop removal.
        unsafe {
            core::arch::asm!("");
        }
    }
}

/// This is the source of the `k spinner` command.
///
/// # Safety
///
/// The caller must ensure `argv` is valid for reads of `argc` initialized `CmdArgs` elements for
/// the duration of the call.
pub(super) unsafe extern "C" fn spinner(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc < 1 || argv.is_null() {
        return -1;
    }
    // SAFETY: `argc >= 1` and `argv` is non-null; the console dispatcher guarantees `argv` points
    // to `argc` valid `CmdArgs` elements.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    if argc < 2 {
        kprintln!("not enough args");
        kprintln!("usage: {:s} <priority>", args[0].as_str());
        return -1;
    }

    // SAFETY: `c"spinner"` is a valid NUL-terminated string, and `spinner_thread` ignores its
    // argument.
    let Ok(t) = (unsafe {
        create_with_priority(
            c"spinner".as_ptr(),
            spinner_thread,
            core::ptr::null_mut(),
            args[1].arg_uint as i32,
        )
    }) else {
        return Status::NO_MEMORY.into_raw();
    };

    // SAFETY: `t` was just created above and has not yet been resumed or detached.
    let _ = unsafe { t.detach_and_resume() };

    0
}
