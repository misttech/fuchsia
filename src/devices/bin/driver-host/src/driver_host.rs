// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::driver::Driver;
use crate::utils::update_process_name;
use anyhow::{Context, Result};
use fidl::encoding::{DefaultFuchsiaResourceDialect, clear_tls_buf};
use fidl::endpoints::{ClientEnd, ServerEnd};
use fidl_fuchsia_driver_framework as fidl_fdf;
use fidl_fuchsia_driver_host as fdh;
use fidl_fuchsia_ldsvc as fldsvc;
use fidl_fuchsia_system_state as fss;
use fuchsia_async as fasync;
use fuchsia_async::Timer;
use fuchsia_component::client;
use fuchsia_sync::Mutex;
use futures::channel::{mpsc, oneshot};
use futures::{StreamExt, TryStreamExt};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Weak};
use zx::Status;

/// Any stored data is removed after this amount of time
const EXCEPTIONS_CLEANUP_DEADLINE_SECONDS: i64 = 600;

/// Maximum number of threads, dispatchers, and queued tasks to include in diagnostic dumps so that
/// a batch of 4 DriverHostInfo entries in DriverHostInfoIterator::GetNext stays well below the
/// 64 KiB channel message size limit.
const MAX_THREADS: usize = 32;
const MAX_DISPATCHERS: usize = 32;
const MAX_QUEUED_TASKS_PER_DISPATCHER: usize = 16;
const MAX_TOTAL_QUEUED_TASKS: usize = 32;

/// We use Weak<Driver> to avoid accidentally extending the lifetime of the Driver. Driver must be
/// droped and have it's destroy hook called in the driver runtime's shutdown observer callback in
/// order to comply with the guarantees the driver framework provides for drivers. The driver host
/// itself doesn't actually ever access the drivers, it strictly uses it for debugging and keeping
/// track of when to shut doesn the driver host.
struct WeakDriver(Weak<Driver>);

impl Ord for WeakDriver {
    fn cmp(&self, other: &WeakDriver) -> std::cmp::Ordering {
        (self.0.as_ptr() as usize).cmp(&(other.0.as_ptr() as usize))
    }
}

impl PartialOrd for WeakDriver {
    fn partial_cmp(&self, other: &WeakDriver) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for WeakDriver {
    fn eq(&self, other: &Self) -> bool {
        (self.0.as_ptr() as usize).eq(&(other.0.as_ptr() as usize))
    }
}

impl Eq for WeakDriver {}

#[derive(Debug)]
pub(crate) struct ExceptionRecord {
    // The point at which this record should be deleted
    deadline: zx::MonotonicInstant,
    // The koid of the thread an exception was observed on
    koid: zx::Koid,
    // The driver info that was associated with the exception
    info: fdh::DriverCrashInfo,
}

/// Decides when the driver host exits.
///
/// The driver host exits once a driver shutting down leaves it without drivers. A driver counts
/// from the moment it has been loaded, which creates its driver runtime dispatchers, until its
/// shutdown completed, which is when all of its dispatchers have been shut down. This includes
/// drivers that are still starting and drivers whose start failed and that are still shutting
/// down. Exiting destroys all dispatchers, which requires all of them to have been shut down, so
/// the exit is also deferred while drivers are being loaded. Once the driver host decided to exit,
/// it doesn't load drivers anymore.
#[derive(Debug, Default)]
struct ExitTracker {
    /// The number of drivers that are being loaded.
    pending_loads: usize,
    /// The number of drivers that have been loaded and haven't completed their shutdown.
    loaded_drivers: usize,
    /// Whether an exit is pending. Set by `request_exit` once a driver shutting down left the
    /// driver host without loaded drivers; the driver host then exits as soon as no drivers are
    /// being loaded. Cleared when a driver is loaded, since that driver requests the exit again
    /// when it shuts down.
    exit_pending: bool,
    /// Whether the driver host decided to exit. Set by `check_and_update_exit_state` when it
    /// returns true, which happens once an exit is pending and no drivers are loaded or being
    /// loaded. Once set, no more drivers are loaded.
    exiting: bool,
}

impl ExitTracker {
    /// Records that a driver is being loaded. Returns false if the driver host is exiting, in which
    /// case the driver must not be loaded.
    fn load_began(&mut self) -> bool {
        if self.exiting {
            return false;
        }
        self.pending_loads += 1;
        true
    }

    /// Records that loading a driver finished. `loaded` is whether the driver was loaded, in which
    /// case `driver_shut_down` must be called once the driver completed its shutdown. Returns true
    /// if the driver host should exit now.
    fn load_finished(&mut self, loaded: bool) -> bool {
        self.pending_loads = self.pending_loads.checked_sub(1).expect("no driver is being loaded");
        if loaded {
            self.loaded_drivers += 1;
            self.exit_pending = false;
        }
        self.check_and_update_exit_state()
    }

