// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::objects::{LocalBinderObject, TransactionData};
use crate::process::{BinderProcess, BinderProcessGuard};

use starnix_core::mm::{MemoryAccessor, MemoryAccessorExt};

use starnix_core::task::{
    EventHandler, Kernel, Pid, SimpleWaiter, Task, WaitCanceler, WaitQueue, Waiter,
};

use starnix_logging::{log_trace, log_warn};
use starnix_sync::{
    BinderThreadRequeueEventLock, BinderThreadStateLock, InterruptibleEvent, LockDepGuard,
    LockDepMutex, ordered_lock,
};
use starnix_uapi::vfs::FdEvents;

use crossbeam::queue::SegQueue;
use starnix_types::ownership::{OwnedRef, Releasable, TempRef, WeakRef};
use starnix_types::user_buffer::UserBuffer;
use starnix_uapi::errors::{EACCES, EPERM, ESRCH, Errno};
use starnix_uapi::user_address::UserRef;
use starnix_uapi::{
    IOCSIZE_MASK, IOCSIZE_SHIFT, binder_driver_return_protocol,
    binder_driver_return_protocol_BR_ACQUIRE,
    binder_driver_return_protocol_BR_CLEAR_DEATH_NOTIFICATION_DONE,
    binder_driver_return_protocol_BR_CLEAR_FREEZE_NOTIFICATION_DONE,
    binder_driver_return_protocol_BR_DEAD_BINDER, binder_driver_return_protocol_BR_DEAD_REPLY,
    binder_driver_return_protocol_BR_DECREFS, binder_driver_return_protocol_BR_ERROR,
    binder_driver_return_protocol_BR_FAILED_REPLY, binder_driver_return_protocol_BR_FROZEN_BINDER,
    binder_driver_return_protocol_BR_FROZEN_REPLY, binder_driver_return_protocol_BR_INCREFS,
    binder_driver_return_protocol_BR_RELEASE, binder_driver_return_protocol_BR_REPLY,
    binder_driver_return_protocol_BR_SPAWN_LOOPER, binder_driver_return_protocol_BR_TRANSACTION,
    binder_driver_return_protocol_BR_TRANSACTION_COMPLETE,
    binder_driver_return_protocol_BR_TRANSACTION_PENDING_FROZEN,
    binder_driver_return_protocol_BR_TRANSACTION_SEC_CTX, binder_extended_error,
    binder_frozen_state_info, binder_ptr_cookie, binder_transaction_data, binder_uintptr_t, error,
};
use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use zerocopy::{Immutable, IntoBytes};

/// A binder command queued with its associated tracing span ID.
#[derive(Debug)]
pub struct QueuedCommand {
    pub command: Command,
    pub trace_id: fuchsia_trace::Id,
}

impl QueuedCommand {
    pub fn new(command: Command, trace_id: fuchsia_trace::Id) -> Self {
        Self { command, trace_id }
    }
}

impl From<Command> for QueuedCommand {
    fn from(command: Command) -> Self {
        Self { command, trace_id: fuchsia_trace::Id::new() }
    }
}

#[derive(Default, Debug)]
pub struct CommandQueueWithWaitQueue {
    pub commands: VecDeque<QueuedCommand>,
    pub waiters: WaitQueue,
}

impl CommandQueueWithWaitQueue {
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn front(&self) -> Option<&QueuedCommand> {
        self.commands.front()
    }

    pub fn pop_front(&mut self) -> Option<QueuedCommand> {
        let queued = self.commands.pop_front()?;
        crate::trace::on_command_dequeued(&queued.command, queued.trace_id);
        Some(queued)
    }

    pub fn push_back(&mut self, command: QueuedCommand) {
        crate::trace::on_command_enqueued(&command.command, command.trace_id);
        self.commands.push_back(command);
        self.waiters.notify_fd_events_count(FdEvents::POLLIN, 1);
    }

