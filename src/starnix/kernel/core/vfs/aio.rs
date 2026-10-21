// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mm::{
    DesiredAddress, IOVecPtr, MappingName, MappingOptions, MemoryAccessorExt, MemoryManager,
    ProtectionFlags, TaskMemoryAccessor,
};
use crate::task::dynamic_thread_spawner::SpawnRequestBuilder;
use crate::task::{CurrentTask, SimpleWaiter, Task, TaskFlags, WaitQueue};
use crate::vfs::eventfd::EventFdFileObject;
use crate::vfs::syscalls::IocbPtr;
use crate::vfs::{
    FdNumber, FileHandle, InputBuffer, UserBuffersInputBuffer, VecInputBuffer, VecOutputBuffer,
    WeakFileHandle, checked_add_offset_and_length,
};
use smallvec::smallvec;
use starnix_logging::track_stub;
use starnix_sync::{AioEventsLock, AioPendingOperationsLock, InterruptibleEvent, LockDepMutex};
use starnix_syscalls::SyscallResult;
use starnix_types::user_buffer::{UserBuffer, UserBuffers};
use starnix_uapi::errors::{EINTR, ETIMEDOUT, Errno};
use starnix_uapi::{
    IOCB_CMD_PREAD, IOCB_CMD_PREADV, IOCB_CMD_PWRITE, IOCB_CMD_PWRITEV, IOCB_FLAG_RESFD,
    aio_context_t, errno, error, io_event, iocb,
};
use std::collections::VecDeque;
use std::sync::{Arc, OnceLock, Weak};
use zerocopy::IntoBytes;

/// From aio.go in gVisor.
const AIO_RING_SIZE: usize = 32;

/// Kernel state-machine-based implementation of asynchronous I/O.
/// See https://man7.org/linux/man-pages/man7/aio.7.html#NOTES
pub struct AioContext {
    inner: Arc<AioContextInner>,
}

impl AioContext {
    pub fn create(
        current_task: &CurrentTask,
        max_operations: usize,
    ) -> Result<aio_context_t, Errno> {
        let mm = current_task.mm()?;
        let inner = AioContextInner::new(max_operations);
        inner.spawn_worker_for(current_task, WorkerType::Read);
        inner.spawn_worker_for(current_task, WorkerType::Write);
        let context = Arc::new(AioContext { inner });
        let context_addr = mm.map_anonymous(
            DesiredAddress::Any,
            AIO_RING_SIZE,
            ProtectionFlags::READ | ProtectionFlags::WRITE,
            MappingOptions::ANONYMOUS | MappingOptions::DONT_EXPAND,
            MappingName::AioContext(context),
        )?;
        Ok(context_addr.ptr() as aio_context_t)
    }

    pub fn get_events(
        &self,
        current_task: &CurrentTask,
        min_results: usize,
        max_results: usize,
        deadline: zx::MonotonicInstant,
    ) -> Result<Vec<io_event>, Errno> {
        self.inner.get_events(current_task, min_results, max_results, deadline)
    }

    pub fn submit(
        self: &Arc<Self>,
        current_task: &CurrentTask,
        control_block: iocb,
        iocb_addr: IocbPtr,
    ) -> Result<(), Errno> {
        self.inner.submit(current_task, control_block, iocb_addr)
    }

    pub fn cancel(
        self: &Arc<Self>,
        _current_task: &CurrentTask,
        control_block: iocb,
        iocb_addr: IocbPtr,
    ) -> Result<(), Errno> {
        self.inner.cancel(control_block, iocb_addr)
    }
}

impl std::fmt::Debug for AioContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AioContext").finish()
    }
}

impl std::cmp::PartialEq for AioContext {
    fn eq(&self, other: &AioContext) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl std::cmp::Eq for AioContext {}

impl Drop for AioContext {
    fn drop(&mut self) {
        self.inner.stop();
    }
}

struct WorkerCompletion {
    thread_id: OnceLock<std::thread::ThreadId>,
    done: zx::Event,
}

impl Default for WorkerCompletion {
    fn default() -> Self {
        Self { thread_id: OnceLock::new(), done: zx::Event::create() }
    }
}

impl WorkerCompletion {
    fn record_current_thread(&self) {
        self.thread_id
            .set(std::thread::current().id())
            .expect("worker thread id should only be set once");
    }

    fn is_current_thread(&self) -> bool {
        self.thread_id.get() == Some(&std::thread::current().id())
    }

    fn signal(&self) {
        let _ = self.done.signal(zx::Signals::NONE, zx::Signals::USER_0);
    }

