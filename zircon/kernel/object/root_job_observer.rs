// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{CStr, c_void};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use fbl::RefPtr;
use kalloc::Box;
use ksync::declare_singleton_mutex;
use object_constants_rs as object_constants;
use zx_status::Status;
use zx_types::{ZX_KOID_INVALID, ZX_MAX_NAME_LEN, zx_koid_t, zx_signals_t};

use super::handle::HandleRef;
use super::job_dispatcher::{JobDispatcher, ZX_JOB_NO_CHILDREN};
use super::process_dispatcher::ProcessDispatcher;
use super::root_job_observer_ffi::{
    cpp_root_job_signal_observer_destroy, cpp_root_job_signal_observer_init,
};
use crate::platform_rs::halt_token::HaltToken;
use crate::platform_rs::power::{PlatformHaltAction, ZirconCrashReason, platform_halt};
use crate::platform_rs::timer::{DurationMono, current_mono_time};

declare_singleton_mutex!(CriticalProcessNameLock);

static CRITICAL_PROCESS_NAME: ksync::KCell<[u8; ZX_MAX_NAME_LEN], CriticalProcessNameLock> =
    ksync::KCell::new([0; ZX_MAX_NAME_LEN]);
static CRITICAL_PROCESS_KOID: ksync::KCell<zx_koid_t, CriticalProcessNameLock> =
    ksync::KCell::new(ZX_KOID_INVALID);
/// Latched to `true` (with `Release`/`Acquire` ordering) when a process critical to the root job
/// begins exiting or being killed so concurrent teardown paths (such as `BusTransactionInitiator`
/// leaked-page checks) observe that system-wide critical-process teardown has started.
static CRITICAL_PROCESS_DYING: AtomicBool = AtomicBool::new(false);

/// Inline storage for C++ `RootJobSignalObserver`.
#[derive(Default)]
#[repr(C, align(8))]
struct RootJobSignalObserverStorage(
    zr::OpaqueBytes<{ object_constants::kRootJobSignalObserverSize }>,
);

zr::static_assert_size_and_align!(
    RootJobSignalObserverStorage,
    object_constants::kRootJobSignalObserverSize,
    object_constants::kRootJobSignalObserverAlign,
);

impl RootJobSignalObserverStorage {
    #[inline]
    fn get(&self) -> *mut c_void {
        self.0.get().cast()
    }
}

#[cfg(ktest)]
type Callback = Box<dyn FnMut() + Send>;

struct RootJobObserverInner {
    root_job: RefPtr<JobDispatcher>,
    signal_observer: RootJobSignalObserverStorage,
    #[cfg(ktest)]
    callback: Option<Callback>,
}

/// Observes termination of the root job and coordinates system shutdown or notifications.
pub struct RootJobObserver {
    inner: NonNull<RootJobObserverInner>,
}

// SAFETY: `RootJobObserver` uniquely owns its heap-allocated `RootJobObserverInner` and
// synchronizes teardown under `root_job`'s lock in `Drop` before deallocating `inner`.
unsafe impl Send for RootJobObserver {}

impl RootJobObserver {
    fn create(
        root_job: RefPtr<JobDispatcher>,
        root_job_handle: Option<HandleRef<'_>>,
        #[cfg(ktest)] callback: Option<Callback>,
    ) -> Result<Self, Status> {
        #[cfg(ktest)]
        let has_callback = callback.is_some();
        let inner = Box::try_new(RootJobObserverInner {
            root_job: root_job.clone(),
            signal_observer: RootJobSignalObserverStorage::default(),
            #[cfg(ktest)]
            callback,
        })
        .map_err(|_| Status::NO_MEMORY)?;
        let raw: *mut RootJobObserverInner = Box::into_raw(inner);
        #[cfg(ktest)]
        let ctx = if has_callback { raw.cast::<c_void>() } else { core::ptr::null_mut() };
        #[cfg(not(ktest))]
        let ctx = core::ptr::null_mut();

        // SAFETY: `raw` points to a valid heap-allocated `RootJobObserverInner` at a stable
        // address; raw field projection to `signal_observer` (`OpaqueBytes`) does not create a
        // whole-struct reference.
        let signal_observer_ptr = unsafe { (*raw).signal_observer.get() };
        // SAFETY: `signal_observer_ptr` points to valid writable storage of size and alignment
        // matching `RootJobSignalObserver` at a stable heap address until `Drop` unregisters it,
        // and `ctx` is either null or points to `raw` (valid until `Drop` unregisters it).
        unsafe {
            cpp_root_job_signal_observer_init(signal_observer_ptr, ctx);
        }
        let handle_ptr = root_job_handle.map_or(core::ptr::null(), |h| h.as_ptr().cast());
        // Observer registration on the root job object will always succeed.
        // Calling `add_observer` on the cloned `root_job` avoids holding any Rust reference to
        // `*raw` across `add_observer`, which may synchronously invoke
        // `rust_root_job_observer_on_match` when `root_job` currently has no children.
        //
        // SAFETY: `signal_observer_ptr` is an initialized `RootJobSignalObserver` at a stable heap
        // address, and `handle_ptr` is either null or a valid `Handle` pointer.
        let _ =
            unsafe { root_job.add_observer(signal_observer_ptr, handle_ptr, ZX_JOB_NO_CHILDREN) };
        // SAFETY: `raw` was obtained from `Box::into_raw` above and is non-null.
        Ok(Self { inner: unsafe { NonNull::new_unchecked(raw) } })
    }