    /// Puts a dequeued command back at the front of the queue, so that it is dequeued next.
    ///
    /// Unlike `push_back`, this does not notify waiters: only the thread owning the queue dequeues
    /// from it, and it only puts back commands while reading, so it is not waiting for them.
    pub fn push_front(&mut self, command: QueuedCommand) {
        crate::trace::on_command_enqueued(&command.command, command.trace_id);
        self.commands.push_front(command);
    }

    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&QueuedCommand) -> bool,
    {
        self.commands.retain(|cmd| {
            let keep = f(cmd);
            if !keep {
                crate::trace::on_command_dequeued(&cmd.command, cmd.trace_id);
            }
            keep
        });
    }

    pub fn has_waiters(&self) -> bool {
        !self.waiters.is_empty()
    }

    pub fn wait_async_simple(&self, waiter: &mut SimpleWaiter) {
        self.waiters.wait_async_simple(waiter);
    }

    pub fn wait_async_fd_events(
        &self,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> WaitCanceler {
        self.waiters.wait_async_fd_events(waiter, events, handler)
    }
}

/// transaction's `sender_thread`.
pub(crate) fn generate_dead_replies(
    commands: impl IntoIterator<Item = QueuedCommand>,
    target_proc: u64,
    target_thread: Option<&Pid>,
) {
    // Notify all callers that had transactions scheduled for this process that the recipient is
    // dead.
    for queued in commands {
        if let Command::Transaction { sender, .. } = queued.command {
            if let Some(sender_thread) = sender.thread.upgrade() {
                let sender_thread = &mut sender_thread.lock();

                generate_dead_replies_for_transactions(sender_thread, target_proc, target_thread);
            }
        }
    }
}

/// Generates dead replies for all the `sender_thread`'s transactions targeting `target_thread`
/// and `target_proc`.
///
/// If a transaction has a target thread specified, it must match `target_thread` in order to be
/// marked dead. If a transaction does not specify a target thread, it is marked dead if the
/// target process matches `target_proc`.
///
/// If the top transaction of the transaction's `sender_thread` is targeting `target_thread` or
/// `target_proc`, the transaction is popped and a `DeadReply` command is enqueued on the
/// transaction's `sender_thread`.
pub(crate) fn generate_dead_replies_for_transactions(
    sender_thread: &mut BinderThreadState,
    target_proc: u64,
    target_thread: Option<&Pid>,
) {
    if let Some((top_transaction, remaining_transactions)) =
        sender_thread.transactions.split_last_mut()
    {
        // Check if the currently active transaction of the sender_thread is
        // targeting this process. If it is, that means that we might need
        // to pop the transaction and schedule a dead reply.
        let top_transaction_was_marked_dead = top_transaction.mark_dead(target_proc, target_thread);

        // Mark all other sender_thread transactions as dead if they are targeting
        // a dead process.
        for transaction in remaining_transactions {
            transaction.mark_dead(target_proc, target_thread);
        }

        // If the top transaction is targeting this process, then pop the
        // transaction and enqueue the `DeadReply`.
        if top_transaction_was_marked_dead {
            if let Some(TransactionRole::Sender(sender)) = sender_thread.transactions.pop() {
                sender_thread
                    .enqueue_command(QueuedCommand::new(Command::DeadReply, sender.trace_id));
            }
        }
    }
}

#[derive(Debug)]
pub struct BinderThread {
    /// Weak reference to self.
    pub weak_self: WeakRef<BinderThread>,

    pub tid: Pid,

    // The underlying Zircon thread which backs this binder thread.
    pub thread: Arc<zx::Thread>,

    /// The mutable state of the binder thread, protected by a single lock.
    pub state: LockDepMutex<BinderThreadState, BinderThreadStateLock>,

    /// A hint as to the registration state of the thread. This is eventually consistent with the
    /// state protected by the lock.
    pub registration: AtomicU8,

    /// A reference to the wait queue for the command queue. This allows other threads to notify
    /// this thread without acquiring the lock.
    pub command_queue_waiters: WaitQueue,

    /// A shared queue of available threads for the `BinderProcess`.
    pub available_threads: Arc<SegQueue<WeakRef<BinderThread>>>,

    /// The event that this thread is currently blocked on when waiting for an unscheduled
    /// transaction, if any. This allows dequeuing worker threads to inspect and requeue the
    /// event to inherit priority without acquiring `state`.
    pub requeue_event: LockDepMutex<Option<Arc<InterruptibleEvent>>, BinderThreadRequeueEventLock>,
}

impl BinderThread {
    pub fn new(binder_proc: &BinderProcess, tid: Pid, thread: Arc<zx::Thread>) -> OwnedRef<Self> {
        let inner_state = BinderThreadState::new(tid.clone(), binder_proc.identifier());
        let command_queue_waiters = inner_state.command_queue.waiters.clone();
        let available_threads = binder_proc.available_threads.clone();
        let state = inner_state.into();

        OwnedRef::new_cyclic(|weak_self| Self {
            weak_self,
            tid,
            thread,
            state,
            registration: AtomicU8::new(RegistrationState::Unregistered.to_u8()),
            command_queue_waiters,
            available_threads,
            requeue_event: LockDepMutex::new(None),
        })
    }

