// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(non_upper_case_globals)]

use crate::objects::{
    BinderObject, BinderObjectFlags, BinderObjectRef, Handle, LocalBinderObject, RefCountActions,
    StrongRefGuard,
};
use crate::resource_accessor::{
    RemoteMemoryAccessor, RemoteResourceAccessor, ResourceAccessor, get_resource_accessor,
};
use crate::shared_memory::SharedMemory;
use crate::thread::{
    BinderThread, BinderThreadState, Command, CommandQueueWithWaitQueue, generate_dead_replies,
};
use starnix_core::mm::MemoryAccessor;
use starnix_core::mm::memory::MemoryObject;
use starnix_core::mutable_state::Guard;
use starnix_core::task::{CurrentTask, Kernel, Task, ThreadGroupKey};
use starnix_core::vfs::FdNumber;
use starnix_logging::{log_trace, log_warn, track_stub};
use starnix_sync::{Mutex, MutexGuard};
use starnix_types::ownership::{
    DropGuard, OwnedRef, Releasable, ReleaseGuard, Share, TempRef, WeakRef, release_after,
};
use starnix_uapi::errors::Errno;
use starnix_uapi::user_address::UserAddress;
use starnix_uapi::{
    binder_driver_command_protocol, binder_driver_command_protocol_BC_ACQUIRE,
    binder_driver_command_protocol_BC_ACQUIRE_DONE, binder_driver_command_protocol_BC_DECREFS,
    binder_driver_command_protocol_BC_INCREFS, binder_driver_command_protocol_BC_INCREFS_DONE,
    binder_driver_command_protocol_BC_RELEASE, binder_frozen_state_info, binder_uintptr_t, errno,
    error, pid_t,
};
use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::vec::Vec;

#[derive(Debug, Default)]
pub struct BinderProcessState {
    /// Maximum number of thread to spawn.
    pub max_thread_count: usize,
    /// Whether a new thread has been requested, but not registered yet.
    pub thread_requested: bool,
    /// The set of threads that are interacting with the binder driver.
    pub thread_pool: ThreadPool,
    /// Binder objects hosted by the process shared with other processes.
    pub objects: BTreeMap<UserAddress, Arc<BinderObject>>,
    /// Handle table of remote binder objects.
    pub handles: ReleaseGuard<HandleTable>,
    /// State associated with active transactions, keyed by the userspace addresses of the buffers
    /// allocated to them. When the process frees a transaction buffer with `BC_FREE_BUFFER`, the
    /// state is dropped, releasing temporary strong references and the memory allocated to the
    /// transaction.
    pub active_transactions: BTreeMap<UserAddress, ReleaseGuard<ActiveTransaction>>,
    /// The list of processes that should be notified if this process dies.
    pub death_subscribers: Vec<(WeakRef<BinderProcess>, binder_uintptr_t)>,
    /// The list of processes that should be notified if this process is frozen.
    pub freeze_subscribers: Vec<(WeakRef<BinderProcess>, binder_uintptr_t)>,
    /// Whether the binder connection for this process is closed. Once closed, any blocking
    /// operation will be aborted and return an EBADF error.
    pub closed: bool,
    /// Whether the binder connection for this process is interrupted. A process that is
    /// interrupted either just before or while waiting must abort the operation and return a EINTR
    /// error.
    pub interrupted: bool,
    /// Status of the binder freeze.
    pub freeze_status: FreezeStatus,
}

impl BinderProcessState {
    pub fn freeze(&mut self) {
        self.freeze_status.frozen = true;
        self.freeze_status.has_async_recv = false;
        self.freeze_status.has_sync_recv = false;
        self.freeze_subscribers.retain(|(proc, cookie)| {
            let Some(proc) = proc.upgrade() else {
                return false; // remove if the process is already dead
            };
            proc.enqueue_command(Command::FrozenBinder(binder_frozen_state_info {
                cookie: *cookie,
                is_frozen: 1,
                reserved: 0,
            }));
            true
        });
    }

    pub fn thaw(&mut self) {
        self.freeze_status.frozen = false;
        self.freeze_status.has_async_recv = false;
        self.freeze_status.has_sync_recv = false;
        self.freeze_subscribers.retain(|(proc, cookie)| {
            let Some(proc) = proc.upgrade() else {
                return false; // remove if the process is already dead
            };
            proc.enqueue_command(Command::FrozenBinder(binder_frozen_state_info {
                cookie: *cookie,
                is_frozen: 0,
                reserved: 0,
            }));
            true
        });
    }

    pub fn has_pending_transactions(&self) -> bool {
        !self.active_transactions.is_empty()
    }
}

