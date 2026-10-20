// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Deferred Procedure Calls - queue callback to invoke on the current cpu in thread context.
//! Dpcs are executed with interrupts enabled, and do not ever migrate cpus while executing.
//! A Dpc may not execute on the original current cpu if it is hotunplugged/offlined.
//! Dpcs may block, though this may starve other queued work.

use crate::kernel::event::Event;
use crate::kernel::percpu::PerCpu;
use crate::kernel::scheduler_state::SchedulerStateBaseProfile;
use crate::kernel::thread::{
    AutoPreemptDisabler, DEFAULT_PRIORITY, HIGH_PRIORITY, ThreadLockGuard, ThreadPtr,
    create_with_priority,
};
use crate::kernel::types::{INVALID_CPU, cpu_num_t, cpu_num_to_mask};
use crate::platform_rs::timer::{DurationMono, InstantMono};
use core::ffi::c_void;
use core::fmt::Write as _;
use core::mem::{MaybeUninit, align_of, offset_of, size_of};
use core::ptr::NonNull;
use fbl::{DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode, StringBuffer};
use ksync::{KMutex, LockToken, PhantomMutex};
use pin_init::{InPlaceWrite, PinInit, pin_data, pin_init};
use zx_status::Status;
use zx_types::{ZX_MAX_NAME_LEN, zx_instant_mono_t, zx_status_t};

const DPC_THREAD_PRIORITY: i32 = HIGH_PRIORITY;

/// The Dpc thread for timers may use up to 150us out of every 300us (i.e. 50% of the CPU) in
/// the worst case. DPCs usually take only a small fraction of this and have a much lower
/// frequency than 3.333KHz.
///
/// TODO(https://fxbug.dev/42114336): Make this runtime tunable. It may be necessary to change
/// the Dpc deadline params later in boot, after configuration is loaded somehow.
const PROFILE_LOW_LATENCY_CAPACITY: DurationMono = DurationMono::from_micros(150);
/// Deadline period for the low-latency DPC worker thread.
const PROFILE_LOW_LATENCY_DEADLINE: DurationMono = DurationMono::from_micros(300);

// A single spinlock protects the data members of all DpcRunners.
//
// Take care to drop this lock before calling any code that might acquire a chainlock.
ksync::declare_singleton_spinlock!(DpcLock);

/// Callback function signature for a [`Dpc`].
pub type DpcFunc = extern "C" fn(&Dpc);

/// Deferred Procedure Call item that can be queued on a [`DpcRunner`].
///
/// `Dpc`s are executed with interrupts enabled, and do not ever migrate CPUs while executing.
/// A `Dpc` may not execute on the original current CPU if it is hotunplugged/offlined.
/// `Dpc`s may block, though this may starve other queued work.
#[derive(Debug, DoublyLinkedListContainable)]
#[repr(C)]
pub struct Dpc {
    #[dll_node]
    node: DoublyLinkedListNode<Dpc>,
    func: DpcFunc,
    arg: Option<NonNull<c_void>>,
}

zr::static_assert_size_and_align!(
    Dpc,
    object_constants_rs::kDpcStorageSize,
    object_constants_rs::kDpcStorageAlign,
);
zr::static_assert!(offset_of!(Dpc, node) == 0);
zr::static_assert!(offset_of!(Dpc, func) == 16);
zr::static_assert!(offset_of!(Dpc, arg) == 24);

impl Dpc {
    /// Creates a new `Dpc` with the given callback function `func` and optional argument `arg`.
    pub const fn new<T>(func: DpcFunc, arg: Option<NonNull<T>>) -> Self {
        Self {
            node: DoublyLinkedListNode::new(),
            func,
            arg: match arg {
                Some(ptr) => Some(ptr.cast()),
                None => None,
            },
        }
    }

    /// Returns the argument pointer cast to `Option<NonNull<T>>`.
    pub fn arg<T>(&self) -> Option<NonNull<T>> {
        self.arg.map(NonNull::cast)
    }