    /// Acquire the lock to the binder thread's mutable state.
    pub fn lock(&self) -> BinderThreadGuard<'_> {
        BinderThreadGuard { guard: self.state.lock(), thread: self }
    }

    pub fn ordered_lock<'a>(
        t1: &'a Self,
        t2: &'a Self,
    ) -> (BinderThreadGuard<'a>, BinderThreadGuard<'a>) {
        let (g1, g2) = ordered_lock(&t1.state, &t2.state);
        (BinderThreadGuard { guard: g1, thread: t1 }, BinderThreadGuard { guard: g2, thread: t2 })
    }

    /// Returns the task associated with this thread, if it is running and has an intact file
    /// table.
    pub fn get_task(&self) -> Option<Arc<Task>> {
        self.tid.get_task().ok().filter(|task| task.is_running() && task.files().is_ok())
    }
}

/// An RAII registration that associates an `InterruptibleEvent` with a `BinderThread` while waiting.
///
/// Note: This does NOT hold a lock while alive. It only sets the event upon creation and
/// clears it back to `None` on drop.
pub struct RequeueEventRegistration<'a>(&'a BinderThread);

impl<'a> RequeueEventRegistration<'a> {
    pub fn new(thread: &'a BinderThread, event: Arc<InterruptibleEvent>) -> Self {
        *thread.requeue_event.lock() = Some(event);
        Self(thread)
    }
}

impl Drop for RequeueEventRegistration<'_> {
    fn drop(&mut self) {
        *self.0.requeue_event.lock() = None;
    }
}

impl Releasable for BinderThread {
    type Context<'a> = &'a Kernel;

    fn release<'a>(self, context: Self::Context<'a>) {
        self.state.into_inner().release(context);
    }
}

/// The mutable state of a binder thread.
#[derive(Debug)]
pub struct BinderThreadState {
    pub tid: Pid,

    /// The process identifier of the `BinderProcess` to which this thread belongs. Note that this
    /// is not the same as the actual `pid` of the process.
    pub process_identifier: u64,
    /// The registered state of the thread.
    pub registration: RegistrationState,
    /// The stack of transactions that are active for this thread.
    pub transactions: Vec<TransactionRole>,
    /// The binder driver uses this queue to communicate with a binder thread. When a binder thread
    /// issues a [`uapi::BINDER_WRITE_READ`] ioctl, it will read from this command queue.
    pub command_queue: CommandQueueWithWaitQueue,
    /// The thread should finish waiting without returning anything, then reset the flag. Used by
    /// kick_all_threads by flush.
    pub request_kick: bool,
    /// Tracks whether this thread is registered as an available thread.
    pub available: bool,
    /// The extended error recorded for the calling thread since it last started a transaction or
    /// reply, or `None` if no error has been recorded. Reported by `BINDER_GET_EXTENDED_ERROR`.
    pub extended_error: Option<binder_extended_error>,
}

pub struct BinderThreadGuard<'a> {
    guard: LockDepGuard<'a, BinderThreadState>,
    thread: &'a BinderThread,
}

impl Deref for BinderThreadGuard<'_> {
    type Target = BinderThreadState;

    fn deref(&self) -> &Self::Target {
        self.guard.deref()
    }
}

impl DerefMut for BinderThreadGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.deref_mut()
    }
}

impl Drop for BinderThreadGuard<'_> {
    fn drop(&mut self) {
        let was_available = self.guard.available;
        let is_available = self.guard.is_available();
        self.guard.available = is_available;
        if is_available && !was_available {
            self.thread.available_threads.push(self.thread.weak_self.clone());
        }
        self.thread.registration.store(self.guard.registration.to_u8(), Ordering::Release);
    }
}

impl BinderThreadState {
    pub fn new(tid: Pid, process_identifier: u64) -> Self {
        Self {
            tid,
            process_identifier,
            registration: RegistrationState::default(),
            transactions: Default::default(),
            command_queue: Default::default(),
            request_kick: false,
            available: false,
            extended_error: None,
        }
    }

    pub fn reset_extended_error(&mut self) {
        self.extended_error = None;
    }

