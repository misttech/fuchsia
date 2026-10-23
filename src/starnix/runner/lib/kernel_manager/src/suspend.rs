// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::Kernels;
use anyhow::Error;
use fidl::Peered;
use fidl_fuchsia_starnix_runner as fstarnixrunner;
use fuchsia_inspect::{self as inspect, UintExponentialHistogramProperty, UintProperty};
use log::warn;
use starnix_sync::{
    CondVar, LockDepMutex, SuspendWorkerPoolStateLock, SuspendWorkerResultsLock,
    SuspendWorkerRunLock, WakeSourcesLock, WakeWatchersLock,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use zx::Task;

/// The signal that the kernel raises to indicate that it's awake.
pub const AWAKE_SIGNAL: zx::Signals = zx::Signals::USER_0;

/// The signal that the kernel raises to indicate that it's suspended.
pub const ASLEEP_SIGNAL: zx::Signals = zx::Signals::USER_1;

pub struct WakeSource {
    handle: zx::NullableHandle,
    name: String,
    signals: zx::Signals,
}

impl WakeSource {
    pub fn from_counter(counter: zx::Counter, name: String) -> Self {
        Self { handle: counter.into_handle(), name, signals: zx::Signals::COUNTER_POSITIVE }
    }

    pub fn from_handle(handle: zx::NullableHandle, name: String, signals: zx::Signals) -> Self {
        Self { handle, name, signals }
    }

    fn as_wait_item(&self) -> zx::WaitItem<'_> {
        self.handle.wait_item(self.signals)
    }
}

pub type WakeSources = std::collections::HashMap<zx::Koid, WakeSource>;

pub struct SuspendContext {
    pub wake_sources: Arc<LockDepMutex<WakeSources, WakeSourcesLock>>,
    pub wake_watchers: Arc<LockDepMutex<Vec<zx::EventPair>, WakeWatchersLock>>,

    /// Inspect node for suspend-related metrics.
    pub node: inspect::Node,
    /// Histogram recording the boot timeline duration (in nanoseconds) of successful container
    /// suspensions.
    pub suspend_duration_histogram: UintExponentialHistogramProperty,
    /// The total number of times the container has attempted to suspend.
    pub suspend_attempts_count: UintProperty,
    /// The total number of times the container has successfully suspended.
    pub suspend_successes_count: UintProperty,
    /// The total number of times the container has failed to suspend.
    pub suspend_failures_count: UintProperty,
}

impl Default for SuspendContext {
    fn default() -> Self {
        let inspector = inspect::component::inspector();
        let node = inspector.root().create_child("suspend");
        let suspend_duration_histogram = node.create_uint_exponential_histogram(
            "suspend_duration_boot_ns",
            inspect::ExponentialHistogramParams {
                floor: 100_000,
                initial_step: 100_000,
                step_multiplier: 2,
                buckets: 32,
            },
        );
        let suspend_attempts_count = node.create_uint("suspend_attempts_count", 0);
        let suspend_successes_count = node.create_uint("suspend_successes_count", 0);
        let suspend_failures_count = node.create_uint("suspend_failures_count", 0);
        // Create the threads used to suspend containers ahead of the first suspend.
        LazyLock::force(&SUSPEND_WORKER_POOL);
        Self {
            wake_sources: Default::default(),
            wake_watchers: Default::default(),
            node,
            suspend_duration_histogram,
            suspend_attempts_count,
            suspend_successes_count,
            suspend_failures_count,
        }
    }
}

/// Suspends the container specified by the `payload`.
pub async fn suspend_container(
    payload: fstarnixrunner::ManagerSuspendContainerRequest,
    suspend_context: &Arc<SuspendContext>,
    kernels: &Kernels,
) -> Result<
    Result<fstarnixrunner::ManagerSuspendContainerResponse, fstarnixrunner::SuspendError>,
    Error,