    /// Called by `DpcRunner`.
    fn invoke(&self) {
        (self.func)(self);
    }
}

/// `Dpc`s are placed into different queues depending on their ServiceLevel. One queue for those
/// that need to execute with [`QueueType::LowLatency`] (e.g. signaling a `TimerDispatcher` from
/// interrupt context). The other queue ([`QueueType::General`]) is for the rest.
///
/// [`QueueType::LowLatency`] `Dpc`s are executed by a thread with a high-bandwidth scheduler
/// profile so take care to avoid creating undesirable PI interactions. In other words, don't use
/// [`QueueType::LowLatency`] unless you really need to, and be sure that the `Dpc` won't hold
/// likely-to-be-contested locks for a significant amount of time. See also
/// https://fxbug.dev/395669867.
///
/// [`QueueType::General`] `Dpc`s are serviced by a thread with a default scheduler profile.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueType(pub u32);

#[allow(non_upper_case_globals)]
impl QueueType {
    /// General DPC queue serviced by a thread with a default scheduler profile.
    pub const General: Self = Self(0);
    /// Low-latency DPC queue serviced by a thread with a deadline scheduler profile.
    pub const LowLatency: Self = Self(1);
}

/// `DpcQueue` encapsulates a list of `Dpc` tasks and a thread that pops tasks off the list and
/// executes them in order.
///
/// A `DpcQueue` must be initialized (`init`) prior to use. A `DpcQueue` that's been initialized
/// must be `shutdown` before it may be reinitialized.
#[ksync::guarded]
#[pin_data]
#[repr(C)]
struct DpcQueue {
    #[mutex(DpcLock)]
    lock: KMutex<PhantomMutex>,

    /// Request the thread to stop by setting to true.
    ///
    /// This is guarded by the static global `DpcLock`.
    #[guarded_by(lock)]
    stop: bool,

    #[pin]
    #[guarded_by(lock)]
    list: DoublyLinkedList<NonNull<Dpc>>,

    /// The thread that executes DPCs queued in `list`.
    #[guarded_by(lock)]
    thread: Option<ThreadPtr>,

    #[pin]
    event: Event,
}

zr::static_assert!(size_of::<DpcQueue>() == 96);
zr::static_assert!(align_of::<DpcQueue>() == 8);
zr::static_assert!(offset_of!(DpcQueue, stop) == 0);
zr::static_assert!(offset_of!(DpcQueue, list) == 8);
zr::static_assert!(offset_of!(DpcQueue, thread) == 16);
zr::static_assert!(offset_of!(DpcQueue, event) == 24);