    pub fn take_extended_error(&mut self) -> Option<binder_extended_error> {
        self.extended_error.take()
    }

    pub fn is_main_or_registered(&self) -> bool {
        self.registration != RegistrationState::Unregistered
    }

    pub fn is_available(&self) -> bool {
        if !self.is_main_or_registered() {
            log_trace!("thread {:?} not registered {:?}", self.tid, self.registration);
            return false;
        }
        if !self.command_queue.is_empty() {
            log_trace!("thread {:?} has non empty queue", self.tid);
            return false;
        }
        if !self.command_queue.has_waiters() {
            log_trace!("thread {:?} is not waiting", self.tid);
            return false;
        }
        if !self.transactions.is_empty() {
            log_trace!("thread {:?} is in a transaction {:?}", self.tid, self.transactions);
            return false;
        }
        log_trace!("thread {:?} is available", self.tid);
        true
    }

    /// Handle a binder thread's request to register itself with the binder driver.
    /// This makes the binder thread eligible for receiving commands from the driver.
    pub fn handle_looper_registration(
        &mut self,
        binder_process: &mut BinderProcessGuard<'_>,
        registration: RegistrationState,
    ) -> Result<(), Errno> {
        log_trace!("BinderThreadState id={} looper registration", self.tid);
        if self.is_main_or_registered() {
            // This thread is already registered.
            error!(EINVAL)
        } else {
            if registration == RegistrationState::Auxiliary {
                binder_process.thread_requested = false;
                binder_process.auxiliary_threads_count += 1;
            }
            self.registration = registration;
            Ok(())
        }
    }

    /// Enqueues `command` for the thread and wakes it up if necessary.
    pub fn enqueue_command(&mut self, command: QueuedCommand) {
        log_trace!("BinderThreadState id={} enqueuing command {:?}", self.tid, command.command);
        self.command_queue.push_back(command);
    }

    /// Pops the transaction this thread is replying to, and returns the peer that sent it, which
    /// may be dead. Fails if this thread is not handling a transaction.
    pub fn pop_transaction_caller(
        &mut self,
    ) -> Result<(WeakBinderPeer, fuchsia_trace::Id), TransactionError> {
        match self.transactions.pop() {
            Some(TransactionRole::Receiver { peer, trace_id }) => {
                log_trace!(
                    "binder transaction popped from thread {} for peer {:?}",
                    self.tid,
                    peer
                );
                Ok((peer, trace_id))
            }
            Some(transaction @ TransactionRole::Sender(_)) => {
                // The thread is waiting for the reply to its own transaction, which must stay on
                // its transaction stack.
                self.transactions.push(transaction);
                log_warn!("caller got confused, nothing to reply to!");
                error!(EINVAL)?
            }
            None => error!(EINVAL)?,
        }
    }

    /// Notifies this thread that the reply to the transaction it sent to `target_proc` could not
    /// be delivered because of `error`, and pops that transaction from its transaction stack.
    ///
    /// Does nothing if this thread is not waiting for the reply to a transaction sent to
    /// `target_proc`, e.g. if the transaction already completed with a `DeadReply`.
    pub fn deliver_failed_reply(
        &mut self,
        target_proc: u64,
        error: &TransactionError,
        trace_id: fuchsia_trace::Id,
    ) {
        if !matches!(
            self.transactions.last(),
            Some(TransactionRole::Sender(sender)) if sender.target_proc == target_proc
        ) {
            log_trace!(
                "binder thread {} is not waiting for a reply from process {}",
                self.tid,
                target_proc
            );
            return;
        }
        self.transactions.pop();
        if self.extended_error.is_none() {
            self.extended_error = Some(error.extended_error());
        }
        self.enqueue_command(QueuedCommand::new(error.to_command(), trace_id));
    }

    pub fn has_pending_transactions(&self) -> bool {
        !self.transactions.is_empty()
    }
}

impl Releasable for BinderThreadState {
    type Context<'a> = &'a Kernel;

