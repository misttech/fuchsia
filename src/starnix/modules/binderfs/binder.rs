// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(non_upper_case_globals)]

use crate::objects::{
    BinderObject, BinderObjectFlags, BinderObjectRef, FlatBinderObject, Handle, LocalBinderObject,
    RefCountActions, SerializedBinderObject, StrongRefGuard, TransactionData,
};
use crate::resource_accessor::{
    RemoteIoctl, RemoteMemoryAccessor, RemoteResourceAccessor, ResourceAccessor,
    get_resource_accessor,
};
use crate::shared_memory::{SharedBuffer, SharedMemory, TransactionBuffers};
use crate::thread::{
    BinderThread, BinderThreadState, Command, CommandQueueWithWaitQueue, RegistrationState,
    SchedulerGuard, TransactionRole, TransactionSender, WeakBinderPeer, generate_dead_replies,
};
use crate::user_memory_cursor::UserMemoryCursor;
use fidl::endpoints::ClientEnd;
use starnix_core::device::DeviceOps;
use starnix_core::device::mem::new_null_file;
use starnix_core::fs::fuchsia::new_remote_file;
use starnix_core::mm::memory::MemoryObject;
use starnix_core::mm::{
    DesiredAddress, MappingName, MappingOptions, MemoryAccessor, MemoryAccessorExt, ProtectionFlags,
};
use starnix_core::mutable_state::Guard;
use starnix_core::security;
use starnix_core::task::{
    CurrentTask, CurrentTaskAndLocked, EventHandler, Kernel, SchedulerState, SimpleWaiter, Task,
    ThreadGroupKey, WaitCanceler, Waiter,
};
use starnix_core::vfs::buffers::{InputBuffer, OutputBuffer};
use starnix_core::vfs::{
    FdFlags, FdNumber, FileObject, FileObjectState, FileOps, FsStr, FsString, NamespaceNode,
    fileops_impl_nonseekable, fileops_impl_noop_sync,
};
use starnix_lifecycle::AtomicU64Counter;
use starnix_logging::{
    CATEGORY_STARNIX, log_error, log_trace, log_warn, trace_duration, track_stub, with_zx_name,
};
use starnix_sync::{
    FileOpsCore, InterruptibleEvent, LockEqualOrBefore, Locked, Mutex, MutexGuard,
    ResourceAccessorLevel, RwLock, Unlocked, ordered_lock_vec,
};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_types::convert::IntoFidl as _;
use starnix_types::ownership::{
    DropGuard, OwnedRef, Releasable, ReleaseGuard, Share, TempRef, WeakRef, release_after,
    release_iter_after, release_on_error,
};
use starnix_types::user_buffer::UserBuffer;
use starnix_uapi::device_type::DeviceType;
use starnix_uapi::errors::{EACCES, EINTR, EPERM, Errno};
use starnix_uapi::math::round_up_to_increment;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    BINDER_BUFFER_FLAG_HAS_PARENT, BINDER_CURRENT_PROTOCOL_VERSION, binder_driver_command_protocol,
    binder_driver_command_protocol_BC_ACQUIRE, binder_driver_command_protocol_BC_ACQUIRE_DONE,
    binder_driver_command_protocol_BC_CLEAR_DEATH_NOTIFICATION,
    binder_driver_command_protocol_BC_CLEAR_FREEZE_NOTIFICATION,
    binder_driver_command_protocol_BC_DEAD_BINDER_DONE, binder_driver_command_protocol_BC_DECREFS,
    binder_driver_command_protocol_BC_ENTER_LOOPER, binder_driver_command_protocol_BC_FREE_BUFFER,
    binder_driver_command_protocol_BC_FREEZE_NOTIFICATION_DONE,
    binder_driver_command_protocol_BC_INCREFS, binder_driver_command_protocol_BC_INCREFS_DONE,
    binder_driver_command_protocol_BC_REGISTER_LOOPER, binder_driver_command_protocol_BC_RELEASE,
    binder_driver_command_protocol_BC_REPLY, binder_driver_command_protocol_BC_REPLY_SG,
    binder_driver_command_protocol_BC_REQUEST_DEATH_NOTIFICATION,
    binder_driver_command_protocol_BC_REQUEST_FREEZE_NOTIFICATION,
    binder_driver_command_protocol_BC_TRANSACTION,
    binder_driver_command_protocol_BC_TRANSACTION_SG, binder_freeze_info, binder_frozen_state_info,
    binder_frozen_status_info, binder_transaction_data, binder_transaction_data_sg,
    binder_uintptr_t, binder_version, binder_write_read, errno, error, flat_binder_object, pid_t,
    transaction_flags_TF_ONE_WAY, uapi,
};
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};

use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::vec::Vec;

use zerocopy::IntoBytes;
use {fidl_fuchsia_starnix_binder as fbinder, zx};

/// The trace category used for binder command tracing.

/// The name used to track the duration of a local binder ioctl.
const NAME_BINDER_IOCTL: &'static str = "binder_ioctl";

#[derive(Debug, Default, Clone)]
pub struct BinderDevice(Arc<BinderDriver>);

impl Deref for BinderDevice {
    type Target = Arc<BinderDriver>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DeviceOps for BinderDevice {
    fn open(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        current_task: &CurrentTask,
        _id: DeviceType,
        _node: &NamespaceNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let identifier = self.create_local_process(current_task.thread_group_key.clone());
        log_trace!("opened new BinderConnection id={}", identifier);
        Ok(Box::new(BinderConnection {
            identifier,
            device: self.clone(),
            security_state: security::binder_connection_alloc(current_task),
        }))
    }
}

/// An instance of the binder driver, associated with the process that opened the binder device.
#[derive(Debug)]
pub struct BinderConnection {
    /// The process that opened the binder device.
    pub identifier: u64,
    /// The implementation of the binder driver.
    device: BinderDevice,
    /// Security state associated this file object.
    security_state: security::BinderConnectionState,
}

impl BinderConnection {
    pub fn proc(&self, current_task: &CurrentTask) -> Result<OwnedRef<BinderProcess>, Errno> {
        let process = self.device.find_process(self.identifier)?;
        if process.key == current_task.thread_group_key.clone() {
            Ok(process)
        } else {
            process.release(current_task.kernel());
            error!(EINVAL)
        }
    }

    pub fn interrupt(&self) {
        log_trace!("interrupting BinderConnection id={}", self.identifier);
        if let Some(binder_process) = self.device.procs.read().get(&self.identifier) {
            binder_process.interrupt();
        }
    }

    fn close(&self, kernel: &Kernel) {
        log_trace!("closing BinderConnection id={}", self.identifier);
        if let Some(binder_process) = self.device.procs.write().remove(&self.identifier) {
            binder_process.close();
            binder_process.release(kernel);
        }
    }
}

impl FileOps for BinderConnection {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn close(
        self: Box<Self>,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObjectState,
        current_task: &CurrentTask,
    ) {
        (*self).close(current_task.kernel());
    }

    fn query_events(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        let binder_process = self.proc(current_task);
        release_after!(binder_process, current_task.kernel(), {
            Ok(match &binder_process {
                Ok(binder_process) => {
                    let binder_thread =
                        binder_process.lock().find_or_register_thread(current_task.get_tid());
                    release_after!(binder_thread, current_task.kernel(), {
                        let mut thread_state = binder_thread.lock();
                        let mut process_command_queue = binder_process.command_queue.lock();
                        BinderDriver::get_active_queue(
                            &mut thread_state,
                            &mut process_command_queue,
                        )
                        .query_events()
                    })
                }
                Err(_) => FdEvents::POLLERR,
            })
        })
    }

    fn wait_async(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        log_trace!("binder wait_async");
        let binder_process = self.proc(current_task);
        release_after!(binder_process, current_task.kernel(), {
            match &binder_process {
                Ok(binder_process) => {
                    let binder_thread =
                        binder_process.lock().find_or_register_thread(current_task.get_tid());
                    release_after!(binder_thread, current_task.kernel(), {
                        Some(self.device.wait_async(
                            &binder_process,
                            &binder_thread,
                            waiter,
                            events,
                            handler,
                        ))
                    })
                }
                Err(_) => {
                    handler.handle(FdEvents::POLLERR);
                    Some(waiter.fake_wait())
                }
            }
        })
    }

    fn ioctl(
        &self,
        locked: &mut Locked<Unlocked>,
        _file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let binder_process = self.proc(current_task)?;
        release_after!(binder_process, current_task.kernel(), {
            self.device.ioctl(
                locked,
                current_task,
                &self.security_state,
                &binder_process,
                None,
                request,
                arg,
                Vec::new(),
            )
        })
    }

    fn get_memory(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _length: Option<usize>,
        _prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        error!(EINVAL)
    }

    fn mmap(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        current_task: &CurrentTask,
        addr: DesiredAddress,
        _memory_offset: u64,
        length: usize,
        prot_flags: ProtectionFlags,
        mapping_options: MappingOptions,
        filename: NamespaceNode,
    ) -> Result<UserAddress, Errno> {
        let binder_process = self.proc(current_task)?;
        release_after!(binder_process, current_task.kernel(), {
            self.device.mmap(
                current_task,
                &binder_process,
                addr,
                length,
                prot_flags,
                mapping_options,
                filename,
            )
        })
    }

    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        _data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        error!(EOPNOTSUPP)
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        _data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        error!(EOPNOTSUPP)
    }

    fn flush(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        current_task: &CurrentTask,
    ) {
        // Errors are not meaningful on flush.
        let Ok(binder_process) = self.proc(current_task) else { return };
        release_after!(binder_process, current_task.kernel(), {
            binder_process.kick_all_threads()
        });
    }
}