    /// Records that a loaded driver completed its shutdown. Returns true if no drivers are loaded
    /// anymore, in which case the caller may call `request_exit`.
    fn driver_shut_down(&mut self) -> bool {
        self.loaded_drivers = self.loaded_drivers.checked_sub(1).expect("no driver is loaded");
        self.loaded_drivers == 0
    }

    /// Requests that the driver host exits if no drivers are loaded. The exit is deferred while
    /// drivers are being loaded. Returns true if the driver host should exit now.
    fn request_exit(&mut self) -> bool {
        if self.loaded_drivers == 0 {
            self.exit_pending = true;
        }
        self.check_and_update_exit_state()
    }

    /// Checks whether the driver host should exit now and, if so, records that it is exiting. The
    /// driver host exits once an exit is pending and no drivers are being loaded; the exit is
    /// deferred while drivers are being loaded. Returns true exactly once.
    fn check_and_update_exit_state(&mut self) -> bool {
        if self.exiting || !self.exit_pending || self.loaded_drivers > 0 || self.pending_loads > 0 {
            return false;
        }
        self.exiting = true;
        true
    }
}

/// Tracks a driver that is being loaded. The driver host doesn't exit while it exists.
struct PendingLoad<'a> {
    host: &'a DriverHost,
    loaded: bool,
}

impl<'a> PendingLoad<'a> {
    /// Returns `ZX_ERR_BAD_STATE` if the driver host is exiting.
    fn new(host: &'a DriverHost) -> Result<Self, Status> {
        if !host.exit_tracker.borrow_mut().load_began() {
            log::warn!("Not starting driver because the driver host is exiting");
            return Err(Status::BAD_STATE);
        }
        Ok(PendingLoad { host, loaded: false })
    }

    /// Marks the driver as loaded. This consumes the guard, and the load is recorded when the
    /// guard is dropped at the end of this method. From now on, the driver counts until a shutdown
    /// watcher observes that it completed its shutdown.
    ///
    /// If the start of the driver is cancelled after this, the driver is never shut down and keeps
    /// counting, so the driver host doesn't exit because its drivers are gone. That's intended,
    /// since the dispatchers of the driver remain.
    fn loaded(mut self) {
        self.loaded = true;
    }
}

impl Drop for PendingLoad<'_> {
    fn drop(&mut self) {
        // If this was the last pending load, it didn't load a driver, and an exit is pending, the
        // driver host exits now. The termination system state was already checked when the exit
        // was requested, so it isn't checked again.
        let should_exit = self.host.exit_tracker.borrow_mut().load_finished(self.loaded);
        if should_exit {
            self.host.signal_no_more_drivers();
        }
    }
}

pub(crate) struct DriverHost {
    env: fdf_env::Environment,
    drivers: RefCell<BTreeSet<WeakDriver>>,
    exit_tracker: RefCell<ExitTracker>,
    no_more_drivers_signaler: RefCell<Option<oneshot::Sender<()>>>,
    exceptions: RefCell<Vec<ExceptionRecord>>,
    scope: fuchsia_async::Scope,
}

impl DriverHost {
    pub fn new(
        env: fdf_env::Environment,
        no_more_drivers_signaler: oneshot::Sender<()>,
    ) -> DriverHost {
        DriverHost {
            env,
            drivers: RefCell::new(BTreeSet::new()),
            exit_tracker: RefCell::new(ExitTracker::default()),
            no_more_drivers_signaler: RefCell::new(Some(no_more_drivers_signaler)),
            exceptions: RefCell::new(Vec::new()),
            scope: fuchsia_async::Scope::new(),
        }
    }