> {
    fuchsia_trace::duration!("power", "starnix-runner:suspending-container");
    let Some(container_job) = payload.container_job else {
        warn!(
            "error suspending container: could not find container job {:?}",
            payload.container_job
        );
        return Ok(Err(fstarnixrunner::SuspendError::SuspendFailure));
    };
    let container_job = Arc::new(container_job);

    // These handles need to kept alive until the end of the block, as they will
    // resume the kernel when dropped.
    log::info!("Suspending all container processes.");
    let _suspend_handles = match suspend_job(&container_job) {
        Ok(handles) => handles,
        Err(e) => {
            warn!("error suspending container {:?}", e);
            fuchsia_trace::instant!(
                "power",
                "starnix-runner:suspend-failed-actual",
                fuchsia_trace::Scope::Process
            );
            return Ok(Err(fstarnixrunner::SuspendError::SuspendFailure));
        }
    };
    log::info!("Finished suspending all container processes.");

    let suspend_start = zx::BootInstant::get();
    let resume_reason = {
        // Take locks in a scope that will be closed before awaiting to ensure no deadlock.
        if let Some(wake_locks) = payload.wake_locks {
            match wake_locks
                .wait_one(zx::Signals::EVENT_SIGNALED, zx::MonotonicInstant::ZERO)
                .to_result()
            {
                Ok(_) => {
                    // There were wake locks active after suspending all processes, resume
                    // and fail the suspend call.
                    warn!("error suspending container: Linux wake locks exist");
                    fuchsia_trace::instant!(
                        "power",
                        "starnix-runner:suspend-failed-with-wake-locks",
                        fuchsia_trace::Scope::Process
                    );
                    return Ok(Err(fstarnixrunner::SuspendError::WakeLocksExist));
                }
                Err(_) => {}
            };
        }

        {
            log::info!("Notifying wake watchers of container suspend.");
            let mut watchers = suspend_context.wake_watchers.lock();
            let (clear_mask, set_mask) = (AWAKE_SIGNAL, ASLEEP_SIGNAL);
            watchers.retain(|event| match event.signal_peer(clear_mask, set_mask) {
                Err(zx::Status::PEER_CLOSED) => false,
                Ok(()) => true,
                Err(e) => {
                    log::warn!("Failed to signal wake watcher of suspension: {e:?}");
                    true
                }
            });
        }
        log::info!("Pre-drop wake lease");
        kernels.drop_wake_lease(&container_job)?;
        log::info!("Post-drop wake lease");

        let wake_sources = suspend_context.wake_sources.lock();
        let mut wait_items: Vec<zx::WaitItem<'_>> =
            wake_sources.values().map(|w| w.as_wait_item()).collect();

        // TODO: We will likely have to handle a larger number of wake sources in the
        // future, at which point we may want to consider a Port-based approach. This
        // would also allow us to unblock this thread.
        let wait_result = {
            fuchsia_trace::duration!("power", "starnix-runner:waiting-on-container-wake");
            if wait_items.len() > 0 {
                log::info!("Waiting on container to receive incoming message on wake proxies");
                zx::object_wait_many(
                    &mut wait_items,
                    zx::MonotonicInstant::after(zx::Duration::from_seconds(9)),
                )
                .inspect_err(|e| {
                    warn!("error waiting for wake event {:?}", e);
                })
                .map(|_| ())
            } else {
                Ok(())
            }
        };
        log::info!("Finished waiting on container wake proxies.");

        let mut resume_reasons: Vec<String> = Vec::new();
        for (wake_source, wait_item) in wake_sources.values().zip(&wait_items) {
            if (wait_item.pending() & wait_item.waiting_for()) != zx::Signals::NONE {
                log::info!("Woke container from sleep for: {}", wake_source.name,);
                resume_reasons.push(wake_source.name.clone());
            }
        }

        if resume_reasons.is_empty() {
            match wait_result {
                // Expose the suspend timeout injected by Starnix.
                Err(zx::Status::TIMED_OUT) => Some("starnix-container-timeout".into()),
                // An error was already printed earlier. Ok(_) was always silent.
                _ => None,
            }
        } else {
            Some(resume_reasons.join(","))
        }
    };

    log::info!("Pre-acquire wake lease");
    kernels.acquire_wake_lease(&container_job).await?;
    log::info!("Post-acquire wake lease");

    log::info!("Notifying wake watchers of container wakeup.");
    let mut watchers = suspend_context.wake_watchers.lock();
    let (clear_mask, set_mask) = (ASLEEP_SIGNAL, AWAKE_SIGNAL);
    watchers.retain(|event| match event.signal_peer(clear_mask, set_mask) {
        Err(zx::Status::PEER_CLOSED) => false,
        Ok(()) => true,
        Err(e) => {
            log::warn!("Failed to signal wake watcher of wakeup: {e:?}");
            true
        }
    });

    log::info!("Returning successfully from suspend container");
    Ok(Ok(fstarnixrunner::ManagerSuspendContainerResponse {
        suspend_time: Some((zx::BootInstant::get() - suspend_start).into_nanos()),
        resume_reason,
        ..Default::default()
    }))
}