#[derive(Debug, Default, Clone)]

pub struct FreezeStatus {
    pub frozen: bool,
    /// Indicates whether the process has received any sync calls since last
    /// freeze (cleared at freeze/unfreeze)
    pub has_sync_recv: bool,
    /// Indicates whether the process has received any async calls since last
    /// freeze (cleared at freeze/unfreeze)
    pub has_async_recv: bool,
}

/// An active binder transaction.
#[derive(Debug)]
pub struct ActiveTransaction {
    /// The transaction's request type.
    pub request_type: RequestType,
    /// The state associated with the transaction. Not read, exists to be dropped along with the
    /// [`ActiveTransaction`] object.
    pub state: ReleaseGuard<TransactionState>,
}

impl Releasable for ActiveTransaction {
    type Context<'a> = ();

    fn release<'a>(self, context: Self::Context<'a>) {
        self.state.release(context);
    }
}

/// State held for the duration of a transaction. When a transaction completes (or fails), this
/// state is dropped, decrementing temporary strong references to binder objects.
#[derive(Debug)]
pub struct TransactionState {
    /// The process whose handle table `handles` belong to.
    pub proc: WeakRef<BinderProcess>,
    /// The target process.
    pub key: ThreadGroupKey,
    /// The remote resource accessor of the target process. This is None when the receiving process
    /// is a local process.
    pub remote_resource_accessor: Option<Arc<RemoteResourceAccessor>>,
    /// The objects to strongly owned for the duration of the transaction.
    pub guards: Vec<StrongRefGuard>,
    /// The handles to decrement their strong reference count.
    pub handles: Vec<Handle>,
    /// The FDs of the target process that the kernel is responsible for closing, because they were
    /// sent with BINDER_TYPE_FDA.
    pub owned_fds: Vec<FdNumber>,
}

impl Releasable for TransactionState {
    type Context<'a> = ();

    fn release<'a>(self, _: Self::Context<'a>) {
        log_trace!("Releasing binder TransactionState");
        let mut drop_actions = RefCountActions::default();
        // Release the owned objects unconditionally.
        for guard in self.guards {
            guard.release(&mut drop_actions);
        }
        if let Some(proc) = self.proc.upgrade() {
            // Release handles only if the owning process is still alive.
            let mut proc_state = proc.lock();
            for handle in &self.handles {
                if let Err(error) =
                    proc_state.handles.dec_strong(handle.object_index(), &mut drop_actions)
                {
                    // Ignore the error because there is little we can do about it.
                    // Panicking would be wrong, in case the client issued an extra strong decrement.
                    log_warn!(
                        "Error when dropping transaction state for process {}: {:?}",
                        proc.key.pid(),
                        error
                    );
                }
            }
        }
        // Releasing action must be done without holding BinderProcess lock.
        drop_actions.release(());

        // Close the owned fd.
        if !self.owned_fds.is_empty() {
            if let Some(task) = get_task_for_thread_group(&self.key) {
                let resource_accessor =
                    get_resource_accessor(task.deref(), &self.remote_resource_accessor);
                if let Err(error) = resource_accessor.close_files(self.owned_fds) {
                    log_warn!(
                        "Error when dropping transaction state while closing fd for task {}: {:?}",
                        task.tid,
                        error
                    );
                }
            }
        }
    }
}

/// Transaction state held during the processing and dispatching of a transaction. In the event of
/// an error while dispatching a transaction, this object is meant to cleanup any temporary
/// resources that were allocated. Once a transaction has been dispatched successfully, this object
/// can be converted into a [`TransactionState`] to be held for the lifetime of the transaction.
pub struct TransientTransactionState<'a> {
    /// The part of the transient state that will live for the lifetime of the transaction.
    pub state: Option<ReleaseGuard<TransactionState>>,
    /// The task to which the transient file descriptors belong.
    pub accessor: &'a dyn ResourceAccessor,
    /// The file descriptors to close in case of an error.
    pub transient_fds: Vec<FdNumber>,
    /// A guard that will ensure a panic on drop if `state` has not been released.
    pub drop_guard: DropGuard,
}

impl<'a> Releasable for TransientTransactionState<'a> {
    type Context<'b> = ();
    fn release<'b>(self, _: ()) {
        let _ = self.accessor.close_files(self.transient_fds);
        self.state.release(());
        self.drop_guard.disarm();
    }
}

impl<'a> std::fmt::Debug for TransientTransactionState<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransientTransactionState")
            .field("state", &self.state)
            .field("accessor", &self.accessor)
            .field("transient_fds", &self.transient_fds)
            .finish()
    }
}