/// A connection to a binder driver from a remote process.
#[derive(Debug)]
pub struct RemoteBinderConnection {
    binder_connection: BinderConnection,
}

impl RemoteBinderConnection {
    pub fn map_external_vmo(
        &self,
        current_task: &CurrentTask,
        vmo: fidl::Vmo,
        mapped_address: u64,
    ) -> Result<(), Errno> {
        let binder_process = self.binder_connection.proc(current_task)?;
        release_after!(binder_process, current_task.kernel(), {
            binder_process.map_external_vmo(vmo, mapped_address)
        })
    }

    pub fn ioctl(
        &self,
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
        vmo: zx::Vmo,
        files: Vec<fbinder::FileHandle>,
    ) -> Result<Vec<fbinder::IoctlWrite>, Errno> {
        let binder_process = self.binder_connection.proc(current_task)?;
        release_after!(binder_process, current_task.kernel(), {
            let remote_ioctl = RemoteIoctl { ioctl_writes: Cell::new(Vec::new()), vmo };
            self.binder_connection.device.ioctl(
                locked,
                current_task,
                &self.binder_connection.security_state,
                &binder_process,
                Some(&remote_ioctl),
                request,
                arg,
                files,
            )?;
            Ok(remote_ioctl.ioctl_writes.take())
        })
    }

    pub fn interrupt(&self) {
        self.binder_connection.interrupt();
    }

    pub fn close(&self, kernel: &Kernel) {
        self.binder_connection.close(kernel);
    }
}

#[derive(Debug, Default)]
pub struct BinderProcessState {
    /// Maximum number of thread to spawn.
    max_thread_count: usize,
    /// Whether a new thread has been requested, but not registered yet.
    pub thread_requested: bool,
    /// The set of threads that are interacting with the binder driver.
    thread_pool: ThreadPool,
    /// Binder objects hosted by the process shared with other processes.
    pub objects: BTreeMap<UserAddress, Arc<BinderObject>>,
    /// Handle table of remote binder objects.
    pub handles: ReleaseGuard<HandleTable>,
    /// State associated with active transactions, keyed by the userspace addresses of the buffers
    /// allocated to them. When the process frees a transaction buffer with `BC_FREE_BUFFER`, the
    /// state is dropped, releasing temporary strong references and the memory allocated to the
    /// transaction.
    active_transactions: BTreeMap<UserAddress, ReleaseGuard<ActiveTransaction>>,
    /// The list of processes that should be notified if this process dies.
    death_subscribers: Vec<(WeakRef<BinderProcess>, binder_uintptr_t)>,
    /// The list of processes that should be notified if this process is frozen.
    freeze_subscribers: Vec<(WeakRef<BinderProcess>, binder_uintptr_t)>,
    /// Whether the binder connection for this process is closed. Once closed, any blocking
    /// operation will be aborted and return an EBADF error.
    closed: bool,
    /// Whether the binder connection for this process is interrupted. A process that is
    /// interrupted either just before or while waiting must abort the operation and return a EINTR
    /// error.
    interrupted: bool,
    /// Status of the binder freeze.
    freeze_status: FreezeStatus,
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