/// The maximum number of threads (including the calling thread) used to suspend the processes of
/// a container in parallel.
///
/// Suspending a container with thousands of threads is dominated by the cost of the
/// `zx_task_suspend` calls (which have to interrupt every thread of the process) and by the
/// per-thread syscalls needed to confirm that every thread is suspended. These are independent
/// across processes, so spreading them over a few CPUs reduces the latency of container suspend.
const MAX_SUSPEND_WORKERS: usize = 4;

/// The number of items (processes) up to which the suspend work is done on the calling thread
/// only, as waking up the helper threads would cost more than it saves.
const PARALLEL_SUSPEND_THRESHOLD: usize = 8;

/// The signals that indicate that a thread will not run again until it is resumed.
const THREAD_STOPPED_SIGNALS: zx::Signals =
    zx::Signals::THREAD_SUSPENDED.union(zx::Signals::THREAD_TERMINATED);

/// The suspend tokens of the processes of a suspended container.
///
/// The processes are resumed when this is dropped, which closes the suspend tokens.
struct SuspendedProcesses(Vec<zx::NullableHandle>);

impl SuspendedProcesses {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

/// Suspends the provided `zx::Job` by suspending each process in the job individually.
///
/// Returns a `SuspendedProcesses` guard holding the suspend tokens of all the suspended processes,
/// which resumes them when dropped.
///
/// Returns an error if any individual suspend failed, in which case the processes that were
/// suspended are resumed before the error is returned.
fn suspend_job(kernel_job: &Arc<zx::Job>) -> Result<SuspendedProcesses, Error> {
    // The processes that were already handled, whether they were suspended or not.
    let mut known_process_koids = std::collections::HashSet::<zx::Koid>::new();
    // Resumes the suspended processes if an error is returned.
    let mut suspended_processes = SuspendedProcesses(vec![]);
    loop {
        // Processes that could not be suspended because they no longer exist or are dying are not
        // retried: a dying process can remain in the job for an arbitrarily long time, but none of
        // its threads will run user code again.
        let new_process_koids: Vec<zx::Koid> = kernel_job
            .processes()?
            .into_iter()
            .filter(|process_koid| known_process_koids.insert(*process_koid))
            .collect();

        // Processes can be created by threads that were not suspended yet. Once every thread of
        // every known process is suspended, no new processes can appear, so we are done.
        if new_process_koids.is_empty() {
            break;
        }

        // Request the suspension of all the processes before waiting for any of their threads,
        // so that the kernel suspends the threads of all the processes concurrently.
        let job = kernel_job.clone();
        let new_suspended_processes =
            parallel_try_filter_map(new_process_koids, move |process_koid, _cancelled| {
                suspend_process(&job, *process_koid)
            })?;

        let mut processes = Vec::with_capacity(new_suspended_processes.len());
        for (process, suspend_handle) in new_suspended_processes {
            suspended_processes.0.push(suspend_handle);
            processes.push(process);
        }

        parallel_try_filter_map(processes, |process, cancelled| {
            wait_for_process_suspended(process, cancelled).map(|()| None::<()>)
        })?;
    }

    Ok(suspended_processes)
}

/// Requests the suspension of the process identified by `process_koid` in `kernel_job`.
///
/// Returns the process and its suspend handle, or `None` if the process no longer exists or is
/// dying, in which case it does not need to be suspended. Returns any other error.
fn suspend_process(
    kernel_job: &zx::Job,
    process_koid: zx::Koid,
) -> Result<Option<(zx::Process, zx::NullableHandle)>, zx::Status> {
    let process_handle = match kernel_job.get_child(&process_koid, zx::Rights::SAME_RIGHTS) {
        Ok(process_handle) => process_handle,
        Err(zx::Status::NOT_FOUND) => {
            // The process exited after the job was enumerated.
            return Ok(None);
        }
        Err(e) => {
            log::warn!("Failed to get container process {process_koid:?}: {e:?}");
            return Err(e);
        }
    };
    let process = zx::Process::from(process_handle);
    match process.suspend() {
        Ok(suspend_handle) => Ok(Some((process, suspend_handle))),
        Err(zx::Status::BAD_STATE) => {
            // The process was already dead or dying, and thus can't be suspended.
            Ok(None)
        }
        Err(e) => {
            log::warn!("Failed process suspension: {:?}", e);
            Err(e)
        }
    }
}

/// Waits until every thread of `process`, which must have been suspended, is suspended or
/// terminated.
///
/// Threads that are blocked on an exception are skipped, as they will not be suspended until the
/// exception is handled.
///
/// Gives up waiting without returning an error if `cancelled` becomes true.
fn wait_for_process_suspended(
    process: &zx::Process,
    cancelled: &AtomicBool,
) -> Result<(), zx::Status> {
    let threads = process.threads()?;
    for thread_koid in &threads {
        if cancelled.load(Ordering::Relaxed) {
            // Suspending another process failed, so the whole suspension is abandoned.
            return Ok(());
        }
        fuchsia_trace::duration!("power", "starnix-runner:suspend_kernel", "thread_koid" => *thread_koid);
        let thread_obj = match process.get_child(thread_koid, zx::Rights::SAME_RIGHTS) {
            Ok(thread_obj) => thread_obj,
            Err(zx::Status::NOT_FOUND) => {
                // The thread exited after the process was enumerated.
                continue;
            }
            Err(e) => {
                log::warn!("Failed to get container thread {thread_koid:?}: {e:?}");
                return Err(e);
            }
        };

        // Most threads are already suspended by the time they are checked, so first check without
        // blocking to avoid querying the state of each thread.
        if thread_obj
            .wait_one(THREAD_STOPPED_SIGNALS, zx::MonotonicInstant::ZERO)
            .to_result()
            .is_ok()
        {
            continue;
        }

        let mut watchdog_count = 0;
        loop {
            if let Ok(info) = thread_obj.info() {
                if let zx::ThreadState::Blocked(zx::ThreadBlockType::Exception(_)) = info.state {
                    let thread_name = thread_obj
                        .get_name()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|_| "unknown".to_string());
                    log::warn!(
                        "Thread {} (Koid: {:?}) is blocked on exception, skipping suspend wait.",
                        thread_name,
                        thread_koid
                    );
                    break;
                }
            }

            match thread_obj
                .wait_one(
                    THREAD_STOPPED_SIGNALS,
                    zx::MonotonicInstant::after(zx::Duration::from_millis(100)),
                )
                .to_result()
            {
                Err(zx::Status::TIMED_OUT) => {
                    if cancelled.load(Ordering::Relaxed) {
                        // Suspending another process failed, so the whole suspension is
                        // abandoned.
                        return Ok(());
                    }
                    watchdog_count += 1;
                    if watchdog_count == 100 || watchdog_count % 600 == 0 {
                        let process_name = process
                            .get_name()
                            .map(|n| n.to_string())
                            .unwrap_or_else(|_| "unknown".to_string());
                        let thread_name = thread_obj
                            .get_name()
                            .map(|n| n.to_string())
                            .unwrap_or_else(|_| "unknown".to_string());
                        let thread_state = thread_obj
                            .info()
                            .map(|info| format!("{:?}", info.state))
                            .unwrap_or_else(|_| "unknown".to_string());
                        log::warn!(
                            "[SUSPEND_WATCHDOG] Timeout waiting for task suspension. Thread Koid: {:?} Name: '{}', Process: '{}', State: {}, continuing to wait...",
                            thread_koid,
                            thread_name,
                            process_name,
                            thread_state
                        );
                    }
                }
                Err(e) => {
                    log::warn!("Error waiting for task suspension: {:?}", e);
                    return Err(e);
                }
                _ => break,
            }
        }
    }
    Ok(())
}