impl<'a> TransientTransactionState<'a> {
    /// Creates a new [`TransientTransactionState`], whose resources will belong to `accessor` and
    /// `target_proc` for FDs and binder handles respectively.
    pub fn new(accessor: &'a dyn ResourceAccessor, target_proc: &BinderProcess) -> Self {
        TransientTransactionState {
            state: Some(
                TransactionState {
                    proc: target_proc.weak_self.clone(),
                    key: target_proc.key.clone(),
                    remote_resource_accessor: target_proc.remote_resource_accessor.clone(),
                    guards: vec![],
                    handles: vec![],
                    owned_fds: vec![],
                }
                .into(),
            ),
            accessor,
            transient_fds: vec![],
            drop_guard: DropGuard::default(),
        }
        .into()
    }

    /// Schedule `handle` to have its strong reference count decremented when the transaction ends
    /// (both in case of success or failure).
    pub fn push_handle(&mut self, handle: Handle) {
        self.state.as_mut().unwrap().handles.push(handle)
    }

    /// Schedule `guard` to be released when the transaction ends (both in case of success or
    /// failure).
    pub fn push_guard(&mut self, guard: StrongRefGuard) {
        self.state.as_mut().unwrap().guards.push(guard);
    }

    /// Schedule `fd` to be removed from the file descriptor table when the transaction ends (both
    /// in case of success or failure).
    pub fn push_owned_fd(&mut self, fd: FdNumber) {
        self.state.as_mut().unwrap().owned_fds.push(fd)
    }

    /// Schedule `fd` to be removed from the file descriptor table if the transaction fails.
    pub fn push_transient_fd(&mut self, fd: FdNumber) {
        self.transient_fds.push(fd)
    }

    pub fn into_state(mut self) -> ReleaseGuard<TransactionState> {
        // Clear the transient FD list, so that these FDs no longer get closed.
        self.transient_fds.clear();
        let result = self.state.take().unwrap();
        self.release(());
        result
    }
}

/// The request type of a transaction.
#[derive(Debug)]
pub enum RequestType {
    /// A fire-and-forget request, which has special ordering guarantees.
    Oneway {
        /// The recipient of the transaction. Oneway transactions are ordered for a given binder
        /// object.
        object: Arc<BinderObject>,
    },
    /// A request/response type.
    RequestResponse,
}

#[derive(Debug)]
pub struct BinderProcess {
    /// Weak reference to self.
    pub weak_self: WeakRef<BinderProcess>,

    /// A global identifier at the driver level for this binder process.
    pub identifier: u64,

    /// The identifier of the process associated with this binder process.
    pub key: ThreadGroupKey,

    /// Resource accessor to access remote resource in case of a remote binder process. None in
    /// case of a local process.
    pub remote_resource_accessor: Option<Arc<RemoteResourceAccessor>>,

    // The mutable state of `BinderProcess` is protected by 3 locks. For ordering purpose, locks
    // must be taken in the order they are defined in this class, even across `BinderProcess`
    // instances.
    // Moreover, any `BinderThread` lock must be ordered after any `state` lock from a
    // `BinderProcess`.
    /// The [`SharedMemory`] region mapped in both the driver and the binder process. Allows for
    /// transactions to copy data once from the sender process into the receiver process.
    pub shared_memory: Mutex<Option<SharedMemory>>,

    /// The main mutable state of the `BinderProcess`.
    pub state: Mutex<BinderProcessState>,

    /// A queue for commands that could not be scheduled on any existing binder threads. Binder
    /// threads that exhaust their own queue will read from this one.
    ///
    /// When there are no commands in a thread's and the process' command queue, a binder thread can
    /// register with this [`WaitQueue`] to be notified when commands are available.
    pub command_queue: Mutex<CommandQueueWithWaitQueue>,
}

pub struct BinderProcessGuard<'a>(Guard<'a, BinderProcess, MutexGuard<'a, BinderProcessState>>);

