// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Thread sleep accuracy console command.

use core::ffi::c_int;
use kprint::kprintln;

use crate::console::CmdArgs;
use crate::kernel::thread::sleep_relative;
use crate::platform_rs::timer::{DurationMono, current_mono_time};

/// Tests that `sleep_relative` and `current_mono_time()` are consistent.
fn thread_sleep_test() -> c_int {
    let mut early = 0;
    for _ in 0..5 {
        let now = current_mono_time();
        let _ = sleep_relative(DurationMono::from_millis(500));
        let actual_delay = current_mono_time() - now;
        if actual_delay < DurationMono::from_millis(500) {
            early = 1;
            kprintln!(
                "Thread::Current::SleepRelative(ZX_MSEC(500)) returned after {} ns",
                actual_delay.into_nanos()
            );
        }
    }
    early
}

/// # Safety
///
/// `_argv` is not dereferenced; `unsafe` only to match the console command callback signature.
pub(super) unsafe extern "C" fn sleep_tests(
    _argc: c_int,
    _argv: *const CmdArgs,
    _flags: u32,
) -> c_int {
    let _ = thread_sleep_test();
    0
}