    pub async fn run_driver_host_server(self: Rc<Self>, stream: fdh::DriverHostRequestStream) {
        stream
            .map(|result| result.context("failed request"))
            .try_for_each_concurrent(None, |request| {
                let this = self.clone();
                async move {
                    match request {
                        fdh::DriverHostRequest::Start {
                            start_args,
                            driver,
                            host_name,
                            responder,
                        } => {
                            responder
                                .send(this.start_driver(start_args, driver, &host_name).await)
                                .or_else(ignore_peer_closed)?;
                        }
                        fdh::DriverHostRequest::StartLoadedDriver {
                            start_args,
                            dynamic_linking_abi,
                            driver,
                            responder,
                        } => {
                            responder
                                .send(
                                    this.start_loaded_driver(
                                        start_args,
                                        dynamic_linking_abi,
                                        driver,
                                    )
                                    .await,
                                )
                                .or_else(ignore_peer_closed)?;
                        }
                        fdh::DriverHostRequest::GetProcessInfo { responder } => {
                            let res = this.get_process_info();
                            let res_ref = res
                                .as_ref()
                                .map(|info| {
                                    (
                                        info.job_koid,
                                        info.process_koid,
                                        info.main_thread_koid,
                                        info.threads.as_slice(),
                                        info.dispatchers.as_slice(),
                                    )
                                })
                                .map_err(|e| *e);
                            if let Err(e) = responder.send(res_ref).or_else(ignore_peer_closed) {
                                log::warn!("Failed to send GetProcessInfo response: {e}");
                            }
                        }
                        fdh::DriverHostRequest::InstallLoader { loader, .. } => {
                            install_loader(loader);
                        }
                        fdh::DriverHostRequest::TriggerStackTrace { .. } => {
                            fasync::unblock(|| {
                                debug::backtrace_request_all_threads();
                            })
                            .await;
                        }
                        fdh::DriverHostRequest::FindDriverCrashInfoByThreadKoid {
                            thread_koid,
                            responder,
                        } => {
                            let info = this
                                .take_exception_by_thread_koid(&zx::Koid::from_raw(thread_koid));
                            responder
                                .send(info.ok_or_else(|| Status::NOT_FOUND.into_raw()))
                                .or_else(ignore_peer_closed)?;
                        }
                    }
                    clear_tls_buf::<DefaultFuchsiaResourceDialect>();
                    let _ = scudo::mallopt(scudo::M_PURGE_ALL, 0);
                    Ok(())
                }
            })
            .await
            .expect("Failed to handle request")
    }

    pub fn run_exception_listener(self: Rc<Self>) {
        let this = Rc::downgrade(&self);
        self.scope.spawn_local(async move {
            let mut exceptions = task_exceptions::ExceptionsStream::register_with_task(
                &*fuchsia_runtime::process_self(),
            )
            .expect("To create exception stream on process.");
            loop {
                match exceptions.try_next().await {
                    Ok(Some(exception_info)) => {
                        if let Some(this) = this.upgrade() {
                            this.add_exception(exception_info);
                        } else {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        log::error!("failed to read exception stream: {}", error);
                        break;
                    }
                }
            }
        });
    }

    pub fn run_exception_cleanup_task(self: Rc<Self>) {
        let this = Rc::downgrade(&self);
        self.scope.spawn_local(async move {
            loop {
                let sleep_until = this.upgrade().map(|s| {
                    if s.exceptions.borrow().is_empty() {
                        // If we have no records, then we can sleep for as long as the timeout and
                        // check again
                        zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(
                            EXCEPTIONS_CLEANUP_DEADLINE_SECONDS,
                        ))
                    } else {
                        s.exceptions.borrow()[0].deadline
                    }
                });

                let Some(sleep_until) = sleep_until else {
                    break;
                };

                let timer = Timer::new(sleep_until);
                timer.await;

                let Some(this) = this.upgrade() else {
                    break;
                };

                let mut exceptions = this.exceptions.borrow_mut();
                while !exceptions.is_empty() && zx::MonotonicInstant::get() > exceptions[0].deadline
                {
                    exceptions.remove(0);
                }
            }
        });
    }

    pub fn run_thread_monitor_task(&self) {
        let (tx, mut rx) = mpsc::channel(0);
        let tx = Mutex::new(tx);

        let scanner = fdf_env::StallScanner::new(move |duration| {
            // We don't log errors here as it can be excessive if the
            // rx handling is slow. We don't expect the other side close either.
            let _ = tx.lock().try_send(duration);
        });
        self.env.register_stall_scanner(scanner);

        self.scope.spawn_local(async move {
            loop {
                // Drain queue.
                while rx.try_recv().is_ok() {}

                // SAFETY: this call does not use any memory allocated by rust and only does
                // anything if the fdf_env is currently set up, otherwise it does nothing.
                let mut next_wait = unsafe { fdf_sys::fdf_env_scan_threads_for_stalls2() };
                if next_wait == 0 {
                    // Wait for start.
                    let Some(duration) = rx.next().await else {
                        break;
                    };
                    next_wait = duration;
                }
                Timer::new(zx::MonotonicDuration::from_nanos(next_wait)).await;
            }
        });
    }