impl DpcQueue {
    fn new() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            lock: KMutex::new(PhantomMutex),
            stop: false.into(),
            list <- ksync::kcell_init(DoublyLinkedList::new()),
            thread: None.into(),
            event <- Event::init_unsignaled(),
        })
    }

    fn as_ffi_arg(&'static self) -> *mut c_void {
        core::ptr::from_ref(self).cast_mut().cast()
    }

    /// Converts an FFI argument back into a `'static` reference to `DpcQueue`.
    ///
    /// # Safety
    ///
    /// `arg` must have been produced by `DpcQueue::as_ffi_arg`.
    unsafe fn from_ffi_arg(arg: *mut c_void) -> &'static Self {
        let ptr: *const Self = arg.cast();
        // SAFETY: The caller guarantees `arg` was produced by `DpcQueue::as_ffi_arg` from a
        // `'static` `DpcQueue` reference.
        unsafe { &*ptr }
    }

    /// Initializes `cpu`'s queue.
    ///
    /// Creates a thread with `name_prefix` and scheduler `profile` that will service this
    /// `DpcQueue`.
    fn init(&'static self, cpu: cpu_num_t, name_prefix: &str, profile: &SchedulerStateBaseProfile) {
        let mut name = StringBuffer::<{ ZX_MAX_NAME_LEN }>::new();
        let _ = write!(name, "{name_prefix}{cpu}");
        let name_cstr = name.as_cstr().unwrap();

        extern "C" fn entry(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` was passed via `self.as_ffi_arg()` from a `'static` `DpcQueue`.
            let queue = unsafe { DpcQueue::from_ffi_arg(arg) };
            queue.do_work()
        }

        // SAFETY: `name_cstr.as_ptr()` is a valid null-terminated C string, `entry` is a valid
        // thread start routine, and `self.as_ffi_arg()` points to this `'static` `DpcQueue`.
        let thread = unsafe {
            create_with_priority(name_cstr.as_ptr(), entry, self.as_ffi_arg(), DPC_THREAD_PRIORITY)
        }
        .expect("Failed to create DPC thread");

        // SAFETY: `thread` was just created and has not been resumed or joined yet, and `init` is
        // called from normal thread context with no spinlocks or `ChainLockTransaction` held.
        unsafe {
            thread.set_base_profile(profile);
            let _ = thread.set_cpu_affinity(cpu_num_to_mask(cpu));
        }

        {
            // SAFETY: `DpcLock` is the global lock protecting all `DpcQueue` instances.
            ksync::lock!(let mut guard = unsafe { self.lock_lock(DpcLock::get()) });
            *guard.as_mut().thread_mut() = Some(thread);
        }

        // SAFETY: `thread` is a valid newly created thread that has not been resumed or joined.
        unsafe {
            thread.resume();
        }
    }

    /// Begins the shutdown process for this `DpcQueue`. After a successful `shutdown`, this
    /// `DpcQueue`'s remaining contents should be moved into elsewhere by calling
    /// `take_from_locked` on another `DpcQueue`.
    ///
    /// Signals the worker thread to terminate and waits until it has terminated or `deadline` is
    /// reached. If shutdown fails, the object is left in an inconsistent state.
    fn shutdown(&self, deadline: InstantMono) -> Result<(), Status> {
        let t;
        let event;
        {
            // SAFETY: `DpcLock` is the global lock protecting all `DpcQueue` instances.
            ksync::lock!(let mut guard = unsafe { self.lock_lock(DpcLock::get()) });
            let fields = guard.as_mut().fields_mut();

            // Ask the thread to terminate.
            debug_assert!(!*fields.stop);
            *fields.stop = true;

            // Remember this Event so we can signal it outside the spinlock.
            event = &self.event;

            // Remember the thread so we can join outside the spinlock.
            t = fields.thread.take().unwrap();
        }

        // Wake the thread and wait for it to terminate.
        let _preempt_disabled = AutoPreemptDisabler::new();
        event.signal();
        // SAFETY: `t` is the valid worker thread created in `init` and taken exclusively by
        // clearing `self.thread` under `DpcLock`, so it has not been joined yet.
        unsafe { t.join(deadline) }.map(|_| ())
    }

    /// Moves any queued `Dpc`s from `source` to `self`. May only be called after a successful
    /// `shutdown` of `source`.
    ///
    /// Returns true if any `Dpc`s were moved.
    fn take_from_locked(&self, source: &Self, token: &mut LockToken<'_, DpcLock>) -> bool {
        pin_init::stack_pin_init!(let source_list = DoublyLinkedList::new());
        let moved;
        {
            // SAFETY: `DpcLock` is the global lock protecting all `DpcQueue` instances.
            let mut source_guard = unsafe { source.guard_lock_mut(token) };
            let mut source_fields = source_guard.fields_mut();

            // The thread must have already been stopped by a call to |shutdown|.
            debug_assert!(*source_fields.stop);
            debug_assert!(source_fields.thread.is_none());

            moved = !source_fields.list.is_empty();
            source_list.as_mut().splice(source_fields.list.as_mut());

            // Reset |source|'s state so we can restart Dpc processing if its cpu comes back online.
            let _ = source.event.unsignal();
            debug_assert!(source_fields.list.is_empty());
            *source_fields.stop = false;
        }

        // Move the contents of |source.list| to the back of our |list|.
        // SAFETY: `DpcLock` is the global lock protecting all `DpcQueue` instances.
        let mut self_guard = unsafe { self.guard_lock_mut(token) };
        self_guard.list_mut().splice(source_list);

        moved
    }

    /// Enqueues `dpc` at the tail of `self.list`.
    ///
    /// # Safety
    ///
    /// `dpc` must not currently be in a container, and must remain valid, pinned in memory, and
    /// not destroyed until dequeued by the worker thread under `DpcLock`.
    unsafe fn enqueue_locked(&self, token: &mut LockToken<'_, DpcLock>, dpc: &Dpc) {
        // SAFETY: `DpcLock` is the global lock protecting all `DpcQueue` instances, and the caller
        // guarantees `dpc` is not in any list and remains valid and unmoved while linked in `list`.
        unsafe {
            let mut token_guard = self.guard_lock_mut(token);
            token_guard.list_mut().push_back_raw(NonNull::from(dpc));
        }
    }

    fn signal(&self) {
        self.event.signal();
    }

    fn do_work(&self) -> i32 {
        loop {
            // Wait for a Dpc to fire.
            let err = self.event.wait_infinite();
            debug_assert!(err.is_ok());

            let dpc_local = {
                // SAFETY: `DpcLock` is the global lock protecting all `DpcQueue` instances.
                ksync::lock!(let mut guard = unsafe { self.lock_lock(DpcLock::get()) });

                if *guard.stop() {
                    return 0;
                }

                // Pop a Dpc off our list, and make a local copy.
                let dpc = guard.as_mut().list_mut().pop_front();

                // If our list is now empty, unsignal the event so we block until it is.
                let Some(dpc) = dpc else {
                    let _ = self.event.unsignal();
                    continue;
                };

                // Copy the Dpc to the stack.
                // SAFETY: `dpc` was queued via `DpcRunner::enqueue`, whose caller guarantees that
                // the `Dpc` remains valid and unmodified until dequeued under `DpcLock`.
                let dpc_ref = unsafe { dpc.as_ref() };
                Dpc { node: DoublyLinkedListNode::new(), func: dpc_ref.func, arg: dpc_ref.arg }
            };

            // Call the Dpc.
            dpc_local.invoke();
        }
    }
}