    /// Create a `RootJobObserver` that halts the system when the root job terminates
    /// (i.e. asserts `ZX_JOB_NO_CHILDREN`).
    pub(super) fn new(
        root_job: RefPtr<JobDispatcher>,
        root_job_handle: Option<HandleRef<'_>>,
    ) -> Result<Self, Status> {
        Self::create(
            root_job,
            root_job_handle,
            #[cfg(ktest)]
            None,
        )
    }

    /// Creates a new `RootJobObserver` that calls `callback` when the root job asserts
    /// `ZX_JOB_NO_CHILDREN`.
    ///
    /// The callback is called while holding the watched `JobDispatcher`'s lock, so the callback
    /// must avoid calling anything that may attempt to acquire that lock again, introduce a lock
    /// cycle, etc.
    #[cfg(ktest)]
    fn new_with_callback<F: FnMut() + Send + 'static>(
        root_job: RefPtr<JobDispatcher>,
        root_job_handle: Option<HandleRef<'_>>,
        callback: F,
    ) -> Result<Self, Status> {
        let boxed = Box::try_new(callback).map_err(|_| Status::NO_MEMORY)?;
        let raw_callback: *mut (dyn FnMut() + Send) = Box::<F>::into_raw(boxed);
        // SAFETY: `raw_callback` was allocated via `Box::try_new` with the default allocator.
        let callback_box: Callback = unsafe { Box::from_raw(raw_callback) };
        Self::create(root_job, root_job_handle, Some(callback_box))
    }

    /// Handles `ZX_JOB_NO_CHILDREN` signal notifications from C++ `RootJobSignalObserver::OnMatch`.
    ///
    /// # Safety
    ///
    /// The caller must ensure this is called at most once per observer registration while holding
    /// the watched `JobDispatcher`'s lock (after erasing the observer from `observers_`), and that
    /// non-null `rust_ctx` points to the live `RootJobObserverInner` passed during initialization
    /// with no concurrent or reentrant calls to `on_match` on `rust_ctx`.
    pub(super) unsafe fn on_match(rust_ctx: *mut c_void) {
        // Remember, the root job's dispatcher lock is held for the duration of
        // this method. Take care to avoid calling anything that might attempt to
        // acquire that lock.
        #[cfg(ktest)]
        if !rust_ctx.is_null() {
            // SAFETY: `rust_ctx` points to a live `RootJobObserverInner` whose `callback` field is
            // never mutated after allocation until `Drop` completes `remove_observer` under
            // `root_job`'s lock. Projecting to `callback` only reads that disjoint field, and the
            // shared borrow ends before invoking `callback_ptr`.
            let callback_ptr = unsafe {
                (*rust_ctx.cast::<RootJobObserverInner>()).callback.as_ref().map(Box::as_ptr)
            };
            if let Some(callback_ptr) = callback_ptr {
                // SAFETY: `callback_ptr` points to the heap-allocated closure owned by this
                // observer. `OnMatch` runs at most once under `root_job`'s lock, and `Drop` cannot
                // deallocate the closure until `remove_observer` acquires and releases `root_job`'s
                // lock.
                unsafe { (*callback_ptr)() };
                return;
            }
        }
        #[cfg(not(ktest))]
        let _ = rust_ctx;
        Self::halt();
    }

    /// Halts or reboots the platform in response to root job termination.
    ///
    /// May or may not return depending on whether halt/reboot is already in progress.
    /// Once committed with the halt token, this function does not return.
    fn halt() {
        let boot_options = boot_options::BootOptions::get();
        if !HaltToken::get().take() {
            kprint::kprintln!("root-job: halt/reboot already in progress; returning");
            return;
        }
        // We now have the halt token so we're committed. There is no return from this point.

        if let Ok(notice) = CStr::from_bytes_until_nul(&boot_options.root_job_notice) {
            let notice_len = CStr::count_bytes(notice);
            if notice_len != 0 {
                kprint::kprintln!("root-job: notice: {:cs}", notice);
            }
        }

        let (action, action_name) = match boot_options.root_job_behavior {
            boot_options::RootJobBehavior::Halt => (PlatformHaltAction::Halt, "halt"),
            boot_options::RootJobBehavior::Bootloader => {
                (PlatformHaltAction::RebootBootloader, "bootloader")
            }
            boot_options::RootJobBehavior::Recovery => {
                (PlatformHaltAction::RebootRecovery, "recovery")
            }
            boot_options::RootJobBehavior::Shutdown => (PlatformHaltAction::Shutdown, "shutdown"),
            boot_options::RootJobBehavior::Reboot => (PlatformHaltAction::Reboot, "reboot"),
        };

        kprint::kprintln!("root-job: taking {:s} action", action_name);
        let dlog_deadline = current_mono_time() + DurationMono::from_seconds(5);
        let _ = crate::debuglog_rs::dlog_shutdown(dlog_deadline);
        // Does not return.
        platform_halt(action, ZirconCrashReason::UserspaceRootJobTermination);
    }

    /// Record that any critical process is in some stage of being torn down.
    pub fn set_critical_process_dying() {
        CRITICAL_PROCESS_DYING.store(true, Ordering::Release);
    }

    /// Returns whether any critical process is currently dying.
    pub fn get_critical_process_dying() -> bool {
        CRITICAL_PROCESS_DYING.load(Ordering::Acquire)
    }

    /// Record the dead process responsible for getting the root job killed.
    pub fn critical_process_kill(dead_process: &ProcessDispatcher) {
        ksync::lock!(let mut guard = CriticalProcessNameLock::lock());
        let token = guard.as_mut().token_mut();
        // SAFETY: `token` proves the lock is held.
        let current_koid = unsafe { *CRITICAL_PROCESS_KOID.get(token) };
        if current_koid == ZX_KOID_INVALID {
            let mut name = [0u8; ZX_MAX_NAME_LEN];
            let status = dead_process.get_name(&mut name);
            debug_assert!(status.is_ok());
            // SAFETY: `token` proves exclusive access to the cell.
            unsafe {
                *CRITICAL_PROCESS_NAME.get_mut(token) = name;
                *CRITICAL_PROCESS_KOID.get_mut(token) = dead_process.get_koid();
            }
        }
    }

    /// Returns the name of the critical process that caused root job termination.
    pub fn get_critical_process_name() -> [u8; ZX_MAX_NAME_LEN] {
        ksync::lock!(let guard = CriticalProcessNameLock::lock());
        let token = guard.token();
        // SAFETY: `token` proves the lock is held.
        unsafe { *CRITICAL_PROCESS_NAME.get(token) }
    }

    /// Returns the KOID of the critical process that caused root job termination.
    pub fn get_critical_process_koid() -> zx_koid_t {
        ksync::lock!(let guard = CriticalProcessNameLock::lock());
        let token = guard.token();
        // SAFETY: `token` proves the lock is held.
        unsafe { *CRITICAL_PROCESS_KOID.get(token) }
    }

    #[cfg(ktest)]
    fn reset_critical_process_state_for_test() -> SavedCriticalProcessState {
        ksync::lock!(let mut guard = CriticalProcessNameLock::lock());
        let token = guard.as_mut().token_mut();
        // SAFETY: `token` proves exclusive access to the cells.
        let (name, koid) = unsafe {
            let old_name = *CRITICAL_PROCESS_NAME.get(token);
            let old_koid = *CRITICAL_PROCESS_KOID.get(token);
            *CRITICAL_PROCESS_NAME.get_mut(token) = [0u8; ZX_MAX_NAME_LEN];
            *CRITICAL_PROCESS_KOID.get_mut(token) = ZX_KOID_INVALID;
            (old_name, old_koid)
        };
        let dying = CRITICAL_PROCESS_DYING.swap(false, Ordering::AcqRel);
        SavedCriticalProcessState { name, koid, dying }
    }
}