    fn release<'a>(self, _context: Self::Context<'a>) {
        log_trace!("Dropping BinderThreadState id={}", self.tid);
        // If there are any transactions queued, we need to tell the caller that this thread is now
        // dead.
        let command_queue = self.command_queue.commands;
        generate_dead_replies(command_queue, self.process_identifier, Some(&self.tid));

        // If there are any transactions that this thread was processing, we need to tell the caller
        // that this thread is now dead and to not expect a reply.
        for transaction in self.transactions {
            if let TransactionRole::Receiver { peer, .. } = transaction {
                if let Some(peer_thread) = peer.thread.upgrade() {
                    let sender_thread = &mut peer_thread.lock();
                    generate_dead_replies_for_transactions(
                        sender_thread,
                        self.process_identifier,
                        Some(&self.tid),
                    );
                }
            }
        }
    }
}

/// The registration state of a thread.
#[derive(Debug, PartialEq, Eq)]
pub enum RegistrationState {
    /// The thread is not registered.
    Unregistered,
    /// The thread is the main binder thread.
    Main,
    /// The thread is an auxiliary binder thread.
    Auxiliary,
}

impl RegistrationState {
    pub fn to_u8(&self) -> u8 {
        match self {
            Self::Unregistered => 0,
            Self::Main => 1,
            Self::Auxiliary => 2,
        }
    }
}

impl Default for RegistrationState {
    fn default() -> Self {
        Self::Unregistered
    }
}

/// A pair of weak references to the process and thread of a binder transaction peer.
#[derive(Debug)]
pub struct WeakBinderPeer {
    pub proc: WeakRef<BinderProcess>,
    pub thread: WeakRef<BinderThread>,
}

impl WeakBinderPeer {
    pub fn new(proc: &BinderProcess, thread: &BinderThread) -> Self {
        Self { proc: proc.weak_self.clone(), thread: thread.weak_self.clone() }
    }

    /// Upgrades the process and thread weak references as a tuple.
    pub fn upgrade(
        &self,
    ) -> Option<(TempRef<'static, BinderProcess>, TempRef<'static, BinderThread>)> {
        self.proc
            .upgrade()
            .map(TempRef::into_static)
            .zip(self.thread.upgrade().map(TempRef::into_static))
    }
}

/// Commands for a binder thread to execute.
#[derive(Debug)]
pub enum Command {
    /// Notifies a binder thread that a remote process acquired a strong reference to the specified
    /// binder object. The object should not be destroyed until a [`Command::ReleaseRef`] is
    /// delivered.
    AcquireRef(LocalBinderObject),
    /// Notifies a binder thread that there are no longer any remote processes holding strong
    /// references to the specified binder object. The object may still have references within the
    /// owning process.
    ReleaseRef(LocalBinderObject),
    /// Notifies a binder thread that a remote process acquired a weak reference to the specified
    /// binder object. The object should not be destroyed until a [`Command::DecRef`] is
    /// delivered.
    IncRef(LocalBinderObject),
    /// Notifies a binder thread that there are no longer any remote processes holding weak
    /// references to the specified binder object. The object may still have references within the
    /// owning process.
    DecRef(LocalBinderObject),
    /// Notifies a binder thread that the last processed command contained an error.
    Error(i32),
    /// Commands a binder thread to start processing an incoming oneway transaction, which requires
    /// no reply.
    OnewayTransaction(TransactionData),
    /// Commands a binder thread to start processing an incoming synchronous transaction from
    /// another binder process.
    /// Sent from the client to the server.
    Transaction {
        /// The binder peer that sent this transaction.
        sender: WeakBinderPeer,
        /// The transaction payload.
        data: TransactionData,
    },
    /// Commands a binder thread to process an incoming reply to its transaction.
    /// Sent from the server to the client.
    Reply(TransactionData),
    /// Notifies a binder thread that its reply has been sent.
    /// Sent from binder to the server.
    TransactionComplete,
    /// Notifies a binder thread that its two-way transaction has been sent. It is followed by the
    /// outcome of the transaction, e.g. a [`Command::Reply`].
    /// Sent from binder to the client.
    TwoWayTransactionComplete,
    /// Notifies a binder thread that a oneway transaction has been sent.
    /// Sent from binder to the client.
    OnewayTransactionComplete,
    /// The transaction was well formed but failed. Possible causes are a nonexistent handle, no
    /// more memory available to allocate a buffer.
    FailedReply,
    /// Notifies the initiator of a transaction that the recipient is dead.
    DeadReply,
    /// Notifies a binder process that a binder object has died.
    DeadBinder(binder_uintptr_t),
    /// Notifies the initiator of a sync transaction that the recipient is frozen.
    FrozenReply,
    /// Notifies the initiator of an async transaction that the recipient is frozen and transaction
    /// is queued.
    PendingFrozen,
    /// Notifies a binder process that the death notification has been cleared.
    ClearDeathNotificationDone(binder_uintptr_t),
    /// Notified the binder process that it should spawn a new looper.
    SpawnLooper,
    /// Notifies a binder process whether it is transitioned into a frozen state.
    FrozenBinder(binder_frozen_state_info),
    /// Notifies a binder process that the freeze notification has been cleared.
    ClearFreezeNotificationDone(binder_uintptr_t),
}