/// A DpcRunner is responsible for running queued Dpcs. Under the hood, a given runner may
/// manage multiple independent queues of Dpcs.
///
/// Each cpu maintains a DpcRunner, in its percpu structure.
#[ksync::guarded]
#[pin_data]
#[repr(C)]
pub struct DpcRunner {
    #[mutex(DpcLock)]
    lock: KMutex<PhantomMutex>,

    /// The cpu that owns this `DpcRunner`.
    #[guarded_by(lock)]
    cpu: cpu_num_t,

    /// Whether the `DpcRunner` has been initialized for the owning cpu.
    #[guarded_by(lock)]
    initialized: bool,

    #[pin]
    queue_general: DpcQueue,
    #[pin]
    queue_low_latency: DpcQueue,
}

zr::static_assert!(size_of::<DpcRunner>() == 200);
zr::static_assert!(align_of::<DpcRunner>() == 8);
zr::static_assert!(offset_of!(DpcRunner, cpu) == 0);
zr::static_assert!(offset_of!(DpcRunner, initialized) == 4);
zr::static_assert!(offset_of!(DpcRunner, queue_general) == 8);
zr::static_assert!(offset_of!(DpcRunner, queue_low_latency) == 104);

impl DpcRunner {
    fn new() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            lock: KMutex::new(PhantomMutex),
            cpu: INVALID_CPU.into(),
            initialized: false.into(),
            queue_general <- DpcQueue::new(),
            queue_low_latency <- DpcQueue::new(),
        })
    }

    /// Initializes this DpcRunner for the current cpu.
    ///
    /// The calling thread must have hard-affinity to exactly one CPU.
    pub fn init_for_current_cpu(&'static self) {
        if cfg!(debug_assertions) {
            let current_thread = crate::kernel::thread::current_get();
            // SAFETY: `current_thread` is a valid, non-null pointer to the running `Thread`,
            // no chain lock transaction is active on this CPU, and the guard is stack-pinned
            // and dropped immediately.
            ksync::lock!(let guard = unsafe {
                ThreadLockGuard::lock(
                    current_thread,
                    crate::clt_tag!("DpcRunner::InitForCurrentCpu"),
                )
            });
            let mask = guard.scheduler_state().hard_affinity();
            debug_assert!(mask.count_ones() == 1, "mask {mask:#x}");
        }

        let cpu = crate::arch_rs::curr_cpu_num();

        {
            // SAFETY: `DpcLock` is the global lock protecting all `DpcRunner` instances.
            ksync::lock!(let mut guard = unsafe { self.lock_lock(DpcLock::get()) });
            let fields = guard.as_mut().fields_mut();

            // This cpu's DpcRunner was initialized on a previous hotplug event.
            if *fields.initialized {
                return;
            }

            debug_assert_eq!(*fields.cpu, INVALID_CPU);
            *fields.cpu = cpu;
            *fields.initialized = true;
        }

        self.queue_general.init(
            cpu,
            "dpcg-",
            &SchedulerStateBaseProfile::fair(DEFAULT_PRIORITY, true),
        );
        let profile_low_latency = SchedulerStateBaseProfile::deadline(
            PROFILE_LOW_LATENCY_CAPACITY,
            PROFILE_LOW_LATENCY_DEADLINE,
        );
        self.queue_low_latency.init(cpu, "dpcll-", &profile_low_latency);
    }

    /// Begins the DpcRunner shutdown process.
    ///
    /// Shutting down a DpcRunner is a two-phase process. This is the first phase. See
    /// |transition_off_cpu| for the second phase.
    ///
    /// This method:
    /// - tells the owning cpu's DpcRunner's thread to stop servicing its queue then
    /// - waits, up to |deadline|, for it to finish any in-progress DPC and join
    ///
    /// Because this method blocks until the thread has terminated, it is critical that the caller
    /// not hold any locks that might be needed by any previously queued DPCs. Otherwise, deadlock
    /// may occur.
    ///
    /// Upon successful completion, this DpcRunner may contain unexecuted DPCs and new ones may be
    /// added by |enqueue|. However, they will not execute (on any cpu) until |transition_off_cpu|
    /// is called.
    ///
    /// Once |shutdown| has completed successfully, finish the shutdown process by calling
    /// |transition_off_cpu| on some cpu other than the owning cpu.
    ///
    /// If |shutdown| fails, this DpcRunner is left in an undefined state and
    /// |transition_off_cpu| must not be called.
    pub fn shutdown(&self, deadline: InstantMono) -> Result<(), Status> {
        self.queue_general.shutdown(deadline)?;
        self.queue_low_latency.shutdown(deadline)
    }

    /// Moves queued Dpcs from |source| to this DpcRunner.
    ///
    /// This is the second phase of Dpc shutdown. See |shutdown|.
    ///
    /// This must only be called after |shutdown| has completed successfully.
    ///
    /// This must only be called on the current cpu.
    pub fn transition_off_cpu(&self, source: &DpcRunner) {
        let took_from_general;
        let took_from_low_latency;

        {
            ksync::lock!(let mut guard = DpcLock::get().lock());
            let token = guard.token_mut();

            // |source|'s cpu is shutting down. Assert that we are migrating to the current cpu.
            {
                // SAFETY: `DpcLock` is the global lock protecting all `DpcRunner` instances.
                let self_guard = unsafe { self.guard_lock(&*token) };
                // SAFETY: `DpcLock` is the global lock protecting all `DpcRunner` instances.
                let source_guard = unsafe { source.guard_lock(&*token) };
                debug_assert_eq!(*self_guard.cpu(), crate::arch_rs::curr_cpu_num());
                debug_assert_ne!(*self_guard.cpu(), *source_guard.cpu());
            }

            took_from_general = self.queue_general.take_from_locked(&source.queue_general, token);
            took_from_low_latency =
                self.queue_low_latency.take_from_locked(&source.queue_low_latency, token);

            // SAFETY: `DpcLock` is the global lock protecting all `DpcRunner` instances.
            let mut source_guard = unsafe { source.guard_lock_mut(token) };
            let source_fields = source_guard.fields_mut();
            *source_fields.initialized = false;
            *source_fields.cpu = INVALID_CPU;
        }

        if took_from_general {
            self.queue_general.signal();
        }
        if took_from_low_latency {
            self.queue_low_latency.signal();
        }
    }

    /// Enqueue |dpc| in the specified queue |queue_type| and signal its worker thread to execute
    /// it.
    ///
    /// |enqueue| will not block, but it may wait briefly for a spinlock.
    ///
    /// |enqueue| may return before or after the Dpc has executed. It is the caller's
    /// responsibility to ensure that a queued Dpc object is not destroyed prior to its execution.
    ///
    /// Note: it is important that the thread calling enqueue() not be holding its own lock if
    /// there is _any_ possibility that the DPC thread could be involved in a PI interaction with
    /// the thread who is performing the enqueue operation.
    ///
    /// Returns Status::ALREADY_EXISTS if |dpc| is already queued.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `dpc` remains valid, pinned in memory (not moved), and is not
    /// destroyed prior to its execution, and that its callback function and argument are safe to
    /// invoke on the DPC worker thread.
    pub unsafe fn enqueue(dpc: &Dpc, queue_type: QueueType) -> Result<(), Status> {
        let queue;
        {
            ksync::lock!(let mut guard = DpcLock::get().lock());

            if dpc.node.in_container() {
                return Err(Status::ALREADY_EXISTS);
            }

            // Select the queue.
            let runner = &PerCpu::get_current().dpc_runner;
            match queue_type {
                QueueType::General => {
                    queue = &runner.queue_general;
                }
                QueueType::LowLatency => {
                    queue = &runner.queue_low_latency;
                }
                other => {
                    panic!("unknown QueueType {}", other.0);
                }
            }

            // Put this Dpc at the tail of the list. Signal the worker outside the lock.
            // SAFETY: We checked `!dpc.in_container()` under `DpcLock`, and the caller guarantees
            // `dpc` remains valid and unmoved until dequeued.
            unsafe {
                queue.enqueue_locked(guard.token_mut(), dpc);
            }
        }

        // Signal outside of the lock to avoid lock order inversion with the thread
        // lock.
        queue.signal();
        Ok(())
    }
}