    async fn start_driver(
        self: Rc<Self>,
        start_args: fidl_fdf::DriverStartArgs,
        request: ServerEnd<fdh::DriverMarker>,
        host_name: &str,
    ) -> Result<(), i32> {
        let pending_load = PendingLoad::new(&self).map_err(Status::into_raw)?;
        let (driver, start_args) =
            Driver::load(&self.env, start_args).await.map_err(Status::into_raw)?;
        // The driver has dispatchers now, so it counts until the shutdown watcher observes that it
        // completed its shutdown. `Driver::start` shuts the driver down if it fails.
        pending_load.loaded();
        let (shutdown_signaler, shutdown_event) = oneshot::channel();
        self.spawn_shutdown_watcher(shutdown_event, /* check_termination_state= */ true);

        driver
            .start(start_args, request, shutdown_signaler, &self.scope)
            .await
            .map_err(Status::into_raw)?;
        update_process_name(host_name, driver.get_url(), self.drivers.borrow().len());
        self.drivers.borrow_mut().insert(WeakDriver(Arc::downgrade(&driver)));

        Ok(())
    }

    async fn start_loaded_driver(
        self: Rc<Self>,
        start_args: fidl_fdf::DriverStartArgs,
        dynamic_linking_abi: u64,
        request: ServerEnd<fdh::DriverMarker>,
    ) -> Result<(), i32> {
        let pending_load = PendingLoad::new(&self).map_err(Status::into_raw)?;
        let (driver, start_args) = Driver::initialize(&self.env, start_args, dynamic_linking_abi)
            .await
            .map_err(Status::into_raw)?;
        // The driver has dispatchers now, so it counts until the shutdown watcher observes that it
        // completed its shutdown. `Driver::start` shuts the driver down if it fails.
        pending_load.loaded();
        let (shutdown_signaler, shutdown_event) = oneshot::channel();
        self.spawn_shutdown_watcher(shutdown_event, /* check_termination_state= */ false);

        driver
            .start(start_args, request, shutdown_signaler, &self.scope)
            .await
            .map_err(Status::into_raw)?;
        update_process_name("", driver.get_url(), self.drivers.borrow().len());
        self.drivers.borrow_mut().insert(WeakDriver(Arc::downgrade(&driver)));
        Ok(())
    }

    /// Spawns a task that waits until the driver completed its shutdown, removes it from the
    /// running drivers, and exits the driver host if that was the last loaded driver.
    ///
    /// If `check_termination_state` is set, the driver host only exits if the system isn't
    /// shutting down.
    fn spawn_shutdown_watcher(
        self: &Rc<Self>,
        shutdown_event: oneshot::Receiver<Weak<Driver>>,
        check_termination_state: bool,
    ) {
        // We carry a weak reference to avoid accidentally extending the lifetime of the
        // driver_host.
        let this = Rc::downgrade(self);
        self.scope.spawn_local(async move {
            // The signaler is only dropped without sending if the start was cancelled before
            // `Driver::start` took it. The driver then keeps counting, like after any cancelled
            // start.
            let Ok(driver) = shutdown_event.await else {
                return;
            };
            let Some(this) = this.upgrade() else {
                return;
            };
            this.drivers.borrow_mut().remove(&WeakDriver(driver));
            let no_drivers = this.exit_tracker.borrow_mut().driver_shut_down();

            // If this is the last driver instance, we should exit.
            if !no_drivers {
                return;
            }

            if check_termination_state {
                // We only exit if we're not shutting down in order to match DFv1 behavior.
                // TODO(https://fxbug.dev/42075187): We should always exit driver hosts when we
                // get down to 0 drivers.
                let client = client::connect_to_protocol::<fss::SystemStateTransitionMarker>()
                    .expect("Could not connect to SystemStateTransition protocol.");
                match client.get_termination_system_state().await {
                    Err(_) | Ok(fss::SystemPowerState::FullyOn) => (),
                    _ => return,
                };
            }

            // Drivers may have been loaded while waiting for the termination state, which
            // `request_exit` accounts for. The exit is deferred while drivers are being loaded.
            let should_exit = this.exit_tracker.borrow_mut().request_exit();
            if should_exit {
                this.signal_no_more_drivers();
            }
        });
    }

    fn signal_no_more_drivers(&self) {
        if let Some(signaler) = self.no_more_drivers_signaler.borrow_mut().take() {
            // The receiver is gone if the driver host is already exiting for another reason.
            let _ = signaler.send(());
        }
    }