impl Command {
    /// The size of the largest serialized command: a `BR_TRANSACTION_SEC_CTX`, i.e. the return
    /// code followed by a `binder_transaction_data` and a security context pointer.
    ///
    /// A read only dequeues a command after another one if the rest of the read buffer has room
    /// for `MAX_SIZE` bytes, so that the dequeued command always fits and is never dropped.
    pub const MAX_SIZE: usize = std::mem::size_of::<binder_driver_return_protocol>()
        + std::mem::size_of::<binder_transaction_data>()
        + std::mem::size_of::<binder_uintptr_t>();

    /// Returns the command's BR_* code for serialization.
    pub fn driver_return_code(&self) -> binder_driver_return_protocol {
        match self {
            Self::AcquireRef(..) => binder_driver_return_protocol_BR_ACQUIRE,
            Self::ReleaseRef(..) => binder_driver_return_protocol_BR_RELEASE,
            Self::IncRef(..) => binder_driver_return_protocol_BR_INCREFS,
            Self::DecRef(..) => binder_driver_return_protocol_BR_DECREFS,
            Self::Error(..) => binder_driver_return_protocol_BR_ERROR,
            Self::OnewayTransaction(data) | Self::Transaction { data, .. } => {
                if data.buffers.security_context.is_none() {
                    binder_driver_return_protocol_BR_TRANSACTION
                } else {
                    binder_driver_return_protocol_BR_TRANSACTION_SEC_CTX
                }
            }
            Self::Reply(..) => binder_driver_return_protocol_BR_REPLY,
            Self::TransactionComplete
            | Self::TwoWayTransactionComplete
            | Self::OnewayTransactionComplete => {
                binder_driver_return_protocol_BR_TRANSACTION_COMPLETE
            }
            Self::FailedReply => binder_driver_return_protocol_BR_FAILED_REPLY,
            Self::DeadReply { .. } => binder_driver_return_protocol_BR_DEAD_REPLY,
            Self::DeadBinder(..) => binder_driver_return_protocol_BR_DEAD_BINDER,
            Self::FrozenReply => binder_driver_return_protocol_BR_FROZEN_REPLY,
            Self::PendingFrozen => binder_driver_return_protocol_BR_TRANSACTION_PENDING_FROZEN,
            Self::ClearDeathNotificationDone(..) => {
                binder_driver_return_protocol_BR_CLEAR_DEATH_NOTIFICATION_DONE
            }
            Self::SpawnLooper => binder_driver_return_protocol_BR_SPAWN_LOOPER,
            Self::FrozenBinder(..) => binder_driver_return_protocol_BR_FROZEN_BINDER,
            Self::ClearFreezeNotificationDone(..) => {
                binder_driver_return_protocol_BR_CLEAR_FREEZE_NOTIFICATION_DONE
            }
        }
    }

    /// Returns the minimum buffer size in bytes required to write this command into memory.
    pub fn required_buffer_size(&self) -> usize {
        // The size of the parameters of a command is encoded in its return code, which
        // `write_command` asserts.
        std::mem::size_of::<binder_driver_return_protocol>() + ioc_size(self.driver_return_code())
    }

