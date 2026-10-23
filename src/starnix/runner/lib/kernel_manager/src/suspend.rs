// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::Kernels;
use anyhow::Error;
use fidl::Peered;
use fidl_fuchsia_starnix_runner as fstarnixrunner;
use fuchsia_inspect::{self as inspect, UintExponentialHistogramProperty, UintProperty};
use log::warn;
use starnix_sync::{LockDepMutex, WakeSourcesLock, WakeWatchersLock};
use std::sync::Arc;
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
fn suspend_job(kernel_job: &zx::Job) -> Result<SuspendedProcesses, Error> {
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
        let mut processes = Vec::with_capacity(new_process_koids.len());
        for process_koid in new_process_koids {
            if let Some((process, suspend_handle)) = suspend_process(kernel_job, process_koid)? {
                suspended_processes.0.push(suspend_handle);
                processes.push(process);
            }
        }

        for process in &processes {
            wait_for_process_suspended(process)?;
        }
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
fn wait_for_process_suspended(process: &zx::Process) -> Result<(), zx::Status> {
    let threads = process.threads()?;
    for thread_koid in &threads {
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

    /// A child job for a test, which is killed (and waited on) when dropped, so that the test
    /// processes are killed before their suspend tokens are released. Exceptions in the job are
    /// left unhandled in its exception channel, so they never escape the job.
    struct TestJob {
        job: zx::Job,
        _exception_channel: zx::Channel,
    }

    impl TestJob {
        fn new() -> Self {
            let job = fuchsia_runtime::job_default().create_child_job().unwrap();
            let _exception_channel = job.create_exception_channel().unwrap();
            Self { job, _exception_channel }
        }
    }

    impl std::ops::Deref for TestJob {
        type Target = zx::Job;

        fn deref(&self) -> &zx::Job {
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
}