#[cfg(ktest)]
#[must_use]
struct SavedCriticalProcessState {
    name: [u8; ZX_MAX_NAME_LEN],
    koid: zx_koid_t,
    dying: bool,
}

#[cfg(ktest)]
impl SavedCriticalProcessState {
    fn restore(self) {
        ksync::lock!(let mut guard = CriticalProcessNameLock::lock());
        let token = guard.as_mut().token_mut();
        // SAFETY: `token` proves exclusive access to the cells.
        unsafe {
            *CRITICAL_PROCESS_NAME.get_mut(token) = self.name;
            *CRITICAL_PROCESS_KOID.get_mut(token) = self.koid;
        }
        CRITICAL_PROCESS_DYING.store(self.dying, Ordering::Release);
    }
}

impl Drop for RootJobObserver {
    fn drop(&mut self) {
        let raw = self.inner.as_ptr();
        let mut out_signals: zx_signals_t = 0;
        // SAFETY: `raw` points to a valid `RootJobObserverInner`. Raw field projections to
        // `signal_observer` and `root_job` avoid forming any whole-struct reference to `*raw`
        // while `on_match` may still be running under `root_job`'s lock on another CPU.
        // `remove_observer` acquires `root_job`'s lock, guaranteeing that any concurrent `OnMatch`
        // or `OnCancel` callback has completed before `signal_observer` is destroyed and `raw` is
        // reclaimed via `Box::from_raw`.
        unsafe {
            let signal_observer_ptr = (*raw).signal_observer.get();
            (*raw).root_job.remove_observer(signal_observer_ptr, &mut out_signals);
            cpp_root_job_signal_observer_destroy(signal_observer_ptr);
            drop(Box::from_raw(raw));
        }
    }
}

