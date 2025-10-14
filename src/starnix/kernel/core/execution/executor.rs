// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::execution::loop_entry::enter_syscall_loop;
use crate::task::{
    CurrentTask, DelayedReleaser, ExitStatus, PtraceCoreState, TaskBuilder,
    ptrace_attach_from_state,
};
use anyhow::Error;
use starnix_logging::{log_error, log_warn};
use starnix_sync::{LockBefore, Locked, TaskRelease, Unlocked};
use starnix_types::ownership::WeakRef;
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, error};
use std::os::unix::thread::JoinHandleExt;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use zx::{
    AsHandleRef, {self as zx},
};

pub fn execute_task_with_prerun_result<L, F, R, G>(
    locked: &mut Locked<L>,
    task_builder: TaskBuilder,
    pre_run: F,
    task_complete: G,
    ptrace_state: Option<PtraceCoreState>,
) -> Result<R, Errno>
where
    L: LockBefore<TaskRelease>,
    F: FnOnce(&mut Locked<Unlocked>, &mut CurrentTask) -> Result<R, Errno> + Send + Sync + 'static,
    R: Send + Sync + 'static,
    G: FnOnce(Result<ExitStatus, Error>) + Send + Sync + 'static,
{
    let (sender, receiver) = sync_channel::<Result<R, Errno>>(1);
    execute_task(
        locked,
        task_builder,
        move |current_task, locked| match pre_run(current_task, locked) {
            Err(errno) => {
                let _ = sender.send(Err(errno.clone()));
                Err(errno)
            }
            Ok(value) => sender.send(Ok(value)).map_err(|error| {
                log_error!("Unable to send `pre_run` result: {error:?}");
                errno!(EINVAL)
            }),
        },
        task_complete,
        ptrace_state,
    )?;
    receiver.recv().map_err(|e| {
        log_error!("Unable to retrieve result from `pre_run`: {e:?}");
        errno!(EINVAL)
    })?
}

pub fn execute_task<L, F, G>(
    locked: &mut Locked<L>,
    task_builder: TaskBuilder,
    pre_run: F,
    task_complete: G,
    ptrace_state: Option<PtraceCoreState>,
) -> Result<(), Errno>
where
    L: LockBefore<TaskRelease>,
    F: FnOnce(&mut Locked<Unlocked>, &mut CurrentTask) -> Result<(), Errno> + Send + Sync + 'static,
    G: FnOnce(Result<ExitStatus, Error>) + Send + Sync + 'static,
{
    // Set the process handle to the new task's process, so the new thread is spawned in that
    // process.
    let process_handle = task_builder.task.thread_group().process.raw_handle();
    #[allow(
        clippy::undocumented_unsafe_blocks,
        reason = "Force documented unsafe blocks in Starnix"
    )]
    let old_process_handle = unsafe { thrd_set_zx_process(process_handle) };

    let weak_task = WeakRef::from(&task_builder.task);
    let ref_task = weak_task.upgrade().unwrap();
    if let Some(ptrace_state) = ptrace_state {
        let _ = ptrace_attach_from_state(
            locked.cast_locked::<TaskRelease>(),
            &task_builder.task,
            ptrace_state,
        );
    }

    // Hold a lock on the task's thread slot until we have a chance to initialize it.
    let mut task_thread_guard = ref_task.thread.write();

    // Spawn the process' thread. Note, this closure ends up executing in the process referred to by
    // `process_handle`.
    let (sender, receiver) = sync_channel::<TaskBuilder>(1);
    let result = std::thread::Builder::new().name("user-thread".to_string()).spawn(move || {
        // It's safe to create a new lock context since we are on a new thread.
        #[allow(
            clippy::undocumented_unsafe_blocks,
            reason = "Force documented unsafe blocks in Starnix"
        )]
        let locked = unsafe { Unlocked::new() };

        // Note, cross-process shared resources allocated in this function that aren't freed by the
        // Zircon kernel upon thread and/or process termination (like mappings in the shared region)
        // should be freed using the delayed finalizer mechanism and Task drop.
        let mut current_task: CurrentTask = receiver
            .recv()
            .expect("caller should always send task builder before disconnecting")
            .into();

        // We don't need the receiver anymore. If we don't drop the receiver now, we'll keep it
        // allocated for the lifetime of the thread.
        std::mem::drop(receiver);

        let pre_run_result = { pre_run(locked, &mut current_task) };
        if pre_run_result.is_err() {
            // Only log if the pre run didn't exit the task. Otherwise, consider this is expected
            // by the caller.
            if current_task.exit_status().is_none() {
                log_error!("Pre run failed from {pre_run_result:?}. The task will not be run.");
            }

            // Drop the task_complete callback to ensure that the closure isn't holding any
            // releasables.
            std::mem::drop(task_complete);
        } else {
            let exit_status = enter_syscall_loop(locked, &mut current_task);
            current_task.write().set_exit_status(exit_status.clone());
            task_complete(Ok(exit_status));
        }

        // `release` must be called as the absolute last action on this thread to ensure that
        // any deferred release are done before it.
        current_task.release(locked);

        // Ensure that no releasables are registered after this point as we unwind the stack.
        DelayedReleaser::finalize();
    });
    let join_handle = match result {
        Ok(handle) => handle,
        Err(e) => {
            task_builder.release(locked);
            match e.kind() {
                std::io::ErrorKind::WouldBlock => return error!(EAGAIN),
                other => panic!("unexpected error on thread spawn: {other}"),
            }
        }
    };

    // Update the thread and task information before sending the task_builder to the spawned thread.
    // This will make sure the mapping between linux tid and fuchsia koid is set before trace events
    // are emitted from the linux code.

    // Set the task's thread handle
    let pthread = join_handle.as_pthread_t();
    #[allow(
        clippy::undocumented_unsafe_blocks,
        reason = "Force documented unsafe blocks in Starnix"
    )]
    let raw_thread_handle =
        unsafe { zx::Unowned::<'_, zx::Thread>::from_raw_handle(thrd_get_zx_handle(pthread)) };
    *task_thread_guard = Some(Arc::new(
        raw_thread_handle
            .duplicate(zx::Rights::SAME_RIGHTS)
            .expect("must have RIGHT_DUPLICATE on handle we created"),
    ));
    // Now that the task has a thread handle, update the thread's role using the policy configured.
    drop(task_thread_guard);
    if let Err(err) = ref_task.sync_scheduler_state_to_role() {
        log_warn!(err:?; "Couldn't update freshly spawned thread's profile.");
    }

    // Record the thread and process ids for tracing after the task_thread is unlocked.
    ref_task.record_pid_koid_mapping();

    // Wait to send the `TaskBuilder` to the spawned thread until we know that it
    // spawned successfully, as we need to ensure the builder is always explicitly
    // released.
    sender
        .send(task_builder)
        .expect("receiver should not be disconnected because thread spawned successfully");

    // We're done with the sender now. We drop the sender explicitly to free the memory earlier.
    std::mem::drop(sender);

    // Reset the process handle used to create threads.
    #[allow(
        clippy::undocumented_unsafe_blocks,
        reason = "Force documented unsafe blocks in Starnix"
    )]
    unsafe {
        thrd_set_zx_process(old_process_handle);
    };

    Ok(())
}

