// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The API for spawning dynamic kernel threads.
//!
//! If you want to run a closure on a kernel thread, check out [SpawnRequestBuilder] on
//! how to start and configure tasks that run closures.

use crate::execution::create_kernel_thread;
use crate::task::{
    CurrentTask, DelayedReleaser, LockedAndTask, Task, WrappedFuture, with_new_current_task,
};
use fuchsia_sync::Mutex;
use futures::TryFutureExt;
use futures::channel::oneshot;
use starnix_logging::{log_debug, log_error};
use starnix_sync::{Locked, Unlocked};
use starnix_task_command::TaskCommand;
use starnix_types::ownership::{WeakRef, release_after};
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use std::future::Future;
use std::sync::Arc;
use std::sync::mpsc::{SendError, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;

type BoxedClosure = Box<dyn FnOnce(&mut Locked<Unlocked>, &CurrentTask) + Send + 'static>;

const DEFAULT_THREAD_ROLE: &str = "fuchsia.starnix.fair.16";

/// A builder for configuring new tasks to spawn.
///
/// The builder allows us to set up several different flavors of possible
/// tasks to spawn, with a menu of options as follows:
///
/// - A task may or may not have a name.
/// - A task may or may not have an assigned scheduler role.
/// - A task may be a sync, or an async closure.
/// - A task may return a result, or may not return a result.
/// - The task's result can be collected synchronously, asynchronously, or not collected at all.
///
/// Note that these parameters are not perfectly orthogonal. For example, a task spawned from an
/// async closure can not return a value asynchronously. (This is not a limitation of the approach,
/// rather it's the API that we explicitly use today.) Also, some parameter combinations do not
/// make sense, for example a spawn request can not have both a sync and an async closure to run.
///
/// The builder API is designed in a way that only allows chaining the configuration options which
/// are valid at that point in the configuration process. Invalid option combinations are
/// compile-time(!) errors. It will only allow creating a [SpawnRequest] if enough parameters have
/// been passed such that there is enough information to create a request. It will not allow
/// passing conflicting parameters: for example, if you already passed one synchronous closure, it
/// is impossible to pass another closure and have the code compile without errors.
///
/// ## Usage
///
/// Call [SpawnRequestBuilder::new()] to start building. Refer to the unit tests in this module for
/// usage examples.
pub struct SpawnRequestBuilder<C: ClosureKind> {
    role: Option<&'static str>,
    closure_kind: C,
}

/// You can only create an empty request builder.
impl SpawnRequestBuilder<ClosureNone> {
    /// Creates a new spawn request builder.
    pub fn new() -> Self {
        Self { role: None, closure_kind: ClosureNone {} }
    }
}

/// You can call these at any point in the builder's lifecycle.
impl<C: ClosureKind> SpawnRequestBuilder<C> {
    /// Set a role to apply to the thread that will run your closure,
    pub fn with_role(self, role: &'static str) -> Self {
        Self { role: Some(role), ..self }
    }
}

/// You can call these only if you have not provided a closure yet.
impl SpawnRequestBuilder<ClosureNone> {
    /// Provides the closure that the spawner will run.
    pub fn with_sync_closure<F, T>(self, f: F) -> SpawnRequestBuilder<ClosureSome<T>>
    where
        F: FnOnce(&mut Locked<Unlocked>, &CurrentTask) -> T + Send + 'static,
        T: Send + 'static,
    {
        let SpawnRequestBuilder { role, closure_kind: _ } = self;
        let (sender, receiver) = sync_channel::<T>(1);
        let _keepalive = sender.clone();
        let closure = Box::new(move |locked: &mut Locked<Unlocked>, current_task: &CurrentTask| {
            let _ = sender.send(f(locked, current_task));
        });
        SpawnRequestBuilder { role, closure_kind: ClosureSome { closure, receiver, _keepalive } }
    }

    /// Provides the closure that the spawner will run.
    pub fn with_async_closure<F, T>(self, f: F) -> SpawnRequestBuilder<ClosureSome<T>>
    where
        F: AsyncFnOnce(LockedAndTask<'_>) -> T + Send + 'static,
        T: Send + 'static,
    {
        let sync_fn = async_to_sync(f);
        self.with_sync_closure(sync_fn)
    }
}

/// A fully configured spawn request.
pub struct SpawnRequest {
    /// The closure to run.
    closure: BoxedClosure,
}

impl<T> SpawnRequestBuilder<ClosureSome<T>>
where
    T: Send + 'static,
{
    /// Build a spawn request.
    pub fn build(self) -> SpawnRequest {
        let (_, req) = self.build_with_sync_result();
        req
    }

    /// Like [build], but allows receiving a result synchronously.
    /// Do not forget to submit the spawn request to a spawner.
    ///
    /// Example:
    ///
    /// ```
    /// let (result_fn, request) = /*...*/ .build_with_sync_result();
    /// // spawn `request`
    /// let result = result_fn();
    /// ```
    pub fn build_with_sync_result(self) -> (impl FnOnce() -> Result<T, Errno>, SpawnRequest) {
        let Self { role, closure_kind } = self;
        let ClosureSome { closure, receiver, _keepalive } = closure_kind;
        let result_fn = move || {
            let result =
                receiver.recv().map_err(|err| errno!(EINTR, format!("while receiving: {err:?}")));
            // Ok to allow _keepalive to close now that `result` was consumed from
            // `receiver`.
            std::mem::drop(_keepalive);
            result
        };
        let run_fn = maybe_apply_role(role, closure);
        (result_fn, SpawnRequest { closure: run_fn })
    }

    /// Like [build], but allows receiving a result as a future.
    /// Do not forget to submit the spawn request to a spawner.
    ///
    /// Example:
    ///
    /// ```
    /// let (result_fut, request) = /*...*/ .build_with_async_result();
    /// // spawn `request`
    /// let result = result_fut.await;
    /// ```
    pub fn build_with_async_result(self) -> (impl Future<Output = Result<T, Errno>>, SpawnRequest) {
        let Self { role, closure_kind } = self;
        let ClosureSome { closure, receiver, _keepalive } = closure_kind;
        let (sender_async, result_fut) = oneshot::channel::<Result<T, Errno>>();
        let maybe_with_role = maybe_apply_role(role, closure);
        let repackaged =
            Box::new(move |locked: &mut Locked<Unlocked>, current_task: &CurrentTask| {
                maybe_with_role(locked, current_task);
                // Repackage the result for an async receiver: get the result from the sync channel,
                // then forward it to a channel that has an async receiver.
                let result = receiver
                    .recv()
                    .map_err(|err| errno!(EINTR, format!("while receiving: {err:?}")));
                let _ = sender_async.send(result);
                // Allows the channel for which `receiver` is the output end to close.
                std::mem::drop(_keepalive);
            });
        let result_fut = result_fut
            .unwrap_or_else(|err| Err(errno!(EINTR, format!("while receiving async: {err:?}"))));
        (result_fut, SpawnRequest { closure: repackaged })
    }
}

/// A thread pool that immediately execute any new work sent to it and keep a maximum number of
/// idle threads.
///
/// Call [DynamicThreadSpawner::new_with_max_idle_threads] to start creating a new instance.
#[derive(Debug)]
pub struct DynamicThreadSpawner {
    state: Arc<Mutex<DynamicThreadSpawnerState>>,
    /// The weak system task to create the kernel thread associated with each thread.
    system_task: WeakRef<Task>,
    /// A persistent thread that is used to create new thread. This ensures that threads are
    /// created from the initial starnix process and are not tied to a specific task.
    persistent_thread: RunningThread,
}

/// Wrap a closure with a thread role assignment, if one is available.
fn maybe_apply_role<F>(
    role: Option<&'static str>,
    f: F,
) -> Box<dyn FnOnce(&mut Locked<Unlocked>, &CurrentTask) + Send + 'static>
where
    F: FnOnce(&mut Locked<Unlocked>, &CurrentTask) + Send + 'static,
{
    if let Some(role) = role {
        Box::new(move |locked, current_task| {
            if let Err(e) = fuchsia_scheduler::set_role_for_this_thread(role) {
                log_debug!(e:%; "failed to set kthread role");
            }
            f(locked, current_task);
            if let Err(e) = fuchsia_scheduler::set_role_for_this_thread(DEFAULT_THREAD_ROLE) {
                log_debug!(e:%; "failed to reset kthread role to default priority");
            }
        })
    } else {
        Box::new(f)
    }
}

/// Convert async closure to sync closure that can be submitted to the spawner.
fn async_to_sync<F, R>(
    f: F,
) -> impl FnOnce(&mut Locked<Unlocked>, &CurrentTask) -> R + Send + 'static
where
    F: AsyncFnOnce(LockedAndTask<'_>) -> R + Send + 'static,
    R: Send + 'static,
{
    move |locked, current_task| {
        let mut exec = fuchsia_async::LocalExecutor::default();
        let locked_and_task = LockedAndTask::new(locked, current_task);

        let (sender, receiver) = sync_channel::<R>(1);
        let locked_and_task_clone = locked_and_task.clone();
        let fut = async move {
            let result = f(locked_and_task_clone).await;
            let _ = sender.send(result);
        };
        let wrapped_future = WrappedSpawnedFuture::new(locked_and_task, fut);
        exec.run_singlethreaded(wrapped_future);
        receiver.recv().expect("recv call worked")
    }
}

/// Denotes whether a closure has been provided. A request can not be
/// built at all without a closure.
///
/// See [SpawnRequestBuilder] for usage details.
pub trait ClosureKind {}

/// A builder type state where no closure has been provided yet.
///
/// See [SpawnRequestBuilder] for usage details.
pub struct ClosureNone {}
impl ClosureKind for ClosureNone {}

/// A builder type state where a closure has been provided.
///
/// See [SpawnRequestBuilder] for usage details.
pub struct ClosureSome<T>
where
    T: Send + 'static,
{
    closure: BoxedClosure,
    // Used to receive the computation result from `closure`.
    receiver: std::sync::mpsc::Receiver<T>,

    // Unused, except to ensure that the channel for which `receiver` is the receiving end is not
    // closed before receiver gets a value out. This avoids a race condition where the scheduling
    // happens so that sender is dropped before the receive happens, resulting in a receive error.
    // We prevent that by keeping around an extra sender end which we only release when appropriate.
    _keepalive: std::sync::mpsc::SyncSender<T>,
}
impl<T> ClosureKind for ClosureSome<T> where T: Send + 'static {}

#[derive(Debug)]
struct DynamicThreadSpawnerState {
    threads: Vec<RunningThread>,
    idle_threads: u8,
    max_idle_threads: u8,
}

impl DynamicThreadSpawner {
    pub fn new(max_idle_threads: u8, system_task: WeakRef<Task>) -> Self {
        let persistent_thread = RunningThread::new_persistent(system_task.clone());
        Self {
            state: Arc::new(Mutex::new(DynamicThreadSpawnerState {
                max_idle_threads,
                idle_threads: 0,
                threads: vec![],
            })),
            system_task,
            persistent_thread,
        }
    }

    /// TODO: b/465144050: can be removed.
    /// Run the given closure on a thread and block to get the result.
    ///
    /// This method will use an idle thread in the pool if one is available, otherwise it will
    /// start a new thread.
    pub fn spawn_and_get_result_sync<R, F>(&self, f: F) -> Result<R, Errno>
    where
        F: FnOnce(&mut Locked<Unlocked>, &CurrentTask) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (result, req) =
            SpawnRequestBuilder::new().with_sync_closure(f).build_with_sync_result();
        self.spawn_from_request(req);
        result()
    }

    // TODO(b/465144050): can be removed.
    /// Run the given closure on a thread with `role` applied if possible.
    ///
    /// This method will use an idle thread in the pool if one is available, otherwise it will
    /// start a new thread. When this method returns, it is guaranteed that a thread is
    /// responsible to start running the closure.
    pub fn spawn_with_role<F>(&self, role: &'static str, f: F)
    where
        F: FnOnce(&mut Locked<Unlocked>, &CurrentTask) + Send + 'static,
    {
        let req = SpawnRequestBuilder::new().with_role(role).with_sync_closure(f).build();
        self.spawn_from_request(req);
    }

    // TODO(b/465144050): this will replace `spawn` once all extra methods are removed.
    pub fn spawn_from_request(&self, named_closure: SpawnRequest) {
        self.spawn(named_closure.closure)
    }

    /// Run the given closure on a thread.
    ///
    /// This method will use an idle thread in the pool if one is available, otherwise it will
    /// start a new thread. When this method returns, it is guaranteed that a thread is
    /// responsible to start running the closure.
    pub fn spawn<F>(&self, f: F)
    where
        F: FnOnce(&mut Locked<Unlocked>, &CurrentTask) + Send + 'static,
    {
        // Check whether a thread already exists to handle the request.
        let mut function: BoxedClosure = Box::new(f);
        let mut state = self.state.lock();
        if state.idle_threads > 0 {
            let mut i = 0;
            while i < state.threads.len() {
                // Increases `i` immediately, so that it can be decreased it the thread must be
                // dropped.
                let thread_index = i;
                i += 1;
                match state.threads[thread_index].try_dispatch(function) {
                    Ok(_) => {
                        // The dispatch succeeded.
                        state.idle_threads -= 1;
                        return;
                    }
                    Err(TrySendError::Full(f)) => {
                        // The thread is busy.
                        function = f;
                    }
                    Err(TrySendError::Disconnected(f)) => {
                        // The receiver is disconnected, it means the thread has terminated, drop it.
                        state.idle_threads -= 1;
                        state.threads.remove(thread_index);
                        i -= 1;
                        function = f;
                    }
                }
            }
        }

        // A new thread must be created. It needs to be done from the persistent thread.
        let (sender, receiver) = sync_channel::<RunningThread>(0);
        let dispatch_function: BoxedClosure = Box::new({
            let state = self.state.clone();
            let system_task = self.system_task.clone();
            move |_, _| {
                sender
                    .send(RunningThread::new(state, system_task, function))
                    .expect("receiver must not be dropped");
            }
        });
        self.persistent_thread
            .dispatch(dispatch_function)
            .expect("persistent thread should not have ended.");
        state.threads.push(receiver.recv().expect("persistent thread should not have ended."));
    }

    // TODO(b/465144050): remove in favor of spawn_from_request.
    /// Run an async function on a thread with `role` applied if possible.
    ///
    /// The given function must return the async function to run. It will be passed as
    /// instance of `LockedAndTask` than can be used to retrieve a `Locked` or `CurrentTask`.
    ///
    /// This method will use an idle thread in the pool if one is available, otherwise it will
    /// start a new thread. When this method returns, it is guaranteed that a thread is
    /// responsible to start running the closure.
    pub fn spawn_async_with_role<'b, F: 'b>(&'b self, role: &'static str, f: F)
    where
        F: AsyncFnOnce(LockedAndTask<'_>) + Send + 'static,
    {
        let req = SpawnRequestBuilder::new().with_role(role).with_async_closure(f).build();
        self.spawn_from_request(req);
    }

    // TODO(b/465144050): remove in favor of spawn_from_request.
    /// Run an async function on a thread.
    ///
    /// The given function must return the async function to run. It will be passed as
    /// instance of `LockedAndTask` than can be used to retrieve a `Locked` or `CurrentTask`.
    ///
    /// This method will use an idle thread in the pool if one is available, otherwise it will
    /// start a new thread. When this method returns, it is guaranteed that a thread is
    /// responsible to start running the closure.
    pub fn spawn_async<F>(&self, f: F)
    where
        F: AsyncFnOnce(LockedAndTask<'_>) + Send + 'static,
    {
        self.spawn(move |locked, current_task| {
            let mut exec = fuchsia_async::LocalExecutor::default();
            let locked_and_task = LockedAndTask::new(locked, current_task);
            let fut = f(locked_and_task.clone());
            let wrapped_future = WrappedSpawnedFuture::new(locked_and_task, fut);
            exec.run_singlethreaded(wrapped_future);
        });
    }
}

type WrappedSpawnedFuture<'a, F> = WrappedFuture<F, LockedAndTask<'a>>;

impl<'a, F: 'a> WrappedSpawnedFuture<'a, F> {
    fn new(locked_and_task: LockedAndTask<'a>, fut: F) -> Self {
        Self::new_with_cleaner(locked_and_task, trigger_delayed_releaser, fut)
    }
}

fn trigger_delayed_releaser(locked_and_task: LockedAndTask<'_>) {
    locked_and_task.current_task().trigger_delayed_releaser(&mut locked_and_task.unlocked());
}

#[derive(Debug)]
struct RunningThread {
    thread: Option<JoinHandle<()>>,
    sender: Option<SyncSender<BoxedClosure>>,
}

impl RunningThread {
    fn new(
        state: Arc<Mutex<DynamicThreadSpawnerState>>,
        system_task: WeakRef<Task>,
        f: BoxedClosure,
    ) -> Self {
        let (sender, receiver) = sync_channel::<BoxedClosure>(0);
        let thread = Some(
            std::thread::Builder::new()
                .name("kthread-dynamic-worker".to_string())
                .spawn(move || {
                    // It's ok to create a new lock context here, since we are on a new thread.
                    #[allow(
                        clippy::undocumented_unsafe_blocks,
                        reason = "Force documented unsafe blocks in Starnix"
                    )]
                    let locked = unsafe { Unlocked::new() };
                    let result =
                        with_new_current_task(locked, &system_task, |locked, current_task| {
                            while let Ok(f) = receiver.recv() {
                                f(locked, &current_task);
                                // Apply any delayed releasers.
                                current_task.trigger_delayed_releaser(locked);
                                let mut state = state.lock();
                                state.idle_threads += 1;
                                if state.idle_threads > state.max_idle_threads {
                                    // If the number of idle thread is greater than the max, the
                                    // thread terminates.  This disconnects the receiver, which will
                                    // ensure that the thread will be joined and remove from the list
                                    // of available threads the next time the pool tries to use it.
                                    return;
                                }
                            }
                        });
                    if let Err(e) = result {
                        log_error!("Unable to create a kernel thread: {e:?}");
                    }
                })
                .expect("able to create threads"),
        );
        let result = Self { thread, sender: Some(sender) };
        // The dispatch cannot fail because the thread can only finish after having executed at
        // least one task, and this is the first task ever dispatched to it.
        result
            .sender
            .as_ref()
            .expect("sender should never be None")
            .send(f)
            .expect("Dispatch cannot fail");
        result
    }

    fn new_persistent(system_task: WeakRef<Task>) -> Self {
        // The persistent thread doesn't need to do any rendez-vous when received task.
        let (sender, receiver) = sync_channel::<BoxedClosure>(20);
        let thread = Some(
            std::thread::Builder::new()
                .name("kthread-persistent-worker".to_string())
                .spawn(move || {
                    // It's ok to create a new lock context here, since we are on a new thread.
                    #[allow(
                        clippy::undocumented_unsafe_blocks,
                        reason = "Force documented unsafe blocks in Starnix"
                    )]
                    let locked = unsafe { Unlocked::new() };
                    let current_task = {
                        let Some(system_task) = system_task.upgrade() else {
                            return;
                        };
                        match create_kernel_thread(
                            locked,
                            &system_task,
                            TaskCommand::new(b"kthreadd"),
                        ) {
                            Ok(task) => task,
                            Err(e) => {
                                log_error!("Unable to create a kernel thread: {e:?}");
                                return;
                            }
                        }
                    };
                    release_after!(current_task, locked, {
                        while let Ok(f) = receiver.recv() {
                            f(locked, &current_task);

                            // Apply any delayed releasers.
                            current_task.trigger_delayed_releaser(locked);
                        }
                    });

                    // Ensure that no releasables are registered after this point as we unwind the stack.
                    DelayedReleaser::finalize();
                })
                .expect("able to create threads"),
        );
        Self { thread, sender: Some(sender) }
    }

    fn try_dispatch(&self, f: BoxedClosure) -> Result<(), TrySendError<BoxedClosure>> {
        self.sender.as_ref().expect("sender should never be None").try_send(f)
    }

    fn dispatch(&self, f: BoxedClosure) -> Result<(), SendError<BoxedClosure>> {
        self.sender.as_ref().expect("sender should never be None").send(f)
    }
}