/// Calls `f` on every item of `items`, spreading the calls over up to `MAX_SUSPEND_WORKERS`
/// threads (the calling thread and helper threads of `SUSPEND_WORKER_POOL`) if there are more than
/// `PARALLEL_SUSPEND_THRESHOLD` items, and returns the `Some` values returned by `f`, in no
/// particular order.
///
/// If `f` fails for any item, no further items are processed, the `cancelled` flag passed to the
/// calls of `f` that are still running is set, and the first error is returned once all the calls
/// have returned. All the values returned by `f` are dropped in that case, mostly by the threads
/// that returned them.
fn parallel_try_filter_map<T, R, F>(items: Vec<T>, f: F) -> Result<Vec<R>, zx::Status>
where
    T: Send + Sync + 'static,
    R: Send + 'static,
    F: Fn(&T, &AtomicBool) -> Result<Option<R>, zx::Status> + Send + Sync + 'static,
{
    struct State<T, R, F> {
        items: Vec<T>,
        f: F,
        next_index: AtomicUsize,
        cancelled: AtomicBool,
        first_error: OnceLock<zx::Status>,
        results: LockDepMutex<Vec<R>, SuspendWorkerResultsLock>,
    }

    let num_items = items.len();
    let state = Arc::new(State {
        items,
        f,
        next_index: AtomicUsize::new(0),
        cancelled: AtomicBool::new(false),
        first_error: OnceLock::new(),
        results: LockDepMutex::new(Vec::new()),
    });
    let worker_state = state.clone();
    run_on_workers(num_items, move || {
        let state = &*worker_state;
        let mut results = Vec::new();
        while !state.cancelled.load(Ordering::Relaxed) {
            let Some(item) = state.items.get(state.next_index.fetch_add(1, Ordering::Relaxed))
            else {
                break;
            };
            match (state.f)(item, &state.cancelled) {
                Ok(Some(result)) => results.push(result),
                Ok(None) => {}
                Err(e) => {
                    let _ = state.first_error.set(e);
                    state.cancelled.store(true, Ordering::Relaxed);
                }
            }
        }
        // If the work was cancelled, drop the results here, so that the workers release them
        // (e.g. close the suspend tokens) in parallel.
        if !results.is_empty() && !state.cancelled.load(Ordering::Relaxed) {
            state.results.lock().append(&mut results);
        }
    });

    if let Some(e) = state.first_error.get() {
        return Err(*e);
    }
    Ok(std::mem::take(&mut *state.results.lock()))
}