    fn add_exception(&self, info: task_exceptions::ExceptionInfo) {
        let Ok(thread_koid) = info.thread.koid() else {
            log::error!("failed to get the exception thread's koid.");
            return;
        };
        let driver_on_thread_koid = self.env.get_driver_on_thread_koid(thread_koid);

        // No driver on exception thread indicates the exception occurred in the driver runtime.
        // Forensics can just fallback to component attribution and report the driver host.
        if let Some(driver_ref) = driver_on_thread_koid {
            let found = self.drivers.borrow().iter().find_map(|d| {
                let locked_driver = d.0.upgrade();
                if let Some(driver) = locked_driver
                    && *driver == driver_ref
                {
                    return Some(driver);
                }

                None
            });

            if let Some(found) = found {
                let mut exceptions = self.exceptions.borrow_mut();
                let crash_info = fdh::DriverCrashInfo {
                    url: Some(found.get_url().to_string()),
                    node_token: found.duplicate_node_token(),
                    ..Default::default()
                };
                exceptions.push(ExceptionRecord {
                    deadline: zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(
                        EXCEPTIONS_CLEANUP_DEADLINE_SECONDS,
                    )),
                    koid: thread_koid,
                    info: crash_info,
                });
                log::error!("Driver exception in driver host: Driver url: {}", found.get_url());
            } else {
                log::warn!(
                    "{} {}",
                    "Failed to validate driver with the driver host.",
                    "This might indicate exceptions in the driver's Stop() or destructor."
                );
            }
        }
    }

    fn take_exception_by_thread_koid(
        &self,
        thread_koid: &zx::Koid,
    ) -> Option<fdh::DriverCrashInfo> {
        let mut exceptions = self.exceptions.borrow_mut();

        let index_to_remove = exceptions
            .iter()
            .enumerate()
            .find_map(|(i, exception)| if &exception.koid == thread_koid { Some(i) } else { None });

        index_to_remove.map(|i| exceptions.remove(i).info)
    }

    fn get_process_info(&self) -> Result<fdh::ProcessInfo, i32> {
        let job_koid =
            fuchsia_runtime::job_default().koid().map_err(zx::Status::into_raw)?.raw_koid();
        let process_koid =
            fuchsia_runtime::process_self().koid().map_err(Status::into_raw)?.raw_koid();
        let main_thread_koid = fuchsia_runtime::with_thread_self(|thread| {
            thread.koid().map_err(zx::Status::into_raw)
        })?
        .raw_koid();

        let thread_koids = fuchsia_runtime::process_self().threads().unwrap_or_default();
        let mut fdf_threads: HashMap<u64, (String, String)> = self
            .env
            .dump_all_threads()
            .into_iter()
            .map(|t| (t.koid, (t.name, t.scheduler_role)))
            .collect();
        let mut threads = Vec::new();
        let live_fdf_count =
            thread_koids.iter().filter(|k| fdf_threads.contains_key(&k.raw_koid())).count();
        let mut non_fdf_budget = MAX_THREADS.saturating_sub(live_fdf_count);
        for koid in &thread_koids {
            if threads.len() >= MAX_THREADS {
                break;
            }
            if let Some((name, scheduler_role)) = fdf_threads.remove(&koid.raw_koid()) {
                threads.push(fdh::ThreadInfo { koid: koid.raw_koid(), name, scheduler_role });
            } else if non_fdf_budget > 0 {
                non_fdf_budget -= 1;
                let name = fuchsia_runtime::process_self()
                    .get_child(koid, zx::Rights::SAME_RIGHTS)
                    .and_then(|handle| handle.get_name())
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                threads.push(fdh::ThreadInfo {
                    koid: koid.raw_koid(),
                    name,
                    scheduler_role: String::new(),
                });
            }
        }
        if threads.len() < MAX_THREADS && !fdf_threads.is_empty() {
            let mut remaining_fdf: Vec<_> = fdf_threads.into_iter().collect();
            remaining_fdf.sort_by_key(|(koid, _)| *koid);
            for (koid, (name, scheduler_role)) in remaining_fdf {
                if threads.len() >= MAX_THREADS {
                    break;
                }
                if thread_koids.is_empty()
                    || fuchsia_runtime::process_self()
                        .get_child(&zx::Koid::from_raw(koid), zx::Rights::SAME_RIGHTS)
                        .is_ok()
                {
                    threads.push(fdh::ThreadInfo { koid, name, scheduler_role });
                }
            }
        }

        let entries = self.env.dump_all_dispatchers();
        let driver_urls: HashMap<u64, String> = self
            .drivers
            .borrow()
            .iter()
            .filter_map(|d| {
                d.0.upgrade().map(|drv| (d.0.as_ptr() as u64, drv.get_url().to_string()))
            })
            .collect();
        let dispatcher_names: HashMap<u64, String> =
            entries.iter().map(|e| (e.dispatcher_ptr, e.name.clone())).collect();

        let mut total_queued_tasks_remaining = MAX_TOTAL_QUEUED_TASKS;
        let mut dispatchers = Vec::new();
        for entry in entries.into_iter().take(MAX_DISPATCHERS) {
            let driver = driver_urls.get(&entry.driver).cloned().unwrap_or_default();
            let state = match entry.state {
                fdf_env::DispatcherState::Running => fdh::DispatcherState::Running,
                fdf_env::DispatcherState::ShuttingDown => fdh::DispatcherState::ShuttingDown,
                fdf_env::DispatcherState::Shutdown => fdh::DispatcherState::Shutdown,
                fdf_env::DispatcherState::Destroyed => fdh::DispatcherState::Destroyed,
            };
            let (has_destroy_user_initiated, destroy_user_initiated) =
                match entry.destroy_user_initiated {
                    Some(val) => (true, val),
                    None => (false, false),
                };
            let num_queued_tasks = entry.queued_tasks.len() as u64;
            let limit = MAX_QUEUED_TASKS_PER_DISPATCHER.min(total_queued_tasks_remaining);
            let queued_tasks: Vec<fdh::QueuedTaskInfo> = entry
                .queued_tasks
                .into_iter()
                .take(limit)
                .map(|task| {
                    let initiating_dispatcher_name = dispatcher_names
                        .get(&task.initiating_dispatcher)
                        .cloned()
                        .unwrap_or_default();
                    let initiating_driver_url =
                        driver_urls.get(&task.initiating_driver).cloned().unwrap_or_default();
                    fdh::QueuedTaskInfo {
                        ptr: task.ptr,
                        handler: task.handler,
                        initiating_dispatcher: task.initiating_dispatcher,
                        initiating_dispatcher_name,
                        initiating_driver: task.initiating_driver,
                        initiating_driver_url,
                    }
                })
                .collect();
            total_queued_tasks_remaining -= queued_tasks.len();
            dispatchers.push(fdh::DispatcherInfo {
                driver,
                name: entry.name,
                options: entry.options,
                scheduler_role: entry.scheduler_role,
                dispatcher_ptr: entry.dispatcher_ptr,
                driver_ptr: entry.driver,
                synchronized: entry.synchronized,
                allow_sync_calls: entry.allow_sync_calls,
                state,
                destroy_context: entry.destroy_context,
                has_destroy_user_initiated,
                destroy_user_initiated,
                debug_stats: fdh::DispatcherDebugStats {
                    num_total_requests: entry.debug_stats.num_total_requests,
                    num_inlined_requests: entry.debug_stats.num_inlined_requests,
                    non_inlined: fdh::NonInlinedRequestStats {
                        allow_sync_calls: entry.debug_stats.non_inlined.allow_sync_calls,
                        parallel_dispatch: entry.debug_stats.non_inlined.parallel_dispatch,
                        task: entry.debug_stats.non_inlined.task,
                        unknown_thread: entry.debug_stats.non_inlined.unknown_thread,
                        reentrant: entry.debug_stats.non_inlined.reentrant,
                        channel_wait_not_yet_registered: entry
                            .debug_stats
                            .non_inlined
                            .channel_wait_not_yet_registered,
                        no_thread_migration: entry.debug_stats.non_inlined.no_thread_migration,
                    },
                },
                num_queued_tasks,
                queued_tasks,
            });
        }

        Ok(fdh::ProcessInfo { job_koid, process_koid, main_thread_koid, threads, dispatchers })
    }
}