impl Drop for RunningThread {
    fn drop(&mut self) {
        self.sender = None;
        match self.thread.take() {
            Some(thread) => thread.join().expect("Thread should join."),
            _ => panic!("Thread should never be None"),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::spawn_kernel_and_run;

    #[fuchsia::test]
    async fn run_simple_task() {
        spawn_kernel_and_run(async |_, current_task| {
            let spawner = DynamicThreadSpawner::new(2, current_task.weak_task());
            // Type decorations are needed sometimes to avoid "closure type is
            // not general enough" error.
            let closure = move |_: &mut Locked<Unlocked>, _: &CurrentTask| {};
            let req = SpawnRequestBuilder::new().with_sync_closure(closure).build();
            spawner.spawn_from_request(req);
        })
        .await;
    }

    #[fuchsia::test]
    async fn run_10_tasks() {
        spawn_kernel_and_run(async |_, current_task| {
            let spawner = DynamicThreadSpawner::new(2, current_task.weak_task());
            for _ in 0..10 {
                let closure = move |_: &mut Locked<Unlocked>, _: &CurrentTask| {};
                let opts = SpawnRequestBuilder::new().with_sync_closure(closure).build();
                spawner.spawn_from_request(opts);
            }
        })
        .await;
    }

    #[fuchsia::test]
    async fn blocking_task_do_not_prevent_further_processing() {
        spawn_kernel_and_run(async |_, current_task| {
            let spawner = DynamicThreadSpawner::new(1, current_task.weak_task());

            let pair = Arc::new((fuchsia_sync::Mutex::new(false), fuchsia_sync::Condvar::new()));
            for _ in 0..10 {
                let pair2 = Arc::clone(&pair);
                let closure = move |_: &mut Locked<Unlocked>, _: &CurrentTask| {
                    let (lock, cvar) = &*pair2;
                    let mut cont = lock.lock();
                    while !*cont {
                        cvar.wait(&mut cont);
                    }
                };
                let req = SpawnRequestBuilder::new().with_sync_closure(closure).build();
                spawner.spawn_from_request(req);
            }

            let closure = move |_: &mut Locked<Unlocked>, _: &CurrentTask| {
                let (lock, cvar) = &*pair;
                let mut cont = lock.lock();
                *cont = true;
                cvar.notify_all();
            };

            let (result, req) =
                SpawnRequestBuilder::new().with_sync_closure(closure).build_with_sync_result();
            spawner.spawn_from_request(req);

            assert_eq!(result(), Ok(()));
        })
        .await;
    }

    #[fuchsia::test]
    async fn run_spawn_and_get_result() {
        spawn_kernel_and_run(async |_, current_task| {
            let spawner = DynamicThreadSpawner::new(2, current_task.weak_task());

            let (result, req) =
                SpawnRequestBuilder::new().with_sync_closure(|_, _| 3).build_with_sync_result();
            spawner.spawn_from_request(req);
            assert_eq!(result(), Ok(3));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_spawn_async() {
        spawn_kernel_and_run(async |_, current_task| {
            let spawner = DynamicThreadSpawner::new(2, current_task.weak_task());

            // The closure free variables must be decorated with their respective types,
            // the rust compiler gets confused otherwise and is unable to infer the correct
            // lifetimes. Interestingly, adding your own lifetimes here does *not* help.
            let closure = move |locked: &mut Locked<Unlocked>, current_task: &CurrentTask| {
                let mut exec = fuchsia_async::LocalExecutor::default();
                let locked_and_task = LockedAndTask::new(locked, current_task);
                let fut = async {};
                let wrapped_future = WrappedSpawnedFuture::new(locked_and_task, fut);
                exec.run_singlethreaded(wrapped_future);
            };
            let req = SpawnRequestBuilder::new().with_sync_closure(closure).build();
            spawner.spawn_from_request(req);
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_spawn_async_closure() {
        spawn_kernel_and_run(async |_, current_task| {
            let spawner = DynamicThreadSpawner::new(2, current_task.weak_task());
            let fut = async |_: LockedAndTask<'_>| 42;
            let (result, req) =
                SpawnRequestBuilder::new().with_async_closure(fut).build_with_sync_result();
            spawner.spawn_from_request(req);
            assert_eq!(result(), Ok(42));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_spawn_sync_to_async_result() {
        spawn_kernel_and_run(async |_, current_task| {
            let spawner = DynamicThreadSpawner::new(2, current_task.weak_task());
            let fut = async |_: LockedAndTask<'_>| 42;
            let (result, req) =
                SpawnRequestBuilder::new().with_async_closure(fut).build_with_sync_result();

            let fut2 = async move |_: LockedAndTask<'_>| result().unwrap();
            let (result2, req2) =
                SpawnRequestBuilder::new().with_async_closure(fut2).build_with_sync_result();
            spawner.spawn_from_request(req2);
            spawner.spawn_from_request(req);
            assert_eq!(result2(), Ok(42));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_spawn_async_to_async_result() {
        spawn_kernel_and_run(async |_, current_task| {
            let spawner = DynamicThreadSpawner::new(2, current_task.weak_task());
            let fut = async |_: LockedAndTask<'_>| 42;
            let (result_fut, req) =
                SpawnRequestBuilder::new().with_async_closure(fut).build_with_async_result();

            let fut2 = async move |_: LockedAndTask<'_>| result_fut.await.unwrap();
            let (result2, req2) =
                SpawnRequestBuilder::new().with_async_closure(fut2).build_with_sync_result();
            spawner.spawn_from_request(req2);
            spawner.spawn_from_request(req);
            assert_eq!(result2(), Ok(42));
        })
        .await;
    }
}