/// In-tree kernel unit tests for `RootJobObserver`.
#[cfg(ktest)]
#[unittest::suite(name = "root_job_observer")]
mod tests {
    use core::sync::atomic::{AtomicU32, Ordering};
    use fbl::RefPtr;
    use unittest::expect_true;
    use zx_status::Status;

    use super::super::handle::KernelHandle;
    use super::super::job_dispatcher::JobDispatcher;
    use super::super::process_dispatcher::{ProcessDispatcher, ProcessState};
    use super::super::thread_dispatcher::ThreadDispatcher;
    use super::RootJobObserver;
    use crate::arch_rs::UserEntryState;
    use crate::kernel::deadline::{Deadline, TimerSlack};
    use crate::kernel::event::Event;
    use crate::platform_rs::timer::DurationMono;

    /// Create a process inside the given job.
    fn create_process(parent_job: RefPtr<JobDispatcher>) -> KernelHandle<ProcessDispatcher> {
        let (proc_handle, _rights, _vmar_handle, _vmar_rights) =
            ProcessDispatcher::create(parent_job, b"unittest_process", 0)
                .expect("failed to create process");
        proc_handle
    }

    /// Create a suspended thread inside the given process.
    fn create_thread(parent_process: RefPtr<ProcessDispatcher>) -> KernelHandle<ThreadDispatcher> {
        let (thread_handle, _rights) =
            ThreadDispatcher::create(parent_process, 0, b"unittest_thread")
                .expect("failed to create thread");
        let thread = thread_handle.dispatcher();
        thread.initialize().expect("failed to initialize thread");
        thread.suspend().expect("failed to suspend thread");
        let entry = UserEntryState { pc: 0, sp: 0, arg1: 0, arg2: 0, tp: 0, abi_reg: 0 };
        thread.start(entry, true).expect("failed to start thread");
        thread_handle
    }

    struct SendPtr<T>(*const T);
    impl<T> Clone for SendPtr<T> {
        fn clone(&self) -> Self {
            *self
        }
    }
    impl<T> Copy for SendPtr<T> {}
    // SAFETY: Raw pointer wrapper used to pass pointers into test closures safely.
    unsafe impl<T> Send for SendPtr<T> {}
    unsafe impl<T> Sync for SendPtr<T> {}