/// Runs `job` on the calling thread and, if `num_items` is more than `PARALLEL_SUSPEND_THRESHOLD`,
/// on the helper threads of `SUSPEND_WORKER_POOL`, and returns once all of them have returned.
///
/// The threads running `job` are expected to share its work dynamically: a helper thread that
/// doesn't start running `job` before the calling thread is done with it doesn't run it at all.
fn run_on_workers(num_items: usize, job: impl Fn() + Send + Sync + 'static) {
    if num_items <= PARALLEL_SUSPEND_THRESHOLD || SUSPEND_WORKER_POOL.num_helpers == 0 {
        job();
    } else {
        SUSPEND_WORKER_POOL.run(Arc::new(job));
    }
}

/// A job run by the threads of a `SuspendWorkerPool`.
type SuspendWorkerJob = Arc<dyn Fn() + Send + Sync>;

/// The pool of helper threads used to suspend the processes of a container in parallel.
///
/// The helper threads are created once and kept for the lifetime of the runner, so that no thread
/// is created or destroyed while suspending a container. It is forced when the `SuspendContext` is
/// created, so that its threads are not created during the first suspend.
static SUSPEND_WORKER_POOL: LazyLock<SuspendWorkerPool> = LazyLock::new(SuspendWorkerPool::new);

/// A pool of persistent helper threads which run jobs along with the calling thread.
struct SuspendWorkerPool {
    /// Serializes the jobs, as the helper threads run one job at a time.
    run_lock: LockDepMutex<(), SuspendWorkerRunLock>,
    shared: Arc<SuspendWorkerPoolShared>,
    num_helpers: usize,
}

/// The state of a `SuspendWorkerPool` shared with its helper threads.
struct SuspendWorkerPoolShared {
    state: LockDepMutex<SuspendWorkerPoolState, SuspendWorkerPoolStateLock>,
    /// Signaled when a job is available to the helper threads.
    job_available: CondVar,
    /// Signaled when the last helper thread running a job returns from it.
    helpers_done: CondVar,
}

#[derive(Default)]
struct SuspendWorkerPoolState {
    /// The job currently being run.
    job: Option<SuspendWorkerJob>,
    /// The number of helper threads that can still start running `job`.
    unclaimed: usize,
    /// The number of helper threads running `job`.
    running: usize,
}

