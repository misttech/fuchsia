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
    CondVar, LockDepMutex, SuspendHandlesLock, SuspendWorkerPoolStateLock,
    SuspendWorkerResultsLock, SuspendWorkerRunLock, WakeSourcesLock, WakeWatchersLock,
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
        // Create the threads used to suspend and resume containers ahead of the first suspend.
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

fn signal_wake_watchers(
    suspend_context: &SuspendContext,
    clear_mask: zx::Signals,
    set_mask: zx::Signals,
) {
    let mut watchers = suspend_context.wake_watchers.lock();
    watchers.retain(|event| match event.signal_peer(clear_mask, set_mask) {
        Err(zx::Status::PEER_CLOSED) => false,
        Ok(()) => true,
        Err(e) => {
            log::warn!("Failed to signal wake watcher ({set_mask:?}): {e:?}");
            true
        }
    });
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

    // These handles keep the container processes frozen until dropped.
    log::info!("Suspending all container processes.");
    let suspend_handles = match suspend_job(&container_job) {
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

    log::info!("Notifying wake watchers of container suspend.");
    signal_wake_watchers(suspend_context, AWAKE_SIGNAL, ASLEEP_SIGNAL);
    // Restore wake watchers to `AWAKE_SIGNAL` once `acquire_wake_lease` completes (preserving the
    // wake-lease baton handoff), or if an early error/cancellation exits `suspend_container`.
    let wake_watchers_awake_guard = scopeguard::guard(suspend_context, |suspend_context| {
        log::info!("Notifying wake watchers of container wakeup.");
        signal_wake_watchers(suspend_context, ASLEEP_SIGNAL, AWAKE_SIGNAL);
    });

    log::info!("Pre-drop wake lease");
    kernels.drop_wake_lease(&container_job)?;
    log::info!("Post-drop wake lease");

    let resume_reason = {
        // Hold `wake_sources` only while waiting and scanning `wait_items`, so the terminal
        // `WakeSourcesLock` is released before dropping `suspend_handles` or awaiting.
        let wake_sources = suspend_context.wake_sources.lock();
        let mut wait_items: Vec<zx::WaitItem<'_>> =
            wake_sources.values().map(|w| w.as_wait_item()).collect();

        // TODO: We will likely have to handle a larger number of wake sources in the
        // future, at which point we may want to consider a Port-based approach. This
        // would also allow us to unblock this thread.
        let wait_result = {
            fuchsia_trace::duration!("power", "starnix-runner:waiting-on-container-wake");
            if !wait_items.is_empty() {
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

    // Thaw the container processes immediately on wake (and on timeout/abort paths) before
    // awaiting `acquire_wake_lease`, so Starnix input relay and Android wakeup run concurrently
    // with Power Broker storage/peripheral resume while SAG's resume control lease and the wake
    // source hold `ExecutionState` active.
    std::mem::drop(suspend_handles);

    log::info!("Pre-acquire wake lease");
    kernels.acquire_wake_lease(&container_job).await?;
    log::info!("Post-acquire wake lease");

    std::mem::drop(wake_watchers_awake_guard);

    log::info!("Returning successfully from suspend container");
    Ok(Ok(fstarnixrunner::ManagerSuspendContainerResponse {
        suspend_time: Some((zx::BootInstant::get() - suspend_start).into_nanos()),
        resume_reason,
        ..Default::default()
    }))
}

/// The maximum number of worker threads used to suspend and resume the processes of a container in
/// parallel.
///
/// Suspending a container with thousands of threads is dominated by the cost of the
/// `zx_task_suspend` calls (which have to interrupt every thread of the process) and by the
/// per-thread syscalls needed to confirm that every thread is suspended, and resuming it by the
/// cost of closing the suspend tokens (which wakes every thread of the process). These are
/// independent across processes, so spreading them over a few CPUs reduces the latency of
/// container suspend and resume.
const MAX_SUSPEND_WORKERS: usize = 4;

/// The number of items (processes) up to which the suspend and resume work is done on the calling
/// thread only, as waking up the worker threads would cost more than it saves.
const PARALLEL_SUSPEND_THRESHOLD: usize = 8;

/// The number of suspend tokens that a worker closes at a time when resuming processes in
/// parallel.
///
/// The number of threads per process varies widely (e.g. system_server has hundreds), and the
/// workers can contend with freshly woken threads, so the tokens are handed out in small chunks to
/// balance the work dynamically across the workers.
const RESUME_CHUNK_SIZE: usize = 4;

/// Thread name assigned to persistent `SUSPEND_WORKER_POOL` threads.
const SUSPEND_WORKER_THREAD_NAME: &str = "starnix-suspend-worker";

/// Zircon scheduler role applied to `SUSPEND_WORKER_POOL` threads so freshly thawed deadline
/// threads cannot preempt the workers mid-close.
const SUSPEND_WORKER_ROLE_NAME: &str = "fuchsia.starnix.runner.suspend_worker";

/// The signals that indicate that a thread will not run again until it is resumed.
const THREAD_STOPPED_SIGNALS: zx::Signals =
    zx::Signals::THREAD_SUSPENDED.union(zx::Signals::THREAD_TERMINATED);

/// The suspend tokens of the processes of a suspended container.
///
/// The processes are resumed when this is dropped, on all the paths out of `suspend_container`
/// (including early returns and cancellation), and on the error paths of `suspend_job`.
struct SuspendedProcesses(Vec<zx::NullableHandle>);

impl SuspendedProcesses {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

impl Drop for SuspendedProcesses {
    fn drop(&mut self) {
        resume_processes(std::mem::take(&mut self.0));
    }
}

/// Resumes the processes by closing their `suspend_handles`, spreading the work over up to
/// `MAX_SUSPEND_WORKERS` threads, in chunks of `RESUME_CHUNK_SIZE` handles, if there are more than
/// `PARALLEL_SUSPEND_THRESHOLD` processes.
///
/// Closing the last handle to a suspend token resumes every thread of its process from within the
/// `zx_handle_close` call, which takes ~100 us for a typical container process, so resuming a
/// large container one process at a time takes milliseconds.
///
/// The handles are closed one at a time with `zx_handle_close` rather than in batches with
/// `zx_handle_close_many`: `HandleTable::remove_handles` (used by `zx_handle_close_many`) releases
/// the handles, and thus resumes the processes, while holding the write lock of the handle table
/// of the runner. Resuming processes with `zx_handle_close_many` would thus serialize the workers,
/// and block the other threads of the runner (e.g. the pager threads that serve the page faults of
/// the resumed threads) from using handles for milliseconds. `zx_handle_close` resumes the process
/// after releasing the lock.
fn resume_processes(mut suspend_handles: Vec<zx::NullableHandle>) {
    if suspend_handles.is_empty() {
        return;
    }
    fuchsia_trace::duration!(
        "power",
        "starnix-runner:resume_kernel",
        "num_processes" => suspend_handles.len() as u64
    );
    if suspend_handles.len() <= PARALLEL_SUSPEND_THRESHOLD
        || !SUSPEND_WORKER_POOL.can_run_in_parallel()
    {
        // Pop from the tail so critical wake-path processes placed at the end of `suspend_handles`
        // are still resumed first when closing sequentially.
        while let Some(suspend_handle) = suspend_handles.pop() {
            drop(suspend_handle);
        }
        return;
    }
    let suspend_handles = LockDepMutex::<_, SuspendHandlesLock>::new(suspend_handles);
    SUSPEND_WORKER_POOL.run(Arc::new(move || {
        loop {
            // Take a chunk of handles while holding the lock, but close them after releasing
            // it, without allocating.
            let mut chunk = [const { zx::NullableHandle::invalid() }; RESUME_CHUNK_SIZE];
            let is_empty = {
                let mut suspend_handles = suspend_handles.lock();
                if suspend_handles.is_empty() {
                    break;
                }
                for slot in &mut chunk {
                    let Some(suspend_handle) = suspend_handles.pop() else {
                        break;
                    };
                    *slot = suspend_handle;
                }
                suspend_handles.is_empty()
            };
            // Dropping the handles closes them one at a time.
            drop(chunk);
            if is_empty {
                break;
            }
        }
    }));
}

/// Returns the resume priority rank for `process` based on its Zircon process name.
///
/// Higher values are placed closer to the tail (`Vec::pop()` end) of `SuspendedProcesses` so they
/// are thawed first in chunk #0 on wake:
/// - `4`: `starnix_kernel` (input event relay)
/// - `3`: `suspend-service` / `android.system.suspend` (serves synchronous
///   `ISystemSuspend::acquireWakeLock` calls made by `InputReader` under `PMS.mLock`)
/// - `2`: `system_server` (`InputReader`, `PowerManagerService`, `DisplayPowerController`)
/// - `1`: `surfaceflinger` (display power mode and frame composition)
/// - `0`: all other processes
fn process_resume_priority(process: &zx::Process) -> u8 {
    let Ok(name) = process.get_name() else {
        return 0;
    };
    process_resume_priority_for_name(name.as_bstr().as_ref())
}

fn process_resume_priority_for_name(name: &[u8]) -> u8 {
    if contains_subslice(name, b"starnix_kernel") {
        4
    } else if contains_subslice(name, b"suspend-service")
        || contains_subslice(name, b"android.system.suspend")
    {
        3
    } else if contains_subslice(name, b"system_server") {
        2
    } else if contains_subslice(name, b"surfaceflinger") {
        1
    } else {
        0
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
}

/// Suspends the provided `zx::Job` by suspending each process in the job individually.
///
/// Returns a `SuspendedProcesses` guard holding the suspend tokens of all the suspended processes,
/// ordered so critical wake-path processes sit at the `Vec::pop()` end and resume first when
/// dropped.
///
/// Returns an error if any individual suspend failed, in which case the processes that were
/// suspended are resumed before the error is returned.
fn suspend_job(kernel_job: &Arc<zx::Job>) -> Result<SuspendedProcesses, Error> {
    // The processes that were already handled, whether they were suspended or not.
    let mut known_process_koids = std::collections::HashSet::<zx::Koid>::new();
    // Resumes the suspended processes if an error is returned.
    let mut suspended_processes = SuspendedProcesses(vec![]);
    let mut priorities = Vec::<u8>::new();
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
        for (process, suspend_handle, priority) in new_suspended_processes {
            suspended_processes.0.push(suspend_handle);
            priorities.push(priority);
            processes.push(process);
        }

        parallel_try_filter_map(processes, |process, cancelled| {
            wait_for_process_suspended(process, cancelled).map(|()| None::<()>)
        })?;
    }

    if priorities.iter().any(|&priority| priority > 0) {
        let mut handles_with_priority: Vec<(u8, zx::NullableHandle)> =
            priorities.into_iter().zip(std::mem::take(&mut suspended_processes.0)).collect();
        handles_with_priority.sort_by_key(|(priority, _)| *priority);
        suspended_processes.0 =
            handles_with_priority.into_iter().map(|(_, suspend_handle)| suspend_handle).collect();
    }

    Ok(suspended_processes)
}

/// Requests the suspension of the process identified by `process_koid` in `kernel_job`.
///
/// Returns the process, its suspend handle, and its resume priority, or `None` if the process no
/// longer exists or is dying, in which case it does not need to be suspended. Returns any other
/// error.
fn suspend_process(
    kernel_job: &zx::Job,
    process_koid: zx::Koid,
) -> Result<Option<(zx::Process, zx::NullableHandle, u8)>, zx::Status> {
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
        Ok(suspend_handle) => {
            let priority = process_resume_priority(&process);
            Ok(Some((process, suspend_handle, priority)))
        }
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
/// threads of `SUSPEND_WORKER_POOL` if there are more than `PARALLEL_SUSPEND_THRESHOLD` items, and
/// returns the `Some` values returned by `f`, in no particular order.
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

/// Runs `job` across the dedicated worker threads of `SUSPEND_WORKER_POOL` when `num_items` is
/// more than `PARALLEL_SUSPEND_THRESHOLD` (or on the calling thread otherwise), and returns once
/// all workers running `job` have returned.
///
/// The threads running `job` are expected to share its work dynamically: a worker thread that
/// doesn't start running `job` before another worker finishes all remaining items doesn't run it
/// at all.
fn run_on_workers(num_items: usize, job: impl Fn() + Send + Sync + 'static) {
    if num_items <= PARALLEL_SUSPEND_THRESHOLD || !SUSPEND_WORKER_POOL.can_run_in_parallel() {
        job();
    } else {
        SUSPEND_WORKER_POOL.run(Arc::new(job));
    }
}

/// A job run by the threads of a `SuspendWorkerPool`.
type SuspendWorkerJob = Arc<dyn Fn() + Send + Sync>;

/// The pool of worker threads used to suspend and resume the processes of a container in parallel.
///
/// The worker threads are created once with the `SUSPEND_WORKER_ROLE_NAME` scheduler role and kept
/// for the lifetime of the runner, so that no thread is created or destroyed while suspending or
/// resuming a container. It is forced when the `SuspendContext` is created, so that its threads are
/// not created during the first suspend.
static SUSPEND_WORKER_POOL: LazyLock<SuspendWorkerPool> = LazyLock::new(SuspendWorkerPool::new);

/// A pool of persistent worker threads that run suspend and resume jobs in parallel.
struct SuspendWorkerPool {
    /// Serializes the jobs, as the worker threads run one job at a time.
    run_lock: LockDepMutex<(), SuspendWorkerRunLock>,
    shared: Arc<SuspendWorkerPoolShared>,
    num_workers: usize,
}

/// The state of a `SuspendWorkerPool` shared with its worker threads.
struct SuspendWorkerPoolShared {
    state: LockDepMutex<SuspendWorkerPoolState, SuspendWorkerPoolStateLock>,
    /// Signaled when a job is available to the worker threads.
    job_available: CondVar,
    /// Signaled when the last worker thread running a job returns from it.
    workers_done: CondVar,
}

#[derive(Default)]
struct SuspendWorkerPoolState {
    /// Monotonically increasing job generation so each worker claims a job at most once.
    generation: u64,
    /// The job currently being run.
    job: Option<SuspendWorkerJob>,
    /// The number of worker threads that can still start running `job`.
    unclaimed: usize,
    /// The number of worker threads running `job`.
    running: usize,
}

impl SuspendWorkerPool {
    fn new() -> Self {
        let shared = Arc::new(SuspendWorkerPoolShared {
            state: LockDepMutex::new(SuspendWorkerPoolState::default()),
            job_available: CondVar::new(),
            workers_done: CondVar::new(),
        });
        let available_workers =
            std::thread::available_parallelism().map_or(1, |n| n.get()).min(MAX_SUSPEND_WORKERS);
        let max_workers = if available_workers >= 2 { available_workers } else { 0 };
        let mut num_workers = 0;
        for _ in 0..max_workers {
            let shared = shared.clone();
            match std::thread::Builder::new().name(SUSPEND_WORKER_THREAD_NAME.to_string()).spawn(
                move || {
                    let _ = fuchsia_scheduler::set_role_for_this_thread(SUSPEND_WORKER_ROLE_NAME);
                    shared.run_worker();
                },
            ) {
                Ok(_) => num_workers += 1,
                Err(e) => {
                    // Fewer workers only reduces parallelism; if fewer than two workers spawn, work
                    // falls back to the calling thread.
                    log::warn!("Failed to spawn suspend worker: {e:?}");
                    break;
                }
            }
        }
        Self { run_lock: LockDepMutex::new(()), shared, num_workers }
    }

    /// Whether the pool has at least two dedicated worker threads to execute jobs in parallel.
    fn can_run_in_parallel(&self) -> bool {
        self.num_workers >= 2
    }

    /// Runs `job` across the pool's dedicated worker threads (or on the calling thread if fewer
    /// than two worker threads are available), and returns once all workers running `job` have
    /// returned.
    ///
    /// Must not be called from within a job, as it would deadlock.
    fn run(&self, job: SuspendWorkerJob) {
        if !self.can_run_in_parallel() {
            job();
            return;
        }
        let _run_guard = self.run_lock.lock();
        {
            let mut state = self.shared.state.lock();
            state.generation += 1;
            state.job = Some(job);
            state.unclaimed = self.num_workers;
        }
        self.shared.job_available.notify_all();

        let mut state = self.shared.state.lock();
        while state.unclaimed > 0 || state.running > 0 {
            self.shared.workers_done.wait(&mut state);
        }
    }
}

impl SuspendWorkerPoolShared {
    /// The body of the worker threads, which run the jobs as they become available.
    fn run_worker(&self) {
        let mut last_generation = 0;
        let mut state = self.state.lock();
        loop {
            if state.unclaimed == 0 || state.generation == last_generation {
                self.job_available.wait(&mut state);
                continue;
            }
            last_generation = state.generation;
            state.unclaimed -= 1;
            state.running += 1;
            let job = state.job.clone().expect("a job is available");
            drop(state);

            job();
            // Release the job before reporting that it returned, so that the caller of
            // `SuspendWorkerPool::run` gets back sole ownership of what the job holds.
            drop(job);

            state = self.state.lock();
            // Once a worker finishes `job()`, all remaining items have already been claimed by the
            // currently running workers or cancelled, so any late workers no longer need to start.
            state.unclaimed = 0;
            state.job = None;
            state.running -= 1;
            if state.running == 0 {
                self.workers_done.notify_all();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_power_system as fpower;
    use futures::StreamExt;

    /// Creates a named process in `job` with `num_threads` started threads, which are kept
    /// suspended by the returned suspend handles so that they never execute any code.
    fn create_named_process_with_suspended_threads(
        job: &zx::Job,
        name: &[u8],
        num_threads: usize,
    ) -> (zx::Process, Vec<zx::Thread>, Vec<zx::NullableHandle>) {
        let (process, root_vmar) =
            job.create_child_process(zx::ProcessOptions::empty(), name).unwrap();
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

    /// Creates a process in `job` with `num_threads` started threads, which are kept suspended by
    /// the returned suspend handles so that they never execute any code.
    fn create_process_with_suspended_threads(
        job: &zx::Job,
        num_threads: usize,
    ) -> (zx::Process, Vec<zx::Thread>, Vec<zx::NullableHandle>) {
        create_named_process_with_suspended_threads(job, b"test-process", num_threads)
    }

    fn is_suspended(thread: &zx::Thread) -> bool {
        thread
            .wait_one(zx::Signals::THREAD_SUSPENDED, zx::MonotonicInstant::ZERO)
            .to_result()
            .is_ok()
    }

    fn wait_until_blocked_on_exception(thread: &zx::Thread) {
        while !matches!(
            thread.info().unwrap().state,
            zx::ThreadState::Blocked(zx::ThreadBlockType::Exception(_))
        ) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Whether `parallel_try_filter_map` and `resume_processes` can run work in parallel when
    /// given more than `PARALLEL_SUSPEND_THRESHOLD` items.
    fn can_run_in_parallel() -> bool {
        SUSPEND_WORKER_POOL.can_run_in_parallel()
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

    /// Checks that dropping the result of `suspend_job` for a job with `num_processes` processes
    /// closes all the suspend handles and resumes all the threads.
    fn check_dropping_suspended_processes_resumes_threads(num_processes: usize) {
        const NUM_THREADS: usize = 2;

        // The test job leaves exceptions unhandled, so the resumed threads, which fault since
        // nothing is mapped in their processes, block on the exception.
        let job = TestJob::new();
        let mut processes = vec![];
        let mut threads = vec![];
        let mut thread_suspend_handles = vec![];
        for _ in 0..num_processes {
            let (process, mut process_threads, mut process_thread_suspend_handles) =
                create_process_with_suspended_threads(&job, NUM_THREADS);
            processes.push(process);
            threads.append(&mut process_threads);
            thread_suspend_handles.append(&mut process_thread_suspend_handles);
        }

        let suspended_processes = suspend_job(&job).unwrap();
        assert_eq!(suspended_processes.len(), num_processes);
        let raw_suspend_handles: Vec<_> =
            suspended_processes.0.iter().map(|handle| handle.raw_handle()).collect();
        drop(thread_suspend_handles);
        for thread in &threads {
            assert!(is_suspended(thread));
        }

        drop(suspended_processes);

        for raw_suspend_handle in raw_suspend_handles {
            assert!(zx::Handle::check_raw_valid(raw_suspend_handle).is_err());
        }
        for thread in &threads {
            wait_until_blocked_on_exception(thread);
        }
    }

    #[fuchsia::test]
    fn dropping_few_suspended_processes_resumes_threads() {
        check_dropping_suspended_processes_resumes_threads(PARALLEL_SUSPEND_THRESHOLD);
    }

    #[fuchsia::test]
    fn dropping_many_suspended_processes_resumes_threads() {
        // More than `PARALLEL_SUSPEND_THRESHOLD`, so that the processes are resumed in parallel.
        check_dropping_suspended_processes_resumes_threads(4 * PARALLEL_SUSPEND_THRESHOLD + 1);
    }

    #[fuchsia::test]
    fn resume_processes_closes_all_handles() {
        for num_handles in [0, 1, PARALLEL_SUSPEND_THRESHOLD, 4 * PARALLEL_SUSPEND_THRESHOLD + 1] {
            let events: Vec<_> = (0..num_handles).map(|_| zx::Event::create()).collect();
            let mut handles = vec![];
            for event in &events {
                handles
                    .push(event.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap().into_handle());
            }
            resume_processes(handles);
            // All the duplicates were closed, so only the original handles remain.
            for event in &events {
                assert_eq!(event.count_info().unwrap().handle_count, 1);
            }
        }
    }

    #[fuchsia::test]
    fn process_resume_priority_orders_critical_wake_processes() {
        let kernel_prio = process_resume_priority_for_name(b"starnix_kernel.cm");
        let suspend_service_prio =
            process_resume_priority_for_name(b"android.system.suspend-service");
        let short_suspend_service_prio = process_resume_priority_for_name(b"suspend-service");
        let system_server_prio = process_resume_priority_for_name(b"system_server");
        let surfaceflinger_prio = process_resume_priority_for_name(b"surfaceflinger");
        let normal_prio = process_resume_priority_for_name(b"com.android.systemui");

        assert_eq!(suspend_service_prio, short_suspend_service_prio);
        assert!(kernel_prio > suspend_service_prio);
        assert!(suspend_service_prio > system_server_prio);
        assert!(system_server_prio > surfaceflinger_prio);
        assert!(surfaceflinger_prio > normal_prio);
        assert_eq!(normal_prio, 0);
        assert_eq!(process_resume_priority_for_name(b"starnix_mlock_pins"), 0);
        assert_eq!(process_resume_priority_for_name(b"starnix_zx_map_info_cache"), 0);
        assert_eq!(process_resume_priority_for_name(b"audioserver"), 0);
        assert_eq!(process_resume_priority_for_name(b""), 0);
    }

    #[fuchsia::test]
    fn suspend_job_places_critical_processes_at_pop_end() {
        // Spawn more than `PARALLEL_SUSPEND_THRESHOLD` total processes in scrambled order so
        // `suspend_job` exercises parallel enumeration while sorting critical wake-path processes
        // to the `Vec::pop()` tail.
        let job = TestJob::new();
        let mut critical_thread_suspend_handles = vec![];
        let mut app_thread_suspend_handles = vec![];
        let mut app_processes = vec![];
        let mut app_threads = vec![];

        let mut spawn_app = |name: &[u8]| {
            let (process, mut threads, mut suspend_handles) =
                create_named_process_with_suspended_threads(&job, name, 2);
            app_processes.push(process);
            app_threads.append(&mut threads);
            app_thread_suspend_handles.append(&mut suspend_handles);
        };

        spawn_app(b"com.android.settings");
        spawn_app(b"audioserver");
        let (_sf_proc, sf_threads, mut sf_handles) =
            create_named_process_with_suspended_threads(&job, b"surfaceflinger", 2);
        critical_thread_suspend_handles.append(&mut sf_handles);

        spawn_app(b"zygote64");
        spawn_app(b"com.android.systemui");
        let (_ss_proc, ss_threads, mut ss_handles) =
            create_named_process_with_suspended_threads(&job, b"system_server", 2);
        critical_thread_suspend_handles.append(&mut ss_handles);

        spawn_app(b"netd");
        spawn_app(b"logd");
        let (_susp_proc, susp_threads, mut susp_handles) =
            create_named_process_with_suspended_threads(&job, b"android.system.suspend-service", 2);
        critical_thread_suspend_handles.append(&mut susp_handles);

        spawn_app(b"servicemanager");
        let (_kernel_proc, kernel_threads, mut kernel_handles) =
            create_named_process_with_suspended_threads(&job, b"starnix_kernel.cm", 2);
        critical_thread_suspend_handles.append(&mut kernel_handles);

        spawn_app(b"vold");
        spawn_app(b"lmkd");

        let mut suspended_processes = suspend_job(&job).unwrap();
        assert_eq!(suspended_processes.len(), 13);
        drop(critical_thread_suspend_handles);
        drop(app_thread_suspend_handles);

        for thread in kernel_threads
            .iter()
            .chain(&susp_threads)
            .chain(&ss_threads)
            .chain(&sf_threads)
            .chain(&app_threads)
        {
            assert!(is_suspended(thread));
        }

        // Pop 1st handle (tail of `Vec` / chunk #0 slot 0): `starnix_kernel.cm` thaws first.
        drop(suspended_processes.0.pop().unwrap());
        for thread in &kernel_threads {
            wait_until_blocked_on_exception(thread);
        }
        for thread in susp_threads.iter().chain(&ss_threads).chain(&sf_threads).chain(&app_threads)
        {
            assert!(is_suspended(thread));
        }

        // Pop 2nd handle (chunk #0 slot 1): `android.system.suspend-service` thaws second.
        drop(suspended_processes.0.pop().unwrap());
        for thread in &susp_threads {
            wait_until_blocked_on_exception(thread);
        }
        for thread in ss_threads.iter().chain(&sf_threads).chain(&app_threads) {
            assert!(is_suspended(thread));
        }

        // Pop 3rd handle (chunk #0 slot 2): `system_server` thaws third.
        drop(suspended_processes.0.pop().unwrap());
        for thread in &ss_threads {
            wait_until_blocked_on_exception(thread);
        }
        for thread in sf_threads.iter().chain(&app_threads) {
            assert!(is_suspended(thread));
        }

        // Pop 4th handle (chunk #0 slot 3): `surfaceflinger` thaws fourth.
        drop(suspended_processes.0.pop().unwrap());
        for thread in &sf_threads {
            wait_until_blocked_on_exception(thread);
        }
        for thread in &app_threads {
            assert!(is_suspended(thread));
        }

        // Dropping `suspended_processes` resumes the remaining 9 (> `PARALLEL_SUSPEND_THRESHOLD`)
        // background processes in parallel.
        assert_eq!(suspended_processes.len(), 9);
        drop(suspended_processes);
        for thread in &app_threads {
            wait_until_blocked_on_exception(thread);
        }
    }

    #[fuchsia::test]
    async fn suspend_container_thaws_processes_before_wake_lease_completes() {
        let job = TestJob::new();
        let (_process, threads, thread_suspend_handles) =
            create_named_process_with_suspended_threads(&job, b"starnix_kernel.cm", 2);

        let (activity_governor, mut sag_stream) =
            fidl::endpoints::create_proxy_and_stream::<fpower::ActivityGovernorMarker>();
        let kernels = Arc::new(
            Kernels::new_for_test(job.clone(), "test-container", activity_governor).unwrap(),
        );

        let suspend_context = Arc::new(SuspendContext::default());
        let wake_counter = zx::Counter::create();
        let wake_koid = wake_counter.koid().unwrap();
        // Pre-signal the wake source so `zx::object_wait_many` wakes immediately and attributes
        // the wake reason without blocking the executor thread.
        wake_counter.add(1).unwrap();
        suspend_context
            .wake_sources
            .lock()
            .insert(wake_koid, WakeSource::from_counter(wake_counter, "power-button".to_string()));

        let (watcher_runner, watcher_client) = zx::EventPair::create();
        watcher_runner.signal_peer(zx::Signals::NONE, AWAKE_SIGNAL).unwrap();
        suspend_context.wake_watchers.lock().push(watcher_runner);

        let container_job = job.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let suspend_context_clone = suspend_context.clone();
        let kernels_clone = kernels.clone();
        let suspend_task = fuchsia_async::Task::local(async move {
            suspend_container(
                fstarnixrunner::ManagerSuspendContainerRequest {
                    container_job: Some(container_job),
                    wake_locks: None,
                    ..Default::default()
                },
                &suspend_context_clone,
                &kernels_clone,
            )
            .await
        });

        // Receive `TakeApplicationActivityLease` and hold `responder` unanswered to simulate
        // Power Broker / storage resume latency inside `acquire_wake_lease`.
        let Some(Ok(fpower::ActivityGovernorRequest::TakeApplicationActivityLease {
            name,
            responder,
        })) = sag_stream.next().await
        else {
            panic!("expected TakeApplicationActivityLease request");
        };
        assert_eq!(name, "starnix-kernel-test-container");
        assert!(!kernels.has_wake_lease(&job));

        // Release the test's initial thread suspend handles while `acquire_wake_lease` is still
        // pending. Because `suspend_container` already dropped `suspend_handles` before awaiting
        // `acquire_wake_lease`, the container threads thaw immediately instead of staying frozen.
        drop(thread_suspend_handles);
        for thread in &threads {
            wait_until_blocked_on_exception(thread);
        }

        // Wake watchers must remain `ASLEEP_SIGNAL` until `acquire_wake_lease` completes so wake
        // baton holders (such as Netstack3 wake groups) keep their lease until ours is active.
        assert!(
            watcher_client.wait_one(ASLEEP_SIGNAL, zx::MonotonicInstant::ZERO).to_result().is_ok()
        );
        assert!(
            watcher_client.wait_one(AWAKE_SIGNAL, zx::MonotonicInstant::ZERO).to_result().is_err()
        );

        let (lease_client, _lease_server) = zx::EventPair::create();
        responder.send(lease_client).unwrap();

        let response = suspend_task.await.unwrap().unwrap();
        assert_eq!(response.resume_reason.as_deref(), Some("power-button"));
        assert!(kernels.has_wake_lease(&job));
        assert!(
            watcher_client.wait_one(AWAKE_SIGNAL, zx::MonotonicInstant::ZERO).to_result().is_ok()
        );
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
        // Run the function twice, as the worker threads are reused across calls.
        for _ in 0..2 {
            // More than `PARALLEL_SUSPEND_THRESHOLD` items, so that the items are processed in
            // parallel across the dedicated `starnix-suspend-worker` threads.
            let items: Vec<usize> = (0..=PARALLEL_SUSPEND_THRESHOLD).collect();
            let second_worker_ran = Arc::new(AtomicBool::new(false));
            let second_worker_ran_clone = second_worker_ran.clone();
            parallel_try_filter_map(items, move |item, _cancelled| {
                assert_eq!(std::thread::current().name(), Some(SUSPEND_WORKER_THREAD_NAME));
                if *item == 0 {
                    // Hold the worker that picked up item 0 until a second worker thread runs a
                    // call on one of the remaining items.
                    while !second_worker_ran_clone.load(Ordering::Relaxed) {
                        std::thread::yield_now();
                    }
                } else {
                    second_worker_ran_clone.store(true, Ordering::Relaxed);
                }
                Ok(None::<()>)
            })
            .unwrap();
            assert!(second_worker_ran.load(Ordering::Relaxed));
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