    fn wait(&self) {
        // `AioContext` is owned by `MemoryManager` (via `MappingName::AioContext`) and is normally
        // dropped by the user task on `io_destroy`, `munmap`, or task exit (`CurrentTask::exit`).
        // However, because Starnix uses a single `Arc<MemoryManager>` reference count (rather than
        // separate `mm_users` / `mm_count` with an explicit `exit_aio` step on last user exit), a
        // worker transiently upgrading `Weak<MemoryManager>` to write into user memory in
        // `IoOperation::do_read` can race with `CurrentTask::exit` and end up dropping the last
        // `Arc<MemoryManager>` (and thus `AioContext`) on the worker thread itself. Skip waiting if
        // `stop()` is running on this worker thread to avoid self-deadlock.
        if !self.is_current_thread() {
            let _ = self.done.wait_one(zx::Signals::USER_0, zx::MonotonicInstant::INFINITE);
        }
    }
}

struct AioContextInner {
    operations: OperationQueue,
    results: ResultQueue,
    read_worker: WorkerCompletion,
    write_worker: WorkerCompletion,
}

impl AioContextInner {
    fn new(max_operations: usize) -> Arc<Self> {
        Arc::new(Self {
            operations: OperationQueue::new(max_operations),
            results: Default::default(),
            read_worker: Default::default(),
            write_worker: Default::default(),
        })
    }

    fn worker_for(&self, worker_type: WorkerType) -> &WorkerCompletion {
        match worker_type {
            WorkerType::Read => &self.read_worker,
            WorkerType::Write => &self.write_worker,
        }
    }

    fn stop(&self) {
        self.operations.stop();
        self.write_worker.wait();
        self.read_worker.wait();
    }

    fn get_events(
        &self,
        current_task: &CurrentTask,
        min_results: usize,
        max_results: usize,
        deadline: zx::MonotonicInstant,
    ) -> Result<Vec<io_event>, Errno> {
        let mut events = self.results.dequeue(max_results);
        if events.len() >= min_results {
            return Ok(events);
        }
        let event = InterruptibleEvent::new();
        loop {
            let (mut waiter, guard) = SimpleWaiter::new(&event);
            self.results.waiters.wait_async_simple(&mut waiter);
            events.extend(self.results.dequeue(max_results - events.len()));
            if events.len() >= min_results {
                return Ok(events);
            }
            match current_task.block_until(guard, deadline) {
                Err(err) if err == ETIMEDOUT => {
                    return Ok(events);
                }
                Err(err) if err == EINTR => {
                    if events.is_empty() {
                        Err(err)
                    } else {
                        return Ok(events);
                    }
                }
                result => result,
            }?;
        }
    }

    fn submit(
        self: &Arc<Self>,
        current_task: &CurrentTask,
        control_block: iocb,
        iocb_addr: IocbPtr,
    ) -> Result<(), Errno> {
        let op = IoOperation::new(current_task, control_block, iocb_addr)?;
        self.operations.enqueue(op)
    }

    fn cancel(self: &Arc<Self>, control_block: iocb, iocb_addr: IocbPtr) -> Result<(), Errno> {
        let op_type: OpType = (control_block.aio_lio_opcode as u32).try_into()?;
        self.operations.remove(op_type.worker_type(), iocb_addr)
    }

    fn spawn_worker_for(self: &Arc<Self>, current_task: &CurrentTask, worker_type: WorkerType) {
        let creds = current_task.current_creds().clone();
        let inner = self.clone();
        let closure = move |current_task: &CurrentTask| {
            let worker = inner.worker_for(worker_type);
            worker.record_current_thread();
            let _guard = scopeguard::guard((), |_| {
                if matches!(worker_type, WorkerType::Read) {
                    current_task.write().set_flags(TaskFlags::WORK_CANCELLED, false);
                }
                worker.signal();
            });
            current_task
                .override_creds(creds, || inner.perform_next_action(current_task, worker_type))
        };
        let req = SpawnRequestBuilder::new()
            .with_debug_name("aio-worker")
            .with_sync_closure(closure)
            .build();
        current_task.kernel().kthreads.spawner().spawn_from_request(req);
    }

