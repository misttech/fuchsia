// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
use core::pin::Pin;
use core::ptr::NonNull;
use executor_bindings as bindings;
use pin_init::pin_data;

use super::handle::HandleOwner;
use super::job_dispatcher::JobDispatcher;
use super::memory_watchdog::MemoryWatchdog;
use super::root_job_observer::RootJobObserver;

/// An `Executor` encapsulates the kernel state necessary to implement the Zircon system calls. It
/// depends on an interface from the kernel below it, presenting primitives like threads and wait
/// queues. It presents an interface to the system call implementations.
///
/// The goals of factoring this into such a layer include:
///
/// - The ability to test code in this layer separately from low-level kernel implementation
///   details, and from the syscall mechanism. This includes correctness as well as performance
///   tests.
///
/// - Centralize resource management in order to make progress on things like not reporting
///   `ZX_ERR_NO_MEMORY` when creating a `zx::event`, or reporting bad handle faults.
///
/// TODO(kulakowski) The above comment is aspirational. So far, only the root job (and its observer)
/// is managed by the `Executor`. Other subsystems, like port arenas and handle arenas, are not yet
/// included. And e.g. tests are not yet written against the `Executor`.
#[pin_data]
#[repr(C)]
pub struct Executor {
    /// All jobs and processes of this `Executor` are rooted at this job.
    root_job: UnsafeCell<Option<fbl::RefPtr<JobDispatcher>>>,
    root_job_handle: UnsafeCell<Option<HandleOwner>>,

    /// Watch the root job, taking action (such as a system reboot) if it ends up
    /// with no children.
    root_job_observer: UnsafeCell<Option<Pin<fbl::UniquePtr<RootJobObserver>>>>,

    /// The memory watchdog for this `Executor`. When it observes low memory
    /// conditions, it notifies the root job of this executor.
    #[pin]
    memory_watchdog: MemoryWatchdog,
}

// SAFETY: `root_job` and `root_job_handle` are initialized once in `init` before any concurrent
// access and are read-only afterwards; `root_job_observer` is initialized once in
// `start_root_job_observer` and is not accessed concurrently; `memory_watchdog` is `Sync` and
// `Send`.
unsafe impl Sync for Executor {}
// SAFETY: `Executor` can be safely transferred across threads under the same invariants as `Sync`.
unsafe impl Send for Executor {}

// Compile-time layout assertions against the C++ Executor type via bindgen.
zr::static_assert!(core::mem::size_of::<Executor>() == core::mem::size_of::<bindings::Executor>());
zr::static_assert!(
    core::mem::align_of::<Executor>() == core::mem::align_of::<bindings::Executor>()
);
zr::static_assert!(
    core::mem::offset_of!(Executor, root_job)
        == core::mem::offset_of!(bindings::Executor, root_job_)
);
zr::static_assert!(
    core::mem::offset_of!(Executor, root_job_handle)
        == core::mem::offset_of!(bindings::Executor, root_job_handle_)
);
zr::static_assert!(
    core::mem::offset_of!(Executor, root_job_observer)
        == core::mem::offset_of!(bindings::Executor, root_job_observer_)
);
zr::static_assert!(
    core::mem::offset_of!(Executor, memory_watchdog)
        == core::mem::offset_of!(bindings::Executor, memory_watchdog_)
);

impl Executor {
    /// Initializes the executor by creating the root job and its handle.
    pub fn init(&self) {
        // Create root job.
        let root_job = JobDispatcher::create_root_job();

        // Create handle.
        let root_job_handle =
            HandleOwner::make_from_ref(root_job.clone(), JobDispatcher::default_rights());
        assert!(root_job_handle.is_some());

        // SAFETY: `init` is called once during boot before any other methods or threads access
        // `self`.
        unsafe {
            *self.root_job_handle.get() = root_job_handle;
            *self.root_job.get() = Some(root_job);
        }
    }

    /// Returns a reference to the root job dispatcher.
    pub fn get_root_job_dispatcher(&self) -> &fbl::RefPtr<JobDispatcher> {
        // SAFETY: `root_job` is initialized once in `init` before any calls to this method and is
        // not modified afterwards.
        unsafe { (*self.root_job.get()).as_ref().unwrap() }
    }

    /// Start watching the root job, taking a system-level action (such as restart) if
    /// all its children are removed.
    ///
    /// This must be called after the root job has at least one child process or child job.
    pub fn start_root_job_observer(&self) {
        // SAFETY: `start_root_job_observer` is called once during boot after `init`, and is the
        // only place that accesses `root_job_observer`. `root_job` and `root_job_handle` are not
        // modified after `init`.
        unsafe {
            assert!((*self.root_job_observer.get()).is_none());
            debug_assert!((*self.root_job.get()).is_some());

            let root_job = (*self.root_job.get()).as_ref().unwrap().clone();
            let root_job_handle = (*self.root_job_handle.get()).as_ref().map(|h| h.as_ref());
            let Ok(observer) = RootJobObserver::new(root_job, root_job_handle) else {
                panic!("root-job: failed to allocate observer\n");
            };
            *self.root_job_observer.get() = Some(observer);
        }

        // Initialize the memory watchdog.
        let self_ptr = NonNull::from(self);
        // SAFETY: `self_ptr` points to `self`, which outlives `self.memory_watchdog`.
        unsafe {
            self.memory_watchdog.init(self_ptr);
        }
    }
}

/// Initializes `executor`.
///
/// # Safety
///
/// `executor` must point to a valid `Executor`, and `rust_executor_init` must only be called once
/// before any other methods on `executor`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_executor_init(executor: *mut Executor) {
    // SAFETY: `executor` points to a valid `Executor`.
    let executor = unsafe { &*executor };
    executor.init();
}

/// Starts the root job observer and initializes the memory watchdog on `executor`.
///
/// # Safety
///
/// `executor` must point to a valid `Executor` that has been initialized via `rust_executor_init`
/// and will remain valid for the lifetime of the memory watchdog, and this function must only be
/// called once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_executor_start_root_job_observer(executor: *mut Executor) {
    // SAFETY: `executor` points to a valid `Executor`.
    let executor = unsafe { &*executor };
    executor.start_root_job_observer();
}