fn dpc_init(_level: init::LkInitLevel) {
    // Initialize the DpcRunner for the main cpu.
    PerCpu::get_current().dpc_runner.init_for_current_cpu();
}

init::lk_init_hook!(dpc, dpc_init, init::LK_INIT_LEVEL_THREADING);

// FFI entry points for C++ callers in `<kernel/dpc.h>`.

/// Initializes a `DpcRunner` in-place.
///
/// # Safety
///
/// `runner` must point to valid, properly aligned, uninitialized storage for a `DpcRunner` that
/// will remain pinned in memory until `rust_dpc_runner_destroy` is called.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_dpc_runner_init(runner: *mut MaybeUninit<DpcRunner>) {
    // SAFETY: The caller guarantees `runner` points to valid, pinned uninitialized storage.
    let runner = unsafe { &mut *runner };
    let _ = runner.write_pin_init(DpcRunner::new());
}

/// Destroys a `DpcRunner` in-place.
///
/// # Safety
///
/// `runner` must point to a valid, initialized `DpcRunner` that is no longer in use.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_dpc_runner_destroy(runner: *mut DpcRunner) {
    // SAFETY: The caller guarantees `runner` points to a valid initialized `DpcRunner`.
    unsafe {
        core::ptr::drop_in_place(runner);
    }
}