    /// Returns true if this command terminates a read batch in `handle_thread_read`: a terminal
    /// command is only read as the first command of a batch, and ends it.
    ///
    /// After a `TwoWayTransactionComplete`, `handle_thread_read` may still wait for the outcome of
    /// the transaction and return it in the same read, as the first command of a new batch.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Transaction { .. }
            | Self::Reply(..)
            | Self::TransactionComplete
            | Self::TwoWayTransactionComplete
            | Self::OnewayTransaction(..)
            | Self::OnewayTransactionComplete
            | Self::PendingFrozen
            | Self::DeadReply
            | Self::FailedReply
            | Self::FrozenReply
            | Self::DeadBinder(..)
            | Self::FrozenBinder(..)
            | Self::SpawnLooper
            | Self::Error(..) => true,
            Self::AcquireRef(..)
            | Self::ReleaseRef(..)
            | Self::IncRef(..)
            | Self::DecRef(..)
            | Self::ClearDeathNotificationDone(..)
            | Self::ClearFreezeNotificationDone(..) => false,
        }
    }

    /// Serializes and writes the command into userspace memory at `buffer`.
    pub fn write_to_memory(
        &self,
        memory_accessor: &dyn MemoryAccessor,
        buffer: &UserBuffer,
    ) -> Result<usize, Errno> {
        match self {
            Self::AcquireRef(obj)
            | Self::ReleaseRef(obj)
            | Self::IncRef(obj)
            | Self::DecRef(obj) => self.write_command(
                memory_accessor,
                buffer,
                binder_ptr_cookie {
                    ptr: obj.weak_ref_addr.ptr() as binder_uintptr_t,
                    cookie: obj.strong_ref_addr.ptr() as binder_uintptr_t,
                },
            ),
            Self::Error(error_val) => self.write_command(memory_accessor, buffer, *error_val),
            Self::OnewayTransaction(data) | Self::Transaction { data, .. } | Self::Reply(data) => {
                if let Some(security_context_buffer) = data.buffers.security_context.as_ref() {
                    #[repr(C, packed)]
                    #[derive(IntoBytes, Immutable)]
                    struct TransactionDataSecCtx {
                        data: [u8; std::mem::size_of::<binder_transaction_data>()],
                        secctx: binder_uintptr_t,
                    }

                    self.write_command(
                        memory_accessor,
                        buffer,
                        TransactionDataSecCtx {
                            data: data.as_bytes(),
                            secctx: security_context_buffer.address.ptr() as binder_uintptr_t,
                        },
                    )
                } else {
                    self.write_command(memory_accessor, buffer, data.as_bytes())
                }
            }
            Self::TransactionComplete
            | Self::TwoWayTransactionComplete
            | Self::OnewayTransactionComplete
            | Self::FailedReply
            | Self::FrozenReply
            | Self::PendingFrozen
            | Self::DeadReply { .. }
            | Self::SpawnLooper => self.write_command(memory_accessor, buffer, ()),
            Self::DeadBinder(cookie)
            | Self::ClearDeathNotificationDone(cookie)
            | Self::ClearFreezeNotificationDone(cookie) => {
                self.write_command(memory_accessor, buffer, *cookie)
            }
            Self::FrozenBinder(state) => self.write_command(memory_accessor, buffer, *state),
        }
    }

    /// Writes the command's `BR_*` return code followed by `parameters` into userspace memory at
    /// `buffer`, asserting that the size of `parameters` matches the payload size encoded in the
    /// return code.
    fn write_command<A: IntoBytes + Immutable>(
        &self,
        memory_accessor: &dyn MemoryAccessor,
        buffer: &UserBuffer,
        parameters: A,
    ) -> Result<usize, Errno> {
        assert_eq!(std::mem::size_of::<A>(), ioc_size(self.driver_return_code()));

        #[repr(C, packed)]
        #[derive(IntoBytes, Immutable)]
        struct Data<A> {
            command: binder_driver_return_protocol,
            parameters: A,
        }
        // `handle_thread_read` relies on `MAX_SIZE` being an upper bound of the size of any
        // command, so that a command it dequeues always fits in the rest of the read buffer. This
        // fails to compile if a larger command is added without updating `MAX_SIZE`.
        const {
            assert!(std::mem::size_of::<Data<A>>() <= Self::MAX_SIZE);
        }

        if buffer.length < std::mem::size_of::<Data<A>>() {
            return error!(ENOMEM);
        }

        memory_accessor.write_object(
            UserRef::new(buffer.address),
            &Data { command: self.driver_return_code(), parameters },
        )
    }
}

/// A binder thread's role (sender or receiver) in a synchronous transaction. Oneway transactions
/// do not record roles, since they end as soon as they begin.
#[derive(Debug)]
pub enum TransactionRole {
    /// The binder thread initiated the transaction and is awaiting a reply from a peer.
    Sender(TransactionSender),

    /// The binder thread is receiving a transaction and is expected to reply to the peer binder
    /// process and thread.
    Receiver { peer: WeakBinderPeer, trace_id: fuchsia_trace::Id },
}

#[derive(Debug)]
pub struct TransactionSender {
    /// The target process of the transaction. Used to determine whether or not this transaction is
    /// still alive.
    pub target_proc: u64,