    fn perform_next_action(&self, current_task: &CurrentTask, worker_type: WorkerType) {
        while let Ok(IoAction::Op(mut op)) =
            self.operations.block_until_dequeue(current_task, worker_type)
        {
            let result = op.execute(current_task);
            if matches!(worker_type, WorkerType::Read) {
                // Clear `active_read_task` now that execution is complete so subsequent `stop()`
                // calls do not interrupt the worker during eventfd notifications or file cleanup.
                // Also reset `WORK_CANCELLED` if this operation was interrupted, ensuring
                // `trigger_delayed_releaser()` / `FileOps::close` does not fail with `EINTR`.
                self.operations.finish_active_read_op(current_task);
            }
            if let Some(result) = result {
                self.results.enqueue(op.complete(result));

                if let Some(eventfd) = op.eventfd.as_ref().and_then(Weak::upgrade) {
                    let mut input_buffer = VecInputBuffer::new(1u64.as_bytes());
                    let _ = eventfd.write(current_task, &mut input_buffer);
                }
            }
            drop(op);
            // Release closed files immediately instead of waiting for the worker thread to exit.
            current_task.trigger_delayed_releaser();
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum WorkerType {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy)]
enum OpType {
    PRead,
    PReadV,
    // TODO: IOCB_CMD_FSYNC
    // TODO: IOCB_CMD_FDSYNC
    // TODO: IOCB_CMD_POLL
    // TODO: IOCB_CMD_NOOP
    PWrite,
    PWriteV,
}

impl OpType {
    fn worker_type(self) -> WorkerType {
        match self {
            OpType::PRead | OpType::PReadV => WorkerType::Read,
            OpType::PWrite | OpType::PWriteV => WorkerType::Write,
        }
    }
}

impl TryFrom<u32> for OpType {
    type Error = Errno;

    fn try_from(opcode: u32) -> Result<Self, Self::Error> {
        match opcode {
            IOCB_CMD_PREAD => Ok(Self::PRead),
            IOCB_CMD_PREADV => Ok(Self::PReadV),
            IOCB_CMD_PWRITE => Ok(Self::PWrite),
            IOCB_CMD_PWRITEV => Ok(Self::PWriteV),
            _ => {
                track_stub!(TODO("https://fxbug.dev/297433877"), "io_submit opcode", opcode);
                return error!(ENOSYS);
            }
        }
    }
}

enum IoOperationType {
    Read { file: WeakFileHandle, mm: Weak<MemoryManager>, buffers: UserBuffers },
    Write { file: FileHandle, buffer: Result<VecInputBuffer, Errno> },
}

struct IoOperation {
    op: IoOperationType,
    offset: usize,
    id: u64,
    iocb_addr: IocbPtr,
    eventfd: Option<WeakFileHandle>,
}

impl IoOperation {
    fn new(
        current_task: &CurrentTask,
        control_block: iocb,
        iocb_addr: IocbPtr,
    ) -> Result<Self, Errno> {
        if control_block.aio_reserved2 != 0 {
            return error!(EINVAL);
        }
        let file = current_task.files().get(FdNumber::from_raw(control_block.aio_fildes as i32))?;
        let op_type = (control_block.aio_lio_opcode as u32).try_into()?;
        let offset = control_block.aio_offset.try_into().map_err(|_| errno!(EINVAL))?;
        let flags = control_block.aio_flags;

        match op_type {
            OpType::PRead | OpType::PReadV => {
                if !file.can_read() {
                    return error!(EBADF);
                }
            }
            OpType::PWrite | OpType::PWriteV => {
                if !file.can_write() {
                    return error!(EBADF);
                }
            }
        }
        let mut buffers = match op_type {
            OpType::PRead | OpType::PWrite => smallvec![UserBuffer {
                address: control_block.aio_buf.into(),
                length: control_block.aio_nbytes as usize,
            }],
            OpType::PReadV | OpType::PWriteV => {
                let iovec_addr = IOVecPtr::new(current_task, control_block.aio_buf);
                let count: i32 = control_block.aio_nbytes.try_into().map_err(|_| errno!(EINVAL))?;
                current_task.read_iovec(iovec_addr, count.into())?
            }
        };

        // Validate the user buffers and offset synchronously.
        let buffer_length = UserBuffer::cap_buffers_to_max_rw_count(
            current_task.maximum_valid_address().ok_or_else(|| errno!(EINVAL))?,
            &mut buffers,
        )?;
        checked_add_offset_and_length(offset, buffer_length)?;

        let eventfd = if flags & IOCB_FLAG_RESFD != 0 {
            let eventfd =
                current_task.files().get(FdNumber::from_raw(control_block.aio_resfd as i32))?;
            if eventfd.downcast_file::<EventFdFileObject>().is_none() {
                return error!(EINVAL);
            }
            Some(Arc::downgrade(&eventfd))
        } else {
            None
        };

        let op = match op_type {
            OpType::PRead | OpType::PReadV => {
                let mm = current_task.mm()?;
                IoOperationType::Read {
                    file: Arc::downgrade(&file),
                    mm: Arc::downgrade(&mm),
                    buffers,
                }
            }
            OpType::PWrite | OpType::PWriteV => {
                let buffer = UserBuffersInputBuffer::syscall_new(current_task, buffers)
                    .and_then(|mut source| source.read_all())
                    .map(VecInputBuffer::from);
                IoOperationType::Write { file, buffer }
            }
        };

        Ok(IoOperation { op, offset, id: control_block.aio_data, iocb_addr, eventfd })
    }

    fn worker_type(&self) -> WorkerType {
        match &self.op {
            IoOperationType::Read { .. } => WorkerType::Read,
            IoOperationType::Write { .. } => WorkerType::Write,
        }
    }

    fn execute(&mut self, current_task: &CurrentTask) -> Option<Result<SyscallResult, Errno>> {
        let offset = self.offset;
        let result = match &mut self.op {
            IoOperationType::Read { file, mm, buffers } => {
                let Some(file) = file.upgrade() else {
                    // The FileHandle can close while async IO operations are ongoing.
                    // Ignore this operation when this happens.
                    return None;
                };
                Self::do_read(current_task, file, mm, buffers, offset).map(Into::into)
            }
            IoOperationType::Write { file, buffer } => match buffer {
                Ok(input_buffer) => {
                    Self::do_write(current_task, file, offset, input_buffer).map(Into::into)
                }
                Err(errno) => Err(errno.clone()),
            },
        };
        Some(result)
    }

    fn complete(&self, result: Result<SyscallResult, Errno>) -> io_event {
        let res = match result {
            Ok(return_value) => return_value.value() as i64,
            Err(errno) => errno.return_value() as i64,
        };

        io_event { data: self.id, obj: self.iocb_addr.addr().into(), res, ..Default::default() }
    }

    fn do_read(
        current_task: &CurrentTask,
        file: FileHandle,
        mm: &Weak<MemoryManager>,
        buffers: &UserBuffers,
        offset: usize,
    ) -> Result<usize, Errno> {
        let mut output_buffer = VecOutputBuffer::new(buffers.iter().map(|b| b.length).sum());

        if file.ops().is_seekable() {
            file.read_at(current_task, offset, &mut output_buffer)?;
        } else {
            file.read(current_task, &mut output_buffer)?;
        }
        drop(file);

        let mm = mm.upgrade().ok_or_else(|| errno!(EFAULT))?;
        let mut bytes = output_buffer.data();
        let mut bytes_written = 0;
        for buffer in buffers {
            if buffer.is_null() {
                continue;
            }
            let bytes_len = std::cmp::min(bytes.len(), buffer.length);
            let (to_write, remaining) = bytes.split_at(bytes_len);
            bytes = remaining;
            bytes_written += mm.syscall_write_memory(buffer.address, to_write)?;
            if bytes_len < buffer.length {
                break;
            }
        }
        Ok(bytes_written)
    }

    fn do_write(
        current_task: &CurrentTask,
        file: &FileHandle,
        offset: usize,
        input_buffer: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        if file.ops().is_seekable() {
            file.write_at(current_task, offset, input_buffer)
        } else {
            file.write(current_task, input_buffer)
        }
    }
}

enum IoAction {
    Op(IoOperation),
    Stop,
}

#[derive(Default)]
struct PendingOperations {
    is_stopped: bool,
    // We currently queue the read and write operations to separate threads.
    // That behavior is incorrect, but it keeps our clients working well enough while we work on
    // getting the correct parallelism.
    read_ops: VecDeque<IoOperation>,
    write_ops: VecDeque<IoOperation>,
    active_read_task: Weak<Task>,
}

impl PendingOperations {
    fn ops_for(&mut self, worker_type: WorkerType) -> &mut VecDeque<IoOperation> {
        match worker_type {
            WorkerType::Read => &mut self.read_ops,
            WorkerType::Write => &mut self.write_ops,
        }
    }

    fn ops_len(&self) -> usize {
        self.read_ops.len() + self.write_ops.len()
    }
}

struct OperationQueue {
    max_operations: usize,
    pending: LockDepMutex<PendingOperations, AioPendingOperationsLock>,
    read_waiters: WaitQueue,
    write_waiters: WaitQueue,
}

impl OperationQueue {
    fn new(max_operations: usize) -> Self {
        Self {
            max_operations,
            pending: Default::default(),
            read_waiters: Default::default(),
            write_waiters: Default::default(),
        }
    }

    fn waiters_for(&self, worker_type: WorkerType) -> &WaitQueue {
        match worker_type {
            WorkerType::Read => &self.read_waiters,
            WorkerType::Write => &self.write_waiters,
        }
    }

    fn enqueue(&self, op: IoOperation) -> Result<(), Errno> {
        let worker_type = op.worker_type();
        {
            let mut pending = self.pending.lock();
            if pending.is_stopped {
                return error!(EINVAL);
            }
            if pending.ops_len() >= self.max_operations {
                return error!(EAGAIN);
            }
            pending.ops_for(worker_type).push_back(op);
        }
        self.waiters_for(worker_type).notify_unordered_count(1);
        Ok(())
    }

    fn stop(&self) {
        {
            let mut pending = self.pending.lock();
            pending.is_stopped = true;
            pending.read_ops.clear();
            if let Some(task) = std::mem::take(&mut pending.active_read_task).upgrade() {
                task.write().set_flags(TaskFlags::WORK_CANCELLED, true);
                task.interrupt();
            }
        }
        self.read_waiters.notify_all();
        self.write_waiters.notify_all();
    }

    fn finish_active_read_op(&self, current_task: &CurrentTask) {
        let mut pending = self.pending.lock();
        pending.active_read_task = Default::default();
        current_task.write().set_flags(TaskFlags::WORK_CANCELLED, false);
    }

    fn dequeue(&self, current_task: &CurrentTask, worker_type: WorkerType) -> Option<IoAction> {
        let mut pending = self.pending.lock();
        match worker_type {
            WorkerType::Read => {
                if pending.is_stopped {
                    return Some(IoAction::Stop);
                }
                if let Some(op) = pending.read_ops.pop_front() {
                    pending.active_read_task = current_task.weak_task();
                    Some(IoAction::Op(op))
                } else {
                    None
                }
            }
            WorkerType::Write => {
                if let Some(op) = pending.write_ops.pop_front() {
                    return Some(IoAction::Op(op));
                }
                if pending.is_stopped {
                    return Some(IoAction::Stop);
                }
                None
            }
        }
    }

    fn remove(&self, worker_type: WorkerType, iocb_addr: IocbPtr) -> Result<(), Errno> {
        {
            let mut pending = self.pending.lock();
            if pending.is_stopped {
                return error!(EINVAL);
            }
            // TODO: Use pop_front_if when available.
            if let Some(idx) = pending
                .ops_for(worker_type)
                .iter()
                .position(|value| value.iocb_addr.addr() == iocb_addr.addr())
            {
                let _ = pending.ops_for(worker_type).remove(idx);
            } else {
                return error!(EAGAIN);
            }
        }
        Ok(())
    }

    fn block_until_dequeue(
        &self,
        current_task: &CurrentTask,
        worker_type: WorkerType,
    ) -> Result<IoAction, Errno> {
        if let Some(action) = self.dequeue(current_task, worker_type) {
            return Ok(action);
        }
        loop {
            let event = InterruptibleEvent::new();
            let (mut waiter, guard) = SimpleWaiter::new(&event);
            self.waiters_for(worker_type).wait_async_simple(&mut waiter);
            if let Some(action) = self.dequeue(current_task, worker_type) {
                return Ok(action);
            }
            match current_task.block_until(guard, zx::MonotonicInstant::INFINITE) {
                Err(err) if err == EINTR => continue,
                result => result?,
            }
        }
    }
}

#[derive(Default)]
struct ResultQueue {
    waiters: WaitQueue,
    events: LockDepMutex<VecDeque<io_event>, AioEventsLock>,
}

impl ResultQueue {
    fn enqueue(&self, event: io_event) {
        self.events.lock().push_back(event);
        self.waiters.notify_unordered_count(1);
    }

    fn dequeue(&self, limit: usize) -> Vec<io_event> {
        let mut events = self.events.lock();
        let len = std::cmp::min(events.len(), limit);
        events.drain(..len).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::{MemoryAccessor, PAGE_SIZE};
    use crate::task::Waiter;
    use crate::testing::{map_memory, spawn_kernel_and_run};
    use crate::vfs::{
        Anon, FdFlags, FileObject, FileOps, OutputBuffer, fileops_impl_noop_sync,
        fileops_impl_seekless,
    };
    use starnix_sync::Mutex;
    use starnix_uapi::open_flags::OpenFlags;
    use starnix_uapi::user_address::{UserAddress, UserRef};
    use starnix_uapi::{CLONE_SIGHAND, CLONE_THREAD, CLONE_VM};

    struct TestSinkFile {
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl FileOps for TestSinkFile {
        fileops_impl_seekless!();
        fileops_impl_noop_sync!();

        fn read(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            _data: &mut dyn OutputBuffer,
        ) -> Result<usize, Errno> {
            error!(EINVAL)
        }

        fn write(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            data: &mut dyn InputBuffer,
        ) -> Result<usize, Errno> {
            let bytes = data.read_all()?;
            let len = bytes.len();
            self.written.lock().extend_from_slice(&bytes);
            Ok(len)
        }
    }

    struct TestBlockedReadFile {
        read_started: Arc<zx::Event>,
        file_released: Arc<zx::Event>,
    }

    impl Drop for TestBlockedReadFile {
        fn drop(&mut self) {
            let _ = self.file_released.signal(zx::Signals::NONE, zx::Signals::USER_0);
        }
    }

    impl FileOps for TestBlockedReadFile {
        fileops_impl_seekless!();
        fileops_impl_noop_sync!();

        fn read(
            &self,
            _file: &FileObject,
            current_task: &CurrentTask,
            _offset: usize,
            _data: &mut dyn OutputBuffer,
        ) -> Result<usize, Errno> {
            let _ = self.read_started.signal(zx::Signals::NONE, zx::Signals::USER_0);
            let waiter = Waiter::new();
            waiter.wait(current_task)?;
            unreachable!("blocked read should be interrupted on context drop");
        }

        fn write(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            _data: &mut dyn InputBuffer,
        ) -> Result<usize, Errno> {
            error!(EINVAL)
        }
    }

    struct TestGatedReadFile {
        unblock: Arc<zx::Event>,
        payload: Vec<u8>,
    }

    impl FileOps for TestGatedReadFile {
        fileops_impl_seekless!();
        fileops_impl_noop_sync!();

        fn read(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            data: &mut dyn OutputBuffer,
        ) -> Result<usize, Errno> {
            self.unblock
                .wait_one(zx::Signals::USER_0, zx::MonotonicInstant::INFINITE)
                .map_err(|_| errno!(EINTR))?;
            data.write(&self.payload)
        }

        fn write(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            _data: &mut dyn InputBuffer,
        ) -> Result<usize, Errno> {
            error!(EINVAL)
        }
    }

    #[::fuchsia::test]
    async fn test_pwrite_completes_after_fd_closed_and_buffer_unmapped() {
        spawn_kernel_and_run(async |current_task| {
            let header = b"A_WRTE_HEADER:";
            let payload = b"restarting adbd as root\n";
            let user_buf = map_memory(current_task, UserAddress::default(), *PAGE_SIZE);
            current_task.write_memory(user_buf, header).expect("write header");
            let payload_buf = (user_buf + header.len()).expect("payload addr");
            current_task.write_memory(payload_buf, payload).expect("write user payload");

            let iocb_addr1 = map_memory(current_task, UserAddress::default(), *PAGE_SIZE);
            let iocb_ref1 = IocbPtr::new(current_task, iocb_addr1);
            let iocb_addr2 = (iocb_addr1 + std::mem::size_of::<iocb>()).expect("iocb2 addr");
            let iocb_ref2 = IocbPtr::new(current_task, iocb_addr2);

            let written = Arc::new(Mutex::new(Vec::new()));
            let write_file = Anon::new_private_file(
                current_task,
                Box::new(TestSinkFile { written: Arc::clone(&written) }),
                OpenFlags::WRONLY,
                "[test-sink]",
            );
            let write_fd = current_task.add_file(write_file, FdFlags::empty()).expect("add file");

            let cb1 = iocb {
                aio_fildes: write_fd.raw() as u32,
                aio_lio_opcode: IOCB_CMD_PWRITE as u16,
                aio_buf: user_buf.ptr() as u64,
                aio_nbytes: header.len() as u64,
                aio_offset: 0,
                aio_data: 0x1234,
                ..Default::default()
            };
            let cb2 = iocb {
                aio_fildes: write_fd.raw() as u32,
                aio_lio_opcode: IOCB_CMD_PWRITE as u16,
                aio_buf: payload_buf.ptr() as u64,
                aio_nbytes: payload.len() as u64,
                aio_offset: 0,
                aio_data: 0x1235,
                ..Default::default()
            };

            let ctx_id = AioContext::create(current_task, 16).expect("create aio context");
            let aio_ctx = current_task
                .mm()
                .expect("mm")
                .get_aio_context(ctx_id.into())
                .expect("get aio context");
            aio_ctx.submit(current_task, cb1, iocb_ref1).expect("submit header op");
            aio_ctx.submit(current_task, cb2, iocb_ref2).expect("submit payload op");

            // Simulate adbd exiting immediately after io_submit returns: close the FD table entry,
            // unmap userspace buffers, and stop the AIO context before waiting for background writes.
            current_task.files().close(write_fd).expect("close write fd");
            current_task
                .mm()
                .expect("mm")
                .unmap(user_buf, (*PAGE_SIZE) as usize)
                .expect("unmap user buf");
            aio_ctx.inner.stop();

            let mut expected = Vec::new();
            expected.extend_from_slice(header);
            expected.extend_from_slice(payload);
            assert_eq!(written.lock().as_slice(), expected.as_slice());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_drop_cancels_blocked_read_and_releases_file() {
        spawn_kernel_and_run(async |current_task| {
            let user_buf = map_memory(current_task, UserAddress::default(), *PAGE_SIZE);
            let iocb_addr = map_memory(current_task, UserAddress::default(), *PAGE_SIZE);
            let iocb_ref = IocbPtr::new(current_task, iocb_addr);

            let read_started = Arc::new(zx::Event::create());
            let file_released = Arc::new(zx::Event::create());
            let read_file = Anon::new_private_file(
                current_task,
                Box::new(TestBlockedReadFile {
                    read_started: Arc::clone(&read_started),
                    file_released: Arc::clone(&file_released),
                }),
                OpenFlags::RDONLY,
                "[test-blocked-read]",
            );
            let weak_read_file = Arc::downgrade(&read_file);
            let read_fd = current_task.add_file(read_file, FdFlags::empty()).expect("add file");

            let cb = iocb {
                aio_fildes: read_fd.raw() as u32,
                aio_lio_opcode: IOCB_CMD_PREAD as u16,
                aio_buf: user_buf.ptr() as u64,
                aio_nbytes: 64,
                aio_offset: 0,
                aio_data: 0x5678,
                ..Default::default()
            };

            let ctx_id = AioContext::create(current_task, 16).expect("create aio context");
            let aio_ctx = current_task
                .mm()
                .expect("mm")
                .get_aio_context(ctx_id.into())
                .expect("get aio context");
            aio_ctx.submit(current_task, cb, iocb_ref).expect("submit read op");
            std::mem::drop(aio_ctx);

            // Wait until the read worker has upgraded its `WeakFileHandle` and entered `read()`.
            read_started
                .wait_one(zx::Signals::USER_0, zx::MonotonicInstant::INFINITE)
                .expect("read started");

            current_task.files().close(read_fd).expect("close read fd");
            current_task.trigger_delayed_releaser();
            assert!(
                weak_read_file.upgrade().is_some(),
                "blocked read worker should hold strong FileHandle before context teardown"
            );

            let aio_ctx = current_task
                .mm()
                .expect("mm")
                .destroy_aio_context(ctx_id.into())
                .expect("destroy aio context");
            std::mem::drop(aio_ctx);

            file_released
                .wait_one(zx::Signals::USER_0, zx::MonotonicInstant::INFINITE_PAST)
                .expect("read file released synchronously before context drop returns");

            assert!(
                weak_read_file.upgrade().is_none(),
                "tearing down AioContext must interrupt blocked read and release FileHandle"
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pread_completes_after_submitting_thread_exits() {
        spawn_kernel_and_run(async |current_task| {
            let payload = b"pread-after-submitting-thread-exit";
            let user_buf = map_memory(current_task, UserAddress::default(), *PAGE_SIZE);
            let iocb_addr = map_memory(current_task, UserAddress::default(), *PAGE_SIZE);
            let iocb_ref = IocbPtr::new(current_task, iocb_addr);

            let unblock = Arc::new(zx::Event::create());
            let read_file = Anon::new_private_file(
                current_task,
                Box::new(TestGatedReadFile {
                    unblock: Arc::clone(&unblock),
                    payload: payload.to_vec(),
                }),
                OpenFlags::RDONLY,
                "[test-gated-pread]",
            );
            let read_fd = current_task.add_file(read_file, FdFlags::empty()).expect("add file");

            let cb = iocb {
                aio_fildes: read_fd.raw() as u32,
                aio_lio_opcode: IOCB_CMD_PREAD as u16,
                aio_buf: user_buf.ptr() as u64,
                aio_nbytes: payload.len() as u64,
                aio_offset: 0,
                aio_data: 0x9abc,
                ..Default::default()
            };

            let ctx_id = AioContext::create(current_task, 16).expect("create aio context");
            let aio_ctx = current_task
                .mm()
                .expect("mm")
                .get_aio_context(ctx_id.into())
                .expect("get aio context");

            let submitting_thread = current_task
                .clone_task_for_test((CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64, None);
            let weak_submitting_task = Arc::downgrade(&submitting_thread.task);
            aio_ctx.submit(&submitting_thread, cb, iocb_ref).expect("submit pread op");
            std::mem::drop(submitting_thread);
            assert!(
                weak_submitting_task.upgrade().is_none(),
                "submitting thread Task should be dropped before read completes"
            );

            unblock.signal(zx::Signals::NONE, zx::Signals::USER_0).expect("signal unblock");
            let events = aio_ctx
                .get_events(current_task, 1, 1, zx::MonotonicInstant::INFINITE)
                .expect("get_events");
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].data, 0x9abc);
            assert_eq!(events[0].res, payload.len() as i64);

            let read_bytes =
                current_task.read_memory_to_vec(user_buf, payload.len()).expect("read user_buf");
            assert_eq!(read_bytes.as_slice(), payload);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_preadv_completes_after_submitting_thread_exits() {
        spawn_kernel_and_run(async |current_task| {
            let part1 = b"preadv-part1:";
            let part2 = b"preadv-part2";
            let mut payload = Vec::new();
            payload.extend_from_slice(part1);
            payload.extend_from_slice(part2);

            let part1_buf = map_memory(current_task, UserAddress::default(), *PAGE_SIZE);
            let part2_buf = (part1_buf + part1.len()).expect("part2_buf addr");

            let iovec_addr = map_memory(current_task, UserAddress::default(), *PAGE_SIZE);
            let iovecs = [
                UserBuffer { address: part1_buf, length: part1.len() },
                UserBuffer { address: part2_buf, length: part2.len() },
            ];
            current_task
                .write_objects(UserRef::new(iovec_addr), &iovecs)
                .expect("write iovecs to user memory");

            let iocb_addr = map_memory(current_task, UserAddress::default(), *PAGE_SIZE);
            let iocb_ref = IocbPtr::new(current_task, iocb_addr);

            let unblock = Arc::new(zx::Event::create());
            let read_file = Anon::new_private_file(
                current_task,
                Box::new(TestGatedReadFile {
                    unblock: Arc::clone(&unblock),
                    payload: payload.clone(),
                }),
                OpenFlags::RDONLY,
                "[test-gated-preadv]",
            );
            let read_fd = current_task.add_file(read_file, FdFlags::empty()).expect("add file");

            let cb = iocb {
                aio_fildes: read_fd.raw() as u32,
                aio_lio_opcode: IOCB_CMD_PREADV as u16,
                aio_buf: iovec_addr.ptr() as u64,
                aio_nbytes: iovecs.len() as u64,
                aio_offset: 0,
                aio_data: 0x9abd,
                ..Default::default()
            };

            let ctx_id = AioContext::create(current_task, 16).expect("create aio context");
            let aio_ctx = current_task
                .mm()
                .expect("mm")
                .get_aio_context(ctx_id.into())
                .expect("get aio context");

            let submitting_thread = current_task
                .clone_task_for_test((CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64, None);
            let weak_submitting_task = Arc::downgrade(&submitting_thread.task);
            aio_ctx.submit(&submitting_thread, cb, iocb_ref).expect("submit preadv op");
            std::mem::drop(submitting_thread);
            assert!(
                weak_submitting_task.upgrade().is_none(),
                "submitting thread Task should be dropped before read completes"
            );

            unblock.signal(zx::Signals::NONE, zx::Signals::USER_0).expect("signal unblock");
            let events = aio_ctx
                .get_events(current_task, 1, 1, zx::MonotonicInstant::INFINITE)
                .expect("get_events");
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].data, 0x9abd);
            assert_eq!(events[0].res, payload.len() as i64);

            let read_part1 =
                current_task.read_memory_to_vec(part1_buf, part1.len()).expect("read part1_buf");
            assert_eq!(read_part1.as_slice(), part1);

            let read_part2 =
                current_task.read_memory_to_vec(part2_buf, part2.len()).expect("read part2_buf");
            assert_eq!(read_part2.as_slice(), part2);
        })
        .await;
    }
}
