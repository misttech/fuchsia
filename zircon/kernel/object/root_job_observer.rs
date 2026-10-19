// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{CStr, c_void};
use core::marker::PhantomPinned;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use fbl::{RefPtr, UniquePtr};
use kalloc::Box;
use ksync::declare_singleton_mutex;
use object_constants_rs as object_constants;
use zr::ToMutPtr;
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
    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.0.to_mut_ptr().cast()
    }
}

type Callback = Box<dyn FnMut() + Send>;

/// Observes termination of the root job and coordinates system shutdown or notifications.
#[derive(fbl::Recyclable)]
#[repr(C, align(8))]
pub struct RootJobObserver {
    root_job: RefPtr<JobDispatcher>,
    signal_observer: RootJobSignalObserverStorage,
    #[cfg(ktest)]
    callback: Option<Callback>,
    #[cfg(not(ktest))]
    _reserved: [u8; core::mem::size_of::<Option<Callback>>()],
    _pin: PhantomPinned,
}

// SAFETY: RootJobObserver can be moved across threads safely.
unsafe impl Send for RootJobObserver {}

zr::static_assert_size_and_align!(
    RootJobObserver,
    object_constants::kRootJobObserverStorageSize,
    object_constants::kRootJobObserverStorageAlign,
);

impl RootJobObserver {
    /// Initializes a `RootJobObserver` in the provided `storage` that monitors `root_job`.
    ///
    /// When the root job asserts `ZX_JOB_NO_CHILDREN`, the observer takes action to halt
    /// or reboot the system.
    ///
    /// # Safety
    ///
    /// - `storage` must be non-null, properly aligned to `kRootJobObserverStorageAlign`,
    ///   and point to valid, uninitialized memory of at least `kRootJobObserverStorageSize` bytes.
    /// - The memory pointed to by `storage` must not be moved while initialized.
    /// - The instance in `storage` must be dropped in-place via `core::ptr::drop_in_place` before
    ///   freeing or reusing the memory.
    unsafe fn init_in_storage(
        storage: *mut MaybeUninit<Self>,
        root_job: RefPtr<JobDispatcher>,
        root_job_handle: Option<HandleRef<'_>>,
    ) {
        // SAFETY: Caller guarantees `storage` is non-null, aligned, and valid for writes.
        let observer = unsafe {
            (*storage).write(Self {
                root_job,
                signal_observer: RootJobSignalObserverStorage::default(),
                #[cfg(ktest)]
                callback: None,
                #[cfg(not(ktest))]
                _reserved: Default::default(),
                _pin: PhantomPinned,
            })
        };
        let observer_ptr: *mut Self = observer;
        let signal_observer_ptr = observer.signal_observer.as_mut_ptr();
        // SAFETY: Passing null context creates a SignalObserver that delivers null to
        // `rust_root_job_observer_on_match`.
        unsafe {
            cpp_root_job_signal_observer_init(signal_observer_ptr, core::ptr::null_mut());
        }
        let handle_ptr = root_job_handle.map_or(core::ptr::null(), |h| h.as_ptr());
        // Observer registration on the root job object will always succeed.
        // SAFETY: `signal_observer` is a valid `SignalObserver` and `handle_ptr` is a valid handle
        // pointer or null.
        let _ = unsafe {
            (*observer_ptr).root_job.add_observer(
                signal_observer_ptr,
                handle_ptr,
                ZX_JOB_NO_CHILDREN,
            )
        };
    }

    /// Create a `RootJobObserver` that halts the system when the root job terminates
    /// (i.e. asserts `ZX_JOB_NO_CHILDREN`).
    pub fn new(
        root_job: RefPtr<JobDispatcher>,
        root_job_handle: Option<HandleRef<'_>>,
    ) -> Result<Pin<UniquePtr<Self>>, Status> {
        let mut observer = Box::<Self>::try_new_uninit().map_err(|_| Status::NO_MEMORY)?;
        // SAFETY: `observer` is a valid, properly aligned, uninitialized heap allocation that
        // is pinned below and will not move before being dropped in-place by `UniquePtr::drop`.
        unsafe {
            Self::init_in_storage(&mut *observer, root_job, root_job_handle);
            let raw = Box::into_raw(observer.assume_init());
            Ok(Pin::new_unchecked(UniquePtr::from_raw(raw)))
        }
    }