    impl<T> SendPtr<T> {
        fn get(self) -> *const T {
            self.0
        }
    }

    /// Exercise basic creation/destruction of RootJobObserver::new without a callback.
    #[test]
    fn test_new_create_destroy() {
        let root_job = JobDispatcher::create_root_job();
        let _child_process = create_process(root_job.clone());

        let observer = RootJobObserver::new(root_job.clone(), None);
        expect_true!(observer.is_ok());
        drop(observer);

        expect_true!(root_job.kill(1));
    }

    /// Exercise basic creation/destruction of the RootJobObserver.
    #[test]
    fn test_create_destroy() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        let root_job = JobDispatcher::create_root_job();
        let event_ptr = SendPtr(event.as_raw());

        // Create and destroy the root job observer.
        let observer = RootJobObserver::new_with_callback(root_job, None, move || {
            let event_ptr = event_ptr;
            // SAFETY: `event_ptr` points to the pinned `event` on the caller's stack frame.
            let evt = unsafe { Event::from_raw_ref(event_ptr.get()) };
            evt.signal();
        });
        expect_true!(observer.is_ok());

        // Ensure the callback fired.
        expect_true!(event.wait_infinite().is_ok());
    }

    /// Ensure that the callback fires when the root job is killed.
    #[test]
    fn test_callback_fires_on_root_job_death() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        // Create the root job with a child process, and start watching it.
        let root_job = JobDispatcher::create_root_job();
        let _child_process = create_process(root_job.clone());
        let event_ptr = SendPtr(event.as_raw());

        let observer = RootJobObserver::new_with_callback(root_job.clone(), None, move || {
            let event_ptr = event_ptr;
            // SAFETY: `event_ptr` points to the pinned `event` on the caller's stack frame.
            let evt = unsafe { Event::from_raw_ref(event_ptr.get()) };
            evt.signal();
        });
        expect_true!(observer.is_ok());

        // Shouldn't be signalled yet.
        let deadline = Deadline::after_mono(DurationMono::from_millis(1), TimerSlack::none());
        expect_true!(event.wait(&deadline) == Err(Status::TIMED_OUT));

        // Kill the root job.
        expect_true!(root_job.kill(1));

        // Ensure we are signalled.
        expect_true!(event.wait_infinite().is_ok());
    }

    /// Test that all root job children are already terminated when the callback fires.
    #[test]
    fn test_children_already_dead_when_callback_fires() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        // Create a new root job, containing a process and a thread.
        let root_job = JobDispatcher::create_root_job();
        let child_process = create_process(root_job.clone());
        let child_thread = create_thread(child_process.dispatcher().clone());

        let event_ptr = SendPtr(event.as_raw());
        let proc_ptr = SendPtr(RefPtr::as_ptr(child_process.dispatcher()));
        let thread_ptr = SendPtr(RefPtr::as_ptr(child_thread.dispatcher()));

        // Create a root job observer. The callback ensures that the child process and thread are
        // both dead when it fires.
        let observer = RootJobObserver::new_with_callback(root_job.clone(), None, move || {
            // SAFETY: pointers are valid for the test duration.
            unsafe {
                assert!((*proc_ptr.get()).state() == ProcessState::Dead);
                assert!((*thread_ptr.get()).is_dying_or_dead());
                let evt = Event::from_raw_ref(event_ptr.get());
                evt.signal();
            }
        });
        expect_true!(observer.is_ok());

        // Ensure everything is running.
        expect_true!(child_process.dispatcher().state() == ProcessState::Running);
        expect_true!(!child_thread.dispatcher().is_dying_or_dead());

        // Kill the parent job.
        expect_true!(root_job.kill(1));

        // Wait for the callback to fire.
        expect_true!(event.wait_infinite().is_ok());
    }

    /// Ensure the callback fires when the root job has no children, even if not killed.
    #[test]
    fn test_callback_fires_when_no_children() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        // Create a new root job, containing a process and a thread.
        let root_job = JobDispatcher::create_root_job();
        let child_process = create_process(root_job.clone());
        let child_thread = create_thread(child_process.dispatcher().clone());

        let event_ptr = SendPtr(event.as_raw());
        let proc_ptr = SendPtr(RefPtr::as_ptr(child_process.dispatcher()));
        let thread_ptr = SendPtr(RefPtr::as_ptr(child_thread.dispatcher()));

        // Create a root job observer. The callback ensures that the child process and thread are
        // both dead when it fires.
        let observer = RootJobObserver::new_with_callback(root_job, None, move || {
            let (event_ptr, proc_ptr, thread_ptr) = (event_ptr, proc_ptr, thread_ptr);
            // SAFETY: `event_ptr` points to the pinned `event` on the caller's stack frame.
            unsafe {
                assert!((*proc_ptr.get()).state() == ProcessState::Dead);
                assert!((*thread_ptr.get()).is_dying_or_dead());
                let evt = Event::from_raw_ref(event_ptr.get());
                evt.signal();
            }
        });
        expect_true!(observer.is_ok());

        // Ensure everything is running.
        expect_true!(child_process.dispatcher().state() == ProcessState::Running);
        expect_true!(!child_thread.dispatcher().is_dying_or_dead());

        // Kill the process.
        child_process.dispatcher().kill(1);

        // Ensure the callback fires.
        expect_true!(event.wait_infinite().is_ok());
    }

    /// Ensure that it's safe for multiple observers to observe termination of a root job.
    #[test]
    fn test_multiple_observers_one_job() {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        COUNT.store(0, Ordering::SeqCst);

        // Create a new root job containing one process.
        let root_job = JobDispatcher::create_root_job();
        let child_process = create_process(root_job.clone());

        // Create two observers.
        let observer1 = RootJobObserver::new_with_callback(root_job.clone(), None, || {
            COUNT.fetch_add(1, Ordering::SeqCst);
        });
        let observer2 = RootJobObserver::new_with_callback(root_job, None, || {
            COUNT.fetch_add(1, Ordering::SeqCst);
        });
        expect_true!(observer1.is_ok());
        expect_true!(observer2.is_ok());

        // Kill the process.
        expect_true!(COUNT.load(Ordering::SeqCst) == 0);
        child_process.dispatcher().kill(1);

        // Ensure the callback fired twice.
        expect_true!(COUNT.load(Ordering::SeqCst) == 2);
    }

    /// Verifies critical process tracking records the dead process and ignores subsequent calls.
    #[test]
    fn test_critical_process_kill() {
        let saved = RootJobObserver::reset_critical_process_state_for_test();

        let root_job = JobDispatcher::create_root_job();
        let proc1 = create_process(root_job.clone());
        let koid1 = proc1.dispatcher().get_koid();

        // Kill process 1 and record it.
        RootJobObserver::critical_process_kill(proc1.dispatcher());
        expect_true!(RootJobObserver::get_critical_process_koid() == koid1);
        let name1 = RootJobObserver::get_critical_process_name();
        expect_true!(&name1[..16] == b"unittest_process");

        // Attempting to record a second process must not overwrite the initial dead process.
        let (proc2, _rights, _vmar, _vmar_rights) =
            ProcessDispatcher::create(root_job, b"second_process", 0)
                .expect("failed to create proc2");
        let koid2 = proc2.dispatcher().get_koid();
        expect_true!(koid1 != koid2);

        RootJobObserver::critical_process_kill(proc2.dispatcher());
        expect_true!(RootJobObserver::get_critical_process_koid() == koid1);
        let name2 = RootJobObserver::get_critical_process_name();
        expect_true!(&name2[..16] == b"unittest_process");

        saved.restore();
    }

    /// Verifies setting and getting the critical process dying state.
    #[test]
    fn test_critical_process_dying_state() {
        let saved = RootJobObserver::reset_critical_process_state_for_test();
        expect_true!(!RootJobObserver::get_critical_process_dying());
        RootJobObserver::set_critical_process_dying();
        expect_true!(RootJobObserver::get_critical_process_dying());
        saved.restore();
    }

    /// Verifies reading the critical process name and koid.
    #[test]
    fn test_critical_process_name_and_koid() {
        let saved = RootJobObserver::reset_critical_process_state_for_test();
        let koid = RootJobObserver::get_critical_process_koid();
        let name = RootJobObserver::get_critical_process_name();
        expect_true!(koid == 0);
        expect_true!(name[0] == 0);
        saved.restore();
    }
}