    /// The target thread of the transaction. Used to determine whether or not this transaction is
    /// still alive. If `None`, the transaction will be marked dead when the handling thread in
    /// `target_proc` is released.
    pub target_thread: Option<Pid>,

    /// Whether or not the target of this transaction is still alive. Used to determine whether or
    /// not a `DeadReply` should be inserted into the command queue when a thread is waiting for
    /// this transaction to complete.
    pub is_alive: bool,

    // The underlying zircon thread for the target. Used to arrange futex priority inheritance if
    // available.
    pub target_thread_handle: Option<Arc<zx::Thread>>,

    /// The trace ID associated with this transaction flow.
    pub trace_id: fuchsia_trace::Id,
}

impl TransactionRole {
    /// Marks the transaction as dead if it is a `Sender` targeting `thread` or `process`.
    fn mark_dead(&mut self, process: u64, thread: Option<&Pid>) -> bool {
        match (thread, self) {
            (
                // If a thread is provided to `mark_dead`, it means that the transaction should
                // only be marked dead if the `target_thread` actually matches `thread`.
                Some(thread),
                TransactionRole::Sender(TransactionSender {
                    target_thread: Some(target),
                    is_alive,
                    ..
                }),
            ) if target == thread => {
                *is_alive = false;
                true
            }
            (
                // If there is no target thread for the transaction, the transaction is process
                // bound. This means that the transaction should be marked dead if the
                // `target_proc` matches `process`, regardless of whether or not a `thread` was
                // provided to `mark_dead`.
                _,
                TransactionRole::Sender(TransactionSender {
                    target_thread: None,
                    target_proc,
                    is_alive,
                    ..
                }),
            ) if *target_proc == process => {
                *is_alive = false;
                true
            }
            // The transaction specifies a `target_thread` that does not match `thread`, or the
            // transaction is targeting a different `target_proc`.
            _ => false,
        }
    }
}

/// An error that occurred while processing a transaction that should be communicated back to the
/// calling thread.
#[derive(Debug, PartialEq, Eq)]
pub enum TransactionError {
    /// The transaction had malformed data or commands.
    Malformed(Errno),

    /// A general failure, e.g. the transaction could not be allocated, or was rejected by SELinux.
    Failure(Errno),

    /// The target process is dead.
    Dead,

    /// The target process is frozen.
    Frozen,
}

impl TransactionError {
    /// Dispatches the error, by potentially queueing a command to `binder_thread` and/or returning
    /// an error.
    pub fn dispatch(&self, binder_thread: &BinderThread) -> Result<(), Errno> {
        log_trace!("Dispatching transaction error {:?} for thread {}", self, binder_thread.tid);
        if let TransactionError::Malformed(err) = self {
            log_warn!(
                "binder thread {} sent a malformed transaction: {:?}",
                binder_thread.tid,
                &err
            );
        }
        let mut thread_state = binder_thread.lock();
        thread_state.extended_error = Some(self.extended_error());
        thread_state.enqueue_command(self.to_command().into());
        Ok(())
    }

    pub fn extended_error(&self) -> binder_extended_error {
        let param = match self {
            TransactionError::Malformed(err) | TransactionError::Failure(err) => {
                err.return_value() as i32
            }
            TransactionError::Dead | TransactionError::Frozen => 0,
        };
        binder_extended_error { id: 0, command: self.to_command().driver_return_code(), param }
    }

    /// Returns the command that notifies a binder thread of the error.
    fn to_command(&self) -> Command {
        match self {
            // Negate the value, as the binder runtime assumes error values are already negative.
            TransactionError::Malformed(err) => Command::Error(err.return_value() as i32),
            TransactionError::Failure(_) => Command::FailedReply,
            TransactionError::Dead => Command::DeadReply,
            TransactionError::Frozen => Command::FrozenReply,
        }
    }
}

impl From<Errno> for TransactionError {
    fn from(errno: Errno) -> TransactionError {
        match errno.code {
            EACCES | EPERM => TransactionError::Failure(errno),
            ESRCH => TransactionError::Dead,
            _ => TransactionError::Malformed(errno),
        }
    }
}

// Returns the size of an ioctl command build with the standard bit scheme.
fn ioc_size(n: u32) -> usize {
    ((n & IOCSIZE_MASK) >> IOCSIZE_SHIFT) as usize
}