impl SuspendWorkerPool {
    fn new() -> Self {
        let shared = Arc::new(SuspendWorkerPoolShared {
            state: LockDepMutex::new(SuspendWorkerPoolState::default()),
            job_available: CondVar::new(),
            helpers_done: CondVar::new(),
        });
        let max_helpers = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(MAX_SUSPEND_WORKERS)
            .saturating_sub(1);
        let mut num_helpers = 0;
        for _ in 0..max_helpers {
            let shared = shared.clone();
            match std::thread::Builder::new()
                .name("suspend-worker".to_string())
                .spawn(move || shared.run_helper())
            {
                Ok(_) => num_helpers += 1,
                Err(e) => {
                    // The calling thread also runs the jobs, so this only reduces the parallelism.
                    log::warn!("Failed to spawn suspend worker: {e:?}");
                    break;
                }
            }
        }
        Self { run_lock: LockDepMutex::new(()), shared, num_helpers }
    }

    /// Runs `job` on the calling thread and on up to `self.num_helpers` helper threads, and returns
    /// once all of them have returned.
    ///
    /// Must not be called from within a job, as it would deadlock.
    fn run(&self, job: SuspendWorkerJob) {
        let _run_guard = self.run_lock.lock();
        {
            let mut state = self.shared.state.lock();
            state.job = Some(job.clone());
            state.unclaimed = self.num_helpers;
        }
        self.shared.job_available.notify_all();

        job();

        // The work is done or being finished by the helpers that started running the job, so the
        // others no longer need to run it.
        let mut state = self.shared.state.lock();
        state.unclaimed = 0;
        state.job = None;
        while state.running > 0 {
            self.shared.helpers_done.wait(&mut state);
        }
    }
}

