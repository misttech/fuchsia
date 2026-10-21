// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::power::WakeupSourceOrigin;
use crate::task::{
    CurrentTask, EventHandler, ReadyItem, ReadyItemKey, WaitCanceler, WaitQueue, Waiter,
};
use crate::vfs::{
    Anon, FileHandle, FileObject, FileObjectState, FileOps, WeakFileHandle, fileops_impl_dataless,
    fileops_impl_nonseekable, fileops_impl_noop_sync,
};
use rustc_hash::{FxHashMap, FxHashSet};
use starnix_logging::log_warn;
use starnix_sync::{EpollStateLock, EpollWaitableStateLock, LockDepMutex, allow_subclass};
use starnix_uapi::error;
use starnix_uapi::errors::{EINTR, ETIMEDOUT, Errno};
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::vfs::{EpollEvent, FdEvents};
use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::sync::Arc;

/// Maximum depth of epoll instances monitoring one another.
/// From https://man7.org/linux/man-pages/man2/epoll_ctl.2.html
const MAX_NESTED_DEPTH: u32 = 5;

/// WaitObject represents a FileHandle that is being waited upon.
/// The `data` field is a user defined quantity passed in
/// via `sys_epoll_ctl`. Typically C programs could use this
/// to store a pointer to the data that needs to be processed
/// after an event.
struct WaitObject {
    target: WeakFileHandle,
    events: FdEvents,
    data: u64,
    wait_canceler: Option<WaitCanceler>,
    active_wakeup_source: Option<WakeupSourceOrigin>,
}

impl WaitObject {
    /// Returns the target `FileHandle` of the `WaitObject`, or `None` if the file has been closed.
    ///
    /// It is fine for the `FileHandle` to be closed after being added to an epoll, and subsequent
    /// epoll_waits end up timing out (importantly not returning EBADF).
    fn target(&self) -> Option<FileHandle> {
        self.target.upgrade()
    }

    fn deactivate_wakeup_source(&mut self, current_task: &CurrentTask) {
        if let Some(origin) = self.active_wakeup_source.take() {
            current_task.kernel().suspend_resume_manager.deactivate_wakeup_source(&origin);
        }
    }
}

/// EpollKey acts as an key to a map of WaitObject.
/// In reality it is a pointer to a FileHandle object.
pub type EpollKey = usize;

/// EpollFileObject represents the FileObject used to
/// implement epoll_create1/epoll_ctl/epoll_pwait.
#[derive(Default)]
pub struct EpollFileObject {
    waiter: Waiter,
    /// Mutable state of this epoll object.
    state: LockDepMutex<EpollState, EpollStateLock>,
    waitable_state: Arc<LockDepMutex<EpollWaitableState, EpollWaitableStateLock>>,
    /// A list of waiters waiting for events from this
    /// epoll instance.
    waiters: Arc<WaitQueue>,
}

#[derive(Default)]
struct EpollState {
    /// Any file tracked by this epoll instance
    /// will exist as a key in `wait_objects`.
    ///
    /// The keys are kernel-assigned file object IDs, so the faster non-DoS-resistant
    /// `FxHashMap` is safe here.
    wait_objects: FxHashMap<ReadyItemKey, WaitObject>,
    /// processing_list is a FIFO of events that are being
    /// processed.
    ///
    /// Objects from the `EpollFileObject`'s `trigger_list` are moved into this
    /// list so that we can handle triggered events without holding its lock
    /// longer than we need to. This reduces contention with waited-on objects
    /// that tries to notify this epoll object on subscribed events.
    processing_list: VecDeque<ReadyItem>,
    /// recheck_list is the list of items that need to have query_events checked
    /// at the start of the next EpollFileObject::wait call. This is only items
    /// that were returned from the last wait call, because those are the only
    /// ones that might need to be returned even if no events come in between
    /// wait calls.
    recheck_list: Vec<ReadyItemKey>,
}

#[derive(Default)]
struct EpollWaitableState {
    /// trigger_list is a FIFO of events that have
    /// happened, but have not yet been processed.
    trigger_list: VecDeque<ReadyItem>,
}

impl EpollFileObject {
    /// Allocate a new, empty epoll object.
    pub fn new_file(current_task: &CurrentTask) -> FileHandle {
        let epoll = Box::new(EpollFileObject::default());

        #[cfg(any(test, debug_assertions))]
        {
            let _l1 = epoll.state.lock();
            let _l2 = epoll.waitable_state.lock();
        }

        Anon::new_private_file(current_task, epoll, OpenFlags::RDWR, "[eventpoll]")
    }

    fn wait_on_file(
        &self,
        current_task: &CurrentTask,
        key: ReadyItemKey,
        wait_object: &mut WaitObject,
    ) -> Result<(), Errno> {
        // First start the wait. If an event happens after this, we'll get it.
        self.wait_on_file_edge_triggered(current_task, key, wait_object)?;

        self.do_recheck(current_task, wait_object, key)?;

        Ok(())
    }

    fn do_recheck(
        &self,
        current_task: &CurrentTask,
        wait_object: &mut WaitObject,
        key: ReadyItemKey,
    ) -> Result<(), Errno> {
        let Some(target) = wait_object.target() else { return Ok(()) };
        let events = {
            // Target might be itself an epoll object. Because there is no loop,
            // this allow_subclass is safe.
            let _token = allow_subclass();
            target.query_events(current_task)?
        };
        if !(events & wait_object.events).is_empty() {
            self.waiter.wake_immediately(events, self.new_wait_handler(key));
            if let Some(wait_canceler) = wait_object.wait_canceler.take() {
                wait_canceler.cancel();
            } else {
                log_warn!("wait canceler should have been set by `wait_on_file_edge_triggered`");
            }
        }
        Ok(())
    }