/// Initializes `runner` for the current CPU.
#[unsafe(no_mangle)]
pub extern "C" fn rust_dpc_runner_init_for_current_cpu(runner: &'static DpcRunner) {
    runner.init_for_current_cpu();
}

/// Shuts down `runner`, waiting up to `deadline` for its worker threads to join.
#[unsafe(no_mangle)]
pub extern "C" fn rust_dpc_runner_shutdown(
    runner: &DpcRunner,
    deadline: zx_instant_mono_t,
) -> zx_status_t {
    Status::result_into_raw(runner.shutdown(InstantMono(deadline)))
}

/// Moves queued `Dpc`s from `source` to `runner`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_dpc_runner_transition_off_cpu(runner: &DpcRunner, source: &DpcRunner) {
    runner.transition_off_cpu(source);
}

/// Enqueues `dpc` onto the current CPU's `DpcRunner` in queue `queue_type`.
///
/// # Safety
///
/// The caller must uphold the safety preconditions of [`DpcRunner::enqueue`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_dpc_runner_enqueue(dpc: &Dpc, queue_type: QueueType) -> zx_status_t {
    // SAFETY: The caller upholds the safety requirements of `DpcRunner::enqueue`.
    Status::result_into_raw(unsafe { DpcRunner::enqueue(dpc, queue_type) })
}

