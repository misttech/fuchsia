// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;

unsafe extern "C" {
    /// Initializes a C++ `RootJobSignalObserver` in inline `storage` attached to `rust_ctx`.
    ///
    /// # Safety
    ///
    /// `storage` must point to valid memory of at least `kRootJobSignalObserverSize` bytes
    /// aligned to `kRootJobSignalObserverAlign`. `rust_ctx` must point to a live `RootJobObserver`
    /// or be null.
    pub(crate) fn cpp_root_job_signal_observer_init(storage: *mut c_void, rust_ctx: *mut c_void);

    /// Destroys a C++ `RootJobSignalObserver`.
    ///
    /// # Safety
    ///
    /// `observer` must point to an initialized `RootJobSignalObserver`.
    pub(crate) fn cpp_root_job_signal_observer_destroy(observer: *mut c_void);

    /// Starts the singleton `RootJobObserver` watching the root job.
    ///
    /// # Safety
    ///
    /// Must be called once during userboot initialization after the root job has at least one child.
    pub(crate) fn cpp_start_root_job_observer();
}

/// Starts the `RootJobObserver`. Must be called after the root job has at least one child process
/// or child job.
pub fn start_root_job_observer() {
    // SAFETY: `cpp_start_root_job_observer` starts the root job observer during userboot init.
    unsafe { cpp_start_root_job_observer() }
}
