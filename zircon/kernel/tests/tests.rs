// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Interactive kernel test and benchmark console commands.

mod cache_tests;
mod clock_tests;
mod fibo;
mod mem_tests;
mod owned_wait_queue_topology;
mod sleep_tests;
mod spinner;
mod uart_tests;

use core::ffi::c_int;

use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel_tests::clock_tests::clock_tests;
use crate::kernel_tests::fibo::fibo;
use crate::kernel_tests::sleep_tests::sleep_tests;
use crate::kernel_tests::spinner::spinner;
use crate::kernel_tests::uart_tests::uart_tests;

unsafe extern "C" {
    /// Runs miscellaneous kernel benchmarks.
    ///
    /// # Safety
    ///
    /// `argv` must be valid for reads of `argc` initialized `CmdArgs` entries for the duration of
    /// the call.
    fn benchmarks(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int;

    /// Runs kernel timer diagnostics.
    ///
    /// # Safety
    ///
    /// `argv` must be valid for reads of `argc` initialized `CmdArgs` entries for the duration of
    /// the call.
    fn timer_diag(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int;

    /// Runs kernel timer stress tests.
    ///
    /// # Safety
    ///
    /// `argv` must be valid for reads of `argc` initialized `CmdArgs` entries for the duration of
    /// the call.
    fn timer_stress(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int;
}

static_command!(
    CLOCK_TESTS_CMD,
    c"clock_tests".as_ptr(),
    c"test clocks".as_ptr(),
    clock_tests,
    CMD_AVAIL_NORMAL
);
static_command!(
    SLEEP_TESTS_CMD,
    c"sleep_tests".as_ptr(),
    c"tests sleep".as_ptr(),
    sleep_tests,
    CMD_AVAIL_NORMAL
);
static_command!(
    BENCH_CMD,
    c"bench".as_ptr(),
    c"miscellaneous benchmarks".as_ptr(),
    benchmarks,
    CMD_AVAIL_NORMAL
);
static_command!(FIBO_CMD, c"fibo".as_ptr(), c"threaded fibonacci".as_ptr(), fibo, CMD_AVAIL_NORMAL);
static_command!(
    SPINNER_CMD,
    c"spinner".as_ptr(),
    c"create a spinning thread".as_ptr(),
    spinner,
    CMD_AVAIL_NORMAL
);
static_command!(
    TIMER_DIAG_CMD,
    c"timer_diag".as_ptr(),
    c"prints timer diagnostics".as_ptr(),
    timer_diag,
    CMD_AVAIL_NORMAL
);
static_command!(
    TIMER_STRESS_CMD,
    c"timer_stress".as_ptr(),
    c"runs a timer stress test".as_ptr(),
    timer_stress,
    CMD_AVAIL_NORMAL
);
static_command!(
    UART_TESTS_CMD,
    c"uart_tests".as_ptr(),
    c"tests uart Tx".as_ptr(),
    uart_tests,
    CMD_AVAIL_NORMAL
);