extern "C" {
    /// Sets the process handle used to create new threads, for the current thread.
    fn thrd_set_zx_process(handle: zx::sys::zx_handle_t) -> zx::sys::zx_handle_t;

    // Gets the thread handle underlying a specific thread.
    // In C the 'thread' parameter is thrd_t which on Fuchsia is the same as pthread_t.
    fn thrd_get_zx_handle(thread: u64) -> zx::sys::zx_handle_t;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::SignalInfo;
    use crate::task::StopState;
    use crate::testing::*;
    use starnix_uapi::signals::{SIGCONT, SIGSTOP};

    #[::fuchsia::test]
    async fn test_block_while_stopped_stop_and_continue() {
        spawn_kernel_and_run(async |locked, task| {
            // block_while_stopped must immediately returned if the task is not stopped.
            task.block_while_stopped(locked);

            // Stop the task.
            task.thread_group().set_stopped(
                StopState::GroupStopping,
                Some(SignalInfo::default(SIGSTOP)),
                false,
            );

            let thread = std::thread::spawn({
                let task = task.weak_task();
                move || {
                    let task = task.upgrade().expect("task must be alive");
                    // Wait for the task to have a waiter.
                    while !task.read().is_blocked() {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }

                    // Continue the task.
                    task.thread_group().set_stopped(
                        StopState::Waking,
                        Some(SignalInfo::default(SIGCONT)),
                        false,
                    );
                }
            });

            // Block until continued.
            task.block_while_stopped(locked);

            // Join the thread, which will ensure set_stopped terminated.
            thread.join().expect("joined");

            // The task should not be blocked anymore.
            task.block_while_stopped(locked);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_block_while_stopped_stop_and_exit() {
        spawn_kernel_and_run(async |locked, task| {
            // block_while_stopped must immediately returned if the task is neither stopped nor exited.
            task.block_while_stopped(locked);

            // Stop the task.
            task.thread_group().set_stopped(
                StopState::GroupStopping,
                Some(SignalInfo::default(SIGSTOP)),
                false,
            );

            let thread = std::thread::spawn({
                let task = task.weak_task();
                move || {
                    #[allow(
                        clippy::undocumented_unsafe_blocks,
                        reason = "Force documented unsafe blocks in Starnix"
                    )]
                    let locked = unsafe { Unlocked::new() };
                    let task = task.upgrade().expect("task must be alive");
                    // Wait for the task to have a waiter.
                    while !task.read().is_blocked() {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }

                    // exit the task.
                    task.thread_group().exit(locked, ExitStatus::Exit(1), None);
                }
            });

            // Block until continued.
            task.block_while_stopped(locked);

            // Join the task, which will ensure thread_group.exit terminated.
            thread.join().expect("joined");

            // The task should not be blocked because it is stopped.
            task.block_while_stopped(locked);
        })
        .await;
    }
}