#[cfg(ktest)]
/// Tests of DPCs.
#[unittest::suite(name = "dpc_tests_rust")]
mod dpc_tests {
    use super::{
        AutoPreemptDisabler, Dpc, DpcRunner, Event, NonNull, PinInit, QueueType, Status, pin_data,
        pin_init,
    };
    use crate::kernel::deadline::Deadline;
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use pin_init::stack_pin_init;
    use unittest::{assert_eq, assert_err, assert_true};

    /// basic test of Dpc::Queue
    #[test]
    fn test_dpc_queue() {
        #[pin_data]
        #[repr(C)]
        struct Context {
            dpc: Dpc,
            #[pin]
            event: Event,
            expected_cpu: AtomicU32,
            dpc_started: AtomicBool,
        }

        impl Context {
            fn init() -> impl PinInit<Self, core::convert::Infallible> {
                pin_init!(&this in Self {
                    dpc: Dpc::new(Self::signal_event, Some(this)),
                    event <- Event::init_unsignaled(),
                    expected_cpu: AtomicU32::new(0),
                    dpc_started: AtomicBool::new(false),
                })
            }

            extern "C" fn signal_event(dpc: &Dpc) {
                // SAFETY: `dpc` was initialized with `Some(this)` pointing to its enclosing
                // `Context`, which remains alive until `event.wait_infinite()` returns.
                let context = unsafe { dpc.arg::<Context>().unwrap().as_ref() };
                context.dpc_started.store(true, Ordering::SeqCst);

                // DPCs allow interrupts and blocking.
                debug_assert!(!crate::arch_rs::ints_disabled());
                debug_assert!(!crate::arch_rs::blocking_disallowed());
                debug_assert_eq!(
                    context.expected_cpu.load(Ordering::SeqCst),
                    crate::arch_rs::curr_cpu_num()
                );

                context.event.signal();
            }
        }

        const NUM_DPCS: usize = 72;

        let uninit_context = kalloc::Box::<[Context]>::try_new_uninit_slice(NUM_DPCS);
        assert_true!(uninit_context.is_ok());
        let mut uninit_context = uninit_context.unwrap();
        for slot in uninit_context.iter_mut() {
            // SAFETY: `slot.as_mut_ptr()` points to valid uninitialized `Context` storage in the
            // heap allocation that will not be moved before `context` is dropped.
            let _ = unsafe { PinInit::__pinned_init(Context::init(), slot.as_mut_ptr()) };
        }
        // SAFETY: Every element in `uninit_context` was initialized above via `Context::init()`.
        let context = unsafe { uninit_context.assume_init() };

        // Init all the DPCs and supporting context.
        for item in context.iter() {
            item.dpc_started.store(false, Ordering::SeqCst);
        }

        // Fire off DPCs.
        for (i, item) in context.iter().enumerate() {
            let _preempt_disable = AutoPreemptDisabler::new();
            let int_state = crate::arch_rs::arch_interrupt_save();
            item.expected_cpu.store(crate::arch_rs::curr_cpu_num(), Ordering::SeqCst);
            // Use a mix of both general and low-latency.
            let queue_type = if (i % 2) != 0 { QueueType::General } else { QueueType::LowLatency };
            // SAFETY: `item.dpc` is pinned in the heap-allocated `context` array, which we wait on
            // via `item.event.wait_infinite()` below before dropping `context`.
            let _ = unsafe { DpcRunner::enqueue(&item.dpc, queue_type) };
            crate::arch_rs::arch_interrupt_restore(int_state);
        }
        for item in context.iter() {
            let _ = item.event.wait_infinite();
        }
    }

