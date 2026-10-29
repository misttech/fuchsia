// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use zx_types::zx_signals_t;

use super::process_dispatcher::ProcessDispatcher;
use super::root_job_observer::RootJobObserver;

unsafe extern "C" {
    /// Initializes a C++ `RootJobSignalObserver` in inline `storage` attached to `rust_ctx`.
    ///
    /// # Safety
    ///
    /// The caller must ensure `storage` points to writable memory of at least
    /// `kRootJobSignalObserverSize` bytes aligned to `kRootJobSignalObserverAlign` that remains at
    /// a stable address while registered with a `Dispatcher`, and that non-null `rust_ctx` points
    /// to a live `RootJobObserverInner` until the observer is unregistered.
    pub(super) fn cpp_root_job_signal_observer_init(storage: *mut c_void, rust_ctx: *mut c_void);

    /// Destroys a C++ `RootJobSignalObserver`.
    ///
    /// # Safety
    ///
    /// The caller must ensure `observer` points to an initialized `RootJobSignalObserver` that has
    /// already been unregistered from any `Dispatcher` and is not destroyed or used again.
    pub(super) fn cpp_root_job_signal_observer_destroy(observer: *mut c_void);
}

/// Called by C++ `RootJobSignalObserver::OnMatch` under `JobDispatcher` lock.
///
/// # Safety
///
/// The caller must ensure this is called at most once per observer registration while holding the
/// watched `JobDispatcher`'s lock (after erasing the observer from `observers_`), and that non-null
/// `rust_ctx` points to the live `RootJobObserverInner` passed to
/// `cpp_root_job_signal_observer_init` with no concurrent or reentrant `OnMatch` calls on
/// `rust_ctx`.
// SAFETY: `rust_root_job_observer_on_match` is a unique global symbol matching the `extern "C"`
// declaration in `//zircon/kernel/object/root_job_observer_ffi.cc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_root_job_observer_on_match(
    rust_ctx: *mut c_void,
    _signals: zx_signals_t,
) {
    // SAFETY: Caller upholds all `RootJobObserver::on_match` preconditions.
    unsafe {
        RootJobObserver::on_match(rust_ctx);
    }
}

/// Called by C++ `RootJobSignalObserver::OnCancel` under `JobDispatcher` lock.
///
/// # Safety
///
/// The caller must ensure `rust_ctx` is the context pointer passed during observer creation.
// SAFETY: `rust_root_job_observer_on_cancel` is a unique global symbol matching the `extern "C"`
// declaration in `//zircon/kernel/object/root_job_observer_ffi.cc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_root_job_observer_on_cancel(
    _rust_ctx: *mut c_void,
    _signals: zx_signals_t,
) {
}

/// Notifies Rust that a critical process is dying.
///
/// TODO(adanis): Remove once remaining C++ callers (`process_dispatcher.cc`) are ported to Rust.
// SAFETY: `rust_root_job_observer_set_critical_process_dying` is a unique global symbol matching
// the `extern "C"` declaration in `//zircon/kernel/object/include/object/root_job_observer.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_root_job_observer_set_critical_process_dying() {
    RootJobObserver::set_critical_process_dying();
}

/// Checks whether any critical process is dying.
///
/// TODO(adanis): Remove once remaining C++ callers (`bti.cc`) are ported to Rust.
// SAFETY: `rust_root_job_observer_get_critical_process_dying` is a unique global symbol matching
// the `extern "C"` declaration in `//zircon/kernel/object/include/object/root_job_observer.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_root_job_observer_get_critical_process_dying() -> bool {
    RootJobObserver::get_critical_process_dying()
}

/// Records the dead process causing root job kill.
///
/// TODO(adanis): Remove once remaining C++ callers (`job_dispatcher.cc`, `process_dispatcher.cc`)
/// are ported to Rust.
// SAFETY: `rust_root_job_observer_critical_process_kill` is a unique global symbol matching the
// `extern "C"` declaration in `//zircon/kernel/object/include/object/root_job_observer.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_root_job_observer_critical_process_kill(dead_process: &ProcessDispatcher) {
    RootJobObserver::critical_process_kill(dead_process);
}