impl<'a> Deref for BinderProcessGuard<'a> {
    type Target = Guard<'a, BinderProcess, MutexGuard<'a, BinderProcessState>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> DerefMut for BinderProcessGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl BinderProcess {
    #[allow(clippy::let_and_return)]
    pub fn new(
        identifier: u64,
        key: ThreadGroupKey,
        remote_resource_accessor: Option<Arc<RemoteResourceAccessor>>,
    ) -> OwnedRef<BinderProcess> {
        log_trace!("new BinderProcess id={}", identifier);
        let result = OwnedRef::new_cyclic(|weak_self| Self {
            weak_self,
            identifier,
            key,
            remote_resource_accessor,
            shared_memory: Default::default(),
            state: Default::default(),
            command_queue: Default::default(),
        });
        #[cfg(any(test, debug_assertions))]
        {
            let _l1 = result.shared_memory.lock();
            let _l2 = result.lock();
            let _l3 = result.command_queue.lock();
        }
        result
    }

    pub fn lock<'a>(&'a self) -> BinderProcessGuard<'a> {
        BinderProcessGuard(Guard::new(self, self.state.lock()))
    }

    pub fn close(&self) {
        log_trace!("closing BinderProcess id={}", self.identifier);
        let mut state = self.lock();
        if !state.closed {
            state.closed = true;
            state.thread_pool.notify_all();
            self.command_queue.lock().notify_all();
        }
    }

    pub fn interrupt(&self) {
        log_trace!("interrupting BinderProcess id={}", self.identifier);
        let mut state = self.lock();
        if !state.interrupted {
            state.interrupted = true;
            state.thread_pool.notify_all();
            self.command_queue.lock().notify_all();
        }
    }

    /// Make all blocked threads stop waiting and return nothing. This is used by flush to
    /// make userspace recheck whether binder is being shut down.
    pub fn kick_all_threads(&self) {
        log_trace!("kicking threads for BinderProcess id={}", self.identifier);
        let state = self.lock();
        for thread in state.thread_pool.0.values() {
            thread.lock().request_kick = true;
        }
        state.thread_pool.notify_all();
    }

    /// Return the `ResourceAccessor` to use to access the resources of this process.
    pub fn get_resource_accessor<'a>(
        &'a self,
        task: &'a dyn ResourceAccessor,
    ) -> &'a dyn ResourceAccessor {
        get_resource_accessor(task, &self.remote_resource_accessor)
    }

    pub fn get_memory_accessor<'a>(
        &'a self,
        task: &'a dyn ResourceAccessor,
        remote_memory_accessor: Option<&'a RemoteMemoryAccessor<'_>>,
    ) -> &'a dyn MemoryAccessor {
        if let Some(memory_accessor) = self.get_resource_accessor(task).as_memory_accessor() {
            memory_accessor
        } else {
            remote_memory_accessor.expect("A RemoteMemoryAccessor should have been provided")
        }
    }

    /// Enqueues `command` for the process and wakes up any thread that is waiting for commands.
    pub fn enqueue_command(&self, command: Command) {
        log_trace!("BinderProcess id={} enqueuing command {:?}", self.identifier, command);
        // Handle oneway transactions explicitly. They should always target the process queue to
        // avoid accidentally handling them during an ongoing transaction.
        if matches!(command, Command::OnewayTransaction(_)) {
            self.command_queue.lock().push_back(command);
        } else if let Some(mut thread) = self.state.lock().thread_pool.get_available_thread() {
            thread.enqueue_command(command);
        } else {
            self.command_queue.lock().push_back(command);
        }
    }

    /// A binder thread is done reading a buffer allocated to a transaction. The binder
    /// driver can reclaim this buffer.
    pub fn handle_free_buffer(&self, buffer_ptr: UserAddress) -> Result<(), Errno> {
        log_trace!("BinderProcess id={} freeing buffer {:?}", self.identifier, buffer_ptr);
        // Drop the state associated with the now completed transaction.
        let active_transaction = self.lock().active_transactions.remove(&buffer_ptr);
        release_after!(active_transaction, (), {
            // Check if this was a oneway transaction and schedule the next oneway if this is the case.
            if let Some(ActiveTransaction {
                request_type: RequestType::Oneway { object }, ..
            }) = active_transaction.as_ref().map(|at| at.deref())
            {
                let mut object_state = object.lock();
                assert!(
                    object_state.handling_oneway_transaction,
                    "freeing a oneway buffer implies that a oneway transaction was being handled"
                );
                if let Some(transaction) = object_state.oneway_transactions.pop_front() {
                    // Drop the lock, as we've completed all mutations and don't want to hold this
                    // lock while acquiring any others.
                    drop(object_state);

                    // Schedule the transaction
                    self.enqueue_command(Command::OnewayTransaction(transaction));
                } else {
                    // No more oneway transactions queued, mark the queue handling as done.
                    object_state.handling_oneway_transaction = false;
                }
            }
        });

        // Reclaim the memory.
        let mut shared_memory_lock = self.shared_memory.lock();
        let shared_memory = shared_memory_lock.as_mut().ok_or_else(|| errno!(ENOMEM))?;
        shared_memory.free_buffer(buffer_ptr)
    }

    /// Handle a binder thread's request to increment/decrement a strong/weak reference to a remote
    /// binder object.
    pub fn handle_refcount_operation(
        &self,
        command: binder_driver_command_protocol,
        handle: Handle,
    ) -> Result<(), Errno> {
        let mut actions = RefCountActions::default();
        release_after!(actions, (), {
            self.lock().handle_refcount_operation(command, handle, &mut actions)
        })
    }

    /// Handle a binder thread's notification that it successfully incremented a strong/weak
    /// reference to a local (in-process) binder object. This is in response to a
    /// `BR_ACQUIRE`/`BR_INCREFS` command.
    pub fn handle_refcount_operation_done(
        &self,
        command: binder_driver_command_protocol,
        object: LocalBinderObject,
    ) -> Result<(), Errno> {
        let mut actions = RefCountActions::default();
        release_after!(actions, (), {
            self.lock().handle_refcount_operation_done(command, object, &mut actions)
        })
    }

    /// Subscribe a process to the death of the owner of `handle`.
    pub fn handle_request_death_notification(
        &self,
        handle: Handle,
        cookie: binder_uintptr_t,
    ) -> Result<(), Errno> {
        let proxy = match handle {
            Handle::ContextManager => {
                track_stub!(
                    TODO("https://fxbug.dev/322873363"),
                    "binder death notification for service manager"
                );
                return Ok(());
            }
            Handle::Object { index } => {
                let (proxy, guard) =
                    self.lock().handles.get(index).ok_or_else(|| errno!(ENOENT))?;

                // The object must have strong reference when used to find its owner process,
                // but there is no need to keep it alive afterwards.
                let mut actions = RefCountActions::default();
                guard.release(&mut actions);
                actions.release(());

                proxy
            }
        };
        if let Some(owner) = proxy.owner.upgrade() {
            owner.lock().death_subscribers.push((self.weak_self.clone(), cookie));
        } else {
            // The object is already dead. Notify immediately. To be noted: the requesting thread
            // cannot handle the notification, in case it is holding some mutex while processing a
            // oneway transaction (where its transaction stack will be empty). It is currently not
            // a problem, because enqueue_command never schedule on the current thread.
            self.enqueue_command(Command::DeadBinder(cookie));
        }
        Ok(())
    }

    /// Remove a previously subscribed death notification.
    pub fn handle_clear_death_notification(
        &self,
        handle: Handle,
        cookie: binder_uintptr_t,
    ) -> Result<(), Errno> {
        let owner = match handle {
            Handle::ContextManager => {
                track_stub!(
                    TODO("https://fxbug.dev/322873735"),
                    "binder clear death notification for service manager"
                );
                self.enqueue_command(Command::ClearDeathNotificationDone(cookie));
                return Ok(());
            }
            Handle::Object { index } => {
                self.lock().handles.get_owner(index).ok_or_else(|| errno!(ENOENT))?
            }
        };
        if let Some(owner) = owner.upgrade() {
            let mut owner = owner.lock();
            if let Some((idx, _)) =
                owner.death_subscribers.iter().enumerate().find(|(_idx, (proc, c))| {
                    proc.as_ptr() == self.weak_self.as_ptr() && *c == cookie
                })
            {
                owner.death_subscribers.swap_remove(idx);
            }
        }
        self.enqueue_command(Command::ClearDeathNotificationDone(cookie));
        Ok(())
    }

    /// Subscribe a process to the freeze of the owner of `handle`.
    pub fn handle_request_freeze_notification(
        &self,
        handle: Handle,
        cookie: binder_uintptr_t,
    ) -> Result<(), Errno> {
        let proxy = match handle {
            Handle::ContextManager => {
                track_stub!(
                    TODO("https://fxbug.dev/402188420"),
                    "binder freeze notification for service manager"
                );
                let info = binder_frozen_state_info { cookie, ..Default::default() };
                self.enqueue_command(Command::FrozenBinder(info));
                return Ok(());
            }
            Handle::Object { index } => {
                let (proxy, guard) =
                    self.lock().handles.get(index).ok_or_else(|| errno!(ENOENT))?;

                // The object must have strong reference when used to find its owner process,
                // but there is no need to keep it alive afterwards.
                let mut actions = RefCountActions::default();
                guard.release(&mut actions);
                actions.release(());

                proxy
            }
        };
        let owner = proxy.owner.upgrade().ok_or_else(|| errno!(ENOENT))?;
        let mut owner = owner.lock();
        // Check if the subscriber already exists
        if owner
            .freeze_subscribers
            .iter()
            .find(|(bp, c)| bp.as_ptr() == self.weak_self.as_ptr() && *c == cookie)
            .is_some()
        {
            return error!(EINVAL);
        }
        owner.freeze_subscribers.push((self.weak_self.clone(), cookie));
        let info = binder_frozen_state_info {
            cookie,
            is_frozen: if owner.freeze_status.frozen { 1 } else { 0 },
            reserved: 0,
        };
        self.enqueue_command(Command::FrozenBinder(info));
        Ok(())
    }

    /// Remove a previously subscribed freeze notification.
    pub fn handle_clear_freeze_notification(
        &self,
        handle: Handle,
        cookie: binder_uintptr_t,
    ) -> Result<(), Errno> {
        let owner = match handle {
            Handle::ContextManager => {
                track_stub!(
                    TODO("https://fxbug.dev/402191387"),
                    "binder clear freeze notification for service manager"
                );
                self.enqueue_command(Command::ClearFreezeNotificationDone(cookie));
                return Ok(());
            }
            Handle::Object { index } => {
                self.lock().handles.get_owner(index).ok_or_else(|| errno!(ENOENT))?
            }
        };
        let owner = owner.upgrade().ok_or_else(|| errno!(ENOENT))?;
        let mut owner = owner.lock();
        if let Some((idx, _)) = owner
            .freeze_subscribers
            .iter()
            .enumerate()
            .find(|(_idx, (proc, c))| proc.as_ptr() == self.weak_self.as_ptr() && *c == cookie)
        {
            owner.freeze_subscribers.swap_remove(idx);
        }
        self.enqueue_command(Command::ClearFreezeNotificationDone(cookie));
        Ok(())
    }

    /// Map the external vmo into the driver address space, recording the userspace address.
    pub fn map_external_vmo(&self, vmo: fidl::Vmo, mapped_address: u64) -> Result<(), Errno> {
        let mut shared_memory = self.shared_memory.lock();
        // Do not support mapping shared memory more than once.
        if shared_memory.is_some() {
            return error!(EINVAL);
        }
        let memory = MemoryObject::from(vmo);
        let size = memory.get_size();
        *shared_memory = Some(SharedMemory::map(&memory, mapped_address.into(), size as usize)?);
        Ok(())
    }

    /// Returns a task in the process
    pub fn get_task(&self) -> Option<TempRef<'_, Task>> {
        get_task_for_thread_group(&self.key)
    }
}