    /// Creates a new `RootJobObserver` that calls `callback` when the root job asserts
    /// `ZX_JOB_NO_CHILDREN`.
    ///
    /// The callback is called while holding the watched `JobDispatcher`'s lock, so the callback
    /// must avoid calling anything that may attempt to acquire that lock again, introduce a lock
    /// cycle, etc.
    ///
    /// Exposed for testing.
    #[cfg(ktest)]
    pub fn new_with_callback<F: FnMut() + Send + 'static>(
        root_job: RefPtr<JobDispatcher>,
        root_job_handle: Option<HandleRef<'_>>,
        callback: F,
    ) -> Result<Pin<UniquePtr<Self>>, Status> {
        let boxed = Box::try_new(callback).map_err(|_| Status::NO_MEMORY)?;
        let raw_callback: *mut (dyn FnMut() + Send) = Box::<F>::into_raw(boxed);
        // SAFETY: raw_callback was allocated via Box::try_new.
        let callback_box: Callback = unsafe { Box::from_raw(raw_callback) };

        let mut observer = UniquePtr::try_new(Self {
            root_job,
            signal_observer: RootJobSignalObserverStorage::default(),
            callback: Some(callback_box),
            _pin: PhantomPinned,
        })
        .map_err(|_| Status::NO_MEMORY)?;
        let observer_ptr: *mut Self = UniquePtr::as_mut_ptr(&mut observer);
        let ctx = observer_ptr as *mut c_void;
        // SAFETY: observer_ptr is valid and points to the heap-allocated RootJobObserver.
        let signal_observer_ptr = unsafe { (*observer_ptr).signal_observer.as_mut_ptr() };
        // SAFETY: `observer` is boxed and has a stable heap address; `ctx` points to live
        // `RootJobObserver`.
        unsafe {
            cpp_root_job_signal_observer_init(signal_observer_ptr, ctx);
        }
        let handle_ptr = root_job_handle.map_or(core::ptr::null(), |h| h.as_ptr());
        // SAFETY: `signal_observer` is a valid `SignalObserver` and `handle_ptr` is a valid handle
        // pointer or null.
        let _ = unsafe {
            observer.root_job.add_observer(signal_observer_ptr, handle_ptr, ZX_JOB_NO_CHILDREN)
        };
        // SAFETY: `observer` is heap-allocated and will not be moved out of the pinned UniquePtr.
        Ok(unsafe { Pin::new_unchecked(observer) })
    }

    /// Halts or reboots the platform in response to root job termination.
    ///
    /// May or may not return depending on whether halt/reboot is already in progress.
    /// Once committed with the halt token, this function does not return.
    pub fn halt() {
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
        let mut out_signals: zx_signals_t = 0;
        // SAFETY: self.signal_observer was registered with self.root_job.
        unsafe {
            self.root_job.remove_observer(self.signal_observer.as_mut_ptr(), &mut out_signals);
            cpp_root_job_signal_observer_destroy(self.signal_observer.as_mut_ptr());
        }
    }
}

// Trampolines exported to C++:

/// Called by C++ `SignalObserver::OnMatch` under `JobDispatcher` lock.
///
/// # Safety
///
/// `rust_ctx` must point to a live `RootJobObserver` if non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_root_job_observer_on_match(
    rust_ctx: *mut c_void,
    _signals: zx_signals_t,
) {
    // Remember, the root job's dispatcher lock is held for the duration of
    // this method.  Take care to avoid calling anything that might attempt to
    // acquire that lock.
    #[cfg(ktest)]
    if !rust_ctx.is_null() {
        // SAFETY: rust_ctx is guaranteed to point to a live RootJobObserver and this on_match
        // callback is the only thing accessing the callback field.
        let callback = unsafe { &mut (*(rust_ctx as *mut RootJobObserver)).callback };
        if let Some(callback) = callback.as_mut() {
            callback();
            return;
        }
    }
    #[cfg(not(ktest))]
    let _ = rust_ctx;
    RootJobObserver::halt();
}

/// Called by C++ `SignalObserver::OnCancel` under `JobDispatcher` lock.
///
/// # Safety
///
/// `rust_ctx` is the context pointer passed during observer creation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_root_job_observer_on_cancel(
    _rust_ctx: *mut c_void,
    _signals: zx_signals_t,
) {
}

/// Destroys a `RootJobObserver` initialized in `storage`.
///
/// # Safety
///
/// `storage` must point to an initialized `RootJobObserver`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_root_job_observer_destroy(storage: *mut RootJobObserver) {
    // SAFETY: storage points to a live RootJobObserver.
    unsafe {
        core::ptr::drop_in_place(storage);
    }
}