    fn wait_on_file_edge_triggered(
        &self,
        current_task: &CurrentTask,
        key: ReadyItemKey,
        wait_object: &mut WaitObject,
    ) -> Result<(), Errno> {
        let Some(target) = wait_object.target() else {
            return Ok(());
        };

        wait_object.wait_canceler = target.wait_async(
            current_task,
            &self.waiter,
            wait_object.events,
            self.new_wait_handler(key),
        );
        if wait_object.wait_canceler.is_none() {
            return error!(EPERM);
        }
        Ok(())
    }

    /// Checks if adding self to the `epoll_file_object` at `epoll_file_handle` would cause a loop
    /// or exceed max depth.
    fn check_eloop(&self, parent: &FileHandle, depth_left: u32) -> Result<(), Errno> {
        if depth_left == 0 {
            return error!(ELOOP);
        }

        let state = self.state.lock();
        for nested_object in state.wait_objects.values() {
            let Some(child) = nested_object.target() else {
                continue;
            };
            let Some(child_file) = child.downcast_file::<EpollFileObject>() else {
                continue;
            };

            if Arc::ptr_eq(&child, parent) {
                return error!(ELOOP);
            }
            // Child is not part of a loop, so subclassing is safe.
            let _token = allow_subclass();
            child_file.check_eloop(parent, depth_left - 1)?;
        }

        Ok(())
    }

    /// Asynchronously wait on certain events happening on a FileHandle.
    pub fn add(
        &self,
        current_task: &CurrentTask,
        file: &FileHandle,
        epoll_file_handle: &FileHandle,
        epoll_event: EpollEvent,
    ) -> Result<(), Errno> {
        // Check if adding this file would cause a cycle at a max depth of 5.
        if let Some(epoll_to_add) = file.downcast_file::<EpollFileObject>() {
            // We need to check for `MAX_NESTED_DEPTH - 1` because adding `epoll_to_add` to self
            // would result in a total depth of one more.
            epoll_to_add.check_eloop(epoll_file_handle, MAX_NESTED_DEPTH - 1)?;
        }

        let mut state = self.state.lock();
        let key = file.id.as_epoll_key().into();
        match state.wait_objects.entry(key) {
            Entry::Occupied(_) => error!(EEXIST),
            Entry::Vacant(entry) => {
                let wait_object = entry.insert(WaitObject {
                    target: Arc::downgrade(file),
                    events: epoll_event.events() | FdEvents::POLLHUP | FdEvents::POLLERR,
                    data: epoll_event.data(),
                    wait_canceler: None,
                    active_wakeup_source: None,
                });
                self.wait_on_file(current_task, key, wait_object)
            }
        }
    }

    /// Modify the events we are looking for on a Filehandle.
    pub fn modify(
        &self,
        current_task: &CurrentTask,
        file: &FileHandle,
        epoll_event: EpollEvent,
    ) -> Result<(), Errno> {
        let mut state = self.state.lock();
        let key = file.id.as_epoll_key();
        state.recheck_list.retain(|x| *x != key.into());
        let Some(wait_object) = state.wait_objects.get_mut(&key.into()) else {
            return error!(ENOENT);
        };
        if let Some(wait_canceler) = wait_object.wait_canceler.take() {
            wait_canceler.cancel();
        }
        wait_object.events = epoll_event.events() | FdEvents::POLLHUP | FdEvents::POLLERR;
        wait_object.data = epoll_event.data();
        // If the new epoll event doesn't include EPOLLWAKEUP, we need to take down the
        // wake lease. This ensures that the system doesn't stay awake unnecessarily when
        // the event no longer requires it to be awake.
        if wait_object.events.contains(FdEvents::EPOLLWAKEUP)
            && !epoll_event.events().contains(FdEvents::EPOLLWAKEUP)
        {
            wait_object.deactivate_wakeup_source(current_task);
        }
        self.wait_on_file(current_task, key.into(), wait_object)
    }

    /// Cancel an asynchronous wait on an object. Events triggered before
    /// calling this will still be delivered.
    pub fn delete(&self, current_task: &CurrentTask, file: &FileObject) -> Result<(), Errno> {
        let mut state = self.state.lock();
        let key = file.id.as_epoll_key().into();
        if let Some(mut wait_object) = state.wait_objects.remove(&key) {
            if let Some(wait_canceler) = wait_object.wait_canceler.take() {
                wait_canceler.cancel();
            }
            state.recheck_list.retain(|x| *x != key);
            // Deactivate the wake lock if it was active.
            wait_object.deactivate_wakeup_source(current_task);
            Ok(())
        } else {
            error!(ENOENT)
        }
    }