impl<'a> BinderProcessGuard<'a> {
    /// Return the `BinderThread` with the given `tid`, creating it if it doesn't exist.
    pub fn find_or_register_thread(&mut self, tid: pid_t) -> OwnedRef<BinderThread> {
        if let Some(thread) = self.thread_pool.0.get(&tid) {
            return OwnedRef::share(thread);
        }
        let thread = BinderThread::new(self, tid);
        self.thread_pool.0.insert(tid, OwnedRef::share(&thread));
        thread
    }

    /// Unregister the `BinderThread` with the given `tid`.
    pub fn unregister_thread(&mut self, current_task: &CurrentTask, tid: pid_t) {
        self.thread_pool.0.remove(&tid).release(current_task.kernel());
    }

    /// Inserts a reference to a binder object, returning a handle that represents it.
    /// The handle may be an existing handle if the object was already present in the table.
    /// The object must have at least a strong reference guarded by `guard` to ensure it is kept
    /// alive until it is added to the transaction.
    /// Returns the handle representing the object.
    pub fn insert_for_transaction(
        &mut self,
        guard: StrongRefGuard,
        actions: &mut RefCountActions,
    ) -> Handle {
        self.handles.insert_for_transaction(guard, actions)
    }

    /// Handle a binder thread's request to increment/decrement a strong/weak reference to a remote
    /// binder object.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess`.
    pub fn handle_refcount_operation(
        &mut self,
        command: binder_driver_command_protocol,
        handle: Handle,
        actions: &mut RefCountActions,
    ) -> Result<(), Errno> {
        let idx = match handle {
            Handle::ContextManager => {
                track_stub!(
                    TODO("https://fxbug.dev/322873629"),
                    "binder acquire/release refs for context manager object"
                );
                return Ok(());
            }
            Handle::Object { index } => index,
        };

        match command {
            binder_driver_command_protocol_BC_ACQUIRE => {
                log_trace!("Strong increment on handle {}", idx);
                self.handles.inc_strong(idx, actions)
            }
            binder_driver_command_protocol_BC_RELEASE => {
                log_trace!("Strong decrement on handle {}", idx);
                self.handles.dec_strong(idx, actions)
            }
            binder_driver_command_protocol_BC_INCREFS => {
                log_trace!("Weak increment on handle {}", idx);
                self.handles.inc_weak(idx, actions)
            }
            binder_driver_command_protocol_BC_DECREFS => {
                log_trace!("Weak decrement on handle {}", idx);
                self.handles.dec_weak(idx, actions)
            }
            _ => unreachable!(),
        }
    }

