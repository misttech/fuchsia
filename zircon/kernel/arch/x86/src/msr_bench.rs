// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{c_int, c_void};
use kalloc::Box;
use kprint::kprintln;
use zr::OpaqueBytes;

use super::msr_bench_ffi::{cpp_x86_benchmark_state_destroy, cpp_x86_benchmark_state_run};
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};

#[repr(C, align(8))]
#[derive(Default)]
pub(super) struct BenchmarkState(OpaqueBytes<56>);

impl BenchmarkState {
    fn run(&self) -> c_int {
        // SAFETY: `self` wraps `OpaqueBytes<56>` (`UnsafeCell`) and is a valid, zero-initialized
        // `BenchmarkState` matching C++ `constexpr BenchmarkState() = default`.
        unsafe {
            cpp_x86_benchmark_state_run(core::ptr::from_ref(self).cast_mut().cast::<c_void>())
        }
    }
}

impl Drop for BenchmarkState {
    fn drop(&mut self) {
        // SAFETY: `self` is a valid `BenchmarkState` being dropped.
        unsafe { cpp_x86_benchmark_state_destroy(core::ptr::from_mut(self).cast::<c_void>()) };
    }
}

zr::static_assert!(core::mem::size_of::<BenchmarkState>() == 56);
zr::static_assert!(core::mem::align_of::<BenchmarkState>() == 8);

/// # Safety
///
/// Nothing: `argv` is never dereferenced. The function is `unsafe` only to match the console
/// command signature.
unsafe extern "C" fn msr_bench(_argc: c_int, _argv: *const CmdArgs, _flags: u32) -> c_int {
    let Ok(benchmark) = Box::try_new(BenchmarkState::default()) else {
        kprintln!("Failed to allocate benchmark context!");
        return -1;
    };

    benchmark.run()
}

static_command!(
    CMD_MSR_BENCH,
    c"msr_bench".as_ptr(),
    c"MSR bechmarks".as_ptr(),
    msr_bench,
    CMD_AVAIL_NORMAL
);