/// Notifies Rust that a critical process is dying.
#[unsafe(no_mangle)]
pub extern "C" fn rust_root_job_observer_set_critical_process_dying() {
    RootJobObserver::set_critical_process_dying();
}

/// Checks whether any critical process is dying.
#[unsafe(no_mangle)]
pub extern "C" fn rust_root_job_observer_get_critical_process_dying() -> bool {
    RootJobObserver::get_critical_process_dying()
}

/// Records the dead process causing root job kill.
#[unsafe(no_mangle)]
pub extern "C" fn rust_root_job_observer_critical_process_kill(dead_process: &ProcessDispatcher) {
    RootJobObserver::critical_process_kill(dead_process);
}

/// Retrieves the critical process name.
///
/// # Safety
///
/// `out_name` must point to a buffer of at least `ZX_MAX_NAME_LEN` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_root_job_observer_get_critical_process_name(out_name: *mut u8) {
    let name = RootJobObserver::get_critical_process_name();
    // SAFETY: caller guarantees `out_name` has room for `ZX_MAX_NAME_LEN` bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), out_name, ZX_MAX_NAME_LEN);
    }
}

/// Retrieves the critical process KOID.
#[unsafe(no_mangle)]
pub extern "C" fn rust_root_job_observer_get_critical_process_koid() -> zx_koid_t {
    RootJobObserver::get_critical_process_koid()
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
    use super::super::process_dispatcher::ProcessDispatcher;
    use super::super::root_job_observer_ffi::{
        cpp_test_process_is_dead, cpp_test_process_is_running, cpp_test_thread_is_dying_or_dead,
    };
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
        let proc_ptr = SendPtr((&**child_process.dispatcher()) as *const ProcessDispatcher);
        let thread_ptr = SendPtr((&**child_thread.dispatcher()) as *const ThreadDispatcher);

        // Create a root job observer. The callback ensures that the child process and thread are
        // both dead when it fires.
        let observer = RootJobObserver::new_with_callback(root_job.clone(), None, move || {
            // SAFETY: pointers are valid for the test duration.
            unsafe {
                assert!(cpp_test_process_is_dead(proc_ptr.get()));
                assert!(cpp_test_thread_is_dying_or_dead(thread_ptr.get()));
                let evt = Event::from_raw_ref(event_ptr.get());
                evt.signal();
            }
        });
        expect_true!(observer.is_ok());

        // Ensure everything is running.
        // SAFETY: pointers are valid.
        unsafe {
            expect_true!(cpp_test_process_is_running(proc_ptr.get()));
            expect_true!(!cpp_test_thread_is_dying_or_dead(thread_ptr.get()));
        }

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
        let proc_ptr = SendPtr((&**child_process.dispatcher()) as *const ProcessDispatcher);
        let thread_ptr = SendPtr((&**child_thread.dispatcher()) as *const ThreadDispatcher);

        // Create a root job observer. The callback ensures that the child process and thread are
        // both dead when it fires.
        let observer = RootJobObserver::new_with_callback(root_job, None, move || {
            let (event_ptr, proc_ptr, thread_ptr) = (event_ptr, proc_ptr, thread_ptr);
            // SAFETY: `event_ptr` points to the pinned `event` on the caller's stack frame.
            unsafe {
                assert!(cpp_test_process_is_dead(proc_ptr.get()));
                assert!(cpp_test_thread_is_dying_or_dead(thread_ptr.get()));
                let evt = Event::from_raw_ref(event_ptr.get());
                evt.signal();
            }
        });
        expect_true!(observer.is_ok());

        // Ensure everything is running.
        // SAFETY: pointers are valid.
        unsafe {
            expect_true!(cpp_test_process_is_running(proc_ptr.get()));
            expect_true!(!cpp_test_thread_is_dying_or_dead(thread_ptr.get()));
        }

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

    /// Exercise init_in_storage and destruction.
    #[test]
    fn test_init_in_storage() {
        let root_job = JobDispatcher::create_root_job();
        let _child_process = create_process(root_job.clone());
        let mut storage = core::mem::MaybeUninit::<RootJobObserver>::uninit();
        // SAFETY: storage is properly allocated on the stack with matching size and alignment.
        unsafe { RootJobObserver::init_in_storage(&mut storage, root_job, None) };
        // SAFETY: storage was initialized successfully.
        unsafe {
            core::ptr::drop_in_place(storage.as_mut_ptr());
        }
    }
}