    /// Handle a binder thread's notification that it successfully incremented a strong/weak
    /// reference to a local (in-process) binder object. This is in response to a
    /// `BR_ACQUIRE`/`BR_INCREFS` command.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess`.
    pub fn handle_refcount_operation_done(
        &self,
        command: binder_driver_command_protocol,
        local: LocalBinderObject,
        actions: &mut RefCountActions,
    ) -> Result<(), Errno> {
        let object = self.find_object(&local).ok_or_else(|| errno!(EINVAL))?;
        match command {
            binder_driver_command_protocol_BC_INCREFS_DONE => {
                log_trace!("Acknowledging increment reference for {:?}", object);
                object.ack_incref(actions)
            }
            binder_driver_command_protocol_BC_ACQUIRE_DONE => {
                log_trace!("Acknowleding acquire done for {:?}", object);
                object.ack_acquire(actions)
            }
            _ => unreachable!(),
        }
    }

    /// Finds the binder object that corresponds to the process-local addresses `local`.
    pub fn find_object(&self, local: &LocalBinderObject) -> Option<&Arc<BinderObject>> {
        self.objects.get(&local.weak_ref_addr)
    }

    /// Finds the binder object that corresponds to the process-local addresses `local`, or creates
    /// a new [`BinderObject`] to represent the one in the process.
    pub fn find_or_register_object(
        &mut self,
        binder_thread: &BinderThread,
        local: LocalBinderObject,
        flags: BinderObjectFlags,
    ) -> StrongRefGuard {
        if let Some(object) = self.find_object(&local) {
            // The ref count can grow back from 0 in this instance because the object is being
            // registered again by its owner.
            object.inc_strong_unchecked(binder_thread)
        } else {
            let (object, guard) = BinderObject::new(self.base, local, flags);

            // Tell the owning process that a remote process now has a strong reference to
            // this object.
            binder_thread.lock().enqueue_command(Command::AcquireRef(object.local));

            self.objects.insert(object.local.weak_ref_addr, object);

            guard
        }
    }

