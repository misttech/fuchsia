// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{c_int, c_void};

unsafe extern "C" {
    /// Runs the MSR benchmark on `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid, initialized `BenchmarkState` instance.
    pub(super) fn cpp_x86_benchmark_state_run(state: *mut c_void) -> c_int;

    /// Destroys `state` in place, joining any benchmark threads and freeing `cpu_contexts_`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid, initialized `BenchmarkState` instance that will not be used
    /// after this call.
    pub(super) fn cpp_x86_benchmark_state_destroy(state: *mut c_void);
}
