// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::fmt;
use pin_init::{PinInit, pin_data, pin_init};

use super::handle::{HandleOwner, HandleRef};
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
pub(super) struct Executor {
    /// All jobs and processes of this `Executor` are rooted at this job.
    root_job: fbl::RefPtr<JobDispatcher>,

    /// Immortal singleton handle for `root_job`.
    root_job_handle: HandleOwner,

    /// Watch the root job, taking action (such as a system reboot) if it ends up
    /// with no children.
    root_job_observer: UnsafeCell<Option<RootJobObserver>>,

    /// The memory watchdog for this `Executor`. When it observes low memory
    /// conditions, it notifies the root job of this executor.
    #[pin]
    memory_watchdog: MemoryWatchdog,
}

impl fmt::Debug for Executor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Executor")
            .field("root_job_handle", &self.root_job_handle)
            .field("memory_watchdog", &self.memory_watchdog)
            .finish_non_exhaustive()
    }
}

// SAFETY: `root_job` is `Send + Sync`; `root_job_handle` is immutable after construction and
// shared reads of `Handle` are thread-safe; `root_job_observer` is initialized at most once in
// `unsafe fn start_root_job_observer` during boot and is never accessed concurrently;
// `memory_watchdog` is `Send + Sync`.
unsafe impl Sync for Executor {}
// SAFETY: `Executor` can be safely transferred across threads under the same invariants as `Sync`.
unsafe impl Send for Executor {}

impl Executor {
    /// Creates an in-place initializer for a new `Executor`, creating the root job and its handle.
    pub(super) fn new() -> impl PinInit<Self, Infallible> {
        // Create root job.
        let root_job = JobDispatcher::create_root_job();

        // Create handle.
        let root_job_handle =
            HandleOwner::make_from_ref(root_job.clone(), JobDispatcher::default_rights())
                .expect("root-job: failed to allocate handle");

        pin_init!(Self {
            root_job,
            root_job_handle,
            root_job_observer: UnsafeCell::new(None),
            memory_watchdog <- MemoryWatchdog::new(),
        })
    }

    /// Returns a reference to the root job dispatcher.
    #[inline]
    pub(super) fn get_root_job_dispatcher(&self) -> &fbl::RefPtr<JobDispatcher> {
        &self.root_job
    }

    /// Returns a reference to the root job handle.
    #[inline]
    pub(super) fn get_root_job_handle(&self) -> HandleRef<'_> {
        self.root_job_handle.as_ref()
    }

    /// Returns a reference to the memory watchdog for this executor.
    #[inline]
    pub(super) fn get_memory_watchdog(&self) -> &MemoryWatchdog {
        &self.memory_watchdog
    }

    /// Starts watching the root job, taking a system-level action (such as restart) if all its
    /// children are removed, and initializes the memory watchdog.
    ///
    /// This must be called after the root job has at least one child process or child job.
    ///
    /// # Safety
    ///
    /// The caller must ensure `self` is pinned in `'static` storage and that this method is
    /// called at most once during boot before userspace threads start.
    pub(super) unsafe fn start_root_job_observer(&'static self) {
        // SAFETY: The caller guarantees `start_root_job_observer` is called at most once during
        // boot without concurrent calls, giving exclusive access to `self.root_job_observer`.
        unsafe {
            assert!((*self.root_job_observer.get()).is_none());
        }

        let observer =
            RootJobObserver::new(self.root_job.clone(), Some(self.root_job_handle.as_ref()))
                .expect("root-job: failed to allocate observer");

        // SAFETY: Exclusive one-time boot access to `self.root_job_observer` is guaranteed by the
        // caller's `# Safety` precondition.
        unsafe {
            *self.root_job_observer.get() = Some(observer);
        }

        // Initialize the memory watchdog.
        // SAFETY: The caller guarantees `self` (and therefore `self.memory_watchdog`) is pinned in
        // `'static` storage and that `start_root_job_observer` runs at most once during boot.
        unsafe {
            self.memory_watchdog.init(self);
        }
    }
}