    /// Whether the driver should request that the client starts a new thread.
    pub fn should_request_thread(&self, thread: &BinderThread) -> bool {
        !self.thread_requested
            && self.thread_pool.registered_threads() < self.max_thread_count
            && thread.lock().is_main_or_registered()
            && !self.thread_pool.has_available_thread()
    }

    /// Called back when the driver successfully asked the client to start a new thread.
    pub fn did_request_thread(&mut self) {
        self.thread_requested = true;
    }
}

impl Releasable for BinderProcess {
    type Context<'a> = &'a Kernel;

    fn release<'a>(self, context: Self::Context<'a>) {
        log_trace!("Releasing BinderProcess id={}", self.identifier);
        let state = self.state.into_inner();
        // Notify any subscribers that the objects this process owned are now dead.
        for (proc, cookie) in state.death_subscribers {
            if let Some(target_proc) = proc.upgrade() {
                target_proc.enqueue_command(Command::DeadBinder(cookie));
            }
        }

        // Generate dead replies for transactions currently in the command queue of this process.
        // Transactions that have been scheduled with a specific thread will generate dead replies
        // when the threads are released below.
        generate_dead_replies(self.command_queue.into_inner().commands, self.identifier, None);

        for transaction in state.active_transactions.into_values() {
            transaction.release(());
        }

        state.handles.release(());

        for thread in state.thread_pool.0.into_values() {
            thread.release(context);
        }
    }
}

/// The set of threads that are interacting with the binder driver for a given process.
#[derive(Debug, Default)]
pub struct ThreadPool(pub BTreeMap<pid_t, OwnedRef<BinderThread>>);

impl ThreadPool {
    fn has_available_thread(&self) -> bool {
        self.0.values().any(|t| t.lock().is_available())
    }

    fn get_available_thread(&self) -> Option<MutexGuard<'_, BinderThreadState>> {
        self.0.values().find_map(|t| {
            let thread = t.lock();
            if thread.is_available() { Some(thread) } else { None }
        })
    }

    fn notify_all(&self) {
        for t in self.0.values() {
            t.lock().command_queue.notify_all();
        }
    }

    /// The number of registered thread in the pool. This doesn't count the main thread.
    fn registered_threads(&self) -> usize {
        self.0.values().filter(|t| t.lock().is_registered()).count()
    }
}

/// Table containing handles to remote binder objects.
#[derive(Debug, Default)]
pub struct HandleTable {
    table: slab::Slab<BinderObjectRef>,
}