impl SuspendWorkerPoolShared {
    /// The body of the helper threads, which run the jobs as they become available.
    fn run_helper(&self) {
        let mut state = self.state.lock();
        loop {
            if state.unclaimed == 0 {
                self.job_available.wait(&mut state);
                continue;
            }
            state.unclaimed -= 1;
            state.running += 1;
            let job = state.job.clone().expect("a job is available");
            drop(state);

            job();
            // Release the job before reporting that it returned, so that the caller of
            // `SuspendWorkerPool::run` gets back sole ownership of what the job holds.
            drop(job);

            state = self.state.lock();
            state.running -= 1;
            if state.running == 0 {
                self.helpers_done.notify_all();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a process in `job` with `num_threads` started threads, which are kept suspended by
    /// the returned suspend handles so that they never execute any code.
    fn create_process_with_suspended_threads(
        job: &zx::Job,
        num_threads: usize,
    ) -> (zx::Process, Vec<zx::Thread>, Vec<zx::NullableHandle>) {
        let (process, root_vmar) =
            job.create_child_process(zx::ProcessOptions::empty(), b"test-process").unwrap();
        let addr = root_vmar.info().unwrap().base;
        let mut threads = vec![];
        let mut thread_suspend_handles = vec![];
        for i in 0..num_threads {
            let thread = process.create_thread(b"test-thread").unwrap();
            thread_suspend_handles.push(thread.suspend().unwrap());
            if i == 0 {
                process.start(&thread, addr, addr, zx::NullableHandle::invalid(), 0).unwrap();
            } else {
                thread.start(addr, addr, 0, 0).unwrap();
            }
            threads.push(thread);
        }
        for thread in &threads {
            thread
                .wait_one(zx::Signals::THREAD_SUSPENDED, zx::MonotonicInstant::INFINITE)
                .to_result()
                .unwrap();
        }
        (process, threads, thread_suspend_handles)
    }

    fn is_suspended(thread: &zx::Thread) -> bool {
        thread
            .wait_one(zx::Signals::THREAD_SUSPENDED, zx::MonotonicInstant::ZERO)
            .to_result()
            .is_ok()
    }

    /// Whether `parallel_try_filter_map` can run calls in parallel when given more than
    /// `PARALLEL_SUSPEND_THRESHOLD` items.
    fn can_run_in_parallel() -> bool {
        SUSPEND_WORKER_POOL.num_helpers > 0
    }

    /// A child job for a test, which is killed (and waited on) when dropped, so that the test
    /// processes are killed before their suspend tokens are released. Exceptions in the job are
    /// left unhandled in its exception channel, so they never escape the job.
    struct TestJob {
        job: Arc<zx::Job>,
        _exception_channel: zx::Channel,
    }

    impl TestJob {
        fn new() -> Self {
            let job = fuchsia_runtime::job_default().create_child_job().unwrap();
            let _exception_channel = job.create_exception_channel().unwrap();
            Self { job: Arc::new(job), _exception_channel }
        }
    }

    impl std::ops::Deref for TestJob {
        type Target = Arc<zx::Job>;

        fn deref(&self) -> &Arc<zx::Job> {
            &self.job
        }
    }

    impl Drop for TestJob {
        fn drop(&mut self) {
            if let Err(e) = self.job.kill() {
                log::warn!("Failed to kill test job: {e:?}");
                return;
            }
            let _ = self.job.wait_one(zx::Signals::TASK_TERMINATED, zx::MonotonicInstant::INFINITE);
        }
    }

    // Note: These tests can't cover waiting for threads that are running when `suspend_job` is
    // called (the 100 ms wait loop), as the test can't create threads that execute code: there is
    // no executable code mapped in the test processes. The test threads are thus either already
    // suspended or blocked on an exception when `suspend_job` checks them.

    #[fuchsia::test]
    fn suspend_empty_job() {
        let job = TestJob::new();
        assert_eq!(suspend_job(&job).unwrap().len(), 0);
    }

    #[fuchsia::test]
    fn suspend_job_suspends_all_processes() {
        // More than `PARALLEL_SUSPEND_THRESHOLD`, so that the processes are suspended in parallel.
        const NUM_PROCESSES: usize = 16;
        const NUM_THREADS: usize = 4;

        let job = TestJob::new();
        let mut processes = vec![];
        let mut threads = vec![];
        let mut thread_suspend_handles = vec![];
        for _ in 0..NUM_PROCESSES {
            let (process, mut process_threads, mut process_thread_suspend_handles) =
                create_process_with_suspended_threads(&job, NUM_THREADS);
            processes.push(process);
            threads.append(&mut process_threads);
            thread_suspend_handles.append(&mut process_thread_suspend_handles);
        }

        let suspend_handles = suspend_job(&job).unwrap();
        assert_eq!(suspend_handles.len(), NUM_PROCESSES);

        // The threads must stay suspended once only the handles returned by `suspend_job` remain.
        drop(thread_suspend_handles);
        for thread in &threads {
            assert!(is_suspended(thread));
        }

        // Kill the processes before resuming them, as their threads can't execute any code.
        drop(job);
        drop(suspend_handles);
    }

    #[fuchsia::test]
    fn suspend_job_skips_threads_blocked_in_exception() {
        // The test job leaves exceptions unhandled, so the faulting thread stays blocked on the
        // exception.
        let job = TestJob::new();

        // A process with a thread that faults immediately, since nothing is mapped in the process.
        let (faulting_process, root_vmar) =
            job.create_child_process(zx::ProcessOptions::empty(), b"faulting-process").unwrap();
        let addr = root_vmar.info().unwrap().base;
        let faulting_thread = faulting_process.create_thread(b"faulting-thread").unwrap();
        faulting_process
            .start(&faulting_thread, addr, addr, zx::NullableHandle::invalid(), 0)
            .unwrap();
        while !matches!(
            faulting_thread.info().unwrap().state,
            zx::ThreadState::Blocked(zx::ThreadBlockType::Exception(_))
        ) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let (_process, threads, thread_suspend_handles) =
            create_process_with_suspended_threads(&job, 2);

        let suspend_handles = suspend_job(&job).unwrap();
        assert_eq!(suspend_handles.len(), 2);
        assert!(!is_suspended(&faulting_thread));
        drop(thread_suspend_handles);
        for thread in &threads {
            assert!(is_suspended(thread));
        }

        drop(job);
        drop(suspend_handles);
    }

    #[fuchsia::test]
    fn parallel_try_filter_map_runs_few_items_on_calling_thread() {
        let items: Vec<usize> = (0..PARALLEL_SUSPEND_THRESHOLD).collect();
        let calling_thread = std::thread::current().id();
        let results = parallel_try_filter_map(items.clone(), move |item, _cancelled| {
            assert_eq!(std::thread::current().id(), calling_thread);
            Ok(Some(*item))
        })
        .unwrap();
        // A single worker processes the items in order.
        assert_eq!(results, items);
    }

    #[fuchsia::test]
    fn parallel_try_filter_map_runs_many_items_on_helper_threads() {
        if !can_run_in_parallel() {
            return;
        }
        // Run the function twice, as the helper threads are reused across calls.
        for _ in 0..2 {
            // More than `PARALLEL_SUSPEND_THRESHOLD` items, so that the items are processed in
            // parallel.
            let items: Vec<usize> = (0..=PARALLEL_SUSPEND_THRESHOLD).collect();
            let ran_on_helper = Arc::new(AtomicBool::new(false));
            let ran_on_helper_clone = ran_on_helper.clone();
            parallel_try_filter_map(items, move |item, _cancelled| {
                if std::thread::current().name() == Some("suspend-worker") {
                    ran_on_helper_clone.store(true, Ordering::Relaxed);
                } else if *item == 0 {
                    // If the calling thread picked up item 0, hold it until a helper thread runs
                    // a call, which it does on one of the remaining items.
                    while !ran_on_helper_clone.load(Ordering::Relaxed) {
                        std::thread::yield_now();
                    }
                }
                Ok(None::<()>)
            })
            .unwrap();
            assert!(ran_on_helper.load(Ordering::Relaxed));
        }
    }

    #[fuchsia::test]
    fn parallel_try_filter_map_returns_all_values() {
        let items: Vec<usize> = (0..100).collect();
        let mut results = parallel_try_filter_map(items, |item, cancelled| {
            assert!(!cancelled.load(Ordering::Relaxed));
            Ok((item % 2 == 0).then_some(*item))
        })
        .unwrap();
        results.sort();
        assert_eq!(results, (0..100).step_by(2).collect::<Vec<_>>());
    }

    #[fuchsia::test]
    fn parallel_try_filter_map_handles_no_items() {
        let items: Vec<usize> = vec![];
        let results = parallel_try_filter_map(items, |item, _cancelled| Ok(Some(*item))).unwrap();
        assert!(results.is_empty());
    }

    #[fuchsia::test]
    fn parallel_try_filter_map_returns_first_error() {
        let items: Vec<usize> = (0..100).collect();
        let result = parallel_try_filter_map(items, |item, _cancelled| {
            if *item == 10 { Err(zx::Status::INTERNAL) } else { Ok(Some(*item)) }
        });
        assert_eq!(result, Err(zx::Status::INTERNAL));
    }

    #[fuchsia::test]
    fn parallel_try_filter_map_stops_after_error() {
        let items: Vec<usize> = (0..100).collect();
        let processed = Arc::new(LockDepMutex::<_, SuspendWorkerResultsLock>::new(vec![]));
        let processed_clone = processed.clone();
        let result = parallel_try_filter_map(items, move |item, _cancelled| {
            processed_clone.lock().push(*item);
            // Fail every item, so that no worker can process more than one item.
            Err::<Option<()>, _>(zx::Status::INTERNAL)
        });
        assert_eq!(result, Err(zx::Status::INTERNAL));
        let processed = processed.lock();
        assert!(!processed.is_empty());
        assert!(processed.len() <= MAX_SUSPEND_WORKERS);
    }

    #[fuchsia::test]
    fn parallel_try_filter_map_cancels_running_calls_on_error() {
        if !can_run_in_parallel() {
            return;
        }
        // Items are handed out in order, so the call for item 0 starts first. It only fails once
        // the call for another item is running, which must then be cancelled. There are more than
        // `PARALLEL_SUSPEND_THRESHOLD` items so that the items are processed in parallel.
        let items: Vec<usize> = (0..=PARALLEL_SUSPEND_THRESHOLD).collect();
        let other_call_started = Arc::new(AtomicBool::new(false));
        let other_call_cancelled = Arc::new(AtomicBool::new(false));
        let (started, cancelled_clone) = (other_call_started.clone(), other_call_cancelled.clone());
        let result = parallel_try_filter_map(items, move |item, cancelled| {
            if *item == 0 {
                while !started.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                return Err(zx::Status::INTERNAL);
            }
            started.store(true, Ordering::Relaxed);
            // Wait until the failure of the call for item 0 cancels this call.
            while !cancelled.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            cancelled_clone.store(true, Ordering::Relaxed);
            Ok(Some(*item))
        });
        assert_eq!(result, Err(zx::Status::INTERNAL));
        assert!(other_call_cancelled.load(Ordering::Relaxed));
    }
}