    fn thaw(&mut self) {
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

    fn has_pending_transactions(&self) -> bool {
        !self.active_transactions.is_empty()
    }
}

#[derive(Debug, Default, Clone)]
struct FreezeStatus {
    frozen: bool,
    /// Indicates whether the process has received any sync calls since last
    /// freeze (cleared at freeze/unfreeze)
    has_sync_recv: bool,
    /// Indicates whether the process has received any async calls since last
    /// freeze (cleared at freeze/unfreeze)
    has_async_recv: bool,
}

/// An active binder transaction.
#[derive(Debug)]
struct ActiveTransaction {
    /// The transaction's request type.
    request_type: RequestType,
    /// The state associated with the transaction. Not read, exists to be dropped along with the
    /// [`ActiveTransaction`] object.
    state: ReleaseGuard<TransactionState>,
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
    fn new(accessor: &'a dyn ResourceAccessor, target_proc: &BinderProcess) -> Self {
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
    fn push_handle(&mut self, handle: Handle) {
        self.state.as_mut().unwrap().handles.push(handle)
    }

    /// Schedule `guard` to be released when the transaction ends (both in case of success or
    /// failure).
    fn push_guard(&mut self, guard: StrongRefGuard) {
        self.state.as_mut().unwrap().guards.push(guard);
    }

    /// Schedule `fd` to be removed from the file descriptor table when the transaction ends (both
    /// in case of success or failure).
    fn push_owned_fd(&mut self, fd: FdNumber) {
        self.state.as_mut().unwrap().owned_fds.push(fd)
    }

    /// Schedule `fd` to be removed from the file descriptor table if the transaction fails.
    fn push_transient_fd(&mut self, fd: FdNumber) {
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
enum RequestType {
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
    remote_resource_accessor: Option<Arc<RemoteResourceAccessor>>,

    // The mutable state of `BinderProcess` is protected by 3 locks. For ordering purpose, locks
    // must be taken in the order they are defined in this class, even across `BinderProcess`
    // instances.
    // Moreover, any `BinderThread` lock must be ordered after any `state` lock from a
    // `BinderProcess`.
    /// The [`SharedMemory`] region mapped in both the driver and the binder process. Allows for
    /// transactions to copy data once from the sender process into the receiver process.
    pub shared_memory: Mutex<Option<SharedMemory>>,

    /// The main mutable state of the `BinderProcess`.
    state: Mutex<BinderProcessState>,

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
    fn new(
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

    fn close(&self) {
        log_trace!("closing BinderProcess id={}", self.identifier);
        let mut state = self.lock();
        if !state.closed {
            state.closed = true;
            state.thread_pool.notify_all();
            self.command_queue.lock().notify_all();
        }
    }

    fn interrupt(&self) {
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
    fn get_resource_accessor<'a>(
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
    fn handle_refcount_operation(
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
    fn handle_refcount_operation_done(
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
    fn map_external_vmo(&self, vmo: fidl::Vmo, mapped_address: u64) -> Result<(), Errno> {
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
    fn get_task(&self) -> Option<TempRef<'_, Task>> {
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
    fn unregister_thread(&mut self, current_task: &CurrentTask, tid: pid_t) {
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
    fn handle_refcount_operation(
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
    fn handle_refcount_operation_done(
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
    fn should_request_thread(&self, thread: &BinderThread) -> bool {
        !self.thread_requested
            && self.thread_pool.registered_threads() < self.max_thread_count
            && thread.lock().is_main_or_registered()
            && !self.thread_pool.has_available_thread()
    }

    /// Called back when the driver successfully asked the client to start a new thread.
    fn did_request_thread(&mut self) {
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

/// Generates dead replies for all the transactions in `commands` that are targeting
/// `target_thread` and `target_proc`.
///
/// If a transaction has a target thread specified, it must match `target_thread` in order to be
/// marked dead. If a transaction does not specify a target thread, it is marked dead if the
/// target process matches `target_proc`.
///
/// If the top transaction of the transaction's `sender_thread` is targeting `target_thread` or
/// `target_proc`, the transaction is popped and a `DeadReply` command is enqueued on the

/// The set of threads that are interacting with the binder driver for a given process.
#[derive(Debug, Default)]
struct ThreadPool(BTreeMap<pid_t, OwnedRef<BinderThread>>);

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
    fn get_owner(&self, idx: usize) -> Option<WeakRef<BinderProcess>> {
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

/// Holds the context for a binder operation, including information about the sending process and
/// thread.
pub struct OperationContext<'a> {
    pub current_task: &'a CurrentTask,
    pub connection_security_state: &'a security::BinderConnectionState,
    pub binder_proc: &'a BinderProcess,
    pub binder_thread: &'a BinderThread,
    pub memory_accessor: &'a dyn MemoryAccessor,
}

impl<'a> OperationContext<'a> {
    fn resource_accessor(&self) -> &dyn ResourceAccessor {
        self.binder_proc.get_resource_accessor(self.current_task)
    }
}

/// Android's binder kernel driver implementation.
#[derive(Debug)]
pub struct BinderDriver {
    /// The context manager, the object represented by the zero handle.
    pub context_manager: Mutex<Option<Arc<BinderObject>>>,

    /// Manages the internal state of each process interacting with the binder driver.
    ///
    /// The Driver owns the BinderProcess. There can be at most one connection to the binder driver
    /// per process. When the last file descriptor to the binder in the process is closed, the
    /// value is removed from the map.
    pub procs: RwLock<BTreeMap<u64, OwnedRef<BinderProcess>>>,

    /// The identifier to use for the next created `BinderProcess`.
    next_identifier: AtomicU64Counter,
}

impl Releasable for BinderDriver {
    type Context<'a> = CurrentTaskAndLocked<'a>;

    fn release<'a>(mut self, context: CurrentTaskAndLocked<'a>) {
        let (_locked, current_task) = context;
        for binder_process in std::mem::take(self.procs.get_mut()).into_values() {
            binder_process.release(current_task.kernel());
        }
    }
}

impl Default for BinderDriver {
    #[allow(clippy::let_and_return)]
    fn default() -> Self {
        let driver = Self {
            context_manager: Default::default(),
            procs: Default::default(),
            next_identifier: Default::default(),
        };
        #[cfg(any(test, debug_assertions))]
        {
            let _l1 = driver.context_manager.lock();
            let _l2 = driver.procs.read();
        }
        driver
    }
}

impl BinderDriver {
    pub fn find_process(&self, identifier: u64) -> Result<OwnedRef<BinderProcess>, Errno> {
        self.procs.read().get(&identifier).map(OwnedRef::share).ok_or_else(|| errno!(ENOENT))
    }

    /// Finds all binder processes that associate with the given `pid`.
    fn find_processes_by_pid(&self, pid: pid_t) -> Vec<OwnedRef<BinderProcess>> {
        self.procs
            .read()
            .iter()
            .filter_map(|(_k, v)| if v.key.pid() == pid { Some(OwnedRef::share(v)) } else { None })
            .collect::<Vec<_>>()
    }

    /// Creates and register the binder process state to represent a local process with `key`.
    fn create_local_process(&self, key: ThreadGroupKey) -> u64 {
        self.create_process(key, None)
    }

    /// Creates and register the binder process state to represent a remote process with `key`.
    fn create_remote_process(
        &self,
        key: ThreadGroupKey,
        resource_accessor: RemoteResourceAccessor,
    ) -> u64 {
        self.create_process(key, Some(Arc::new(resource_accessor)))
    }

    /// Creates and register the binder process state to represent a process with `key`.
    fn create_process(
        &self,
        key: ThreadGroupKey,
        resource_accessor: Option<Arc<RemoteResourceAccessor>>,
    ) -> u64 {
        let identifier = self.next_identifier.next();
        let binder_process = BinderProcess::new(identifier, key, resource_accessor);
        assert!(
            self.procs.write().insert(identifier, binder_process).is_none(),
            "process with same identifier created"
        );
        identifier
    }

    /// Creates the binder process and thread state to represent a process with `key` and one main
    /// thread.
    #[cfg(test)]
    /// Return a `RemoteBinderConnection` that can be used to driver a remote connection to the
    /// binder device represented by this driver.
    pub fn create_process_and_thread(
        &self,
        key: ThreadGroupKey,
    ) -> (OwnedRef<BinderProcess>, OwnedRef<BinderThread>) {
        let identifier = self.create_local_process(key.clone());
        let binder_process = self.find_process(identifier).expect("find_process");
        let binder_thread = binder_process.lock().find_or_register_thread(key.pid());
        (binder_process, binder_thread)
    }

    /// Return a `RemoteBinderConnection` that can be used to driver a remote connection to the
    /// binder device represented by this driver.
    pub fn open_remote(
        this: &BinderDevice,
        current_task: &CurrentTask,
        process_accessor: ClientEnd<fbinder::ProcessAccessorMarker>,
        process: zx::Process,
    ) -> Arc<RemoteBinderConnection> {
        let process_accessor =
            fbinder::ProcessAccessorSynchronousProxy::new(process_accessor.into_channel());
        let identifier = this.create_remote_process(
            current_task.thread_group_key.clone(),
            RemoteResourceAccessor {
                process_accessor,
                process,
                remote_creds: current_task.full_current_creds(),
            },
        );
        Arc::new(RemoteBinderConnection {
            binder_connection: BinderConnection {
                identifier,
                device: BinderDevice(Arc::clone(this)),
                security_state: security::binder_connection_alloc(current_task),
            },
        })
    }

    pub fn get_context_manager(
        &self,
        current_task: &CurrentTask,
    ) -> Result<(Arc<BinderObject>, TempRef<'_, BinderProcess>), Errno> {
        let mut context_manager = self.context_manager.lock();
        if let Some(context_manager_object) = context_manager.as_ref().cloned() {
            match context_manager_object.owner.upgrade().map(TempRef::into_static) {
                Some(owner) => {
                    return Ok((context_manager_object, owner));
                }
                None => {
                    *context_manager = None;
                }
            }
        }

        log_trace!(
            "Task {} tried to get context manager but one is not registered or dead. \
            Avoid the race condition by waiting until the context manager is ready.",
            current_task.tid
        );
        error!(ENOENT)
    }

    pub fn ioctl(
        &self,
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        connection_security_state: &security::BinderConnectionState,
        binder_proc: &BinderProcess,
        remote_ioctl: Option<&RemoteIoctl>,
        request: u32,
        arg: SyscallArg,
        mut files: Vec<fbinder::FileHandle>,
    ) -> Result<SyscallResult, Errno> {
        trace_duration!(CATEGORY_STARNIX, NAME_BINDER_IOCTL, "request" => request);
        let user_arg = UserAddress::from(arg);
        let remote_memory_accessor =
            match (binder_proc.remote_resource_accessor.as_ref(), remote_ioctl) {
                (Some(remote_resource_accessor), Some(remote_ioctl)) => {
                    Some(&RemoteMemoryAccessor {
                        remote_resource_accessor: remote_resource_accessor.clone(),
                        remote_ioctl,
                    })
                }
                _ => None,
            };
        let binder_thread = binder_proc.lock().find_or_register_thread(current_task.get_tid());
        release_after!(binder_thread, current_task.kernel(), {
            match request {
                uapi::BINDER_VERSION => {
                    // A thread is requesting the version of this binder driver.
                    if user_arg.is_null() {
                        return error!(EINVAL);
                    }
                    let response =
                        binder_version { protocol_version: BINDER_CURRENT_PROTOCOL_VERSION as i32 };
                    log_trace!("binder version is {:?}", response);
                    binder_proc
                        .get_memory_accessor(current_task, remote_memory_accessor)
                        .write_object(UserRef::new(user_arg), &response)?;
                    Ok(SUCCESS)
                }
                uapi::BINDER_SET_CONTEXT_MGR | uapi::BINDER_SET_CONTEXT_MGR_EXT => {
                    // A process is registering itself as the context manager.
                    security::binder_set_context_mgr(current_task)?;
                    let flags = if request == uapi::BINDER_SET_CONTEXT_MGR_EXT {
                        if user_arg.is_null() {
                            return error!(EINVAL);
                        }
                        let user_ref = UserRef::<flat_binder_object>::new(user_arg);
                        let flat_binder_object = binder_proc
                            .get_memory_accessor(current_task, remote_memory_accessor)
                            .read_object(user_ref)?;
                        BinderObjectFlags::parse(flat_binder_object.flags)?
                    } else {
                        BinderObjectFlags::empty()
                    };

                    log_trace!("binder setting context manager with flags {:x}", flags);

                    *self.context_manager.lock() =
                        Some(BinderObject::new_context_manager_marker(binder_proc, flags));
                    Ok(SUCCESS)
                }
                uapi::BINDER_WRITE_READ => {
                    // A thread is requesting to exchange data with the binder driver.
                    if user_arg.is_null() {
                        return error!(EINVAL);
                    }

                    let memory_accessor =
                        binder_proc.get_memory_accessor(current_task, remote_memory_accessor);
                    let user_ref = UserRef::<binder_write_read>::new(user_arg);
                    let mut input = memory_accessor.read_object(user_ref)?;

                    log_trace!("binder write/read request start {:?}", input);
                    let mut has_consumed_write = false;
                    let result = (|| {
                        let context = OperationContext {
                            current_task,
                            connection_security_state: &connection_security_state,
                            binder_proc,
                            binder_thread: &binder_thread,
                            memory_accessor,
                        };
                        if input.write_size > input.write_consumed {
                            // The calling thread wants to write some data to the binder driver.
                            let mut cursor = UserMemoryCursor::new(
                                memory_accessor,
                                UserAddress::from(input.write_buffer),
                                input.write_size,
                            )?;

                            // Skip already-consumed commands.
                            cursor.advance(input.write_consumed)?;

                            // Handle all the data the calling thread sent, which may include
                            // multiple commands.
                            while cursor.bytes_read() < input.write_size as usize {
                                self.handle_thread_write(
                                    locked,
                                    &context,
                                    &mut files,
                                    &mut cursor,
                                )?;
                                has_consumed_write = true;
                                input.write_consumed = cursor.bytes_read() as u64;
                            }
                        }

                        if input.read_size > input.read_consumed {
                            // The calling thread wants to read some data from the binder driver,
                            // blocking if there is nothing immediately available.
                            let mut read_buffer = UserBuffer {
                                address: UserAddress::from(input.read_buffer),
                                length: input.read_size as usize,
                            };
                            read_buffer.advance(input.read_consumed as usize)?;
                            let read_result = match self.handle_thread_read(&context, &read_buffer)
                            {
                                // If the wait was interrupted and some command has been consumed,
                                // return a success.
                                Err(err) if err == EINTR && has_consumed_write => Ok(0),
                                r => r,
                            };
                            input.read_consumed += read_result? as u64;
                        }

                        log_trace!("binder write/read request end {:?}", input);
                        Ok(SUCCESS)
                    })();

                    // Write back to the calling thread how much data was read/written, even when
                    // returning an error.
                    memory_accessor.write_object(user_ref, &input)?;
                    result
                }
                uapi::BINDER_SET_MAX_THREADS => {
                    if user_arg.is_null() {
                        return error!(EINVAL);
                    }

                    let user_ref = UserRef::<u32>::new(user_arg);
                    let new_max_threads = binder_proc
                        .get_memory_accessor(current_task, remote_memory_accessor)
                        .read_object(user_ref)? as usize;
                    log_trace!("setting max binder threads to {}", new_max_threads);
                    binder_proc.lock().max_thread_count = new_max_threads;
                    Ok(SUCCESS)
                }
                uapi::BINDER_ENABLE_ONEWAY_SPAM_DETECTION => {
                    track_stub!(
                        TODO("https://fxbug.dev/322874289"),
                        "binder ENABLE_ONEWAY_SPAM_DETECTION"
                    );
                    Ok(SUCCESS)
                }
                uapi::BINDER_THREAD_EXIT => {
                    log_trace!("binder thread {} exiting", binder_thread.tid);
                    binder_proc.lock().unregister_thread(current_task, binder_thread.tid);
                    Ok(SUCCESS)
                }
                uapi::BINDER_GET_NODE_DEBUG_INFO => {
                    track_stub!(TODO("https://fxbug.dev/322874232"), "binder GET_NODE_DEBUG_INFO");
                    error!(EOPNOTSUPP)
                }
                uapi::BINDER_GET_NODE_INFO_FOR_REF => {
                    track_stub!(
                        TODO("https://fxbug.dev/322874148"),
                        "binder GET_NODE_INFO_FOR_REF"
                    );
                    error!(EOPNOTSUPP)
                }
                uapi::BINDER_FREEZE => {
                    if user_arg.is_null() {
                        return error!(EINVAL);
                    }

                    let user_ref = UserRef::<binder_freeze_info>::new(user_arg);
                    let binder_freeze_info { pid, enable, timeout_ms } = binder_proc
                        .get_memory_accessor(current_task, remote_memory_accessor)
                        .read_object(user_ref)?;
                    let freezing = match enable {
                        0 => false,
                        1 => true,
                        _ => return error!(EINVAL),
                    };

                    let target_binder_procs = self.find_processes_by_pid(pid as pid_t);
                    if target_binder_procs.is_empty() {
                        return error!(EINVAL);
                    }

                    release_iter_after!(target_binder_procs, current_task.kernel(), {
                        let locks =
                            target_binder_procs.iter().map(|p| &p.state).collect::<Vec<_>>();
                        let mut target_binder_procs_locked = ordered_lock_vec(&locks);
                        if !freezing {
                            target_binder_procs_locked.iter_mut().for_each(|bp| bp.thaw());
                            return Ok(SUCCESS);
                        }

                        // Clone threads in the proc to lock them all until freeze is done.
                        let threads = target_binder_procs_locked
                            .iter()
                            .map(|p| p.thread_pool.0.values().map(|t| OwnedRef::share(t)))
                            .flatten()
                            .collect::<Vec<_>>();
                        release_iter_after!(threads, current_task.kernel(), {
                            let threads_locks =
                                threads.iter().map(|t| &t.state).collect::<Vec<_>>();
                            let threads_locked = ordered_lock_vec(&threads_locks);

                            // Avoid freezing the target procs if there is any pending transaction
                            if target_binder_procs_locked
                                .iter()
                                .any(|binder_process| binder_process.has_pending_transactions())
                                || threads_locked
                                    .iter()
                                    .any(|binder_thread| binder_thread.has_pending_transactions())
                            {
                                if timeout_ms > 0 {
                                    track_stub!(
                                        TODO("https://fxbug.dev/391657004"),
                                        "BINDER_FREEZE timeout"
                                    );
                                }
                                return error!(EAGAIN);
                            }

                            target_binder_procs_locked.iter_mut().for_each(|bp| bp.freeze());
                            Ok(SUCCESS)
                        })
                    })
                }
                uapi::BINDER_GET_FROZEN_INFO => {
                    if user_arg.is_null() {
                        return error!(EINVAL);
                    }

                    let user_ref = UserRef::<binder_frozen_status_info>::new(user_arg);
                    let memory_accessor =
                        binder_proc.get_memory_accessor(current_task, remote_memory_accessor);
                    let binder_frozen_status_info { pid, .. } =
                        memory_accessor.read_object(user_ref)?;
                    let target_binder_procs = self.find_processes_by_pid(pid as pid_t);
                    if target_binder_procs.is_empty() {
                        return error!(EINVAL);
                    }
                    let mut has_sync_recv = false;
                    let mut has_async_recv = false;
                    release_iter_after!(target_binder_procs, current_task.kernel(), {
                        target_binder_procs.iter().for_each(|binder_proc| {
                            let binder_proc_state = binder_proc.lock();
                            has_sync_recv |= binder_proc_state.freeze_status.has_sync_recv;
                            has_async_recv |= binder_proc_state.freeze_status.has_async_recv;
                        });
                    });
                    memory_accessor.write_object(
                        user_ref,
                        &binder_frozen_status_info {
                            pid,
                            // TODO(https://fxbug.dev/391657004): After timeout is supported, use
                            // the second right bit as the indicator whether it has any pending
                            // transactions.
                            sync_recv: has_sync_recv as u32,
                            async_recv: has_async_recv as u32,
                        },
                    )?;
                    Ok(SUCCESS)
                }
                _ => {
                    track_stub!(
                        TODO("https://fxbug.dev/322874384"),
                        "binder unknown ioctl",
                        request
                    );
                    log_error!("binder received unknown ioctl request 0x{:08x}", request);
                    error!(EINVAL)
                }
            }
        })
    }

    /// Consumes one command from the userspace binder_write_read buffer and handles it.
    /// This method will never block.
    fn handle_thread_write<L>(
        &self,
        locked: &mut Locked<L>,
        context: &OperationContext<'_>,
        files: &mut Vec<fbinder::FileHandle>,
        cursor: &mut UserMemoryCursor,
    ) -> Result<(), Errno>
    where
        L: LockEqualOrBefore<ResourceAccessorLevel>,
    {
        let command = cursor.read_object::<binder_driver_command_protocol>()?;
        trace_duration!(CATEGORY_STARNIX, "handle_thread_write", "command" => command);
        let result = match command {
            binder_driver_command_protocol_BC_ENTER_LOOPER => {
                let mut proc_state = context.binder_proc.lock();
                context
                    .binder_thread
                    .lock()
                    .handle_looper_registration(&mut proc_state, RegistrationState::Main)
            }
            binder_driver_command_protocol_BC_REGISTER_LOOPER => {
                let mut proc_state = context.binder_proc.lock();
                context
                    .binder_thread
                    .lock()
                    .handle_looper_registration(&mut proc_state, RegistrationState::Auxilliary)
            }
            binder_driver_command_protocol_BC_INCREFS
            | binder_driver_command_protocol_BC_ACQUIRE
            | binder_driver_command_protocol_BC_DECREFS
            | binder_driver_command_protocol_BC_RELEASE => {
                let handle = cursor.read_object::<u32>()?.into();
                context.binder_proc.handle_refcount_operation(command, handle)
            }
            binder_driver_command_protocol_BC_INCREFS_DONE
            | binder_driver_command_protocol_BC_ACQUIRE_DONE => {
                let object = LocalBinderObject {
                    weak_ref_addr: UserAddress::from(cursor.read_object::<binder_uintptr_t>()?),
                    strong_ref_addr: UserAddress::from(cursor.read_object::<binder_uintptr_t>()?),
                };
                context.binder_proc.handle_refcount_operation_done(command, object)
            }
            binder_driver_command_protocol_BC_FREE_BUFFER => {
                let buffer_ptr = UserAddress::from(cursor.read_object::<binder_uintptr_t>()?);
                context.binder_proc.handle_free_buffer(buffer_ptr)
            }
            binder_driver_command_protocol_BC_REQUEST_DEATH_NOTIFICATION => {
                let handle = cursor.read_object::<u32>()?.into();
                let cookie = cursor.read_object::<binder_uintptr_t>()?;
                context.binder_proc.handle_request_death_notification(handle, cookie)
            }
            binder_driver_command_protocol_BC_CLEAR_DEATH_NOTIFICATION => {
                let handle = cursor.read_object::<u32>()?.into();
                let cookie = cursor.read_object::<binder_uintptr_t>()?;
                context.binder_proc.handle_clear_death_notification(handle, cookie)
            }
            binder_driver_command_protocol_BC_DEAD_BINDER_DONE => {
                let _cookie = cursor.read_object::<binder_uintptr_t>()?;
                Ok(())
            }
            binder_driver_command_protocol_BC_TRANSACTION => {
                let data = cursor.read_object::<binder_transaction_data>()?;
                self.handle_transaction(
                    locked,
                    context,
                    files,
                    binder_transaction_data_sg { transaction_data: data, buffers_size: 0 },
                )
                .or_else(|err| err.dispatch(context.binder_thread))
            }
            binder_driver_command_protocol_BC_REPLY => {
                let data = cursor.read_object::<binder_transaction_data>()?;
                self.handle_reply(
                    locked,
                    context,
                    files,
                    binder_transaction_data_sg { transaction_data: data, buffers_size: 0 },
                )
                .or_else(|err| err.dispatch(context.binder_thread))
            }
            binder_driver_command_protocol_BC_TRANSACTION_SG => {
                let data = cursor.read_object::<binder_transaction_data_sg>()?;
                self.handle_transaction(locked, context, files, data)
                    .or_else(|err| err.dispatch(context.binder_thread))
            }
            binder_driver_command_protocol_BC_REPLY_SG => {
                let data = cursor.read_object::<binder_transaction_data_sg>()?;
                self.handle_reply(locked, context, files, data)
                    .or_else(|err| err.dispatch(context.binder_thread))
            }
            binder_driver_command_protocol_BC_REQUEST_FREEZE_NOTIFICATION => {
                let handle = cursor.read_object::<u32>()?.into();
                let cookie = cursor.read_object::<binder_uintptr_t>()?;
                context.binder_proc.handle_request_freeze_notification(handle, cookie)
            }
            binder_driver_command_protocol_BC_FREEZE_NOTIFICATION_DONE => {
                let _cookie = cursor.read_object::<binder_uintptr_t>()?;
                Ok(())
            }
            binder_driver_command_protocol_BC_CLEAR_FREEZE_NOTIFICATION => {
                let handle = cursor.read_object::<u32>()?.into();
                let cookie = cursor.read_object::<binder_uintptr_t>()?;
                context.binder_proc.handle_clear_freeze_notification(handle, cookie)
            }
            _ => {
                log_error!("binder received unknown RW command: {:#08x}", command);
                error!(EINVAL)
            }
        };

        if let Err(err) = &result {
            // TODO(https://fxbug.dev/42068804): Right now there are many errors that happen that are due to
            // errors in the kernel driver and not because of an issue in userspace. Until the
            // driver is more stable, log these.
            log_error!("binder command {:#x} failed: {:?}", command, err);
        }
        result
    }

    /// A binder thread is starting a transaction on a remote binder object.
    pub fn handle_transaction<L>(
        &self,
        locked: &mut Locked<L>,
        context: &OperationContext<'_>,
        files: &mut Vec<fbinder::FileHandle>,
        data: binder_transaction_data_sg,
    ) -> Result<(), TransactionError>
    where
        L: LockEqualOrBefore<ResourceAccessorLevel>,
    {
        // SAFETY: Transactions can only refer to handles.
        let handle = unsafe { data.transaction_data.target.handle }.into();

        let (object, target_proc, mut guard) = match handle {
            Handle::ContextManager => {
                let (object, owner) = self.get_context_manager(context.current_task)?;
                (object, Some(owner), None)
            }
            Handle::Object { index } => {
                let binder_proc = context.binder_proc.lock();
                let (object, guard) =
                    binder_proc.handles.get(index).ok_or(TransactionError::Failure)?;
                let owner = object.owner.upgrade().map(TempRef::into_static);
                (object, owner, Some(guard))
            }
        };

        let mut actions = RefCountActions::default();
        release_after!(actions, (), {
            release_after!(guard, &mut actions, {
                let target_proc = target_proc.ok_or(TransactionError::Dead)?;
                let oneway = data.transaction_data.flags & transaction_flags_TF_ONE_WAY != 0;
                // Track freeze status if the target proc is frozen
                let is_target_frozen = {
                    let mut state = target_proc.lock();
                    if state.freeze_status.frozen {
                        state.freeze_status.has_sync_recv |= !oneway;
                        state.freeze_status.has_async_recv |= oneway;
                    }
                    state.freeze_status.frozen
                };
                // If the target proc is frozen in the sync transaction, reply with the Frozen error
                if is_target_frozen && !oneway {
                    return Err(TransactionError::Frozen);
                }
                let target_task = target_proc.get_task().ok_or(TransactionError::Dead)?;

                security::binder_transaction(
                    context.current_task,
                    &target_task,
                    context.connection_security_state,
                )?;

                let security_context: Option<FsString> =
                    if object.flags.contains(BinderObjectFlags::TXN_SECURITY_CTX) {
                        let mut security_context = FsString::from(
                            security::binder_get_context(
                                context.current_task,
                                context.connection_security_state,
                            )
                            .unwrap_or_default(),
                        );
                        security_context.push(b'\0');
                        Some(security_context)
                    } else {
                        None
                    };

                // Copy the transaction data to the target process.
                let (buffers, mut transaction_state) = self.copy_transaction_buffers(
                    locked,
                    context,
                    files,
                    &target_task,
                    target_proc.get_resource_accessor(target_task.deref()),
                    &target_proc,
                    &data,
                    security_context.as_ref().map(|c: &FsString| c.as_ref()),
                )?;

                let transaction = TransactionData {
                    peer_pid: context.binder_proc.key.pid(),
                    peer_tid: context.binder_thread.tid,
                    peer_euid: context.current_task.with_current_creds(|creds| creds.euid),
                    object: {
                        if handle.is_handle_0() {
                            // This handle (0) always refers to the context manager, which is always
                            // "remote", even for the context manager itself.
                            FlatBinderObject::Remote { handle }
                        } else {
                            FlatBinderObject::Local { object: object.local }
                        }
                    },
                    code: data.transaction_data.code,
                    flags: data.transaction_data.flags,

                    buffers: buffers.clone(),
                };

                if let Some(guard) = guard.take() {
                    transaction_state.push_guard(guard);
                }

                let (target_thread, command) = if oneway {
                    // The caller is not expecting a reply.
                    context.binder_thread.lock().enqueue_command(if is_target_frozen {
                        Command::PendingFrozen
                    } else {
                        Command::OnewayTransactionComplete
                    });

                    // Register the transaction buffer.
                    target_proc.lock().active_transactions.insert(
                        buffers.data.address,
                        ActiveTransaction {
                            request_type: RequestType::Oneway { object: object.clone() },
                            state: transaction_state.into_state(),
                        }
                        .into(),
                    );

                    // Oneway transactions are enqueued on the binder object and processed one at a time.
                    // This guarantees that oneway transactions are processed in the order they are
                    // submitted, and one at a time.
                    let mut object_state = object.lock();
                    if object_state.handling_oneway_transaction {
                        // Currently, a oneway transaction is being handled. Queue this one so that it is
                        // scheduled when the buffer from the in-progress transaction is freed.
                        object_state.oneway_transactions.push_back(transaction);
                        return Ok(());
                    }

                    // No oneway transactions are being handled, which means that no buffer will be
                    // freed, kicking off scheduling from the oneway queue. Instead, we must schedule
                    // the transaction regularly, but mark the object as handling a oneway transaction.
                    object_state.handling_oneway_transaction = true;

                    (None, Command::OnewayTransaction(transaction))
                } else {
                    let target_thread = match match context.binder_thread.lock().transactions.last()
                    {
                        Some(TransactionRole::Receiver(rx, _)) => rx.upgrade(),
                        _ => None,
                    } {
                        Some((proc, thread)) if proc.key == target_proc.key => Some(thread),
                        _ => None,
                    };

                    let transaction_sender = TransactionSender {
                        target_proc: target_proc.identifier,
                        target_thread: target_thread.as_ref().map(|t| t.tid),
                        is_alive: true,
                    };

                    // Make the sender thread part of the transaction so it doesn't get scheduled to handle
                    // any other transactions.
                    context
                        .binder_thread
                        .lock()
                        .transactions
                        .push(TransactionRole::Sender(transaction_sender));

                    // Register the transaction buffer.
                    target_proc.lock().active_transactions.insert(
                        buffers.data.address,
                        ActiveTransaction {
                            request_type: RequestType::RequestResponse,
                            state: transaction_state.into_state(),
                        }
                        .into(),
                    );

                    // There are 2 ways to declare a scheduler state for the transaction.
                    // 1. The object might contain a specific minimal scheduler state to use.
                    // 2. The current task has a non-realtime priority[0] or the object has been
                    //    configured to inherit realtime priorities from callers.
                    //
                    // The results must always be the best scheduler state according to these rules.
                    //
                    // [0]: "The binder driver has always supported nice priority inheritance." from
                    // https://source.android.com/docs/core/architecture/hidl/binder-ipc#rt-priority
                    let mut scheduler_state = object.flags.get_scheduler_state();
                    let current_scheduler_state = context.current_task.read().scheduler_state;
                    if !current_scheduler_state.is_realtime()
                        || object.flags.contains(BinderObjectFlags::INHERIT_RT)
                    {
                        // Only supercede the scheduler state from the object if this task's is higher.
                        if scheduler_state.is_none_or(|p: SchedulerState| {
                            p.is_less_than_for_binder(current_scheduler_state)
                        }) {
                            scheduler_state = Some(current_scheduler_state);
                        }
                    }

                    (
                        target_thread,
                        Command::Transaction {
                            sender: WeakBinderPeer::new(context.binder_proc, context.binder_thread),
                            data: transaction,
                            scheduler_state,
                        },
                    )
                };

                // Schedule the transaction on the target_thread if it is specified, otherwise use the
                // process' command queue.
                if let Some(target_thread) = target_thread {
                    target_thread.lock().enqueue_command(command);
                } else {
                    target_proc.enqueue_command(command);
                }
                Ok(())
            })
        })
    }

    /// A binder thread is sending a reply to a transaction.
    pub fn handle_reply<L>(
        &self,
        locked: &mut Locked<L>,
        context: &OperationContext<'_>,
        files: &mut Vec<fbinder::FileHandle>,
        data: binder_transaction_data_sg,
    ) -> Result<(), TransactionError>
    where
        L: LockEqualOrBefore<ResourceAccessorLevel>,
    {
        // Find the process and thread that initiated the transaction. This reply is for them.
        let (target_proc, target_thread, scheduler_state) =
            context.binder_thread.lock().pop_transaction_caller(context.current_task)?;
        if let Err(e) = release_after!(
            scheduler_state,
            context.current_task,
            || -> Result<(), TransactionError> {
                let target_task = target_proc.get_task().ok_or(TransactionError::Dead)?;

                // Copy the transaction data to the target process.
                let (buffers, transaction_state) = self.copy_transaction_buffers(
                    locked,
                    context,
                    files,
                    &target_task,
                    target_proc.get_resource_accessor(target_task.deref()),
                    &target_proc,
                    &data,
                    None,
                )?;

                // Register the transaction buffer.
                target_proc.lock().active_transactions.insert(
                    buffers.data.address,
                    ActiveTransaction {
                        request_type: RequestType::RequestResponse,
                        state: transaction_state.into_state(),
                    }
                    .into(),
                );

                // Atomically enqueue the reply on the target thread and the
                // transaction complete command on the local thread.
                {
                    let (mut target_thread, mut binder_thread) =
                        BinderThread::lock_both(&target_thread, context.binder_thread);
                    target_thread.enqueue_command(Command::Reply(TransactionData {
                        peer_pid: context.binder_proc.key.pid(),
                        peer_tid: context.binder_thread.tid,
                        peer_euid: context.current_task.with_current_creds(|creds| creds.euid),

                        object: FlatBinderObject::Remote { handle: Handle::ContextManager },
                        code: data.transaction_data.code,
                        flags: data.transaction_data.flags,

                        buffers,
                    }));

                    binder_thread.enqueue_command(Command::TransactionComplete);
                }

                Ok(())
            }
        ) {
            // Sending to the target process failed, notify of the transaction failure.
            let _ = e.dispatch(&target_thread);
            return Err(e);
        }
        Ok(())
    }

    /// Select which command queue to read from, preferring the thread-local one.
    /// If a transaction is pending, deadlocks can happen if reading from the process queue.
    fn get_active_queue<'a>(
        thread_state: &'a mut BinderThreadState,
        proc_command_queue: &'a mut CommandQueueWithWaitQueue,
    ) -> &'a mut CommandQueueWithWaitQueue {
        if !thread_state.command_queue.is_empty() || !thread_state.transactions.is_empty() {
            &mut thread_state.command_queue
        } else {
            proc_command_queue
        }
    }

    /// Dequeues a command from the thread's commands' queue, or blocks until commands are available.
    pub fn handle_thread_read(
        &self,
        context: &OperationContext<'_>,
        read_buffer: &UserBuffer,
    ) -> Result<usize, Errno> {
        loop {
            {
                let mut binder_proc_state = context.binder_proc.lock();

                if binder_proc_state.should_request_thread(context.binder_thread) {
                    let bytes_written = Command::SpawnLooper
                        .write_to_memory(context.memory_accessor, read_buffer)?;
                    binder_proc_state.did_request_thread();
                    return Ok(bytes_written);
                }
            }

            // THREADING: Always acquire the [`BinderThread::state`] lock before the
            // [`BinderProcess::command_queue`] lock or else it may lead to deadlock.
            let mut thread_state = context.binder_thread.lock();
            let mut proc_command_queue = context.binder_proc.command_queue.lock();

            if thread_state.request_kick {
                thread_state.request_kick = false;
                return Ok(0);
            }

            // Select which command queue to read from, preferring the thread-local one.
            // If a transaction is pending, deadlocks can happen if reading from the process queue.
            let command_queue = Self::get_active_queue(&mut thread_state, &mut proc_command_queue);

            let command = command_queue.pop_front().or_else(|| {
                // If there is no pending command, but the current transaction is marked as dead,
                // pop the transaction and dispatch a `DeadReply`.
                match thread_state.transactions.last() {
                    Some(TransactionRole::Sender(TransactionSender {
                        is_alive: false, ..
                    })) => {
                        thread_state.transactions.pop();
                        Some(Command::DeadReply)
                    }
                    _ => None,
                }
            });

            if let Some(command) = command {
                // Attempt to write the command to the thread's buffer.
                let bytes_written =
                    command.write_to_memory(context.memory_accessor, read_buffer)?;
                match command {
                    Command::Transaction { sender, scheduler_state, .. } => {
                        // The transaction is synchronous and we're expected to give a reply, so
                        // push the transaction onto the transaction stack.

                        // If the transaction must inherit the sender scheduler state, let update the
                        // scheduler state, and keep track of the previous one.
                        let scheduler_state = (|| {
                            if let Some(scheduler_state) = scheduler_state {
                                let old_scheduler_state =
                                    context.current_task.read().scheduler_state;
                                if old_scheduler_state.is_less_than_for_binder(scheduler_state) {
                                    match context.current_task.set_scheduler_state(scheduler_state)
                                    {
                                        Ok(()) => return SchedulerGuard::from(old_scheduler_state),
                                        Err(e) => {
                                            log_warn!(
                                                "Unable to update scheduler state of task {} to {scheduler_state:?}: {e:?}",
                                                context.current_task.tid
                                            );
                                        }
                                    }
                                }
                            }
                            SchedulerGuard::default()
                        })();
                        let tx = TransactionRole::Receiver(sender, scheduler_state);
                        thread_state.transactions.push(tx);
                    }
                    Command::Reply(..) => {
                        // The sender got a reply, pop the sender entry from the transaction stack.
                        let transaction =
                            thread_state.transactions.pop().expect("transaction stack underflow!");
                        // Command::Reply is sent to the receiver side. So the popped transaction
                        // must be a Sender role.
                        assert!(
                            matches!(transaction, TransactionRole::Sender(_)),
                            "Active Transaction: {:?}, Pending Transactions {:?}, Command: {:?}, Pending Commands: {:?}",
                            transaction,
                            thread_state.transactions,
                            command,
                            thread_state.command_queue,
                        );
                    }
                    Command::TransactionComplete
                    | Command::OnewayTransaction(..)
                    | Command::OnewayTransactionComplete
                    | Command::AcquireRef(..)
                    | Command::ReleaseRef(..)
                    | Command::IncRef(..)
                    | Command::DecRef(..)
                    | Command::Error(..)
                    | Command::FailedReply
                    | Command::DeadReply
                    | Command::DeadBinder(..)
                    | Command::FrozenReply
                    | Command::PendingFrozen
                    | Command::ClearDeathNotificationDone(..)
                    | Command::SpawnLooper
                    | Command::FrozenBinder(..)
                    | Command::ClearFreezeNotificationDone(..) => {}
                }

                return Ok(bytes_written);
            }

            // No commands readily available to read. Wait for work. The thread will wait on both
            // the thread queue and the process queue, and loop back to check whether some work is
            // available.
            let event = InterruptibleEvent::new();
            let (mut waiter, guard) = SimpleWaiter::new(&event);
            proc_command_queue.wait_async_simple(&mut waiter);
            thread_state.command_queue.wait_async_simple(&mut waiter);
            drop(thread_state);
            drop(proc_command_queue);

            {
                let mut proc_state = context.binder_proc.lock();
                // Ensure the file descriptor has not been closed or interrupted, after registering
                // for the waiters but before waiting.
                if proc_state.closed {
                    return error!(EBADF);
                }

                if proc_state.interrupted {
                    proc_state.interrupted = false;
                    return error!(EINTR);
                }
            }

            // Put this thread to sleep.
            // TODO(https://fxbug.dev/401258133) pass a thread handle for priority inheritance
            context.current_task.block_until(guard, zx::MonotonicInstant::INFINITE)?;
        }
    }

    /// Copies transaction buffers from the source process' address space to a new buffer in the
    /// target process' shared binder VMO.
    /// Returns the transaction buffers in the target process, as well as the transaction state.
    ///
    /// If `security_context` is present, it must be null terminated.
    pub fn copy_transaction_buffers<'a, L>(
        &self,
        locked: &mut Locked<L>,
        source: &OperationContext<'_>,
        source_files: &mut Vec<fbinder::FileHandle>,
        target_task: &Task,
        target_resource_accessor: &'a dyn ResourceAccessor,
        target_proc: &BinderProcess,
        data: &binder_transaction_data_sg,
        security_context: Option<&FsStr>,
    ) -> Result<(TransactionBuffers, TransientTransactionState<'a>), TransactionError>
    where
        L: LockEqualOrBefore<ResourceAccessorLevel>,
    {
        // Get the shared memory of the target process.
        let mut shared_memory_lock = target_proc.shared_memory.lock();
        let shared_memory = shared_memory_lock.as_mut().ok_or_else(|| errno!(ENOMEM))?;

        // Allocate a buffer from the target process' shared memory.
        let mut allocations = shared_memory.allocate_buffers(
            data.transaction_data.data_size as usize,
            data.transaction_data.offsets_size as usize,
            data.buffers_size as usize,
            round_up_to_increment(
                security_context.map(|s| s.len()).unwrap_or(0),
                std::mem::size_of::<binder_uintptr_t>(),
            )?,
        )?;

        // Copy the security context content.
        if let Some(data) = security_context {
            let security_buffer = allocations.security_context_buffer.as_mut().unwrap();
            security_buffer.as_mut_bytes()[..data.len()].copy_from_slice(data);
        }

        // SAFETY: `binder_transaction_data` was read from a userspace VMO, which means that all
        // bytes are defined, making union access safe (even if the value is garbage).
        let userspace_addrs = unsafe { data.transaction_data.data.ptr };

        // Copy the data straight into the target's buffer.
        source.memory_accessor.read_memory_to_slice(
            UserAddress::from(userspace_addrs.buffer),
            allocations.data_buffer.as_mut_bytes(),
        )?;
        source.memory_accessor.read_objects_to_slice(
            UserRef::new(UserAddress::from(userspace_addrs.offsets)),
            allocations.offsets_buffer.as_mut_bytes(),
        )?;

        // Translate any handles/fds from the source process' handle table to the target process'
        // handle table.
        let transient_transaction_state = self.translate_objects(
            locked,
            source,
            source_files,
            target_task,
            target_resource_accessor,
            target_proc,
            allocations.offsets_buffer.as_bytes(),
            allocations.data_buffer.as_mut_bytes(),
            &mut allocations.scatter_gather_buffer,
        )?;

        Ok((allocations.into(), transient_transaction_state))
    }

    /// Translates file descriptors from the sending process to the receiving process.
    fn translate_files<'a, L>(
        locked: &mut Locked<L>,
        source: &OperationContext<'_>,
        source_files: &mut Vec<fbinder::FileHandle>,
        target_task: &Task,
        target_resource_accessor: &'a dyn ResourceAccessor,
        fds: Vec<FdNumber>,
        add_action: &mut dyn FnMut(FdNumber),
    ) -> Result<Vec<FdNumber>, Errno>
    where
        L: LockEqualOrBefore<ResourceAccessorLevel>,
    {
        // Create a map of FD to index into `source_files`. This is used to check if the already
        // file exists in `source_files`, allowing us to avoid fetching it via the
        // `source_resource_accessor`.
        let source_map = source_files
            .iter()
            .enumerate()
            .map(|(i, file)| (file.fd.unwrap(), i))
            .collect::<HashMap<_, _>>();
        // Create a vector of FDs to fetch via the `source_resource_accessor`.
        let fds_to_get = fds
            .iter()
            .filter(|fd| !source_map.contains_key(&fd.raw()))
            .copied()
            .collect::<Vec<_>>();
        let locked = locked.cast_locked::<ResourceAccessorLevel>();
        let mut get_files = if fds_to_get.is_empty() {
            Vec::new()
        } else {
            source.resource_accessor().get_files_with_flags(
                locked,
                source.current_task,
                fds_to_get,
            )?
        };
        let mut drain = get_files.drain(0..);
        // Merge `source_files` and `get_files` together.
        let mut target_files = Vec::with_capacity(fds.len());
        for fd in fds {
            let file = if let Some(pos) = source_map.get(&fd.raw()) {
                let source_file = std::mem::replace(&mut source_files[*pos], Default::default());
                let flags = source_file.flags.ok_or_else(|| errno!(ENOENT))?.into_fidl();
                let new_file = if let Some(file) = source_file.file {
                    new_remote_file(locked, source.current_task, file, flags)?
                } else {
                    new_null_file(locked, source.current_task, flags)
                };
                (new_file, FdFlags::empty())
            } else if let Some(file) = drain.next() {
                file
            } else {
                return error!(ENOENT);
            };

            security::binder_transfer_file(source.current_task, target_task, &(file.0))?;

            target_files.push(file);
        }
        // Finally add the files to the `target_resource_accessor`.
        target_resource_accessor.add_files_with_flags(
            locked,
            source.current_task,
            target_files,
            add_action,
        )
    }

    /// Translates binder object handles/FDs from the sending process to the receiver process,
    /// patching the transaction data as needed.
    ///
    /// When a binder object is sent from one process to another, it must be added to the receiving
    /// process' handle table. Conversely, a handle being sent to the process that owns the
    /// underlying binder object should receive the actual pointers to the object.
    ///
    /// When a binder buffer object is sent from one process to another, the buffer described by the
    /// buffer object must be copied into the receiver's address space.
    ///
    /// When a binder file descriptor object is sent from one process to another, the file
    /// descriptor must be `dup`-ed into the receiver's FD table.
    ///
    /// Returns [`TransientTransactionState`], which contains the handles in the target process'
    /// handle table for which temporary strong references were acquired, along with duped FDs. This
    /// object takes care of releasing these resources when dropped, due to an error or a
    /// `BC_FREE_BUFFER` command.
    pub fn translate_objects<'a, L>(
        &self,
        locked: &mut Locked<L>,
        source: &OperationContext<'_>,
        source_files: &mut Vec<fbinder::FileHandle>,
        target_task: &Task,
        target_resource_accessor: &'a dyn ResourceAccessor,
        target_proc: &BinderProcess,
        offsets: &[binder_uintptr_t],
        transaction_data: &mut [u8],
        sg_buffer: &mut SharedBuffer<'_, u8>,
    ) -> Result<TransientTransactionState<'a>, TransactionError>
    where
        L: LockEqualOrBefore<ResourceAccessorLevel>,
    {
        let mut transaction_state =
            TransientTransactionState::new(target_resource_accessor, target_proc);
        release_on_error!(transaction_state, (), {
            let mut sg_remaining_buffer = sg_buffer.user_buffer();
            let mut sg_buffer_offset = 0;
            let mut files = Vec::with_capacity(offsets.len());
            for (offset_idx, object_offset) in offsets.iter().map(|o| *o as usize).enumerate() {
                // Bounds-check the offset.
                if object_offset >= transaction_data.len() {
                    return error!(EINVAL)?;
                }
                let serialized_object =
                    SerializedBinderObject::from_bytes(&transaction_data[object_offset..])?;
                let translated_object = match serialized_object {
                    SerializedBinderObject::Handle { handle, flags, cookie } => {
                        security::binder_transfer_binder(source.current_task, target_task)?;

                        match handle {
                            Handle::ContextManager => {
                                // The special handle 0 does not need to be translated. It is universal.
                                serialized_object
                            }
                            Handle::Object { index } => {
                                // 1. Find the object and add a guard on it in the
                                //    transaction to ensures the receiving process keep
                                //    it alive until the transactions is finished
                                let (proxy, guard) = source
                                    .binder_proc
                                    .lock()
                                    .handles
                                    .get(index)
                                    .ok_or(TransactionError::Failure)?;
                                if proxy.owner.as_ptr() == target_proc.weak_self.as_ptr() {
                                    // The binder object belongs to the receiving process.

                                    transaction_state.push_guard(guard);

                                    // 2. Convert the binder object from a handle to a local object.
                                    SerializedBinderObject::Object { local: proxy.local, flags }
                                } else {
                                    // The binder object does not belong to the receiving
                                    // process.

                                    // Insert the handle in the handle table of the receiving process
                                    // and add a strong reference to it to ensure it survives for the
                                    // lifetime of the transaction.
                                    let mut actions = RefCountActions::default();
                                    let new_handle = target_proc
                                        .lock()
                                        .insert_for_transaction(guard, &mut actions);
                                    actions.release(());
                                    // Tie this handle's strong reference to be held as long as this
                                    // buffer.
                                    transaction_state.push_handle(new_handle);
                                    SerializedBinderObject::Handle {
                                        handle: new_handle,
                                        flags,
                                        cookie,
                                    }
                                }
                            }
                        }
                    }
                    SerializedBinderObject::Object { local, flags } => {
                        security::binder_transfer_binder(source.current_task, target_task)?;

                        let mut actions = RefCountActions::default();
                        release_after!(actions, (), {
                            // We are passing a binder object across process boundaries. We need
                            // to translate this address to some handle.

                            // Register this binder object if it hasn't already been registered.
                            let guard = source.binder_proc.lock().find_or_register_object(
                                source.binder_thread,
                                local,
                                flags,
                            );
                            // Create a handle in the receiving process that references the binder object
                            // in the sender's process.
                            let handle =
                                target_proc.lock().insert_for_transaction(guard, &mut actions);
                            // Tie this handle's strong reference to be held as long as this buffer.
                            transaction_state.push_handle(handle);

                            // Translate the serialized object into a handle.
                            SerializedBinderObject::Handle { handle, flags, cookie: 0 }
                        })
                    }
                    SerializedBinderObject::File { fd, cookie } => {
                        files.push(TransientFile { object_offset, fd, cookie });
                        continue;
                    }
                    SerializedBinderObject::Buffer {
                        buffer,
                        length,
                        flags,
                        parent,
                        parent_offset,
                    } => {
                        // Copy the memory pointed to by this buffer object into the receiver.
                        if length > sg_remaining_buffer.length {
                            return error!(EINVAL)?;
                        }
                        source.memory_accessor.read_memory_to_slice(
                            buffer,
                            &mut sg_buffer.as_mut_bytes()
                                [sg_buffer_offset..sg_buffer_offset + length],
                        )?;

                        let translated_buffer_address = sg_remaining_buffer.address;

                        // If the buffer has a parent, it means that the parent buffer has a pointer to
                        // this buffer. This pointer will need to be translated to the receiver's
                        // address space.
                        if flags & BINDER_BUFFER_FLAG_HAS_PARENT != 0 {
                            // The parent buffer must come earlier in the object list and already be
                            // copied into the receiver's address space. Otherwise we would be fixing
                            // up memory in the sender's address space, which is marked const in the
                            // userspace runtime.
                            if parent >= offset_idx {
                                return error!(EINVAL)?;
                            }

                            // Find the parent buffer payload. There is a pointer in the buffer
                            // that points to this object.
                            let parent_buffer_payload = find_parent_buffer(
                                transaction_data,
                                sg_buffer,
                                offsets[parent] as usize,
                            )?;

                            // Bounds-check that the offset is within the buffer.
                            if parent_offset >= parent_buffer_payload.len() {
                                return error!(EINVAL)?;
                            }

                            // Patch the pointer with the translated address.
                            translated_buffer_address
                                .write_to_prefix(&mut parent_buffer_payload[parent_offset..])
                                .map_err(|_| errno!(EINVAL))?;
                        }

                        // Update the scatter-gather buffer to account for the buffer we just wrote.
                        // We pad the length of this buffer so that the next buffer starts at an aligned
                        // offset.
                        let padded_length =
                            round_up_to_increment(length, std::mem::size_of::<binder_uintptr_t>())?;
                        sg_remaining_buffer = UserBuffer {
                            address: (sg_remaining_buffer.address + padded_length)?,
                            length: sg_remaining_buffer.length - padded_length,
                        };
                        sg_buffer_offset += padded_length;

                        // Patch this buffer with the translated address.
                        SerializedBinderObject::Buffer {
                            buffer: translated_buffer_address,
                            length,
                            flags,
                            parent,
                            parent_offset,
                        }
                    }
                    SerializedBinderObject::FileArray { num_fds, parent, parent_offset } => {
                        // The parent buffer must come earlier in the object list and already be
                        // copied into the receiver's address space. Otherwise we would be fixing
                        // up memory in the sender's address space, which is marked const in the
                        // userspace runtime.
                        if parent >= offset_idx {
                            return error!(EINVAL)?;
                        }

                        // Find the parent buffer payload. The file descriptor array is in here.
                        let parent_buffer_payload = find_parent_buffer(
                            transaction_data,
                            sg_buffer,
                            offsets[parent] as usize,
                        )?;

                        // Bounds-check that the offset is within the buffer.
                        if parent_offset >= parent_buffer_payload.len() {
                            return error!(EINVAL)?;
                        }

                        // Verify alignment and size before reading the data as a [u32].
                        let (layout, _) =
                            zerocopy::Ref::<&mut [u8], [u32]>::from_prefix_with_elems(
                                &mut parent_buffer_payload[parent_offset..],
                                num_fds,
                            )
                            .map_err(|_| errno!(EINVAL))?;
                        let fd_array = zerocopy::Ref::into_mut(layout);

                        // Dup each file descriptor and re-write the value of the new FD.
                        let new_fds = Self::translate_files(
                            locked,
                            source,
                            source_files,
                            target_task,
                            target_resource_accessor,
                            fd_array.iter().map(|fd| FdNumber::from_raw(*fd as i32)).collect(),
                            // Close this FD if the transaction ends either by success or failure.
                            &mut |fd| transaction_state.push_owned_fd(fd),
                        )?;
                        for (fd, new_fd) in std::iter::zip(fd_array, new_fds) {
                            *fd = new_fd.raw() as u32;
                        }

                        SerializedBinderObject::FileArray { num_fds, parent, parent_offset }
                    }
                };

                translated_object.write_to(&mut transaction_data[object_offset..])?;
            }

            let new_fds = Self::translate_files(
                locked,
                source,
                source_files,
                target_task,
                target_resource_accessor,
                files.iter().map(|TransientFile { fd, .. }| *fd).collect(),
                // Close this FD if the transaction fails.
                &mut |fd| transaction_state.push_transient_fd(fd),
            )?;
            for (TransientFile { object_offset, cookie, .. }, new_fd) in
                std::iter::zip(files, new_fds)
            {
                SerializedBinderObject::File { fd: new_fd, cookie }
                    .write_to(&mut transaction_data[object_offset..])?;
            }

            Ok(())
        });
        Ok(transaction_state)
    }

    pub fn mmap(
        &self,
        current_task: &CurrentTask,
        binder_proc: &BinderProcess,
        addr: DesiredAddress,
        length: usize,
        prot_flags: ProtectionFlags,
        mapping_options: MappingOptions,
        filename: NamespaceNode,
    ) -> Result<UserAddress, Errno> {
        // Do not support mapping shared memory more than once.
        let mut shared_memory = binder_proc.shared_memory.lock();
        if shared_memory.is_some() {
            return error!(EINVAL);
        }

        // Create a VMO that will be shared between the driver and the client process.
        let vmo = with_zx_name(
            zx::Vmo::create(length as u64).map_err(|_| errno!(ENOMEM))?,
            b"starnix:device_binder",
        );
        let memory = Arc::new(MemoryObject::from(vmo));

        // Map the VMO into the binder process' address space.
        let mm = current_task.mm()?;
        let user_address = mm.map_memory(
            addr,
            memory.clone(),
            0,
            length,
            prot_flags,
            prot_flags.to_access(),
            mapping_options,
            MappingName::File(filename.into_mapping(None)?),
        )?;

        // Map the VMO into the driver's address space.
        match SharedMemory::map(&memory, user_address, length) {
            Ok(mem) => {
                *shared_memory = Some(mem);
                Ok(user_address)
            }
            Err(err) => {
                // Try to cleanup by unmapping from userspace, but ignore any errors. We
                // can't really recover from them.
                let _ = mm.unmap(user_address, length);
                Err(err)
            }
        }
    }

    fn wait_async(
        &self,
        binder_proc: &BinderProcess,
        binder_thread: &BinderThread,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> WaitCanceler {
        // THREADING: Always acquire the [`BinderThread::state`] lock before the
        // [`BinderProcess::command_queue`] lock or else it may lead to deadlock.
        let thread_state = binder_thread.lock();
        let proc_command_queue = binder_proc.command_queue.lock();

        let handler = match handler {
            EventHandler::None | EventHandler::HandleOnce(_) => handler,
            EventHandler::Enqueue { .. } | EventHandler::Epoll(_) => {
                EventHandler::HandleOnce(Arc::new(Mutex::new(Some(handler))))
            }
        };

        let w1 = thread_state.command_queue.wait_async_fd_events(waiter, events, handler.clone());
        let w2 = proc_command_queue.waiters.wait_async_fd_events(waiter, events, handler);
        WaitCanceler::merge(w1, w2)
    }
}

/// Finds a buffer object's payload in the transaction. The buffer object describing the payload is
/// deserialized from `transaction_data` at `buffer_object_offset`. The actual payload is located in
/// `sg_buffer`. The buffer object must have already been validated and its payload copied to
/// `sg_buffer`. This is true for parent objects, as they are required to be processed before being
/// referenced by child objects.
fn find_parent_buffer<'a>(
    transaction_data: &[u8],
    sg_buffer: &mut SharedBuffer<'a, u8>,
    buffer_object_offset: usize,
) -> Result<&'a mut [u8], Errno> {
    // The buffer object has already been validated, since the requirement is that parent objects
    // are processed before their children. In addition, the payload has been written by us, so it
    // should be guaranteed to be valid. Still, it is possible for userspace to mutate this memory
    // while we are processing it, so we still perform checked arithmetic to avoid panics in
    // starnix.

    // Verify that the offset is within the transaction data.
    if buffer_object_offset >= transaction_data.len() {
        return error!(EINVAL);
    }

    // Deserialize the parent object buffer and extract the relevant data.
    let (buffer_payload_addr, buffer_payload_length) =
        match SerializedBinderObject::from_bytes(&transaction_data[buffer_object_offset..])? {
            SerializedBinderObject::Buffer { buffer, length, .. } => (buffer, length),
            _ => return error!(EINVAL)?,
        };

    // Calculate the start and end of the buffer payload in the scatter gather buffer.
    // The buffer payload will have been copied to the scatter gather buffer, so recover the
    // offset from its userspace address.
    if buffer_payload_addr < sg_buffer.user_buffer().address {
        // This should never happen unless userspace is messing with us, since we wrote this address
        // during translation.
        return error!(EINVAL);
    }
    let buffer_payload_start = buffer_payload_addr - sg_buffer.user_buffer().address;
    let buffer_payload_end =
        buffer_payload_start.checked_add(buffer_payload_length).ok_or_else(|| errno!(EINVAL))?;

    // Return a slice that represents the parent buffer.
    Ok(&mut sg_buffer.as_mut_bytes()[buffer_payload_start..buffer_payload_end])
}

/// Represents a file descriptor during a binder transaction.
struct TransientFile {
    /// Offset of `BINDER_TYPE_FD` object within transaction data.
    object_offset: usize,
    /// A `BINDER_TYPE_FD` object. A file descriptor.
    fd: FdNumber,
    cookie: binder_uintptr_t,
}

/// An error processing a binder transaction/reply.
///
/// Some errors, like a malformed transaction request, should be propagated as the return value of
/// an ioctl. Other errors, like a dead recipient or invalid binder handle, should be propagated
/// through a command read by the binder thread.
///
/// This type differentiates between these strategies.
#[derive(Debug, Eq, PartialEq)]
pub enum TransactionError {
    /// The transaction payload was malformed. Send a [`Command::Error`] command to the issuing
    /// thread.
    Malformed(Errno),
    /// The transaction payload was correctly formed, but either the recipient, or a handle embedded
    /// in the transaction, is invalid. Send a [`Command::FailedReply`] command to the issuing
    /// thread.
    Failure,
    /// The transaction payload was correctly formed, but either the recipient, or a handle embedded
    /// in the transaction, is dead. Send a [`Command::DeadReply`] command to the issuing thread.
    Dead,
    /// The binder thread is frozen. Send a [`Command::FrozenReply`] command to the issuing thread.
    Frozen,
}

impl TransactionError {
    /// Dispatches the error, by potentially queueing a command to `binder_thread` and/or returning
    /// an error.
    pub fn dispatch(&self, binder_thread: &BinderThread) -> Result<(), Errno> {
        log_trace!("Dispatching transaction error {:?} for thread {}", self, binder_thread.tid);
        binder_thread.lock().enqueue_command(match self {
            TransactionError::Malformed(err) => {
                log_warn!(
                    "binder thread {} sent a malformed transaction: {:?}",
                    binder_thread.tid,
                    &err
                );
                // Negate the value, as the binder runtime assumes error values are already
                // negative.
                Command::Error(err.return_value() as i32)
            }
            TransactionError::Failure => Command::FailedReply,
            TransactionError::Dead => Command::DeadReply,
            TransactionError::Frozen => Command::FrozenReply,
        });
        Ok(())
    }
}

impl From<Errno> for TransactionError {
    fn from(errno: Errno) -> TransactionError {
        match errno.code {
            EACCES | EPERM => TransactionError::Failure,
            _ => TransactionError::Malformed(errno),
        }
    }
}

/// Returns a task in the process keyed by `key`.
fn get_task_for_thread_group(key: &ThreadGroupKey) -> Option<TempRef<'_, Task>> {
    key.upgrade().and_then(|tg| {
        let tg = tg.read();
        tg.get_task(tg.leader()).or_else(|| tg.tasks().next()).map(TempRef::into_static)
    })
}