    /// Stores events from the Epoll's trigger list to the parameter `pending_list`. This does not
    /// actually invoke the waiter which is how items are added to the trigger list. The caller
    /// will have to do that before calling if needed.
    ///
    /// If an event in the trigger list is stale, the event will be re-added to the waiter.
    ///
    /// Returns true if any events were added. False means there was nothing in the trigger list.
    fn process_triggered_events(
        &self,
        current_task: &CurrentTask,
        pending_list: &mut Vec<ReadyItem>,
        max_events: usize,
    ) -> Result<(), Errno> {
        let mut state = self.state.lock();
        // Move all the elements from `self.trigger_list` to this intermediary
        // queue that we handle events from. This reduces the time spent holding
        // `self.trigger_list`'s lock which reduces contention with objects that
        // this epoll object has subscribed for notifications from.
        state.processing_list.append(&mut self.waitable_state.lock().trigger_list);
        while pending_list.len() < max_events && !state.processing_list.is_empty() {
            if let Some(pending) = state.processing_list.pop_front() {
                if let Some(wait) = state.wait_objects.get_mut(&pending.key) {
                    // The weak pointer to the FileObject target can be gone if the file was closed
                    // out from under us. If this happens it is not an error: ignore it and
                    // continue.
                    if let Some(target) = wait.target.upgrade() {
                        let events = {
                            // Target might be itself an epoll object. Because there is no loop,
                            // this allow_subclass is safe.
                            let _token = allow_subclass();
                            target.query_events(current_task)?
                        };
                        let ready = ReadyItem { key: pending.key, events };
                        if ready.events.intersects(wait.events) {
                            pending_list.push(ready);
                        } else {
                            // Another thread already handled this event, wait for another one.
                            self.wait_on_file(current_task, pending.key, wait)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Waits until an event exists in `pending_list` or until `timeout` has
    /// been reached.
    fn wait_until_pending_event(
        &self,
        current_task: &CurrentTask,
        max_events: usize,
        mut wait_deadline: zx::MonotonicInstant,
    ) -> Result<Vec<ReadyItem>, Errno> {
        let mut pending_list = Vec::new();

        loop {
            self.process_triggered_events(current_task, &mut pending_list, max_events)?;

            if pending_list.len() == max_events {
                break; // No input events or output list full, nothing more we can do.
            }

            if !pending_list.is_empty() {
                // We now know we have at least one event to return. We shouldn't return
                // immediately, in case there are more events available, but the next loop should
                // wait with a 0 timeout to prevent further blocking.
                wait_deadline = zx::MonotonicInstant::ZERO;
            }

            // Loop back to check if there are more items in the Waiter's queue. Every wait_until()
            // call will process a single event. In order to drain as many events as we can that
            // are synchronously available, keep trying until it reports empty.
            //
            // The handlers in the waits cause items to be appended to trigger_list. See the closure
            // in `wait_on_file` to see how this happens.
            //
            // This wait may return EINTR for nonzero timeouts which is not an error. We must be
            // careful not to lose events if this happens.
            //
            // The first time through this loop we'll use the timeout passed into this function so
            // can get EINTR. But since we haven't done anything or accumulated any results yet it's
            // OK to immediately return and no information will be lost.
            match self.waiter.wait_until(current_task, wait_deadline) {
                Err(err) if err == ETIMEDOUT => break,
                Err(err) if err == EINTR => {
                    // Terminating early will lose any events in the pending_list so that should
                    // only be for unrecoverable errors (not EINTR). The only time there should be a
                    // nonzero wait_deadline (and hence the ability to encounter EINTR) is when the
                    // pending list is empty.
                    debug_assert!(
                        pending_list.is_empty(),
                        "Got EINTR from wait of {}ns with {} items pending.",
                        wait_deadline.into_nanos(),
                        pending_list.len()
                    );
                    return Err(err);
                }
                // TODO check if this is supposed to actually fail!
                result => result?,
            }
        }

        Ok(pending_list)
    }

    /// Blocking wait on all waited upon events with a timeout.
    pub fn wait(
        &self,
        current_task: &CurrentTask,
        max_events: usize,
        deadline: zx::MonotonicInstant,
    ) -> Result<Vec<EpollEvent>, Errno> {
        {
            let mut state = self.state.lock();
            let recheck_list = std::mem::take(&mut state.recheck_list);
            for key in &recheck_list {
                let wait_object = state.wait_objects.get_mut(key).unwrap();
                wait_object.deactivate_wakeup_source(current_task);
            }
            for (idx, key) in recheck_list.iter().enumerate() {
                let wait_object = state.wait_objects.get_mut(key).unwrap();
                if let Err(err) = self.do_recheck(current_task, wait_object, *key) {
                    // Put remaining unprocessed entries (including the failing one) back
                    // onto `recheck_list` so they aren't lost and can be rechecked on the next wait.
                    state.recheck_list.extend(recheck_list[idx..].iter().copied());
                    return Err(err);
                }
            }
        }

        let pending_list = self.wait_until_pending_event(current_task, max_events, deadline)?;

        // Process the pending list and add processed ReadyItem
        // entries to the rearm_list for the next wait.
        let mut result = vec![];
        let mut state = self.state.lock();
        let state = &mut *state;
        let mut seen = FxHashSet::with_capacity_and_hasher(pending_list.len(), Default::default());
        for pending_event in pending_list.iter().filter(|e| seen.insert(e.key)) {
            // The wait could have been deleted by here,
            // so ignore the None case.
            let Some(wait) = state.wait_objects.get_mut(&pending_event.key) else { continue };

            let reported_events = pending_event.events & wait.events;
            result.push(EpollEvent::new(reported_events, wait.data));

            // Files marked with `EPOLLONESHOT` should only notify
            // once and need to be rearmed manually with epoll_ctl_mod().
            if wait.events.contains(FdEvents::EPOLLONESHOT) {
                continue;
            }

            self.wait_on_file_edge_triggered(current_task, pending_event.key, wait)?;

            if !wait.events.contains(FdEvents::EPOLLET) {
                state.recheck_list.push(pending_event.key);
            }

            // TODO: is this really only supposed to happen for level-triggered events?
            if !wait.events.contains(FdEvents::EPOLLET) {
                // When this is the first time epoll_wait on this epoll fd, create and
                // hold a wake lease until the next epoll_wait.
                if wait.events.contains(FdEvents::EPOLLWAKEUP) {
                    if let ReadyItemKey::Usize(key) = pending_event.key {
                        let origin = WakeupSourceOrigin::Epoll(key);
                        current_task
                            .kernel()
                            .suspend_resume_manager
                            .activate_wakeup_source_with_actor(
                                origin.clone(),
                                Some(current_task.command()),
                            );
                        wait.active_wakeup_source = Some(origin);
                    }
                }
            }
        }

        Ok(result)
    }
}

impl FileOps for EpollFileObject {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();
    fileops_impl_dataless!();

    fn wait_async(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        Some(self.waiters.wait_async_fd_events(waiter, events, handler))
    }

    fn query_events(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        let mut events = FdEvents::empty();
        let state = self.state.lock();
        if !state.processing_list.is_empty() || !self.waitable_state.lock().trigger_list.is_empty()
        {
            events |= FdEvents::POLLIN;
        } else {
            for key in &state.recheck_list {
                let wait_object = state.wait_objects.get(key).unwrap();
                let Some(target) = wait_object.target() else { continue };
                // Target might be itself an epoll object. Because there is no loop,
                // this allow_subclass is safe.
                let _token = allow_subclass();
                if !(target.query_events(current_task)? & wait_object.events).is_empty() {
                    events |= FdEvents::POLLIN;
                    break;
                }
            }
        }
        Ok(events)
    }

    fn close(self: Box<Self>, _file: &FileObjectState, current_task: &CurrentTask) {
        let mut guard = self.state.lock();
        for wait_object in guard.wait_objects.values_mut() {
            wait_object.deactivate_wakeup_source(current_task);
        }
    }
}

#[derive(Clone)]
pub struct EpollEventHandler {
    key: ReadyItemKey,
    waitable_state: Arc<LockDepMutex<EpollWaitableState, EpollWaitableStateLock>>,
    waiters: Arc<WaitQueue>,
}

impl EpollEventHandler {
    pub fn handle(self, events: FdEvents) {
        {
            let mut waitable_state = self.waitable_state.lock();
            waitable_state.trigger_list.push_back(ReadyItem { key: self.key, events });
        }
        self.waiters.notify_fd_events(FdEvents::POLLIN);
    }
}

impl EpollFileObject {
    fn new_wait_handler(&self, key: ReadyItemKey) -> EventHandler {
        EventHandler::Epoll(EpollEventHandler {
            key,
            waitable_state: Arc::clone(&self.waitable_state),
            waiters: Arc::clone(&self.waiters),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::fuchsia::create_fuchsia_pipe;
    use crate::task::Waiter;
    use crate::task::dynamic_thread_spawner::SpawnRequestBuilder;
    use crate::testing::{anon_test_file, spawn_kernel_and_run};
    use crate::vfs::buffers::{VecInputBuffer, VecOutputBuffer};
    use crate::vfs::eventfd::{EventFdType, new_eventfd};
    use crate::vfs::fs_registry::FsRegistry;
    use crate::vfs::pipe::{new_pipe, register_pipe_fs};
    use crate::vfs::socket::{SocketDomain, SocketType, UnixSocket};
    use starnix_lifecycle::AtomicCounter;
    use starnix_sync::Mutex;
    use starnix_uapi::vfs::{EpollEvent, FdEvents};
    use syncio::Zxio;

    #[::fuchsia::test]
    async fn test_epoll_read_ready() {
        static WRITE_COUNT: AtomicCounter<usize> = AtomicCounter::<usize>::new_const(0);
        const EVENT_DATA: u64 = 42;

        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();

            let test_string = "hello starnix".to_string();
            let test_len = test_string.len();

            let epoll_file_handle = EpollFileObject::new_file(&current_task);
            let epoll_file = epoll_file_handle.downcast_file::<EpollFileObject>().unwrap();
            epoll_file
                .add(
                    &current_task,
                    &pipe_out,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN, EVENT_DATA),
                )
                .unwrap();

            let (sender, receiver) = std::sync::mpsc::channel();
            let value = test_string.clone();
            let closure = move |task: &CurrentTask| {
                let bytes_written =
                    pipe_in.write(&task, &mut VecInputBuffer::new(value.as_bytes())).unwrap();
                assert_eq!(bytes_written, test_len);
                WRITE_COUNT.add(bytes_written);
                sender.send(()).unwrap();
            };
            let req = SpawnRequestBuilder::new().with_sync_closure(closure).build();
            kernel.kthreads.spawner().spawn_from_request(req);
            let events =
                epoll_file.wait(&current_task, 10, zx::MonotonicInstant::INFINITE).unwrap();
            receiver.recv().unwrap();
            assert_eq!(1, events.len());
            let event = &events[0];
            assert!(event.events().contains(FdEvents::POLLIN));
            assert_eq!(event.data(), EVENT_DATA);

            let mut buffer = VecOutputBuffer::new(test_len);
            let bytes_read = pipe_out.read(&current_task, &mut buffer).unwrap();
            assert_eq!(bytes_read, WRITE_COUNT.get());
            assert_eq!(bytes_read, test_len);
            assert_eq!(buffer.data(), test_string.as_bytes());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_epoll_ready_then_wait() {
        const EVENT_DATA: u64 = 42;

        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();

            let test_string = "hello starnix".to_string();
            let test_bytes = test_string.as_bytes();
            let test_len = test_bytes.len();

            assert_eq!(
                pipe_in.write(&current_task, &mut VecInputBuffer::new(test_bytes)).unwrap(),
                test_bytes.len()
            );

            let epoll_file_handle = EpollFileObject::new_file(&current_task);
            let epoll_file = epoll_file_handle.downcast_file::<EpollFileObject>().unwrap();
            epoll_file
                .add(
                    &current_task,
                    &pipe_out,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN, EVENT_DATA),
                )
                .unwrap();

            let events =
                epoll_file.wait(&current_task, 10, zx::MonotonicInstant::INFINITE).unwrap();
            assert_eq!(1, events.len());
            let event = &events[0];
            assert!(event.events().contains(FdEvents::POLLIN));
            assert_eq!(event.data(), EVENT_DATA);

            let mut buffer = VecOutputBuffer::new(test_len);
            let bytes_read = pipe_out.read(&current_task, &mut buffer).unwrap();
            assert_eq!(bytes_read, test_len);
            assert_eq!(buffer.data(), test_bytes);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_epoll_ctl_cancel() {
        spawn_kernel_and_run(async |current_task| {
            for do_cancel in [true, false] {
                let event = new_eventfd(&current_task, 0, EventFdType::Counter, true);
                let waiter = Waiter::new();

                let epoll_file_handle = EpollFileObject::new_file(&current_task);
                let epoll_file = epoll_file_handle.downcast_file::<EpollFileObject>().unwrap();
                const EVENT_DATA: u64 = 42;
                epoll_file
                    .add(
                        &current_task,
                        &event,
                        &epoll_file_handle,
                        EpollEvent::new(FdEvents::POLLIN, EVENT_DATA),
                    )
                    .unwrap();

                if do_cancel {
                    epoll_file.delete(&current_task, &event).unwrap();
                }

                let wait_canceler = event
                    .wait_async(&current_task, &waiter, FdEvents::POLLIN, EventHandler::None)
                    .expect("wait_async");
                if do_cancel {
                    wait_canceler.cancel();
                }

                let add_val = 1u64;

                assert_eq!(
                    event
                        .write(&current_task, &mut VecInputBuffer::new(&add_val.to_ne_bytes()))
                        .unwrap(),
                    std::mem::size_of::<u64>()
                );

                let events =
                    epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();

                if do_cancel {
                    assert_eq!(0, events.len());
                } else {
                    assert_eq!(1, events.len());
                    let event = &events[0];
                    assert!(event.events().contains(FdEvents::POLLIN));
                    assert_eq!(event.data(), EVENT_DATA);
                }
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_multiple_events() {
        spawn_kernel_and_run(async |current_task| {
            let (client1, server1) = zx::Socket::create_stream();
            let (client2, server2) = zx::Socket::create_stream();
            let pipe1 = create_fuchsia_pipe(&current_task, client1, OpenFlags::RDWR)
                .expect("create_fuchsia_pipe");
            let pipe2 = create_fuchsia_pipe(&current_task, client2, OpenFlags::RDWR)
                .expect("create_fuchsia_pipe");
            let server1_zxio = Zxio::create(server1.into_handle()).expect("Zxio::create");
            let server2_zxio = Zxio::create(server2.into_handle()).expect("Zxio::create");

            let poll = || {
                let epoll_object = EpollFileObject::new_file(&current_task);
                let epoll_file = epoll_object.downcast_file::<EpollFileObject>().unwrap();
                epoll_file
                    .add(&current_task, &pipe1, &epoll_object, EpollEvent::new(FdEvents::POLLIN, 1))
                    .expect("epoll_file.add");
                epoll_file
                    .add(&current_task, &pipe2, &epoll_object, EpollEvent::new(FdEvents::POLLIN, 2))
                    .expect("epoll_file.add");
                epoll_file.wait(&current_task, 2, zx::MonotonicInstant::ZERO).expect("wait")
            };

            let fds = poll();
            assert!(fds.is_empty());

            assert_eq!(server1_zxio.write(&[0]).expect("write"), 1);

            let fds = poll();
            assert_eq!(fds.len(), 1);
            assert_eq!(fds[0].events(), FdEvents::POLLIN);
            assert_eq!(fds[0].data(), 1);
            assert_eq!(pipe1.read(&current_task, &mut VecOutputBuffer::new(64)).expect("read"), 1);

            let fds = poll();
            assert!(fds.is_empty());

            assert_eq!(server2_zxio.write(&[0]).expect("write"), 1);

            let fds = poll();
            assert_eq!(fds.len(), 1);
            assert_eq!(fds[0].events(), FdEvents::POLLIN);
            assert_eq!(fds[0].data(), 2);
            assert_eq!(pipe2.read(&current_task, &mut VecOutputBuffer::new(64)).expect("read"), 1);

            let fds = poll();
            assert!(fds.is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_cancel_after_notify() {
        spawn_kernel_and_run(async |current_task| {
            let event = new_eventfd(&current_task, 0, EventFdType::Counter, true);
            let epoll_file_handle = EpollFileObject::new_file(&current_task);
            let epoll_file = epoll_file_handle.downcast_file::<EpollFileObject>().unwrap();

            // Add a thing
            const EVENT_DATA: u64 = 42;
            epoll_file
                .add(
                    &current_task,
                    &event,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN, EVENT_DATA),
                )
                .unwrap();

            // Make the thing send a notification, wait for it
            let add_val = 1u64;
            assert_eq!(
                event
                    .write(&current_task, &mut VecInputBuffer::new(&add_val.to_ne_bytes()))
                    .unwrap(),
                std::mem::size_of::<u64>()
            );

            assert_eq!(
                epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap().len(),
                1
            );

            // Remove the thing
            epoll_file.delete(&current_task, &event).unwrap();

            // Wait for new notifications
            assert_eq!(
                epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap().len(),
                0
            );
            // That shouldn't crash
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_add_then_modify() {
        spawn_kernel_and_run(async |current_task| {
            let (socket1, _socket2) = UnixSocket::new_pair(
                &current_task,
                SocketDomain::Unix,
                SocketType::Stream,
                OpenFlags::RDWR,
            )
            .expect("Failed to create socket pair.");

            let epoll_file_handle = EpollFileObject::new_file(&current_task);
            let epoll_file = epoll_file_handle.downcast_file::<EpollFileObject>().unwrap();

            const EVENT_DATA: u64 = 42;
            epoll_file
                .add(
                    &current_task,
                    &socket1,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN, EVENT_DATA),
                )
                .unwrap();
            assert_eq!(
                epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap().len(),
                0
            );

            let read_write_event = FdEvents::POLLIN | FdEvents::POLLOUT;
            epoll_file
                .modify(&current_task, &socket1, EpollEvent::new(read_write_event, EVENT_DATA))
                .unwrap();
            let triggered_events =
                epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(1, triggered_events.len());
            let event = &triggered_events[0];
            assert_eq!(event.events(), FdEvents::POLLOUT);
            assert_eq!(event.data(), EVENT_DATA);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_waiter_removal() {
        spawn_kernel_and_run(async |current_task| {
            let event = new_eventfd(&current_task, 0, EventFdType::Counter, true);
            let epoll_file_handle = EpollFileObject::new_file(&current_task);
            let epoll_file = epoll_file_handle.downcast_file::<EpollFileObject>().unwrap();

            const EVENT_DATA: u64 = 42;
            epoll_file
                .add(
                    &current_task,
                    &event,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN, EVENT_DATA),
                )
                .unwrap();

            std::mem::drop(event);

            assert!(epoll_file.waiters.is_empty());
        })
        .await;
    }

    /// A test file whose event query results can be dynamically controlled.
    ///
    /// This allows tests to simulate scenarios where `query_events` succeeds initially
    /// (e.g., during epoll registration and the first `wait`), but returns an error on a
    /// subsequent call (e.g., when `do_recheck` re-evaluates `recheck_list`).
    #[derive(Clone)]
    struct ControlledEventsFile {
        wait_queue: Arc<WaitQueue>,
        events: Arc<Mutex<Result<FdEvents, Errno>>>,
    }

    impl ControlledEventsFile {
        fn new(events: FdEvents) -> Self {
            Self {
                wait_queue: Arc::new(WaitQueue::default()),
                events: Arc::new(Mutex::new(Ok(events))),
            }
        }

        fn set_events(&self, events: Result<FdEvents, Errno>) {
            *self.events.lock() = events;
        }
    }

    impl FileOps for ControlledEventsFile {
        fileops_impl_nonseekable!();
        fileops_impl_noop_sync!();
        fileops_impl_dataless!();

        fn wait_async(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            waiter: &Waiter,
            events: FdEvents,
            handler: EventHandler,
        ) -> Option<WaitCanceler> {
            Some(self.wait_queue.wait_async_fd_events(waiter, events, handler))
        }

        fn query_events(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
        ) -> Result<FdEvents, Errno> {
            self.events.lock().clone()
        }
    }

    #[::fuchsia::test]
    async fn test_epoll_recheck_failure_deactivates_wakeup_sources() {
        spawn_kernel_and_run(async |current_task| {
            let epoll_file_handle = EpollFileObject::new_file(&current_task);
            let epoll_file = epoll_file_handle.downcast_file::<EpollFileObject>().unwrap();

            let ops1 = ControlledEventsFile::new(FdEvents::POLLIN);
            let file1 = anon_test_file(&current_task, Box::new(ops1.clone()), OpenFlags::RDWR);
            let ops2 = ControlledEventsFile::new(FdEvents::POLLIN);
            let file2 = anon_test_file(&current_task, Box::new(ops2.clone()), OpenFlags::RDWR);

            epoll_file
                .add(
                    &current_task,
                    &file1,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN | FdEvents::EPOLLWAKEUP, 1),
                )
                .unwrap();
            epoll_file
                .add(
                    &current_task,
                    &file2,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN | FdEvents::EPOLLWAKEUP, 2),
                )
                .unwrap();

            let events = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events.len(), 2);
            assert!(!current_task.kernel().suspend_resume_manager.lock().can_suspend());

            ops1.set_events(error!(EPERM));
            ops2.set_events(error!(EPERM));

            assert!(epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).is_err());
            assert!(current_task.kernel().suspend_resume_manager.lock().can_suspend());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_level_triggered_remains_ready_until_drained() {
        // Validates standard level-triggered epoll semantics:
        // A level-triggered file remains ready across consecutive wait() calls as long
        // as data remains in its buffer. Once drained, wait() returns 0 events.
        // Once new data arrives, wait() returns ready again.
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let epoll_file_handle = EpollFileObject::new_file(&current_task);
            let epoll_file = epoll_file_handle.downcast_file::<EpollFileObject>().unwrap();
            const EVENT_DATA: u64 = 100;
            epoll_file
                .add(
                    &current_task,
                    &pipe_out,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN, EVENT_DATA),
                )
                .unwrap();

            let key: ReadyItemKey = pipe_out.id.as_epoll_key().into();

            // Write 2 bytes.
            pipe_in.write(&current_task, &mut VecInputBuffer::new(&[1, 2])).unwrap();

            // First wait: harvests ready event.
            let events = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].data(), EVENT_DATA);

            // Level-triggered files are added to recheck_list so subsequent waits recheck readiness.
            assert!(epoll_file.state.lock().recheck_list.contains(&key));

            // Second wait without reading data: the file is STILL READY and must be returned again.
            let events = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].data(), EVENT_DATA);

            // Drain the pipe completely.
            let mut out_buf = VecOutputBuffer::new(10);
            assert_eq!(pipe_out.read(&current_task, &mut out_buf).unwrap(), 2);

            // Third wait: the file is no longer ready, wait returns 0 events.
            let events = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events.len(), 0);

            // Write new data: the wait fires and returns the event.
            pipe_in.write(&current_task, &mut VecInputBuffer::new(&[3])).unwrap();
            let events = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].data(), EVENT_DATA);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_nested_epoll_delayed_drain() {
        // In nested epoll (ep_outer -> ep_inner -> target), when target has data:
        // 1. ep_inner.wait() returns target.
        // 2. Target is not drained immediately (delayed drain).
        // 3. ep_outer.wait() observes ep_inner is ready.
        // 4. Target is then drained by user space.
        // 5. Remote peer writes new data to target.
        // 6. ep_outer and ep_inner MUST both receive notifications for the new data and not hang.
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let epoll_inner_handle = EpollFileObject::new_file(&current_task);
            let epoll_inner = epoll_inner_handle.downcast_file::<EpollFileObject>().unwrap();
            let epoll_outer_handle = EpollFileObject::new_file(&current_task);
            let epoll_outer = epoll_outer_handle.downcast_file::<EpollFileObject>().unwrap();

            const INNER_DATA: u64 = 1;
            const OUTER_DATA: u64 = 2;

            epoll_inner
                .add(
                    &current_task,
                    &pipe_out,
                    &epoll_inner_handle,
                    EpollEvent::new(FdEvents::POLLIN, INNER_DATA),
                )
                .unwrap();
            epoll_outer
                .add(
                    &current_task,
                    &epoll_inner_handle,
                    &epoll_outer_handle,
                    EpollEvent::new(FdEvents::POLLIN, OUTER_DATA),
                )
                .unwrap();

            // Step 1: Write initial data to pipe.
            pipe_in.write(&current_task, &mut VecInputBuffer::new(&[1, 2])).unwrap();

            // Step 2: Harvest in ep_inner.wait().
            let inner_events =
                epoll_inner.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(inner_events.len(), 1);

            // Step 3: Do NOT drain pipe_out yet. It is still ready.
            // Step 4: ep_outer waits on ep_inner.
            let outer_events =
                epoll_outer.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(outer_events.len(), 1);
            assert_eq!(outer_events[0].data(), OUTER_DATA);

            // Step 5: User drains pipe_out now.
            let mut out_buf = VecOutputBuffer::new(10);
            assert_eq!(pipe_out.read(&current_task, &mut out_buf).unwrap(), 2);

            // Step 6: Remote peer sends new data to pipe_out.
            pipe_in.write(&current_task, &mut VecInputBuffer::new(&[3, 4])).unwrap();

            // Step 7: ep_outer must observe the new event without hanging.
            let outer_events2 =
                epoll_outer.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(outer_events2.len(), 1);
            assert_eq!(outer_events2[0].data(), OUTER_DATA);

            // ep_inner also sees the new event.
            let inner_events2 =
                epoll_inner.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(inner_events2.len(), 1);
            assert_eq!(inner_events2[0].data(), INNER_DATA);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_epoll_wait_with_external_waiter_preserves_wait_canceler() {
        // Validates that external waiters registered via wait_async on an epoll object
        // correctly receive event notifications when target events occur.
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let epoll_file_handle = EpollFileObject::new_file(&current_task);
            let epoll_file = epoll_file_handle.downcast_file::<EpollFileObject>().unwrap();
            const EVENT_DATA: u64 = 42;
            epoll_file
                .add(
                    &current_task,
                    &pipe_out,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN, EVENT_DATA),
                )
                .unwrap();

            pipe_in.write(&current_task, &mut VecInputBuffer::new(&[1])).unwrap();

            // Register an external waiter on epoll_file.
            let waiter = Waiter::new();
            let canceler = epoll_file_handle
                .wait_async(&current_task, &waiter, FdEvents::POLLIN, EventHandler::None)
                .expect("wait_async on epoll_file");

            assert!(!epoll_file.waiters.is_empty());

            // First wait on epoll_file while external waiter is registered.
            let events = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events.len(), 1);

            // Cancel the external waiter.
            canceler.cancel();
            assert!(epoll_file.waiters.is_empty());

            // Second wait continues to function properly.
            let events = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events.len(), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_multithreaded_epoll_concurrent_recheck() {
        // Validates multithreaded behavior when multiple workers use the same epoll instance.
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let epoll_file_handle = EpollFileObject::new_file(&current_task);
            let epoll_file = epoll_file_handle.downcast_file::<EpollFileObject>().unwrap();
            const EVENT_DATA: u64 = 42;
            epoll_file
                .add(
                    &current_task,
                    &pipe_out,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN, EVENT_DATA),
                )
                .unwrap();

            // Write 2 bytes.
            pipe_in.write(&current_task, &mut VecInputBuffer::new(&[1, 2])).unwrap();

            // Worker 1 harvests the event.
            let events1 = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events1.len(), 1);

            // Worker 2 calls wait() without any read occurring yet:
            // Worker 2's do_recheck observes data is still present and returns it.
            let events2 = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events2.len(), 1);

            // Now worker 1 drains the pipe.
            let mut buf = VecOutputBuffer::new(10);
            assert_eq!(pipe_out.read(&current_task, &mut buf).unwrap(), 2);

            // Worker 2 calls wait():
            // Pre-sleep do_recheck finds pipe empty, returns 0 events.
            let events3 = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events3.len(), 0);

            // Remote peer writes new data: wait fires for Worker 1.
            pipe_in.write(&current_task, &mut VecInputBuffer::new(&[3])).unwrap();
            let events4 = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events4.len(), 1);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_epoll_query_events_checks_recheck_list() {
        // When an outer epoll checks whether an inner epoll is ready, query_events()
        // inspects state.recheck_list directly if processing_list and trigger_list are empty.
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let epoll_file_handle = EpollFileObject::new_file(&current_task);
            let epoll_file = epoll_file_handle.downcast_file::<EpollFileObject>().unwrap();
            epoll_file
                .add(
                    &current_task,
                    &pipe_out,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN, 42),
                )
                .unwrap();

            pipe_in.write(&current_task, &mut VecInputBuffer::new(&[1, 2])).unwrap();

            // Harvesting the event leaves pipe_out on recheck_list.
            let events = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events.len(), 1);

            // query_events on the epoll file checks recheck_list and returns POLLIN.
            let queried = epoll_file_handle.query_events(&current_task).unwrap();
            assert!(queried.contains(FdEvents::POLLIN));

            // Drain the pipe.
            let mut buf = VecOutputBuffer::new(10);
            assert_eq!(pipe_out.read(&current_task, &mut buf).unwrap(), 2);

            // Now query_events sees the file on recheck_list is empty, so returns empty events.
            let queried = epoll_file_handle.query_events(&current_task).unwrap();
            assert!(!queried.contains(FdEvents::POLLIN));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_recheck_list_query_error_preserves_remaining_keys() {
        // In wait(), if do_recheck on one item fails with an error (e.g. query_events returns Err),
        // remaining unprocessed items must not be dropped from recheck_list.
        spawn_kernel_and_run(async |current_task| {
            let epoll_file_handle = EpollFileObject::new_file(&current_task);
            let epoll_file = epoll_file_handle.downcast_file::<EpollFileObject>().unwrap();

            let ops1 = ControlledEventsFile::new(FdEvents::POLLIN);
            let file1 = anon_test_file(&current_task, Box::new(ops1.clone()), OpenFlags::RDWR);
            let ops2 = ControlledEventsFile::new(FdEvents::POLLIN);
            let file2 = anon_test_file(&current_task, Box::new(ops2.clone()), OpenFlags::RDWR);

            let _key1: ReadyItemKey = file1.id.as_epoll_key().into();
            let key2: ReadyItemKey = file2.id.as_epoll_key().into();

            epoll_file
                .add(
                    &current_task,
                    &file1,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN, 1),
                )
                .unwrap();
            epoll_file
                .add(
                    &current_task,
                    &file2,
                    &epoll_file_handle,
                    EpollEvent::new(FdEvents::POLLIN, 2),
                )
                .unwrap();

            // First wait: both files are ready and returned.
            let events = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events.len(), 2);

            // Both files are on recheck_list.
            assert_eq!(epoll_file.state.lock().recheck_list.len(), 2);

            // Make file1 return an error on query_events.
            ops1.set_events(error!(EPERM));

            // Second wait: do_recheck fails on file1 and returns Err.
            assert!(epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).is_err());

            // Unprocessed items must be preserved on recheck_list.
            assert!(
                epoll_file.state.lock().recheck_list.contains(&key2),
                "file2 should remain on recheck_list despite file1's error"
            );

            // Delete file1 so it does not fail subsequent calls.
            epoll_file.delete(&current_task, &file1).unwrap();

            // Third wait: file2 is still ready and should be returned now.
            let events = epoll_file.wait(&current_task, 10, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].data(), 2);
        })
        .await;
    }
}