    /// repeatedly queue the same dpc
    // Test that it's safe to repeatedly queue up the same DPC over and over.
    #[test]
    fn test_dpc_requeue() {
        // Disable preemption to prevent the DPC worker, which is a deadline thread,
        // from immediately preempting the test thread. This also ensures that the
        // test thread remains on the same CPU as the DPC is enqueued on, otherwise
        // work stealing can move the test thread to another CPU while the DPC worker
        // executes, resulting in a race between the Dpc destructor in the test thread
        // and the DPC worker.
        let _preempt_disable = AutoPreemptDisabler::new();

        extern "C" fn increment(d: &Dpc) {
            // SAFETY: `d` was initialized with a valid pointer to `actual_count` on the stack,
            // which outlives the flush DPC below.
            unsafe { d.arg::<AtomicU64>().unwrap().as_ref() }.fetch_add(1, Ordering::SeqCst);
        }

        let actual_count = AtomicU64::new(0);
        let dpc_increment = Dpc::new(increment, Some(NonNull::from(&actual_count)));

        const NUM_ITERATIONS: u64 = 10000;
        let mut expected_count = 0u64;
        for _ in 0..NUM_ITERATIONS {
            // If we queue faster than the DPC worker thread can dequeue, the call may fail with
            // `Status::ALREADY_EXISTS`. That's OK, we just won't increment `expected_count` in
            // that case.
            // SAFETY: `dpc_increment` remains alive and unmoved until after `dpc_flush` completes
            // on the same queue and CPU (with preemption disabled).
            let status = unsafe { DpcRunner::enqueue(&dpc_increment, QueueType::General) };
            if status.is_ok() {
                expected_count += 1;
            } else {
                assert_err!(status, Status::ALREADY_EXISTS);
            }
        }

        // There might still be one DPC queued up for execution. Wait for it to "flush" the queue.
        extern "C" fn flush(d: &Dpc) {
            // SAFETY: `d` was initialized with a valid pointer to `event_flush` on the stack,
            // which outlives `event_flush.wait(...)`.
            unsafe { d.arg::<Event>().unwrap().as_ref() }.signal();
        }

        stack_pin_init!(let event_flush = Event::init_unsignaled());
        let dpc_flush = Dpc::new(flush, Some(NonNull::from(&*event_flush)));
        // SAFETY: `dpc_flush` remains alive and unmoved until `event_flush.wait(...)` returns.
        let _ = unsafe { DpcRunner::enqueue(&dpc_flush, QueueType::General) };
        let _ = event_flush.wait(&Deadline::infinite());

        assert_eq!(actual_count.load(Ordering::SeqCst), expected_count);
    }
}