impl Drop for DriverHost {
    fn drop(&mut self) {
        // All drivers should now be shutdown and stopped.
        // Destroy all dispatchers in case any weren't freed correctly.
        // This will block until all dispatcher callbacks complete.
        self.env.destroy_all_dispatchers();
    }
}

unsafe extern "C" {
    fn dl_set_loader_service(handle: zx::sys::zx_handle_t) -> zx::sys::zx_handle_t;
}

fn install_loader(loader: ClientEnd<fldsvc::LoaderMarker>) {
    let loader_handle = loader.into_channel().into_raw();
    // SAFETY: The old loader implementation should be a valid channel which should be closed after
    // it is swapped out.
    let _old_loader = unsafe { zx::NullableHandle::from_raw(dl_set_loader_service(loader_handle)) };
}

fn ignore_peer_closed(err: fidl::Error) -> Result<(), fidl::Error> {
    if err.is_closed() { Ok(()) } else { Err(err) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdf::DispatcherBuilder;

    #[fuchsia::test]
    async fn get_process_info_test() {
        let env = fdf_env::Environment::start(0).unwrap();
        let driver = env.new_driver(&42u32);
        let _dispatcher = driver
            .new_dispatcher(DispatcherBuilder::new().name("test-dispatcher"))
            .unwrap()
            .release();
        let _role_dispatcher = driver
            .new_dispatcher(
                DispatcherBuilder::new()
                    .name("role-dispatcher")
                    .scheduler_role("fuchsia.test.role"),
            )
            .unwrap()
            .release();
        let (tx, _rx) = oneshot::channel();
        let driver_host = DriverHost::new(env, tx);
        let fdh::ProcessInfo { main_thread_koid, threads, dispatchers, .. } =
            driver_host.get_process_info().unwrap();
        assert!(!threads.is_empty());
        assert!(threads.iter().any(|t| t.koid == main_thread_koid));
        assert!(threads.iter().any(|t| t.name.starts_with("fdf-dispatcher-thread-")));
        assert!(threads.iter().any(|t| t.scheduler_role == "fuchsia.test.role"));
        assert_eq!(dispatchers.len(), 2);
        assert!(dispatchers.iter().any(|d| d.name == "test-dispatcher"));
        assert!(
            dispatchers
                .iter()
                .any(|d| d.name == "role-dispatcher" && d.scheduler_role == "fuchsia.test.role")
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        driver.shutdown(move |_| {
            let _ = shutdown_tx.send(());
        });
        shutdown_rx.await.unwrap();
    }

    #[fuchsia::test]
    fn exit_tracker_exits_when_last_driver_shuts_down() {
        let mut tracker = ExitTracker::default();
        assert!(tracker.load_began());
        assert!(!tracker.load_finished(/* loaded= */ true));
        assert!(tracker.driver_shut_down());
        assert!(tracker.request_exit());
        // The driver host only exits once and doesn't load drivers anymore.
        assert!(!tracker.request_exit());
        assert!(!tracker.load_began());
    }

    #[fuchsia::test]
    fn exit_tracker_does_not_exit_while_other_drivers_are_loaded() {
        let mut tracker = ExitTracker::default();
        assert!(tracker.load_began());
        assert!(!tracker.load_finished(/* loaded= */ true));
        assert!(tracker.load_began());
        assert!(!tracker.load_finished(/* loaded= */ true));
        assert!(!tracker.driver_shut_down());
        assert!(!tracker.request_exit());
        assert!(tracker.driver_shut_down());
        assert!(tracker.request_exit());
    }

    #[fuchsia::test]
    fn exit_tracker_does_not_exit_when_first_driver_fails_to_load() {
        let mut tracker = ExitTracker::default();
        assert!(tracker.load_began());
        assert!(!tracker.load_finished(/* loaded= */ false));
        assert!(tracker.load_began());
    }

    // A driver that failed to start is shut down asynchronously, and the driver host must not exit
    // before that completed, even if another driver requested the exit.
    #[fuchsia::test]
    fn exit_tracker_waits_for_driver_that_failed_to_start_to_shut_down() {
        let mut tracker = ExitTracker::default();
        // Drivers A and B are loaded and start.
        assert!(tracker.load_began());
        assert!(tracker.load_began());
        assert!(!tracker.load_finished(/* loaded= */ true));
        assert!(!tracker.load_finished(/* loaded= */ true));
        // A fails to start and completes its shutdown while B is starting.
        assert!(!tracker.driver_shut_down());
        assert!(!tracker.request_exit());
        // B fails to start. Nothing happens until B completed its shutdown.
        assert!(tracker.driver_shut_down());
        assert!(tracker.request_exit());
    }

    #[fuchsia::test]
    fn exit_tracker_defers_exit_while_load_is_pending_then_load_fails() {
        let mut tracker = ExitTracker::default();
        // Driver A is loaded and B is being loaded.
        assert!(tracker.load_began());
        assert!(tracker.load_began());
        assert!(!tracker.load_finished(/* loaded= */ true));
        // A fails to start and completes its shutdown, so the exit is deferred.
        assert!(tracker.driver_shut_down());
        assert!(!tracker.request_exit());
        // B fails to load, so the deferred exit happens.
        assert!(tracker.load_finished(/* loaded= */ false));
        assert!(!tracker.load_began());
    }

    #[fuchsia::test]
    fn exit_tracker_defers_exit_while_load_is_pending_then_load_succeeds() {
        let mut tracker = ExitTracker::default();
        // Driver A is loaded and B is being loaded.
        assert!(tracker.load_began());
        assert!(tracker.load_began());
        assert!(!tracker.load_finished(/* loaded= */ true));
        // A fails to start and completes its shutdown, so the exit is deferred.
        assert!(tracker.driver_shut_down());
        assert!(!tracker.request_exit());
        // B is loaded, so the driver host doesn't exit.
        assert!(!tracker.load_finished(/* loaded= */ true));
        // The driver host exits once B completed its shutdown.
        assert!(tracker.driver_shut_down());
        assert!(tracker.request_exit());
    }

    #[fuchsia::test]
    fn exit_tracker_loaded_driver_cancels_deferred_exit() {
        let mut tracker = ExitTracker::default();
        // Driver A is loaded and B is being loaded. A shuts down, so the exit is deferred.
        assert!(tracker.load_began());
        assert!(tracker.load_began());
        assert!(!tracker.load_finished(/* loaded= */ true));
        assert!(tracker.driver_shut_down());
        assert!(!tracker.request_exit());
        // B is loaded, and C is being loaded.
        assert!(!tracker.load_finished(/* loaded= */ true));
        assert!(tracker.load_began());
        // B shuts down without requesting the exit, which happens when the system is shutting
        // down. C failing to load mustn't exit the driver host because of A.
        assert!(tracker.driver_shut_down());
        assert!(!tracker.load_finished(/* loaded= */ false));
    }

    // If a start is cancelled after the driver was loaded, the driver is never shut down. Its
    // dispatchers remain, so the driver host must not exit because its other drivers are gone.
    #[fuchsia::test]
    fn exit_tracker_cancelled_start_keeps_driver_loaded() {
        let mut tracker = ExitTracker::default();
        // Driver A is loaded, and its start is cancelled.
        assert!(tracker.load_began());
        assert!(!tracker.load_finished(/* loaded= */ true));
        // Driver B is loaded and shuts down.
        assert!(tracker.load_began());
        assert!(!tracker.load_finished(/* loaded= */ true));
        assert!(!tracker.driver_shut_down());
        assert!(!tracker.request_exit());
    }

    /// Yields to the executor until `condition` holds.
    async fn wait_until(mut condition: impl FnMut() -> bool) {
        while !condition() {
            Timer::new(zx::MonotonicDuration::from_millis(1)).await;
        }
    }

    /// Marks a driver as loaded and spawns its shutdown watcher, like starting a driver does.
    /// Returns the sender that signals that the driver completed its shutdown.
    fn load_test_driver(driver_host: &Rc<DriverHost>) -> oneshot::Sender<Weak<Driver>> {
        PendingLoad::new(driver_host).unwrap().loaded();
        let (shutdown_signaler, shutdown_event) = oneshot::channel();
        driver_host
            .spawn_shutdown_watcher(shutdown_event, /* check_termination_state= */ false);
        shutdown_signaler
    }

    // A driver that fails to start while another driver is being loaded in the same driver host
    // must not make the driver host exit, since the driver runtime asserts that all dispatchers
    // were shut down when the driver host exits.
    #[fuchsia::test]
    async fn driver_host_does_not_exit_while_load_is_pending() {
        let env = fdf_env::Environment::start(0).unwrap();
        let (no_more_drivers_signaler, mut no_more_drivers) = oneshot::channel();
        let driver_host = Rc::new(DriverHost::new(env, no_more_drivers_signaler));

        // Driver B is being loaded.
        let pending_load = PendingLoad::new(&driver_host).unwrap();

        // Driver A failed to start, so it shuts down without having been added to the drivers.
        load_test_driver(&driver_host).send(Weak::new()).unwrap();
        wait_until(|| driver_host.exit_tracker.borrow().exit_pending).await;
        assert_eq!(no_more_drivers.try_recv(), Ok(None));

        // Driver B fails to load, so the driver host exits now.
        drop(pending_load);
        assert_eq!(no_more_drivers.try_recv(), Ok(Some(())));
        assert_eq!(PendingLoad::new(&driver_host).err(), Some(Status::BAD_STATE));
    }

    // A driver that failed to start is shut down asynchronously, and the driver host must not exit
    // before that completed, since the driver runtime asserts that all dispatchers were shut down
    // when the driver host exits.
    #[fuchsia::test]
    async fn driver_host_waits_for_driver_that_failed_to_start_to_shut_down() {
        let env = fdf_env::Environment::start(0).unwrap();
        let (no_more_drivers_signaler, mut no_more_drivers) = oneshot::channel();
        let driver_host = Rc::new(DriverHost::new(env, no_more_drivers_signaler));

        // Drivers A and B are loaded and start.
        let shutdown_a = load_test_driver(&driver_host);
        let shutdown_b = load_test_driver(&driver_host);

        // A fails to start and completes its shutdown.
        shutdown_a.send(Weak::new()).unwrap();
        wait_until(|| driver_host.exit_tracker.borrow().loaded_drivers == 1).await;
        assert_eq!(no_more_drivers.try_recv(), Ok(None));

        // B fails to start, which shuts it down asynchronously. The driver host only exits once
        // B completed its shutdown.
        shutdown_b.send(Weak::new()).unwrap();
        no_more_drivers.await.unwrap();
    }

    #[fuchsia::test]
    async fn driver_host_exits_when_last_driver_shuts_down() {
        let env = fdf_env::Environment::start(0).unwrap();
        let (no_more_drivers_signaler, no_more_drivers) = oneshot::channel();
        let driver_host = Rc::new(DriverHost::new(env, no_more_drivers_signaler));

        load_test_driver(&driver_host).send(Weak::new()).unwrap();
        no_more_drivers.await.unwrap();
    }
}