/// The HandleTable is released at the time the BinderProcess is released. At this moment, any
/// reference to object owned by another BinderProcess need to be clean.
impl Releasable for HandleTable {
    type Context<'a> = ();
    fn release<'a>(self, _: ()) {
        for (_, r) in self.table.into_iter() {
            let mut actions = RefCountActions::default();
            r.clean_refs(&mut actions);
            actions.release(());
        }
    }
}

impl HandleTable {
    /// Inserts a reference to a binder object, returning a handle that represents it.
    /// The handle may be an existing handle if the object was already present in the table.
    /// Transfer the reference represented by `guard` to the handle.
    /// A new handle will have a single strong reference, an existing handle will have its strong
    /// reference count incremented by one.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess` and the handle representing the object.
    pub fn insert_for_transaction(
        &mut self,
        guard: StrongRefGuard,
        actions: &mut RefCountActions,
    ) -> Handle {
        let index = if let Some((existing_idx, object_ref)) =
            self.find_ref_for_object(&guard.binder_object)
        {
            // Increment the number of reference to the handle as expected by the caller.
            object_ref.inc_strong_with_guard(guard, actions);
            existing_idx
        } else {
            // The new handle will be created with a strong reference as expected by the
            // caller.
            self.table.insert(BinderObjectRef::new(guard))
        };
        Handle::Object { index }
    }

    fn find_ref_for_object(
        &mut self,
        object: &Arc<BinderObject>,
    ) -> Option<(usize, &mut BinderObjectRef)> {
        self.table.iter_mut().filter(|(_, object_ref)| object_ref.is_ref_to_object(object)).next()
    }

    /// Retrieves a reference to a binder object at index `idx`. Returns None if the index doesn't
    /// exist or the object has no strong reference, otherwise returns the object and a guard to
    /// ensure it is kept alive while used.
    pub fn get(&self, idx: usize) -> Option<(Arc<BinderObject>, StrongRefGuard)> {
        let object_ref = self.table.get(idx)?;
        match object_ref.binder_object.inc_strong_checked() {
            Ok(guard) => Some((object_ref.binder_object.clone(), guard)),
            _ => None,
        }
    }

    /// Retrieves the owner of the binder object at index `idx`, whether the object has strong
    /// references or not.
    pub fn get_owner(&self, idx: usize) -> Option<WeakRef<BinderProcess>> {
        self.table.get(idx).map(|r| r.binder_object.owner.clone())
    }

    /// Increments the strong reference count of the binder object reference at index `idx`,
    /// failing if the object no longer exists.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess`.
    pub fn inc_strong(&mut self, idx: usize, actions: &mut RefCountActions) -> Result<(), Errno> {
        Ok(self.table.get_mut(idx).ok_or_else(|| errno!(ENOENT))?.inc_strong(actions)?)
    }

    /// Increments the weak reference count of the binder object reference at index `idx`, failing
    /// if the object does not exist.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess`.
    pub fn inc_weak(&mut self, idx: usize, actions: &mut RefCountActions) -> Result<(), Errno> {
        Ok(self.table.get_mut(idx).ok_or_else(|| errno!(ENOENT))?.inc_weak(actions))
    }

    /// Decrements the strong reference count of the binder object reference at index `idx`, failing
    /// if the object no longer exists.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess`.
    pub fn dec_strong(&mut self, idx: usize, actions: &mut RefCountActions) -> Result<(), Errno> {
        let object_ref = self.table.get_mut(idx).ok_or_else(|| errno!(ENOENT))?;
        object_ref.dec_strong(actions)?;
        if !object_ref.has_ref() {
            self.table.remove(idx);
        }
        Ok(())
    }

    /// Decrements the weak reference count of the binder object reference at index `idx`, failing
    /// if the object does not exist.
    /// Takes a `RefCountActions` that must be `release`d without holding a lock on
    /// `BinderProcess`.
    pub fn dec_weak(&mut self, idx: usize, actions: &mut RefCountActions) -> Result<(), Errno> {
        let object_ref = self.table.get_mut(idx).ok_or_else(|| errno!(ENOENT))?;
        object_ref.dec_weak(actions)?;
        if !object_ref.has_ref() {
            self.table.remove(idx);
        }
        Ok(())
    }
}

/// Returns a task in the process keyed by `key`.
fn get_task_for_thread_group(key: &ThreadGroupKey) -> Option<TempRef<'_, Task>> {
    key.upgrade().and_then(|tg| {
        let tg = tg.read();
        tg.get_task(tg.leader()).or_else(|| tg.tasks().next()).map(TempRef::into_static)
    })
}
