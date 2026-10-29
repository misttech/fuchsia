// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Initialization code for the `kernel/object` module and singleton instances.

use core::pin::Pin;
use lazy_init::LazyInit;

use super::executor::Executor;
use super::handle::{Handle, HandleRef};
use super::job_dispatcher::JobDispatcher;
use super::memory_watchdog::MemoryWatchdog;

/// Global singleton executor instance.
static EXECUTOR: LazyInit<Executor> = LazyInit::uninit();

/// Returns the job that is the ancestor of all other tasks.
#[inline]
pub fn get_root_job_dispatcher() -> &'static fbl::RefPtr<JobDispatcher> {
    EXECUTOR.get_root_job_dispatcher()
}

/// Returns a reference to the immortal singleton root job handle.
#[inline]
pub fn get_root_job_handle() -> HandleRef<'static> {
    EXECUTOR.get_root_job_handle()
}

/// Returns a reference to the global [`MemoryWatchdog`].
#[inline]
pub fn get_memory_watchdog() -> &'static MemoryWatchdog {
    EXECUTOR.get_memory_watchdog()
}

/// Starts the [`RootJobObserver`](super::RootJobObserver) and initializes the global
/// [`MemoryWatchdog`].
///
/// Must be called after the root job has at least one child process or child job.
///
/// # Safety
///
/// The caller must ensure this function is called at most once during boot after `object_init` has
/// initialized `EXECUTOR` and before userspace threads start.
pub unsafe fn start_root_job_observer() {
    // SAFETY: `EXECUTOR` is pinned in `'static` storage by `object_init`, and the caller upholds
    // the single-call boot-time precondition before userspace threads start.
    unsafe {
        EXECUTOR.start_root_job_observer();
    }
}

/// Initializes the `kernel/object` singletons at `LK_INIT_LEVEL_THREADING`.
fn object_init(_level: init::LkInitLevel) {
    Handle::init();

    // SAFETY: `object_init` runs once during single-threaded boot at `LK_INIT_LEVEL_THREADING`
    // before any accesses to `EXECUTOR`.
    unsafe {
        Pin::static_ref(&EXECUTOR).init_pin(Executor::new()).unwrap();
    }
}

init::lk_init_hook!(libobject, object_init, init::LK_INIT_LEVEL_THREADING);

/// Returns a raw pointer to the singleton root job dispatcher for C++ `GetRootJobDispatcher`.
///
/// TODO(adanis): Remove once remaining C++ callers of `GetRootJobDispatcher` are ported to Rust.
// SAFETY: `rust_get_root_job_dispatcher` is a unique global symbol matching the `extern "C"`
// declaration in `//zircon/kernel/object/include/object/job_dispatcher.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_get_root_job_dispatcher() -> *mut JobDispatcher {
    fbl::RefPtr::as_ptr(get_root_job_dispatcher()).cast_mut()
}
