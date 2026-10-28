// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mm::{MemoryAccessorExt, PAGE_SIZE, TaskMemoryAccessor};
use crate::security;
use crate::signals::{SignalInfo, send_standard_signal};
use crate::task::{CurrentTask, EventHandler, WaitCallback, WaitCanceler, WaitQueue, Waiter};
use crate::vfs::buffers::{
    Buffer, InputBuffer, InputBufferCallback, InputBufferExt, MessageData, MessageQueue,
    OutputBuffer, OutputBufferCallback, PeekBufferSegmentsCallback, PipeMessageData,
    UserBuffersOutputBuffer, VecInputBuffer, VecOutputBuffer,
};
use crate::vfs::fs_registry::FsRegistry;
use crate::vfs::{
    CacheMode, FileHandle, FileObject, FileObjectState, FileOps, FileSystem, FileSystemHandle,
    FileSystemOps, FileSystemOptions, FsNodeInfo, FsStr, SpecialNode, default_fcntl,
    fileops_impl_nonseekable, fileops_impl_noop_sync,
};
use starnix_sync::{LockDepGuard, LockDepMutex, PipeLock, ordered_lock};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_types::user_buffer::{UserBuffer, UserBuffers};
use starnix_types::vfs::default_statfs;
use starnix_uapi::auth::CAP_SYS_RESOURCE;
use starnix_uapi::errors::{EAGAIN, EPIPE, Errno};
use starnix_uapi::file_mode::mode;
use starnix_uapi::fs_type::FileSystemTypeFlags;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::resource_limits::Resource;
use starnix_uapi::signals::{SIGPIPE, SIGXFSZ};
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    F_GETPIPE_SZ, F_SETPIPE_SZ, FIONREAD, PIPEFS_MAGIC, errno, error, statfs, uapi,
};
use std::sync::Arc;

const ATOMIC_IO_BYTES: u16 = 4096;

/// The maximum size of a pipe, independent of task capabilities and sysctl limits.
const PIPE_MAX_SIZE: usize = 1 << 31;

/// Wait-queue key notified when an in-progress splice read (`SpliceReadGuard`) completes.
const SPLICE_READ_WAIT_KEY: u64 = 0;

/// Wait-queue key notified when an in-progress splice write (`SpliceWriteGuard`) completes.
const SPLICE_WRITE_WAIT_KEY: u64 = 1;

/// Wait-queue key notified whenever buffer space increases, even if available space remains below
/// `PIPE_BUF`.
const PIPE_WRITE_SPACE_WAIT_KEY: u64 = 2;

#[derive(Debug)]
pub struct Pipe {
    messages: MessageQueue<PipeMessageData>,

    waiters: WaitQueue,

    /// The number of open readers.
    reader_count: usize,

    /// Whether the pipe has ever had a reader.
    had_reader: bool,

    /// The number of open writers.
    writer_count: usize,

    /// Whether the pipe has ever had a writer.
    had_writer: bool,

    /// Whether an in-flight splice read is currently consuming messages from this pipe.
    splice_read_in_progress: bool,

    /// Number of bytes at the front of the queue reserved by an in-progress splice read.
    splice_read_reserved_bytes: usize,

    /// Whether an in-flight splice write is currently producing messages into this pipe.
    splice_write_in_progress: bool,
}

pub type PipeHandle = Arc<LockDepMutex<Pipe, PipeLock>>;

impl Pipe {
    pub fn new(default_pipe_capacity: usize) -> PipeHandle {
        Arc::new(
            Pipe {
                messages: MessageQueue::new(default_pipe_capacity),
                waiters: WaitQueue::default(),
                reader_count: 0,
                had_reader: false,
                writer_count: 0,
                had_writer: false,
                splice_read_in_progress: false,
                splice_read_reserved_bytes: 0,
                splice_write_in_progress: false,
            }
            .into(),
        )
    }

    pub fn open(
        current_task: &CurrentTask,
        pipe: &PipeHandle,
        flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let mut events = FdEvents::empty();
        let mut pipe_locked = pipe.lock();
        let mut must_wait_events = FdEvents::empty();
        if flags.can_read() {
            if !pipe_locked.had_reader {
                events |= FdEvents::POLLOUT;
            }
            pipe_locked.add_reader();
            if !flags.contains(OpenFlags::NONBLOCK) && !flags.can_write() && !pipe_locked.had_writer
            {
                must_wait_events |= FdEvents::POLLIN;
            }
        }
        if flags.can_write() {
            // https://man7.org/linux/man-pages/man2/open.2.html says:
            //
            //  ENXIO  O_NONBLOCK | O_WRONLY is set, the named file is a FIFO,
            //         and no process has the FIFO open for reading.
            if flags.contains(OpenFlags::NONBLOCK) && pipe_locked.reader_count == 0 {
                assert!(!flags.can_read()); // Otherwise add_reader() would have run above.
                return error!(ENXIO);
            }
            if !pipe_locked.had_writer {
                events |= FdEvents::POLLIN;
            }
            pipe_locked.add_writer();
            if !flags.contains(OpenFlags::NONBLOCK) && !pipe_locked.had_reader {
                must_wait_events |= FdEvents::POLLOUT;
            }
        }
        if events != FdEvents::empty() {
            pipe_locked.waiters.notify_fd_events(events);
        }
        let ops = PipeFileObject { pipe: Arc::clone(pipe) };
        if must_wait_events == FdEvents::empty() {
            return Ok(Box::new(ops));
        }

        // Ensures that the new PipeFileObject is closed if is it dropped before being returned.
        let ops = scopeguard::guard(ops, |ops| {
            ops.on_close(flags);
        });

        // Wait for the pipe to be connected.
        let waiter = Waiter::new();
        loop {
            pipe_locked.waiters.wait_async_fd_events(
                &waiter,
                must_wait_events,
                WaitCallback::none(),
            );
            std::mem::drop(pipe_locked);
            match waiter.wait(current_task) {
                Err(e) => {
                    return Err(e);
                }
                _ => {}
            }
            pipe_locked = pipe.lock();
            if pipe_locked.had_writer && pipe_locked.had_reader {
                return Ok(Box::new(scopeguard::ScopeGuard::into_inner(ops)));
            }
        }
    }

    /// Increments the reader count for this pipe by 1.
    pub fn add_reader(&mut self) {
        self.reader_count += 1;
        self.had_reader = true;
    }

    /// Increments the writer count for this pipe by 1.
    pub fn add_writer(&mut self) {
        self.writer_count += 1;
        self.had_writer = true;
    }

    /// Called whenever a fd to the pipe is closed. Reset the pipe state if there is not more
    /// reader or writer.
    pub fn on_close(&mut self) {
        if self.reader_count == 0 && self.writer_count == 0 {
            debug_assert!(!self.splice_read_in_progress);
            debug_assert!(!self.splice_write_in_progress);
            debug_assert_eq!(self.splice_read_reserved_bytes, 0);
            self.had_reader = false;
            self.had_writer = false;
            self.messages = MessageQueue::new(self.messages.capacity());
            self.waiters = WaitQueue::default();
        }
    }

    fn is_empty(&self) -> bool {
        self.readable_len() == 0
    }

    fn readable_len(&self) -> usize {
        self.messages.len().saturating_sub(self.splice_read_reserved_bytes)
    }

    fn capacity(&self) -> usize {
        self.messages.capacity()
    }

    fn set_capacity(
        &mut self,
        current_task: &CurrentTask,
        mut requested_capacity: usize,
    ) -> Result<(), Errno> {
        if requested_capacity > PIPE_MAX_SIZE {
            return error!(EINVAL);
        }
        let page_size = *PAGE_SIZE as usize;
        if requested_capacity < page_size {
            requested_capacity = page_size;
        }
        requested_capacity = requested_capacity.next_power_of_two();
        let old_capacity = self.messages.capacity();
        if requested_capacity > old_capacity
            && requested_capacity
                > current_task
                    .kernel()
                    .system_limits
                    .pipe_max_size
                    .load(std::sync::atomic::Ordering::Relaxed)
        {
            security::check_task_capable(current_task, CAP_SYS_RESOURCE)?;
        }
        let was_writable = self.is_writable(uapi::PIPE_BUF as usize);
        self.messages.set_capacity(requested_capacity)?;
        if requested_capacity > old_capacity {
            self.notify_write_space(was_writable);
        }
        Ok(())
    }

    fn is_readable(&self) -> bool {
        !self.is_empty() || (self.writer_count == 0 && self.had_writer)
    }

    /// Returns whether the pipe can accommodate at least part of a message of length `data_size`.
    fn is_writable(&self, data_size: usize) -> bool {
        let available_capacity = self.messages.available_capacity();
        // POSIX requires that a write smaller than PIPE_BUF be atomic, but requires no
        // atomicity for writes larger than this.
        self.had_reader
            && (self.reader_count == 0
                || available_capacity >= data_size
                || (available_capacity > 0 && data_size > uapi::PIPE_BUF as usize))
    }

    /// Minimum free capacity required for non-merging operations (`splice`, `vmsplice`, `tee`).
    ///
    /// A single-page pipe (`capacity <= PAGE_SIZE`) has only one page slot; when non-empty, its
    /// sole slot is already occupied and cannot accept non-merging splice writes until drained.
    fn min_splice_write_bytes(&self) -> usize {
        let page_size = *PAGE_SIZE as usize;
        if self.messages.capacity() <= page_size { page_size } else { 1 }
    }

    pub fn read(&mut self, data: &mut dyn OutputBuffer) -> Result<usize, Errno> {
        debug_assert!(!self.splice_read_in_progress);
        if data.available() == 0 {
            return Ok(0);
        }

        // Return EAGAIN if the pipe is empty and still has at least one open writer so the caller
        // blocks; otherwise fall through and return 0 to indicate EOF.
        if !self.is_readable() {
            return error!(EAGAIN);
        }

        self.messages.read_stream(data).map(|(info, _)| info.bytes_read)
    }

    pub fn write(&mut self, data: &mut dyn InputBuffer) -> Result<usize, Errno> {
        debug_assert!(!self.splice_write_in_progress);
        if data.available() == 0 {
            return Ok(0);
        }

        if !self.had_reader {
            return error!(EAGAIN);
        }

        if self.reader_count == 0 {
            return error!(EPIPE);
        }

        let total_requested = data.available().saturating_add(data.bytes_read());
        if !self.is_writable(total_requested) {
            return error!(EAGAIN);
        }

        let page_size = *PAGE_SIZE as usize;
        let mut total_written = 0;
        while self.messages.available_capacity() > 0 && data.available() > 0 {
            let chunk_len = std::cmp::min(
                self.messages.available_capacity(),
                std::cmp::min(data.available(), page_size),
            );
            let bytes = data.read_to_vec_limited(chunk_len)?;
            let actual = bytes.len();
            if actual == 0 {
                break;
            }
            self.messages.write_message(PipeMessageData::from(bytes).into());
            total_written += actual;
        }
        Ok(total_written)
    }

    fn query_events(&self, flags: OpenFlags) -> FdEvents {
        let mut events = FdEvents::empty();

        if flags.can_read() && self.is_readable() {
            let writer_closed = self.writer_count == 0 && self.had_writer;
            let has_data = !self.is_empty();
            if writer_closed {
                events |= FdEvents::POLLHUP;
            }
            if has_data {
                events |= FdEvents::POLLIN;
            }
        }

        if flags.can_write() && self.is_writable(uapi::PIPE_BUF as usize) {
            if self.reader_count == 0 && self.had_reader {
                events |= FdEvents::POLLERR;
            }

            events |= FdEvents::POLLOUT;
        }

        events
    }

    fn fcntl(
        &mut self,
        _file: &FileObject,
        current_task: &CurrentTask,
        cmd: u32,
        arg: u64,
    ) -> Result<SyscallResult, Errno> {
        match cmd {
            F_GETPIPE_SZ => Ok(self.capacity().into()),
            F_SETPIPE_SZ => {
                let raw_arg = arg as i32;
                if raw_arg < 0 {
                    return error!(EINVAL);
                }
                self.set_capacity(current_task, raw_arg as usize)?;
                Ok(self.capacity().into())
            }
            _ => default_fcntl(cmd),
        }
    }

    fn notify_fd_events(&self, events: FdEvents) {
        self.waiters.notify_fd_events(events);
    }

    fn notify_write_space(&self, was_writable: bool) {
        if !was_writable {
            if self.is_writable(uapi::PIPE_BUF as usize) {
                self.notify_fd_events(FdEvents::POLLOUT);
            } else if self.is_writable(1) {
                self.waiters.notify_value(PIPE_WRITE_SPACE_WAIT_KEY);
            }
        }
    }

    /// Consumes `bytes_read` bytes from the front of the pipe queue and wakes blocked writers if
    /// buffer space becomes available.
    fn consume(&mut self, bytes_read: usize) {
        let was_writable = self.is_writable(uapi::PIPE_BUF as usize);
        let mut consumed = bytes_read;
        while consumed > 0 {
            let mut message =
                self.messages.read_message().expect("pipe messages shrunk during splice read");
            if let Some(remaining) = MessageData::split_off(&mut message.data, consumed) {
                self.messages.write_front(remaining.into());
                break;
            }
            consumed -= message.len();
        }
        if bytes_read > 0 {
            self.notify_write_space(was_writable);
        }
    }
}

/// Creates a new pipe between the two returned FileObjects.
///
/// The first FileObject is the read endpoint of the pipe. The second is the
/// write endpoint of the pipe. This order matches the order expected by
/// sys_pipe2().
pub fn new_pipe(current_task: &CurrentTask) -> Result<(FileHandle, FileHandle), Errno> {
    let fs = pipe_fs(current_task, FileSystemOptions::default())?;
    let mut info = FsNodeInfo::new(mode!(IFIFO, 0o600), current_task.current_fscred());
    info.blksize = ATOMIC_IO_BYTES.into();
    let node = fs.create_node_and_allocate_node_id(SpecialNode, info);
    let pipe = node.fifo(current_task);
    {
        let mut state = pipe.lock();
        state.add_reader();
        state.add_writer();
    }

    // Creating the readable `FileObject` takes care of initializing the `node` security label.
    let read_ops = PipeFileObject { pipe: Arc::clone(pipe) };
    let read_file = FileObject::new_anonymous(
        current_task,
        Box::new(read_ops),
        Arc::clone(&node),
        OpenFlags::RDONLY,
    );

    // Create the writable `FileObject` using the readable object's `name` to reduce overhead.
    let write_ops = PipeFileObject { pipe: Arc::clone(pipe) };
    let write_file = FileObject::new(
        current_task,
        Box::new(write_ops),
        read_file.name.to_passive(),
        OpenFlags::WRONLY,
    )?;

    Ok((read_file, write_file))
}

/// Creates a new anonymous [`FileObject`] backed by a `pipefs` [`crate::vfs::FsNode`]
/// (`S_IFIFO`) with the specified `ops`.
pub(crate) fn new_pipe_file(
    current_task: &CurrentTask,
    ops: Box<dyn FileOps>,
    flags: OpenFlags,
) -> Result<FileHandle, Errno> {
    let fs = pipe_fs(current_task, FileSystemOptions::default())?;
    let mut info = FsNodeInfo::new(mode!(IFIFO, 0o600), current_task.current_fscred());
    info.blksize = ATOMIC_IO_BYTES.into();
    let node = fs.create_node_and_allocate_node_id(SpecialNode, info);
    Ok(FileObject::new_anonymous(current_task, ops, node, flags))
}

struct PipeFs;
impl FileSystemOps for PipeFs {
    fn statfs(&self, _fs: &FileSystem, _current_task: &CurrentTask) -> Result<statfs, Errno> {
        Ok(default_statfs(PIPEFS_MAGIC))
    }
    fn name(&self) -> &'static FsStr {
        "pipefs".into()
    }
}

fn pipe_fs(
    current_task: &CurrentTask,
    _options: FileSystemOptions,
) -> Result<FileSystemHandle, Errno> {
    struct PipeFsHandle(FileSystemHandle);

    let kernel = current_task.kernel();
    let fs = kernel
        .expando
        .get_or_init(|| {
            PipeFsHandle(
                FileSystem::new(kernel, CacheMode::Uncached, PipeFs, FileSystemOptions::default())
                    .expect("pipefs constructed with valid options"),
            )
        })
        .0
        .clone();
    security::file_system_resolve_security(current_task, &fs)?;
    Ok(fs)
}

pub fn register_pipe_fs(fs_registry: &FsRegistry) {
    fs_registry.register("pipefs".into(), FileSystemTypeFlags::empty(), pipe_fs);
}

pub struct PipeFileObject {
    pipe: PipeHandle,
}

impl FileOps for PipeFileObject {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn close(self: Box<Self>, file: &FileObjectState, _current_task: &CurrentTask) {
        self.on_close(file.flags());
    }

    fn read(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        file.blocking_op(current_task, FdEvents::POLLIN | FdEvents::POLLHUP, None, || {
            let mut pipe = self.lock_pipe_without_splice(current_task)?;
            let was_writable = pipe.is_writable(uapi::PIPE_BUF as usize);
            let actual = pipe.read(data)?;
            if actual > 0 {
                pipe.notify_write_space(was_writable);
            }
            Ok(actual)
        })
    }

    fn write(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        debug_assert!(data.bytes_read() == 0);

        let result =
            file.blocking_op(current_task, FdEvents::POLLOUT | FdEvents::POLLERR, None, || {
                let mut pipe = self.lock_pipe_without_splice(current_task)?;
                let offset_before = data.bytes_read();
                let bytes_written = match pipe.write(data) {
                    Err(e) => {
                        if data.bytes_read() > offset_before {
                            pipe.notify_fd_events(FdEvents::POLLIN);
                        }
                        if e == EPIPE {
                            std::mem::drop(pipe);
                            send_standard_signal(current_task, SignalInfo::kernel(SIGPIPE));
                        }
                        return Err(e);
                    }
                    Ok(bytes_written) => bytes_written,
                };
                debug_assert!(data.bytes_read() - offset_before == bytes_written);
                if bytes_written > 0 {
                    pipe.notify_fd_events(FdEvents::POLLIN);
                }
                if data.available() > 0 {
                    return error!(EAGAIN);
                }
                Ok(())
            });

        let bytes_written = data.bytes_read();
        if bytes_written == 0 {
            // Return an error only if no data was sent; return partial progress otherwise.
            result?;
        }
        Ok(bytes_written)
    }

    fn wait_async(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        mut events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        let flags = file.flags();
        if !flags.can_read() {
            events.remove(FdEvents::POLLIN);
        }
        if !flags.can_write() {
            events.remove(FdEvents::POLLOUT);
        }
        let subscribe_write_space =
            matches!(handler, EventHandler::None) && events.contains(FdEvents::POLLOUT);
        let pipe = self.pipe.lock();
        let canceler = pipe.waiters.wait_async_fd_events(waiter, events, handler);
        if subscribe_write_space {
            let write_space_canceler =
                pipe.waiters.wait_async_value(waiter, PIPE_WRITE_SPACE_WAIT_KEY);
            Some(WaitCanceler::merge(canceler, write_space_canceler))
        } else {
            Some(canceler)
        }
    }

    fn query_events(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        Ok(self.pipe.lock().query_events(file.flags()))
    }

    fn can_query_events_without_wait(&self) -> bool {
        // `wait_async` only registers on the pipe's wait queue.
        true
    }

    fn fcntl(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        cmd: u32,
        arg: u64,
    ) -> Result<SyscallResult, Errno> {
        self.lock_pipe_without_splice(current_task)?.fcntl(file, current_task, cmd, arg)
    }

    fn ioctl(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        match request {
            FIONREAD => {
                let pipe = self.lock_pipe_without_splice(current_task)?;
                let value: i32 = pipe.readable_len().try_into().map_err(|_| errno!(EINVAL))?;
                std::mem::drop(pipe);
                let addr = UserRef::<i32>::new(user_addr);
                current_task.write_object(addr, &value)?;
                Ok(SUCCESS)
            }
            _ => error!(ENOTTY),
        }
    }
}

/// Guard that marks a pipe as having an in-progress splice read while the pipe lock is not held.
struct SpliceReadGuard<'a> {
    pipe: Option<&'a PipeHandle>,
}

impl<'a> SpliceReadGuard<'a> {
    /// Marks `pipe` as having a splice read in progress and drops the pipe lock.
    fn new(
        pipe_handle: &'a PipeHandle,
        mut pipe: LockDepGuard<'a, Pipe>,
        reserved_bytes: usize,
    ) -> Self {
        assert!(!pipe.splice_read_in_progress);
        pipe.splice_read_in_progress = true;
        pipe.splice_read_reserved_bytes = reserved_bytes;
        Self { pipe: Some(pipe_handle) }
    }

    /// Consumes `bytes_read` bytes from the front of the pipe, clears the in-progress splice read
    /// state, and wakes any waiting readers.
    fn consume(mut self, bytes_read: usize) {
        if let Some(pipe_handle) = self.pipe.take() {
            let mut pipe = pipe_handle.lock();
            pipe.consume(bytes_read);
            Self::clear(&mut pipe);
        }
    }

    /// Clears the in-progress splice read state on `pipe` and wakes any waiting readers.
    fn clear(pipe: &mut Pipe) {
        assert!(pipe.splice_read_in_progress);
        pipe.splice_read_in_progress = false;
        pipe.splice_read_reserved_bytes = 0;
        pipe.waiters.notify_value(SPLICE_READ_WAIT_KEY);
        if !pipe.is_empty() {
            pipe.notify_fd_events(FdEvents::POLLIN);
        }
    }
}

impl Drop for SpliceReadGuard<'_> {
    fn drop(&mut self) {
        if let Some(pipe_handle) = self.pipe.take() {
            Self::clear(&mut pipe_handle.lock());
        }
    }
}

/// Guard that marks a pipe as having an in-progress splice write while the pipe lock is not held.
struct SpliceWriteGuard<'a> {
    pipe: Option<&'a PipeHandle>,
}

impl<'a> SpliceWriteGuard<'a> {
    /// Marks `pipe` as having a splice write in progress and drops the pipe lock.
    fn new(pipe_handle: &'a PipeHandle, mut pipe: LockDepGuard<'a, Pipe>) -> Self {
        assert!(!pipe.splice_write_in_progress);
        pipe.splice_write_in_progress = true;
        Self { pipe: Some(pipe_handle) }
    }

    /// Enqueues `bytes` into the back of the pipe, clears the in-progress splice write state, and
    /// wakes any waiting readers or writers.
    fn enqueue(mut self, bytes: Vec<u8>) -> Result<(), Errno> {
        if let Some(pipe_handle) = self.pipe.take() {
            let mut pipe = pipe_handle.lock();
            if pipe.reader_count == 0 {
                Self::clear(&mut pipe);
                return error!(EPIPE);
            }
            if !bytes.is_empty() {
                for chunk in bytes.chunks(*PAGE_SIZE as usize) {
                    pipe.messages.write_message(PipeMessageData::from(chunk.to_vec()).into());
                }
                pipe.notify_fd_events(FdEvents::POLLIN);
            }
            Self::clear(&mut pipe);
        }
        Ok(())
    }

    /// Clears the in-progress splice write state on `pipe` and wakes any waiting writers.
    fn clear(pipe: &mut Pipe) {
        assert!(pipe.splice_write_in_progress);
        pipe.splice_write_in_progress = false;
        pipe.waiters.notify_value(SPLICE_WRITE_WAIT_KEY);
        if pipe.reader_count == 0 {
            pipe.notify_fd_events(FdEvents::POLLOUT | FdEvents::POLLERR);
        } else {
            pipe.notify_write_space(false);
        }
    }
}

impl Drop for SpliceWriteGuard<'_> {
    fn drop(&mut self) {
        if let Some(pipe_handle) = self.pipe.take() {
            Self::clear(&mut pipe_handle.lock());
        }
    }
}

impl std::fmt::Debug for SpliceReadGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpliceReadGuard").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for SpliceWriteGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpliceWriteGuard").finish_non_exhaustive()
    }
}

/// An OutputBuffer that will write the data to `pipe`.
#[derive(Debug)]
pub struct SpliceOutputBuffer<'a> {
    guard: SpliceWriteGuard<'a>,
    buffer: VecOutputBuffer,
}

impl<'a> Buffer for SpliceOutputBuffer<'a> {
    fn segments_count(&self) -> Result<usize, Errno> {
        self.buffer.segments_count()
    }

    fn peek_each_segment(
        &mut self,
        callback: &mut PeekBufferSegmentsCallback<'_>,
    ) -> Result<(), Errno> {
        self.buffer.peek_each_segment(callback)
    }
}

impl<'a> OutputBuffer for SpliceOutputBuffer<'a> {
    fn write_each(&mut self, callback: &mut OutputBufferCallback<'_>) -> Result<usize, Errno> {
        self.buffer.write_each(callback)
    }

    fn available(&self) -> usize {
        self.buffer.available()
    }

    fn bytes_written(&self) -> usize {
        self.buffer.bytes_written()
    }

    fn zero(&mut self) -> Result<usize, Errno> {
        self.buffer.zero()
    }

    unsafe fn advance(&mut self, length: usize) -> Result<(), Errno> {
        // SAFETY: Delegated directly to the inner `VecOutputBuffer` under the same safety
        // contract.
        unsafe { self.buffer.advance(length) }
    }
}

/// An InputBuffer that will read the data from `pipe`.
#[derive(Debug)]
pub struct SpliceInputBuffer<'a> {
    guard: SpliceReadGuard<'a>,
    buffer: VecInputBuffer,
}

impl<'a> Buffer for SpliceInputBuffer<'a> {
    fn segments_count(&self) -> Result<usize, Errno> {
        self.buffer.segments_count()
    }

    fn peek_each_segment(
        &mut self,
        callback: &mut PeekBufferSegmentsCallback<'_>,
    ) -> Result<(), Errno> {
        self.buffer.peek_each_segment(callback)
    }
}

impl<'a> InputBuffer for SpliceInputBuffer<'a> {
    fn peek_each(&mut self, callback: &mut InputBufferCallback<'_>) -> Result<usize, Errno> {
        self.buffer.peek_each(callback)
    }

    fn available(&self) -> usize {
        self.buffer.available()
    }

    fn bytes_read(&self) -> usize {
        self.buffer.bytes_read()
    }

    fn drain(&mut self) -> usize {
        self.buffer.drain()
    }

    fn advance(&mut self, length: usize) -> Result<(), Errno> {
        self.buffer.advance(length)
    }
}

impl PipeFileObject {
    /// Called whenever a fd to a pipe is closed.
    fn on_close(&self, flags: OpenFlags) {
        let mut events = FdEvents::empty();
        let mut pipe = self.pipe.lock();
        if flags.can_read() {
            assert!(pipe.reader_count > 0);
            pipe.reader_count -= 1;
            if pipe.reader_count == 0 {
                events |= FdEvents::POLLOUT | FdEvents::POLLERR;
            }
        }
        if flags.can_write() {
            assert!(pipe.writer_count > 0);
            pipe.writer_count -= 1;
            if pipe.writer_count == 0 {
                if pipe.reader_count > 0 {
                    events |= FdEvents::POLLHUP;
                }
                if !pipe.is_empty() {
                    events |= FdEvents::POLLIN;
                }
            }
        }
        if events != FdEvents::empty() {
            pipe.waiters.notify_fd_events(events);
        }
        pipe.on_close();
    }

    /// Acquires the pipe lock once neither a splice read nor a splice write is in progress.
    fn lock_pipe_without_splice<'a>(
        &'a self,
        current_task: &CurrentTask,
    ) -> Result<LockDepGuard<'a, Pipe>, Errno> {
        loop {
            let pipe = self.pipe.lock();
            if !pipe.splice_read_in_progress && !pipe.splice_write_in_progress {
                return Ok(pipe);
            }
            let waiter = Waiter::new();
            if pipe.splice_read_in_progress {
                pipe.waiters.wait_async_value(&waiter, SPLICE_READ_WAIT_KEY);
            }
            if pipe.splice_write_in_progress {
                pipe.waiters.wait_async_value(&waiter, SPLICE_WRITE_WAIT_KEY);
            }
            std::mem::drop(pipe);
            waiter.wait(current_task)?;
        }
    }

    /// Locks the pipe for reading if it is readable, waiting for any active splice to finish.
    fn try_lock_pipe_for_reading<'a>(
        &'a self,
        current_task: &CurrentTask,
        _non_blocking: bool,
    ) -> Result<LockDepGuard<'a, Pipe>, Errno> {
        let pipe = self.lock_pipe_without_splice(current_task)?;
        if pipe.is_readable() { Ok(pipe) } else { error!(EAGAIN) }
    }

    /// Locks the pipe for writing if it can accept part of `len` bytes without blocking, waiting
    /// for any active splice to finish.
    fn try_lock_pipe_for_writing<'a>(
        &'a self,
        current_task: &CurrentTask,
        _non_blocking: bool,
        len: usize,
    ) -> Result<LockDepGuard<'a, Pipe>, Errno> {
        let pipe = self.lock_pipe_without_splice(current_task)?;
        if pipe.had_reader && pipe.reader_count == 0 {
            std::mem::drop(pipe);
            send_standard_signal(current_task, SignalInfo::kernel(SIGPIPE));
            return error!(EPIPE);
        }
        if !pipe.is_writable(std::cmp::max(len, pipe.min_splice_write_bytes())) {
            return error!(EAGAIN);
        }
        Ok(pipe)
    }

    fn lock_pipe_for_reading<'a>(
        &'a self,
        current_task: &CurrentTask,
        file: &FileHandle,
        non_blocking: bool,
    ) -> Result<LockDepGuard<'a, Pipe>, Errno> {
        if non_blocking {
            return self.try_lock_pipe_for_reading(current_task, true);
        }
        match self.try_lock_pipe_for_reading(current_task, false) {
            Err(errno) if errno == EAGAIN => {}
            result => return result,
        }
        let waiter = Waiter::new();
        loop {
            file.wait_async(
                current_task,
                &waiter,
                FdEvents::POLLIN | FdEvents::POLLHUP,
                WaitCallback::none(),
            );
            match self.try_lock_pipe_for_reading(current_task, false) {
                Err(e) if e == EAGAIN => {}
                result => return result,
            }
            waiter.wait(current_task)?;
        }
    }

    fn lock_pipe_for_writing<'a>(
        &'a self,
        current_task: &CurrentTask,
        file: &FileHandle,
        non_blocking: bool,
        len: usize,
    ) -> Result<LockDepGuard<'a, Pipe>, Errno> {
        if non_blocking {
            return self.try_lock_pipe_for_writing(current_task, true, len);
        }
        match self.try_lock_pipe_for_writing(current_task, false, len) {
            Err(errno) if errno == EAGAIN => {}
            result => return result,
        }
        let waiter = Waiter::new();
        loop {
            file.wait_async(
                current_task,
                &waiter,
                FdEvents::POLLOUT | FdEvents::POLLERR,
                WaitCallback::none(),
            );
            match self.try_lock_pipe_for_writing(current_task, false, len) {
                Err(e) if e == EAGAIN => {}
                result => return result,
            }
            waiter.wait(current_task)?;
        }
    }

    /// Splice from the given file handle to this pipe.
    ///
    /// The given file handle must not be a pipe; for two pipes use `PipeFileObject::splice_pipes`.
    pub fn splice_from(
        &self,
        current_task: &CurrentTask,
        self_file: &FileHandle,
        from: &FileHandle,
        maybe_offset: Option<usize>,
        len: usize,
        non_blocking: bool,
    ) -> Result<usize, Errno> {
        // For pipe-to-pipe transfers, callers must use `PipeFileObject::splice_pipes`.
        assert!(from.downcast_file::<PipeFileObject>().is_none());

        let non_blocking = non_blocking || self_file.is_non_blocking();
        let mut buffer = {
            let pipe = self.lock_pipe_for_writing(current_task, self_file, non_blocking, 1)?;
            let len = std::cmp::min(len, pipe.messages.available_capacity());
            SpliceOutputBuffer {
                guard: SpliceWriteGuard::new(&self.pipe, pipe),
                buffer: VecOutputBuffer::new(len),
            }
        };

        let bytes_read = from
            .splice_read(current_task, maybe_offset, &mut buffer, non_blocking)
            .inspect_err(|err| {
                if *err == EPIPE {
                    send_standard_signal(current_task, SignalInfo::kernel(SIGPIPE));
                }
            })?;
        if bytes_read > 0 {
            buffer.guard.enqueue(Vec::from(buffer.buffer)).inspect_err(|err| {
                if *err == EPIPE {
                    send_standard_signal(current_task, SignalInfo::kernel(SIGPIPE));
                }
            })?;
        }
        Ok(bytes_read)
    }

    /// Splice from this pipe to the given file handle.
    ///
    /// The given file handle must not be a pipe; for two pipes use `PipeFileObject::splice_pipes`.
    pub fn splice_to(
        &self,
        current_task: &CurrentTask,
        self_file: &FileHandle,
        to: &FileHandle,
        maybe_offset: Option<usize>,
        len: usize,
        non_blocking: bool,
    ) -> Result<usize, Errno> {
        // For pipe-to-pipe transfers, callers must use `PipeFileObject::splice_pipes`.
        assert!(to.downcast_file::<PipeFileObject>().is_none());

        let non_blocking = non_blocking || self_file.is_non_blocking();
        let (read_guard, mut data) = {
            let pipe = self.lock_pipe_for_reading(current_task, self_file, non_blocking)?;
            let limit = std::cmp::min(len, pipe.messages.len());
            let mut data = Vec::with_capacity(limit);
            for message in pipe.messages.peek_queue() {
                let remaining = limit - data.len();
                if remaining == 0 {
                    break;
                }
                message.data.with_bytes(|bytes| {
                    let count = std::cmp::min(bytes.len(), remaining);
                    data.extend_from_slice(&bytes[..count]);
                    Ok(())
                })?;
            }
            if data.is_empty() {
                return Ok(0);
            }
            let reserved_bytes = data.len();
            let guard = SpliceReadGuard::new(&self.pipe, pipe, reserved_bytes);
            (guard, data)
        };
        let mut clamped_by_fsize = false;
        if to.node().is_reg() {
            let fsize_limit = current_task.thread_group().get_rlimit(Resource::FSIZE);
            if fsize_limit != uapi::RLIM_INFINITY as u64 {
                let out_offset = match maybe_offset {
                    Some(off) => off as u64,
                    None => to.offset.read().max(0) as u64,
                };
                if out_offset >= fsize_limit {
                    send_standard_signal(current_task, SignalInfo::kernel(SIGXFSZ));
                    return error!(EFBIG);
                }
                let allowed = usize::try_from(fsize_limit - out_offset).unwrap_or(usize::MAX);
                if data.len() > allowed {
                    data.truncate(allowed);
                    clamped_by_fsize = true;
                }
            }
        }
        let mut buffer =
            SpliceInputBuffer { guard: read_guard, buffer: VecInputBuffer::from(data) };
        let bytes_written =
            to.splice_write(current_task, maybe_offset, &mut buffer, non_blocking)?;
        if bytes_written > 0 {
            if clamped_by_fsize {
                send_standard_signal(current_task, SignalInfo::kernel(SIGXFSZ));
            }
            buffer.guard.consume(bytes_written);
        }
        Ok(bytes_written)
    }

    /// Share the mappings backing the given input buffer into the pipe.
    ///
    /// Returns the number of bytes enqueued.
    pub fn vmsplice_from(
        &self,
        current_task: &CurrentTask,
        self_file: &FileHandle,
        mut iovec: UserBuffers,
        non_blocking: bool,
    ) -> Result<usize, Errno> {
        let available = UserBuffer::cap_buffers_to_max_rw_count(
            current_task.maximum_valid_address().ok_or_else(|| errno!(EINVAL))?,
            &mut iovec,
        )?;
        if available == 0 {
            return Ok(0);
        }

        let mut pipe = self.lock_pipe_for_writing(current_task, self_file, non_blocking, 1)?;
        let max_bytes = std::cmp::min(available, pipe.messages.available_capacity());
        let mappings = current_task.mm()?.get_mappings_for_vmsplice(&iovec, max_bytes)?;

        let mut bytes_transferred = 0;
        for mapping in mappings.into_iter() {
            let actual = mapping.len();
            pipe.messages.write_message(PipeMessageData::Vmspliced(mapping).into());
            bytes_transferred += actual;
        }
        if bytes_transferred > 0 {
            pipe.notify_fd_events(FdEvents::POLLIN);
        }
        Ok(bytes_transferred)
    }

    /// Copy data from the pipe to the given output buffer.
    ///
    /// Returns the number of bytes transferred.
    pub fn vmsplice_to(
        &self,
        current_task: &CurrentTask,
        self_file: &FileHandle,
        iovec: UserBuffers,
        non_blocking: bool,
    ) -> Result<usize, Errno> {
        let mut data = UserBuffersOutputBuffer::unified_new(current_task, iovec)?;
        if data.available() == 0 {
            return Ok(0);
        }
        let mut pipe = self.lock_pipe_for_reading(current_task, self_file, non_blocking)?;
        let was_writable = pipe.is_writable(uapi::PIPE_BUF as usize);
        let actual = pipe.read(&mut data)?;
        if actual > 0 {
            pipe.notify_write_space(was_writable);
        }
        Ok(actual)
    }

    /// Copies or moves messages from `pipe_in` to `pipe_out`, locking both pipes in memory-address
    /// order via `starnix_sync::ordered_lock`.
    fn copy_pipes(
        current_task: &CurrentTask,
        file_in: &FileHandle,
        pipe_in: &PipeFileObject,
        file_out: &FileHandle,
        pipe_out: &PipeFileObject,
        len: usize,
        non_blocking: bool,
        consume_source: bool,
    ) -> Result<usize, Errno> {
        // Reject splicing/teeing a pipe to itself before calling ordered_lock,
        // because ordered_lock on identical LockDepMutex instances deadlocks.
        if Arc::ptr_eq(&pipe_in.pipe, &pipe_out.pipe) {
            return error!(EINVAL);
        }
        if len == 0 {
            return Ok(0);
        }

        let non_blocking = non_blocking || file_in.is_non_blocking() || file_out.is_non_blocking();
        let mut waiting_on_out = false;

        let (mut from, mut to) = loop {
            let (pipe_in_guard, pipe_out_guard) = ordered_lock(&*pipe_in.pipe, &*pipe_out.pipe);

            if pipe_in_guard.splice_read_in_progress || pipe_in_guard.splice_write_in_progress {
                let waiter = Waiter::new();
                let key = if pipe_in_guard.splice_read_in_progress {
                    SPLICE_READ_WAIT_KEY
                } else {
                    SPLICE_WRITE_WAIT_KEY
                };
                pipe_in_guard.waiters.wait_async_value(&waiter, key);
                std::mem::drop(pipe_in_guard);
                std::mem::drop(pipe_out_guard);
                waiter.wait(current_task)?;
                continue;
            }

            if !waiting_on_out && !pipe_in_guard.is_readable() {
                if non_blocking {
                    return error!(EAGAIN);
                }
                let waiter = Waiter::new();
                pipe_in_guard.waiters.wait_async_fd_events(
                    &waiter,
                    FdEvents::POLLIN | FdEvents::POLLHUP,
                    WaitCallback::none(),
                );
                std::mem::drop(pipe_in_guard);
                std::mem::drop(pipe_out_guard);
                waiter.wait(current_task)?;
                continue;
            }

            if pipe_out_guard.splice_read_in_progress || pipe_out_guard.splice_write_in_progress {
                let waiter = Waiter::new();
                let key = if pipe_out_guard.splice_read_in_progress {
                    SPLICE_READ_WAIT_KEY
                } else {
                    SPLICE_WRITE_WAIT_KEY
                };
                pipe_out_guard.waiters.wait_async_value(&waiter, key);
                std::mem::drop(pipe_in_guard);
                std::mem::drop(pipe_out_guard);
                waiter.wait(current_task)?;
                continue;
            }

            if pipe_out_guard.reader_count == 0 {
                std::mem::drop(pipe_in_guard);
                std::mem::drop(pipe_out_guard);
                send_standard_signal(current_task, SignalInfo::kernel(SIGPIPE));
                return error!(EPIPE);
            }

            if !pipe_out_guard.is_writable(pipe_out_guard.min_splice_write_bytes()) {
                if non_blocking {
                    return error!(EAGAIN);
                }
                waiting_on_out = true;
                let waiter = Waiter::new();
                pipe_out_guard.waiters.wait_async_fd_events(
                    &waiter,
                    FdEvents::POLLOUT | FdEvents::POLLERR,
                    WaitCallback::none(),
                );
                pipe_out_guard.waiters.wait_async_value(&waiter, PIPE_WRITE_SPACE_WAIT_KEY);
                std::mem::drop(pipe_in_guard);
                std::mem::drop(pipe_out_guard);
                waiter.wait(current_task)?;
                continue;
            }

            if waiting_on_out && pipe_in_guard.is_empty() {
                if pipe_in_guard.is_readable() || !consume_source {
                    return Ok(0);
                }
                waiting_on_out = false;
                continue;
            }

            if pipe_in_guard.is_empty() {
                return Ok(0);
            }

            break (pipe_in_guard, pipe_out_guard);
        };

        let mut bytes_transferred = 0;
        if consume_source {
            let was_writable = from.is_writable(uapi::PIPE_BUF as usize);
            while bytes_transferred < len && to.messages.available_capacity() > 0 {
                let Some(mut message) = from.messages.read_message() else {
                    break;
                };
                let limit =
                    std::cmp::min(len - bytes_transferred, to.messages.available_capacity());
                if let Some(remaining) = message.split_off(limit) {
                    from.messages.write_front(remaining);
                }
                bytes_transferred += message.len();
                to.messages.write_message(message);
            }
            if bytes_transferred > 0 {
                from.notify_write_space(was_writable);
                to.notify_fd_events(FdEvents::POLLIN);
            }
        } else {
            let mut cloned_messages = Vec::new();
            for message in from.messages.peek_queue() {
                let limit = std::cmp::min(
                    len - bytes_transferred,
                    to.messages.available_capacity() - bytes_transferred,
                );
                if limit == 0 {
                    break;
                }
                let cloned = message.clone_at_most(limit);
                bytes_transferred += cloned.len();
                cloned_messages.push(cloned);
            }
            for message in cloned_messages {
                to.messages.write_message(message);
            }
            if bytes_transferred > 0 {
                to.notify_fd_events(FdEvents::POLLIN);
            }
        }

        Ok(bytes_transferred)
    }

    /// Splice messages from `pipe_in` to `pipe_out`.
    pub fn splice_pipes(
        current_task: &CurrentTask,
        file_in: &FileHandle,
        pipe_in: &PipeFileObject,
        file_out: &FileHandle,
        pipe_out: &PipeFileObject,
        len: usize,
        non_blocking: bool,
    ) -> Result<usize, Errno> {
        Self::copy_pipes(
            current_task,
            file_in,
            pipe_in,
            file_out,
            pipe_out,
            len,
            non_blocking,
            true,
        )
    }

    /// Tee messages from `file_in` to `file_out`.
    pub fn tee(
        current_task: &CurrentTask,
        file_in: &FileHandle,
        file_out: &FileHandle,
        len: usize,
        non_blocking: bool,
    ) -> Result<usize, Errno> {
        let pipe_in = file_in.downcast_file::<PipeFileObject>().ok_or_else(|| errno!(EINVAL))?;
        let pipe_out = file_out.downcast_file::<PipeFileObject>().ok_or_else(|| errno!(EINVAL))?;
        Self::copy_pipes(
            current_task,
            file_in,
            &pipe_in,
            file_out,
            &pipe_out,
            len,
            non_blocking,
            false,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::memory::MemoryObject;
    use crate::mm::{ProtectionFlags, VmsplicePayload, VmsplicePayloadSegment};
    use crate::task::dynamic_thread_spawner::SpawnRequestBuilder;
    use crate::testing::{anon_test_file, fill_page, map_memory, spawn_kernel_and_run};
    use crate::vfs::buffers::UserBuffersInputBuffer;
    use crate::vfs::new_memfd;
    use smallvec::smallvec;
    use starnix_sync::Mutex;
    use starnix_uapi::seal_flags::SealFlags;
    use std::sync::mpsc;

    struct BoundedSinkFile {
        max_write: usize,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl FileOps for BoundedSinkFile {
        fileops_impl_nonseekable!();
        fileops_impl_noop_sync!();

        fn read(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            _data: &mut dyn OutputBuffer,
        ) -> Result<usize, Errno> {
            error!(ENOTSUP)
        }

        fn write(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            data: &mut dyn InputBuffer,
        ) -> Result<usize, Errno> {
            let mut remaining = self.max_write;
            data.read_each(&mut |slice| {
                let count = std::cmp::min(slice.len(), remaining);
                self.written.lock().extend_from_slice(&slice[..count]);
                remaining -= count;
                Ok(count)
            })
        }
    }

    struct BarrierSinkFile {
        entered_tx: Mutex<Option<mpsc::Sender<()>>>,
        resume_rx: Mutex<Option<mpsc::Receiver<()>>>,
        max_write: usize,
        written: Mutex<Vec<u8>>,
    }

    impl FileOps for BarrierSinkFile {
        fileops_impl_nonseekable!();
        fileops_impl_noop_sync!();

        fn read(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            _data: &mut dyn OutputBuffer,
        ) -> Result<usize, Errno> {
            error!(ENOTSUP)
        }

        fn write(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            data: &mut dyn InputBuffer,
        ) -> Result<usize, Errno> {
            if let Some(tx) = self.entered_tx.lock().take() {
                tx.send(()).expect("notify entered write");
            }
            if let Some(rx) = self.resume_rx.lock().take() {
                rx.recv().expect("wait to resume write");
            }
            let mut remaining = self.max_write;
            data.read_each(&mut |slice| {
                let count = std::cmp::min(slice.len(), remaining);
                self.written.lock().extend_from_slice(&slice[..count]);
                remaining -= count;
                Ok(count)
            })
        }
    }

    struct StaticSourceFile {
        payload: &'static [u8],
    }

    impl FileOps for StaticSourceFile {
        fileops_impl_nonseekable!();
        fileops_impl_noop_sync!();

        fn read(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            data: &mut dyn OutputBuffer,
        ) -> Result<usize, Errno> {
            data.write(self.payload)
        }

        fn write(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            _data: &mut dyn InputBuffer,
        ) -> Result<usize, Errno> {
            error!(ENOTSUP)
        }
    }

    struct BarrierSourceFile {
        entered_tx: Mutex<Option<mpsc::Sender<()>>>,
        resume_rx: Mutex<Option<mpsc::Receiver<()>>>,
        payload: &'static [u8],
    }

    impl FileOps for BarrierSourceFile {
        fileops_impl_nonseekable!();
        fileops_impl_noop_sync!();

        fn read(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            data: &mut dyn OutputBuffer,
        ) -> Result<usize, Errno> {
            if let Some(tx) = self.entered_tx.lock().take() {
                tx.send(()).expect("notify entered read");
            }
            if let Some(rx) = self.resume_rx.lock().take() {
                rx.recv().expect("wait to resume read");
            }
            data.write(self.payload)
        }

        fn write(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _offset: usize,
            _data: &mut dyn InputBuffer,
        ) -> Result<usize, Errno> {
            error!(ENOTSUP)
        }
    }

    #[::fuchsia::test]
    async fn test_splice_to_partial_write_preserves_fifo_order() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            // Part 1: Single-thread partial write across `[MsgA("AAAA"), MsgB("BBBB")]`.
            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"AAAA")).unwrap(), 4);
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"BBBB")).unwrap(), 4);

            let sink_written = Arc::new(Mutex::new(Vec::new()));
            let sink_file = anon_test_file(
                &current_task,
                Box::new(BoundedSinkFile { max_write: 2, written: Arc::clone(&sink_written) }),
                OpenFlags::WRONLY,
            );
            let spliced = pipe_out
                .downcast_file::<PipeFileObject>()
                .unwrap()
                .splice_to(&current_task, &pipe_out, &sink_file, None, 6, true)
                .unwrap();
            assert_eq!(spliced, 2);
            assert_eq!(*sink_written.lock(), b"AA");

            let mut remaining = Vec::new();
            loop {
                let mut out = VecOutputBuffer::new(16);
                match pipe_out.read(&current_task, &mut out) {
                    Ok(n) if n > 0 => {
                        remaining.extend_from_slice(&out.data()[..n]);
                        if remaining.len() == 6 {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            assert_eq!(remaining, b"AABBBB");

            // Part 2: Concurrent reader + blocked partial `splice_to` on `[MsgA("AAAA"), MsgB("BBBB")]`.
            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"AAAA")).unwrap(), 4);
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"BBBB")).unwrap(), 4);

            let (entered_tx, entered_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let barrier_sink = anon_test_file(
                &current_task,
                Box::new(BarrierSinkFile {
                    entered_tx: Mutex::new(Some(entered_tx)),
                    resume_rx: Mutex::new(Some(resume_rx)),
                    max_write: 2,
                    written: Mutex::new(Vec::new()),
                }),
                OpenFlags::WRONLY,
            );

            let (reader_ready_tx, reader_ready_rx) = mpsc::channel();
            let pipe_out_unblock = pipe_out.clone();
            let (unblock_join, unblock_req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |_worker_task: &CurrentTask| {
                    reader_ready_rx.recv().expect("wait until non-blocking reader starts");
                    while pipe_out_unblock
                        .downcast_file::<PipeFileObject>()
                        .unwrap()
                        .pipe
                        .lock()
                        .waiters
                        .is_empty()
                    {
                        std::thread::yield_now();
                    }
                    resume_tx.send(()).expect("allow splice_to write to complete");
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(unblock_req);

            let pipe_out_clone = pipe_out.clone();
            let (reader_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    entered_rx.recv().expect("wait until splice_to enters write");
                    {
                        let pipe =
                            pipe_out_clone.downcast_file::<PipeFileObject>().unwrap().pipe.lock();
                        assert!(pipe.splice_read_in_progress);
                        assert_eq!(pipe.messages.len(), 8);
                    }
                    reader_ready_tx.send(()).expect("signal unblocker");
                    // A non-blocking read waits for the active splice read to finish and then reads
                    // the remaining bytes in FIFO order.
                    let mut pipe = pipe_out_clone
                        .downcast_file::<PipeFileObject>()
                        .unwrap()
                        .lock_pipe_for_reading(worker_task, &pipe_out_clone, true)
                        .expect("non-blocking read lock succeeds once splice_to finishes");
                    let mut buf = VecOutputBuffer::new(4);
                    let n = pipe.read(&mut buf).unwrap();
                    buf.data()[..n].to_vec()
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            let spliced = pipe_out
                .downcast_file::<PipeFileObject>()
                .unwrap()
                .splice_to(&current_task, &pipe_out, &barrier_sink, None, 8, true)
                .unwrap();
            assert_eq!(spliced, 2);
            unblock_join().unwrap();

            // Concurrent 4-byte stream read receives `"AA"` (from `MsgA`) followed by `"BB"` (from `MsgB`).
            let concurrent_bytes = reader_join().unwrap();
            assert_eq!(concurrent_bytes, b"AABB");

            // The remaining `"BB"` tail of `MsgB` follows in FIFO order.
            let mut tail_buf = VecOutputBuffer::new(4);
            let tail_len = pipe_out.read(&current_task, &mut tail_buf).unwrap();
            assert_eq!(&tail_buf.data()[..tail_len], b"BB");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_splice_from_serializes_concurrent_writers() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let capacity =
                pipe_in.downcast_file::<PipeFileObject>().unwrap().pipe.lock().capacity();
            let prefix = vec![b'P'; capacity - 8];
            assert_eq!(
                pipe_in.write(&current_task, &mut VecInputBuffer::new(&prefix)).unwrap(),
                capacity - 8
            );

            let (entered_tx, entered_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let barrier_source = anon_test_file(
                &current_task,
                Box::new(BarrierSourceFile {
                    entered_tx: Mutex::new(Some(entered_tx)),
                    resume_rx: Mutex::new(Some(resume_rx)),
                    payload: b"SSSS",
                }),
                OpenFlags::RDONLY,
            );

            let (writer_ready_tx, writer_ready_rx) = mpsc::channel();
            let pipe_in_unblock = pipe_in.clone();
            let (unblock_join, unblock_req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |_worker_task: &CurrentTask| {
                    writer_ready_rx.recv().expect("wait until non-blocking writer starts");
                    while pipe_in_unblock
                        .downcast_file::<PipeFileObject>()
                        .unwrap()
                        .pipe
                        .lock()
                        .waiters
                        .is_empty()
                    {
                        std::thread::yield_now();
                    }
                    resume_tx.send(()).expect("allow splice_from read to complete");
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(unblock_req);

            let pipe_in_clone = pipe_in.clone();
            let (writer_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    entered_rx.recv().expect("wait until splice_from enters read");
                    {
                        let pipe =
                            pipe_in_clone.downcast_file::<PipeFileObject>().unwrap().pipe.lock();
                        assert!(pipe.splice_write_in_progress);
                        assert_eq!(pipe.messages.available_capacity(), 8);
                    }
                    writer_ready_tx.send(()).expect("signal unblocker");
                    // A non-blocking write waits for the active splice write to finish and then
                    // writes into the remaining 4 bytes of capacity in FIFO order after `SSSS`.
                    let mut pipe = pipe_in_clone
                        .downcast_file::<PipeFileObject>()
                        .unwrap()
                        .lock_pipe_for_writing(worker_task, &pipe_in_clone, true, 4)
                        .expect("non-blocking write lock succeeds once splice_from finishes");
                    assert_eq!(pipe.write(&mut VecInputBuffer::new(b"WWWW")).unwrap(), 4);
                    std::mem::drop(pipe);

                    std::mem::drop(
                        pipe_in_clone
                            .downcast_file::<PipeFileObject>()
                            .unwrap()
                            .lock_pipe_without_splice(worker_task)
                            .unwrap(),
                    );
                    // Once both 4-byte writes finish, the pipe is full (`EAGAIN`).
                    pipe_in_clone
                        .downcast_file::<PipeFileObject>()
                        .unwrap()
                        .lock_pipe_for_writing(worker_task, &pipe_in_clone, true, 4)
                        .err()
                        .expect("pipe should be full once splice_from and concurrent write finish")
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            let spliced = pipe_in
                .downcast_file::<PipeFileObject>()
                .unwrap()
                .splice_from(&current_task, &pipe_in, &barrier_source, None, 4, true)
                .unwrap();
            assert_eq!(spliced, 4);
            unblock_join().unwrap();
            assert_eq!(writer_join().unwrap(), errno!(EAGAIN));

            let mut pre_buf = VecOutputBuffer::new(capacity - 8);
            assert_eq!(pipe_out.read(&current_task, &mut pre_buf).unwrap(), capacity - 8);
            let mut spliced_buf = VecOutputBuffer::new(8);
            assert_eq!(pipe_out.read(&current_task, &mut spliced_buf).unwrap(), 8);
            assert_eq!(spliced_buf.data(), b"SSSSWWWW");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_splice_from_closed_reader_returns_epipe_and_queues_sigpipe() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            std::mem::drop(pipe_out);
            current_task.trigger_delayed_releaser();

            let source_file = anon_test_file(
                &current_task,
                Box::new(StaticSourceFile { payload: b"DATA" }),
                OpenFlags::RDONLY,
            );
            let err = pipe_in
                .downcast_file::<PipeFileObject>()
                .unwrap()
                .splice_from(&current_task, &pipe_in, &source_file, None, 4, true)
                .unwrap_err();
            assert_eq!(err, errno!(EPIPE));
            assert!(current_task.write().signals_mut().has_queued(SIGPIPE));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_copy_pipes_with_vmspliced_payload() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_a_out, pipe_a_in) = new_pipe(&current_task).unwrap();
            let (pipe_b_out, pipe_b_in) = new_pipe(&current_task).unwrap();

            let mm = current_task.mm().unwrap();
            let page_size = *PAGE_SIZE as usize;
            let memory = Arc::new(MemoryObject::from(zx::Vmo::create(page_size as u64).unwrap()));
            memory.write(b"VMSPLICED_DATA", 0).unwrap();
            let payload = VmsplicePayload::new(
                Arc::downgrade(&mm),
                VmsplicePayloadSegment {
                    addr_offset: UserAddress::NULL,
                    length: 14,
                    memory,
                    memory_offset: 0,
                    should_snapshot_on_unmap: true,
                },
            );
            pipe_a_in
                .downcast_file::<PipeFileObject>()
                .unwrap()
                .pipe
                .lock()
                .messages
                .write_message(PipeMessageData::Vmspliced(payload).into());

            let copied = PipeFileObject::copy_pipes(
                &current_task,
                &pipe_a_out,
                &pipe_a_out.downcast_file::<PipeFileObject>().unwrap(),
                &pipe_b_in,
                &pipe_b_in.downcast_file::<PipeFileObject>().unwrap(),
                10,
                false,
                false,
            )
            .unwrap();
            assert_eq!(copied, 10);

            let mut buf_b = VecOutputBuffer::new(10);
            assert_eq!(pipe_b_out.read(&current_task, &mut buf_b).unwrap(), 10);
            assert_eq!(buf_b.data(), b"VMSPLICED_");

            let mut buf_a = VecOutputBuffer::new(14);
            assert_eq!(pipe_a_out.read(&current_task, &mut buf_a).unwrap(), 14);
            assert_eq!(buf_a.data(), b"VMSPLICED_DATA");
        })
        .await;
    }
    #[::fuchsia::test]
    async fn test_splice_from_reader_closes_during_read_returns_epipe() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let (entered_tx, entered_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let barrier_source = anon_test_file(
                &current_task,
                Box::new(BarrierSourceFile {
                    entered_tx: Mutex::new(Some(entered_tx)),
                    resume_rx: Mutex::new(Some(resume_rx)),
                    payload: b"DATA",
                }),
                OpenFlags::RDONLY,
            );

            let (closer_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    entered_rx.recv().expect("wait until splice_from enters read");
                    std::mem::drop(pipe_out);
                    worker_task.trigger_delayed_releaser();
                    resume_tx.send(()).expect("allow splice_from read to complete");
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            let err = pipe_in
                .downcast_file::<PipeFileObject>()
                .unwrap()
                .splice_from(&current_task, &pipe_in, &barrier_source, None, 4, true)
                .unwrap_err();
            closer_join().unwrap();
            assert_eq!(err, errno!(EPIPE));
            assert!(current_task.write().signals_mut().has_queued(SIGPIPE));
            assert_eq!(
                pipe_in.downcast_file::<PipeFileObject>().unwrap().pipe.lock().messages.len(),
                0
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_partial_read_wakes_blocked_writer() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let capacity =
                pipe_in.downcast_file::<PipeFileObject>().unwrap().pipe.lock().capacity();
            let fill = vec![b'A'; capacity];
            assert_eq!(
                pipe_in.write(&current_task, &mut VecInputBuffer::new(&fill)).unwrap(),
                capacity
            );

            let pipe_in_clone = pipe_in.clone();
            let (writer_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    pipe_in_clone
                        .write(worker_task, &mut VecInputBuffer::new(b"BBBB"))
                        .expect("blocked write should succeed after partial read")
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            while pipe_in.downcast_file::<PipeFileObject>().unwrap().pipe.lock().waiters.is_empty()
            {
                std::thread::yield_now();
            }

            // Read one page from the full pipe; the pipe is still non-empty, but has space.
            let page_size = *PAGE_SIZE as usize;
            let mut partial = VecOutputBuffer::new(page_size);
            assert_eq!(pipe_out.read(&current_task, &mut partial).unwrap(), page_size);
            assert_eq!(partial.data(), &fill[..page_size]);

            assert_eq!(writer_join().unwrap(), 4);
        })
        .await;
    }

    struct WouldBlockEndpointFile;

    impl FileOps for WouldBlockEndpointFile {
        fileops_impl_nonseekable!();
        fileops_impl_noop_sync!();

        fn read(
            &self,
            file: &FileObject,
            current_task: &CurrentTask,
            _offset: usize,
            _data: &mut dyn OutputBuffer,
        ) -> Result<usize, Errno> {
            file.blocking_op(current_task, FdEvents::POLLIN, None, || error!(EAGAIN))
        }

        fn write(
            &self,
            file: &FileObject,
            current_task: &CurrentTask,
            _offset: usize,
            _data: &mut dyn InputBuffer,
        ) -> Result<usize, Errno> {
            file.blocking_op(current_task, FdEvents::POLLOUT, None, || error!(EAGAIN))
        }

        fn wait_async(
            &self,
            _file: &FileObject,
            _current_task: &CurrentTask,
            _waiter: &Waiter,
            _events: FdEvents,
            _handler: EventHandler,
        ) -> Option<WaitCanceler> {
            Some(WaitCanceler::new_noop())
        }
    }

    #[::fuchsia::test]
    async fn test_splice_non_blocking_unix_stream_and_pipe_flags() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let (unix_sock, unix_peer) = crate::vfs::socket::UnixSocket::new_pair(
                &current_task,
                crate::vfs::socket::SocketDomain::Unix,
                crate::vfs::socket::SocketType::Stream,
                OpenFlags::RDWR,
            )
            .unwrap();
            let blocking_sink =
                anon_test_file(&current_task, Box::new(WouldBlockEndpointFile), OpenFlags::WRONLY);

            // SPLICE_F_NONBLOCK (`non_blocking = true`) makes `splice_from` on an empty blocking
            // AF_UNIX stream socket return EAGAIN without marking the socket itself non-blocking.
            assert_eq!(
                pipe_in
                    .downcast_file::<PipeFileObject>()
                    .unwrap()
                    .splice_from(&current_task, &pipe_in, &unix_sock, None, 4, true)
                    .unwrap_err(),
                errno!(EAGAIN)
            );
            assert!(!unix_sock.is_non_blocking());

            // Setting O_NONBLOCK on the pipe write end also makes `splice_from` on an empty
            // blocking AF_UNIX stream socket return EAGAIN even when `non_blocking = false`.
            pipe_in.update_file_flags(OpenFlags::NONBLOCK, OpenFlags::NONBLOCK);
            assert_eq!(
                pipe_in
                    .downcast_file::<PipeFileObject>()
                    .unwrap()
                    .splice_from(&current_task, &pipe_in, &unix_sock, None, 4, false)
                    .unwrap_err(),
                errno!(EAGAIN)
            );
            assert!(!unix_sock.is_non_blocking());

            // Data written to the peer splices into the pipe and subsequent empty splice_from
            // still returns EAGAIN without mutating the socket's O_NONBLOCK flag.
            assert_eq!(
                unix_peer.write(&current_task, &mut VecInputBuffer::new(b"ABCD")).unwrap(),
                4
            );
            assert_eq!(
                pipe_in
                    .downcast_file::<PipeFileObject>()
                    .unwrap()
                    .splice_from(&current_task, &pipe_in, &unix_sock, None, 4, false)
                    .unwrap(),
                4
            );
            let mut read_buf = VecOutputBuffer::new(4);
            assert_eq!(pipe_out.read(&current_task, &mut read_buf).unwrap(), 4);
            assert_eq!(read_buf.data(), b"ABCD");
            assert!(!unix_sock.is_non_blocking());

            // Setting O_NONBLOCK on the pipe read end makes `splice_to` on an empty pipe return
            // EAGAIN even when `non_blocking = false`.
            pipe_out.update_file_flags(OpenFlags::NONBLOCK, OpenFlags::NONBLOCK);
            assert_eq!(
                pipe_out
                    .downcast_file::<PipeFileObject>()
                    .unwrap()
                    .splice_to(&current_task, &pipe_out, &blocking_sink, None, 4, false)
                    .unwrap_err(),
                errno!(EAGAIN)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_fcntl_getpipe_sz_and_setpipe_sz_during_active_splice() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"DATA")).unwrap(), 4);
            let initial_capacity =
                pipe_out.downcast_file::<PipeFileObject>().unwrap().pipe.lock().capacity();

            // 1. Both F_GETPIPE_SZ and F_SETPIPE_SZ serialize behind an active `splice_to`.
            let (entered_tx, entered_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let barrier_sink = anon_test_file(
                &current_task,
                Box::new(BarrierSinkFile {
                    entered_tx: Mutex::new(Some(entered_tx)),
                    resume_rx: Mutex::new(Some(resume_rx)),
                    max_write: 4,
                    written: Mutex::new(Vec::new()),
                }),
                OpenFlags::WRONLY,
            );

            let (ready_tx, ready_rx) = mpsc::channel();
            let pipe_out_unblock = pipe_out.clone();
            let (unblock_join, unblock_req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |_worker_task: &CurrentTask| {
                    ready_rx.recv().expect("wait until F_GETPIPE_SZ starts");
                    while pipe_out_unblock
                        .downcast_file::<PipeFileObject>()
                        .unwrap()
                        .pipe
                        .lock()
                        .waiters
                        .is_empty()
                    {
                        std::thread::yield_now();
                    }
                    resume_tx.send(()).expect("allow splice_to write to complete");
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(unblock_req);

            let pipe_out_clone = pipe_out.clone();
            let (fcntl_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    entered_rx.recv().expect("wait until splice_to enters write");
                    ready_tx.send(()).expect("signal unblocker");
                    // F_GETPIPE_SZ waits for the active splice read to finish.
                    let got_sz =
                        pipe_out_clone.fcntl(worker_task, F_GETPIPE_SZ, 0).expect("F_GETPIPE_SZ");
                    assert_eq!(got_sz, initial_capacity.into());

                    let new_sz = (*PAGE_SIZE as u64) * 2;
                    pipe_out_clone
                        .fcntl(worker_task, F_SETPIPE_SZ, new_sz)
                        .expect("F_SETPIPE_SZ after splice_to")
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            let spliced = pipe_out
                .downcast_file::<PipeFileObject>()
                .unwrap()
                .splice_to(&current_task, &pipe_out, &barrier_sink, None, 4, true)
                .unwrap();
            assert_eq!(spliced, 4);
            unblock_join().unwrap();
            assert_eq!(fcntl_join().unwrap(), ((*PAGE_SIZE as usize) * 2).into());

            // 2. Both F_SETPIPE_SZ and F_GETPIPE_SZ serialize behind an active `splice_from`.
            let (entered_tx2, entered_rx2) = mpsc::channel();
            let (resume_tx2, resume_rx2) = mpsc::channel();
            let barrier_source = anon_test_file(
                &current_task,
                Box::new(BarrierSourceFile {
                    entered_tx: Mutex::new(Some(entered_tx2)),
                    resume_rx: Mutex::new(Some(resume_rx2)),
                    payload: b"DATA",
                }),
                OpenFlags::RDONLY,
            );

            let (ready_tx2, ready_rx2) = mpsc::channel();
            let pipe_in_unblock = pipe_in.clone();
            let (unblock_join2, unblock_req2) = SpawnRequestBuilder::new()
                .with_sync_closure(move |_worker_task: &CurrentTask| {
                    ready_rx2.recv().expect("wait until F_SETPIPE_SZ starts");
                    while pipe_in_unblock
                        .downcast_file::<PipeFileObject>()
                        .unwrap()
                        .pipe
                        .lock()
                        .waiters
                        .is_empty()
                    {
                        std::thread::yield_now();
                    }
                    resume_tx2.send(()).expect("allow splice_from read to complete");
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(unblock_req2);

            let pipe_in_clone = pipe_in.clone();
            let (fcntl_join2, req2) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    entered_rx2.recv().expect("wait until splice_from enters read");
                    ready_tx2.send(()).expect("signal unblocker");
                    let new_sz = (*PAGE_SIZE as u64) * 4;
                    let set_sz = pipe_in_clone
                        .fcntl(worker_task, F_SETPIPE_SZ, new_sz)
                        .expect("F_SETPIPE_SZ after splice_from");
                    assert_eq!(set_sz, ((*PAGE_SIZE as usize) * 4).into());
                    pipe_in_clone.fcntl(worker_task, F_GETPIPE_SZ, 0).expect("F_GETPIPE_SZ")
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req2);

            let spliced2 = pipe_in
                .downcast_file::<PipeFileObject>()
                .unwrap()
                .splice_from(&current_task, &pipe_in, &barrier_source, None, 4, true)
                .unwrap();
            assert_eq!(spliced2, 4);
            unblock_join2().unwrap();
            assert_eq!(fcntl_join2().unwrap(), ((*PAGE_SIZE as usize) * 4).into());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_splice_short_transfer_when_available_capacity_below_pipe_buf() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let capacity =
                pipe_in.downcast_file::<PipeFileObject>().unwrap().pipe.lock().capacity();
            let fill = vec![b'P'; capacity - 4];
            assert_eq!(
                pipe_in.write(&current_task, &mut VecInputBuffer::new(&fill)).unwrap(),
                capacity - 4
            );

            // Requesting 8 bytes (`< PIPE_BUF`) via `splice_from` when 4 bytes of space remain
            // succeeds with a 4-byte transfer instead of failing with `EAGAIN`.
            let source_file = anon_test_file(
                &current_task,
                Box::new(StaticSourceFile { payload: b"12345678" }),
                OpenFlags::RDONLY,
            );
            let spliced = pipe_in
                .downcast_file::<PipeFileObject>()
                .unwrap()
                .splice_from(&current_task, &pipe_in, &source_file, None, 8, true)
                .unwrap();
            assert_eq!(spliced, 4);

            // Likewise for `copy_pipes` into a pipe with 4 bytes of remaining space.
            let (pipe_b_out, pipe_b_in) = new_pipe(&current_task).unwrap();
            assert_eq!(
                pipe_b_in.write(&current_task, &mut VecInputBuffer::new(&fill)).unwrap(),
                capacity - 4
            );
            let copied = PipeFileObject::copy_pipes(
                &current_task,
                &pipe_out,
                &pipe_out.downcast_file::<PipeFileObject>().unwrap(),
                &pipe_b_in,
                &pipe_b_in.downcast_file::<PipeFileObject>().unwrap(),
                8,
                true,
                true,
            )
            .unwrap();
            assert_eq!(copied, 4);
            let _ = pipe_b_out;
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_copy_pipes_honors_nonblock_and_eof_and_epipe() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            // 1. O_NONBLOCK on `file_out` causes `copy_pipes` on an empty `pipe_in` to return EAGAIN.
            let (pipe_a_out, pipe_a_in) = new_pipe(&current_task).unwrap();
            let (pipe_b_out, pipe_b_in) = new_pipe(&current_task).unwrap();
            pipe_b_in.update_file_flags(OpenFlags::NONBLOCK, OpenFlags::NONBLOCK);
            assert_eq!(
                PipeFileObject::copy_pipes(
                    &current_task,
                    &pipe_a_out,
                    &pipe_a_out.downcast_file::<PipeFileObject>().unwrap(),
                    &pipe_b_in,
                    &pipe_b_in.downcast_file::<PipeFileObject>().unwrap(),
                    4,
                    false,
                    false,
                )
                .unwrap_err(),
                errno!(EAGAIN)
            );

            // 2. When `pipe_in` is at EOF and `pipe_out` is writable, `copy_pipes` returns `Ok(0)`;
            // when `pipe_out` is full (with an open reader) and non-blocking, `copy_pipes` returns
            // `EAGAIN`.
            std::mem::drop(pipe_a_in);
            current_task.trigger_delayed_releaser();
            assert_eq!(
                PipeFileObject::copy_pipes(
                    &current_task,
                    &pipe_a_out,
                    &pipe_a_out.downcast_file::<PipeFileObject>().unwrap(),
                    &pipe_b_in,
                    &pipe_b_in.downcast_file::<PipeFileObject>().unwrap(),
                    4,
                    true,
                    false,
                )
                .unwrap(),
                0
            );
            let capacity =
                pipe_b_in.downcast_file::<PipeFileObject>().unwrap().pipe.lock().capacity();
            let fill = vec![b'F'; capacity];
            assert_eq!(
                pipe_b_in.write(&current_task, &mut VecInputBuffer::new(&fill)).unwrap(),
                capacity
            );
            assert_eq!(
                PipeFileObject::copy_pipes(
                    &current_task,
                    &pipe_a_out,
                    &pipe_a_out.downcast_file::<PipeFileObject>().unwrap(),
                    &pipe_b_in,
                    &pipe_b_in.downcast_file::<PipeFileObject>().unwrap(),
                    4,
                    true,
                    false,
                )
                .unwrap_err(),
                errno!(EAGAIN)
            );

            // 3. When `pipe_out` has no readers, `copy_pipes` returns `EPIPE` and queues `SIGPIPE`.
            std::mem::drop(pipe_b_out);
            current_task.trigger_delayed_releaser();
            assert_eq!(
                PipeFileObject::copy_pipes(
                    &current_task,
                    &pipe_a_out,
                    &pipe_a_out.downcast_file::<PipeFileObject>().unwrap(),
                    &pipe_b_in,
                    &pipe_b_in.downcast_file::<PipeFileObject>().unwrap(),
                    4,
                    true,
                    false,
                )
                .unwrap_err(),
                errno!(EPIPE)
            );
            assert!(current_task.write().signals_mut().has_queued(SIGPIPE));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_copy_pipes_wakes_with_epipe_when_destination_reader_closes() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_a_out, pipe_a_in) = new_pipe(&current_task).unwrap();
            let (pipe_b_out, pipe_b_in) = new_pipe(&current_task).unwrap();
            assert_eq!(
                pipe_a_in.write(&current_task, &mut VecInputBuffer::new(b"DATA")).unwrap(),
                4
            );
            let capacity =
                pipe_b_in.downcast_file::<PipeFileObject>().unwrap().pipe.lock().capacity();
            let fill = vec![b'F'; capacity];
            assert_eq!(
                pipe_b_in.write(&current_task, &mut VecInputBuffer::new(&fill)).unwrap(),
                capacity
            );

            let pipe_b_in_clone = pipe_b_in.clone();
            let (closer_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    while pipe_b_in_clone
                        .downcast_file::<PipeFileObject>()
                        .unwrap()
                        .pipe
                        .lock()
                        .waiters
                        .is_empty()
                    {
                        std::thread::yield_now();
                    }
                    std::mem::drop(pipe_b_out);
                    worker_task.trigger_delayed_releaser();
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            let err = PipeFileObject::copy_pipes(
                &current_task,
                &pipe_a_out,
                &pipe_a_out.downcast_file::<PipeFileObject>().unwrap(),
                &pipe_b_in,
                &pipe_b_in.downcast_file::<PipeFileObject>().unwrap(),
                4,
                false,
                true,
            )
            .unwrap_err();
            closer_join().unwrap();
            assert_eq!(err, errno!(EPIPE));
            assert!(current_task.write().signals_mut().has_queued(SIGPIPE));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_splice_to_eof_pipe_does_not_invoke_destination_write() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            std::mem::drop(pipe_in);
            current_task.trigger_delayed_releaser();

            let endpoint =
                anon_test_file(&current_task, Box::new(WouldBlockEndpointFile), OpenFlags::WRONLY);
            let pipe_ops = pipe_out.downcast_file::<PipeFileObject>().unwrap();

            // `WouldBlockEndpointFile::write` would block (or return `EAGAIN` if non-blocking),
            // so returning `Ok(0)` proves `to.write` was skipped on source pipe EOF.
            assert_eq!(
                pipe_ops.splice_to(&current_task, &pipe_out, &endpoint, None, 16, true).unwrap(),
                0
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_pipe_readiness_and_fionread_during_eof_and_active_splice() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"AB")).unwrap(), 2);

            // Register an async waiter for POLLIN while the pipe is already non-empty, then write
            // 2 more bytes; the write must still notify POLLIN.
            let waiter = Waiter::new();
            pipe_out.wait_async(&current_task, &waiter, FdEvents::POLLIN, WaitCallback::none());
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"CD")).unwrap(), 2);
            waiter.wait(&current_task).expect("write to non-empty pipe must notify POLLIN");

            // Reserve all 4 queued bytes in an active splice read.
            let pipe_ops = pipe_out.downcast_file::<PipeFileObject>().unwrap();
            let guard = SpliceReadGuard::new(&pipe_ops.pipe, pipe_ops.pipe.lock(), 4);
            assert_eq!(pipe_out.query_events(&current_task).unwrap(), FdEvents::empty());
            assert_eq!(pipe_ops.pipe.lock().readable_len(), 0);
            assert!(!pipe_ops.pipe.lock().is_readable());

            // A concurrent write serializes behind the active splice read (`lock_pipe_without_splice`)
            // and notifies POLLIN once the splice read completes.
            let pipe_in_clone = pipe_in.clone();
            let (writer_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    pipe_in_clone
                        .write(worker_task, &mut VecInputBuffer::new(b"EF"))
                        .expect("write after active splice read")
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);
            while pipe_ops.pipe.lock().waiters.is_empty() {
                std::thread::yield_now();
            }

            let waiter2 = Waiter::new();
            pipe_out.wait_async(&current_task, &waiter2, FdEvents::POLLIN, WaitCallback::none());
            guard.consume(4);
            assert_eq!(writer_join().unwrap(), 2);
            assert_eq!(
                pipe_out.query_events(&current_task).unwrap(),
                FdEvents::POLLIN | FdEvents::POLLRDNORM
            );
            assert_eq!(pipe_ops.pipe.lock().readable_len(), 2);
            waiter2.wait(&current_task).expect("write after splice read must notify POLLIN");

            let mut remaining = VecOutputBuffer::new(2);
            assert_eq!(pipe_out.read(&current_task, &mut remaining).unwrap(), 2);
            assert_eq!(remaining.data(), b"EF");

            // Close the writer when empty: query_events must report POLLHUP without POLLIN.
            std::mem::drop(pipe_in);
            current_task.trigger_delayed_releaser();
            assert_eq!(pipe_out.query_events(&current_task).unwrap(), FdEvents::POLLHUP);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_zero_length_pipe_io_and_no_empty_message_enqueued() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let pipe_ops = pipe_out.downcast_file::<PipeFileObject>().unwrap();

            // Zero-length read and vmsplice_to on an empty pipe return 0 without blocking or EAGAIN.
            assert_eq!(pipe_ops.pipe.lock().read(&mut VecOutputBuffer::new(0)).unwrap(), 0);
            assert_eq!(pipe_out.read(&current_task, &mut VecOutputBuffer::new(0)).unwrap(), 0);
            assert_eq!(
                pipe_ops
                    .vmsplice_to(&current_task, &pipe_out, UserBuffers::default(), true)
                    .unwrap(),
                0
            );

            // Zero-length write via `Pipe::write` does not enqueue an empty message in `MessageQueue`.
            assert_eq!(pipe_ops.pipe.lock().write(&mut VecInputBuffer::new(b"")).unwrap(), 0);
            assert_eq!(pipe_ops.pipe.lock().messages.peek_queue().len(), 0);

            // Close the reader: zero-length write and vmsplice_from on a broken pipe return 0
            // without EPIPE or SIGPIPE.
            std::mem::drop(pipe_out);
            current_task.trigger_delayed_releaser();
            let pipe_in_ops = pipe_in.downcast_file::<PipeFileObject>().unwrap();
            assert_eq!(pipe_in_ops.pipe.lock().write(&mut VecInputBuffer::new(b"")).unwrap(), 0);
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"")).unwrap(), 0);
            assert_eq!(
                pipe_in_ops
                    .vmsplice_from(&current_task, &pipe_in, UserBuffers::default(), true)
                    .unwrap(),
                0
            );
            assert!(!current_task.write().signals_mut().has_queued(SIGPIPE));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_readv_writev_vmsplice_partial_efault_short_counts_and_rollback() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let page_size = *PAGE_SIZE as usize;
            let two_pages = map_memory(&current_task, UserAddress::default(), (*PAGE_SIZE) * 2);
            let bad_page = (two_pages + *PAGE_SIZE).unwrap();
            fill_page(&current_task, two_pages, 'M');
            current_task
                .mm()
                .unwrap()
                .protect(&current_task, bad_page, page_size, ProtectionFlags::empty())
                .unwrap();

            // 1. Multi-page `writev` (`[page_size valid, page_size PROT_NONE]`) enqueues the first
            // page and returns a short count (`page_size`).
            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let mut writev_buf = UserBuffersInputBuffer::unified_new(
                &current_task,
                smallvec![
                    UserBuffer { address: two_pages, length: page_size },
                    UserBuffer { address: bad_page, length: page_size },
                ],
            )
            .unwrap();
            assert_eq!(pipe_in.write(&current_task, &mut writev_buf).unwrap(), page_size);
            assert_eq!(
                pipe_out.downcast_file::<PipeFileObject>().unwrap().pipe.lock().readable_len(),
                page_size
            );
            std::mem::drop((pipe_out, pipe_in));

            // 2. Sub-PIPE_BUF `writev` (`[100B valid, 100B PROT_NONE]`) preserves POSIX PIPE_BUF
            // atomicity on EFAULT, returning EFAULT with 0 bytes queued.
            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let mut sub_pipe_buf = UserBuffersInputBuffer::unified_new(
                &current_task,
                smallvec![
                    UserBuffer { address: two_pages, length: 100 },
                    UserBuffer { address: bad_page, length: 100 },
                ],
            )
            .unwrap();
            assert_eq!(pipe_in.write(&current_task, &mut sub_pipe_buf), error!(EFAULT));
            assert_eq!(
                pipe_out.downcast_file::<PipeFileObject>().unwrap().pipe.lock().readable_len(),
                0
            );
            std::mem::drop((pipe_out, pipe_in));

            // 3. `vmsplice_from` with `[page_size valid, page_size PROT_NONE]` enqueues the valid
            // prefix and returns `page_size`, while `[PROT_NONE]` alone returns `EFAULT`.
            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let pipe_in_ops = pipe_in.downcast_file::<PipeFileObject>().unwrap();
            assert_eq!(
                pipe_in_ops.vmsplice_from(
                    &current_task,
                    &pipe_in,
                    smallvec![UserBuffer { address: bad_page, length: page_size }],
                    true,
                ),
                error!(EFAULT)
            );
            assert_eq!(
                pipe_in_ops
                    .vmsplice_from(
                        &current_task,
                        &pipe_in,
                        smallvec![
                            UserBuffer { address: two_pages, length: page_size },
                            UserBuffer { address: bad_page, length: page_size },
                        ],
                        true,
                    )
                    .unwrap(),
                page_size
            );
            assert_eq!(
                pipe_out.downcast_file::<PipeFileObject>().unwrap().pipe.lock().readable_len(),
                page_size
            );
            std::mem::drop((pipe_out, pipe_in));

            // 4. `read` into a `PROT_NONE` buffer returns `EFAULT` and preserves the 4-byte
            // message at the front of the pipe.
            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"AAAA")).unwrap(), 4);
            let mut bad_out =
                UserBuffersOutputBuffer::unified_new_at(&current_task, bad_page, 4).unwrap();
            assert_eq!(pipe_out.read(&current_task, &mut bad_out), error!(EFAULT));
            assert_eq!(
                pipe_out.downcast_file::<PipeFileObject>().unwrap().pipe.lock().readable_len(),
                4
            );
            std::mem::drop((pipe_out, pipe_in));

            // 5. `readv` and `vmsplice_to` with `[4B valid, 4B PROT_NONE]` against a single 8-byte
            // message roll back the partial copy, return `EFAULT`, and preserve all 8 bytes.
            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let pipe_out_ops = pipe_out.downcast_file::<PipeFileObject>().unwrap();
            assert_eq!(
                pipe_in.write(&current_task, &mut VecInputBuffer::new(b"AAAABBBB")).unwrap(),
                8
            );
            let fault_iov = smallvec![
                UserBuffer { address: two_pages, length: 4 },
                UserBuffer { address: bad_page, length: 4 },
            ];
            assert_eq!(
                pipe_out_ops.vmsplice_to(&current_task, &pipe_out, fault_iov.clone(), true),
                error!(EFAULT)
            );
            assert_eq!(pipe_out_ops.pipe.lock().readable_len(), 8);

            let mut readv_out =
                UserBuffersOutputBuffer::unified_new(&current_task, fault_iov.clone()).unwrap();
            assert_eq!(pipe_out.read(&current_task, &mut readv_out), error!(EFAULT));
            assert_eq!(readv_out.bytes_written(), 0);
            assert_eq!(pipe_out_ops.pipe.lock().readable_len(), 8);
            std::mem::drop((pipe_out, pipe_in));

            // 6. `vmsplice_to` and `readv` with `[4B valid, 4B PROT_NONE]` across two separate
            // 4-byte messages consume the first message, re-queue the second message on EFAULT,
            // and return the short count (`4`).
            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let pipe_out_ops = pipe_out.downcast_file::<PipeFileObject>().unwrap();
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"AAAA")).unwrap(), 4);
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"BBBB")).unwrap(), 4);
            assert_eq!(
                pipe_out_ops
                    .vmsplice_to(&current_task, &pipe_out, fault_iov.clone(), true)
                    .unwrap(),
                4
            );
            assert_eq!(pipe_out_ops.pipe.lock().readable_len(), 4);
            let mut rem_buf = VecOutputBuffer::new(4);
            assert_eq!(pipe_out.read(&current_task, &mut rem_buf).unwrap(), 4);
            assert_eq!(rem_buf.data(), b"BBBB");

            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"CCCC")).unwrap(), 4);
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"DDDD")).unwrap(), 4);
            let mut readv_out2 =
                UserBuffersOutputBuffer::unified_new(&current_task, fault_iov).unwrap();
            assert_eq!(pipe_out.read(&current_task, &mut readv_out2).unwrap(), 4);
            assert_eq!(pipe_out_ops.pipe.lock().readable_len(), 4);
            let mut rem_buf2 = VecOutputBuffer::new(4);
            assert_eq!(pipe_out.read(&current_task, &mut rem_buf2).unwrap(), 4);
            assert_eq!(rem_buf2.data(), b"DDDD");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_blocking_write_wakes_when_reader_frees_less_than_pipe_buf() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let capacity =
                pipe_in.downcast_file::<PipeFileObject>().unwrap().pipe.lock().capacity();
            let fill = vec![b'A'; capacity];
            assert_eq!(
                pipe_in.write(&current_task, &mut VecInputBuffer::new(&fill)).unwrap(),
                capacity
            );

            let pipe_in_clone = pipe_in.clone();
            let (writer_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    pipe_in_clone
                        .write(worker_task, &mut VecInputBuffer::new(b"BBBB"))
                        .expect("blocked 4-byte write wakes when < PIPE_BUF bytes are freed")
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            while pipe_in.downcast_file::<PipeFileObject>().unwrap().pipe.lock().waiters.is_empty()
            {
                std::thread::yield_now();
            }

            // Register a poll (`EventHandler::Enqueue`) waiter and an edge-triggered epoll waiter
            // watching `POLLOUT` on the full pipe alongside the blocked writer.
            let poll_waiter = Waiter::new();
            let poll_ready_items = Arc::default();
            pipe_in.wait_async(
                &current_task,
                &poll_waiter,
                FdEvents::POLLOUT,
                EventHandler::Enqueue {
                    key: crate::task::ReadyItemKey::Usize(1),
                    queue: Arc::clone(&poll_ready_items),
                    sought_events: FdEvents::POLLOUT | FdEvents::POLLERR | FdEvents::POLLHUP,
                },
            );
            let epoll_file = crate::vfs::epoll::EpollFileObject::new_file(&current_task);
            let epoll_ops =
                epoll_file.downcast_file::<crate::vfs::epoll::EpollFileObject>().unwrap();
            epoll_ops
                .add(
                    &current_task,
                    &pipe_in,
                    &epoll_file,
                    starnix_uapi::vfs::EpollEvent::new(FdEvents::POLLOUT | FdEvents::EPOLLET, 1),
                )
                .unwrap();

            let mut small_buf = VecOutputBuffer::new(100);
            assert_eq!(pipe_out.read(&current_task, &mut small_buf).unwrap(), 100);

            assert_eq!(writer_join().unwrap(), 4);
            // Free capacity is 96 bytes (`< PIPE_BUF = 4096`), so neither poll nor epoll may report
            // spurious POLLOUT readiness.
            assert!(poll_ready_items.lock().is_empty());
            assert!(
                epoll_ops.wait(&current_task, 1, zx::MonotonicInstant::ZERO).unwrap().is_empty()
            );

            // Reading `PIPE_BUF` bytes brings available capacity above `PIPE_BUF`, notifying both
            // poll and epoll with `POLLOUT`.
            let mut page_buf = VecOutputBuffer::new(uapi::PIPE_BUF as usize);
            assert_eq!(
                pipe_out.read(&current_task, &mut page_buf).unwrap(),
                uapi::PIPE_BUF as usize
            );
            assert!(!poll_ready_items.lock().is_empty());
            let epoll_events =
                epoll_ops.wait(&current_task, 1, zx::MonotonicInstant::ZERO).unwrap();
            assert_eq!(epoll_events.len(), 1);
            assert!(epoll_events[0].events().contains(FdEvents::POLLOUT));
        })
        .await;
    }
    #[::fuchsia::test]
    async fn test_splice_to_rlimit_fsize_truncation_and_sigxfsz() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let reg_file = new_memfd(
                &current_task,
                b"rlimit_fsize".into(),
                SealFlags::empty(),
                OpenFlags::RDWR,
            )
            .unwrap();
            assert!(reg_file.node().is_reg());
            current_task
                .thread_group()
                .limits
                .lock()
                .set(Resource::FSIZE, uapi::rlimit { rlim_cur: 4, rlim_max: 4 });

            let take_sigxfsz = |task: &CurrentTask| -> bool {
                task.write().signals_mut().take_next_where(|sig| sig.signal == SIGXFSZ).is_some()
            };

            // Subtest 1: `off = 0 < limit = 4, len = 8` with 8 bytes in pipe truncates to 4 bytes,
            // raises SIGXFSZ, and leaves the remaining 4 bytes in the pipe.
            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let pipe_ops = pipe_out.downcast_file::<PipeFileObject>().unwrap();
            assert_eq!(
                pipe_in.write(&current_task, &mut VecInputBuffer::new(b"AAAABBBB")).unwrap(),
                8
            );
            assert_eq!(
                pipe_ops.splice_to(&current_task, &pipe_out, &reg_file, Some(0), 8, true).unwrap(),
                4
            );
            assert!(take_sigxfsz(&current_task));
            let mut rem = VecOutputBuffer::new(8);
            assert_eq!(pipe_out.read(&current_task, &mut rem).unwrap(), 4);
            assert_eq!(rem.data(), b"BBBB");

            // Subtest 2: `off = 0 < limit = 4, len = 8` with 4 bytes in pipe writes 4 bytes
            // without raising SIGXFSZ.
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"AAAA")).unwrap(), 4);
            assert_eq!(
                pipe_ops.splice_to(&current_task, &pipe_out, &reg_file, Some(0), 8, true).unwrap(),
                4
            );
            assert!(!take_sigxfsz(&current_task));

            // Subtest 3: `off = 0 < limit = 4, len = 8` on an EOF pipe returns 0 without SIGXFSZ.
            let (eof_out, eof_in) = new_pipe(&current_task).unwrap();
            std::mem::drop(eof_in);
            current_task.trigger_delayed_releaser();
            let eof_ops = eof_out.downcast_file::<PipeFileObject>().unwrap();
            assert_eq!(
                eof_ops.splice_to(&current_task, &eof_out, &reg_file, Some(0), 8, false).unwrap(),
                0
            );
            assert!(!take_sigxfsz(&current_task));

            // Subtest 4: `off = 0 < limit = 4, len = 8` on an empty non-blocking pipe returns
            // EAGAIN without SIGXFSZ.
            assert_eq!(
                pipe_ops.splice_to(&current_task, &pipe_out, &reg_file, Some(0), 8, true),
                error!(EAGAIN)
            );
            assert!(!take_sigxfsz(&current_task));

            // Subtest 5: `off = 4 >= limit = 4, len = 4` on an EOF pipe returns 0 without SIGXFSZ.
            assert_eq!(
                eof_ops.splice_to(&current_task, &eof_out, &reg_file, Some(4), 4, false).unwrap(),
                0
            );
            assert!(!take_sigxfsz(&current_task));

            // Subtest 6: `off = 4 >= limit = 4, len = 4` on an empty non-blocking pipe returns
            // EAGAIN without SIGXFSZ.
            assert_eq!(
                pipe_ops.splice_to(&current_task, &pipe_out, &reg_file, Some(4), 4, true),
                error!(EAGAIN)
            );
            assert!(!take_sigxfsz(&current_task));

            // Subtest 7: `off = 4 >= limit = 4, len = 4` with 4 bytes in pipe returns EFBIG,
            // raises SIGXFSZ, and leaves all 4 bytes unconsumed in the pipe.
            assert_eq!(pipe_in.write(&current_task, &mut VecInputBuffer::new(b"AAAA")).unwrap(), 4);
            assert_eq!(
                pipe_ops.splice_to(&current_task, &pipe_out, &reg_file, Some(4), 4, true),
                error!(EFBIG)
            );
            assert!(take_sigxfsz(&current_task));
            let mut rem2 = VecOutputBuffer::new(8);
            assert_eq!(pipe_out.read(&current_task, &mut rem2).unwrap(), 4);
            assert_eq!(rem2.data(), b"AAAA");
        })
        .await;
    }
    #[::fuchsia::test]
    async fn test_poll_and_nonblocking_read_during_inflight_splice_to() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            assert_eq!(
                pipe_in.write(&current_task, &mut VecInputBuffer::new(b"AAAABBBB")).unwrap(),
                8
            );
            pipe_out.update_file_flags(OpenFlags::NONBLOCK, OpenFlags::NONBLOCK);

            let (entered_tx, entered_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let barrier_sink = anon_test_file(
                &current_task,
                Box::new(BarrierSinkFile {
                    entered_tx: Mutex::new(Some(entered_tx)),
                    resume_rx: Mutex::new(Some(resume_rx)),
                    max_write: 4,
                    written: Mutex::new(Vec::new()),
                }),
                OpenFlags::WRONLY,
            );

            let (ready_tx, ready_rx) = mpsc::channel();
            let pipe_out_unblock = pipe_out.clone();
            let (unblock_join, unblock_req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |_worker_task: &CurrentTask| {
                    ready_rx.recv().expect("wait until non-blocking read starts");
                    while pipe_out_unblock
                        .downcast_file::<PipeFileObject>()
                        .unwrap()
                        .pipe
                        .lock()
                        .waiters
                        .is_empty()
                    {
                        std::thread::yield_now();
                    }
                    resume_tx.send(()).expect("allow splice_to write to complete");
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(unblock_req);

            let pipe_out_clone = pipe_out.clone();
            let (reader_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    entered_rx.recv().expect("wait until splice_to enters write");
                    // 4 unreserved bytes remain, so query_events reports POLLIN.
                    assert_eq!(
                        pipe_out_clone.query_events(worker_task).unwrap(),
                        FdEvents::POLLIN | FdEvents::POLLRDNORM
                    );
                    ready_tx.send(()).expect("signal unblocker");
                    let mut out = VecOutputBuffer::new(4);
                    let n = pipe_out_clone
                        .read(worker_task, &mut out)
                        .expect("O_NONBLOCK read waits for in-flight splice_to and succeeds");
                    out.data()[..n].to_vec()
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            let spliced = pipe_out
                .downcast_file::<PipeFileObject>()
                .unwrap()
                .splice_to(&current_task, &pipe_out, &barrier_sink, None, 4, true)
                .unwrap();
            assert_eq!(spliced, 4);
            unblock_join().unwrap();
            assert_eq!(reader_join().unwrap(), b"BBBB");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_nonblocking_write_to_readerless_pipe_during_inflight_splice_returns_epipe() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            pipe_in.update_file_flags(OpenFlags::NONBLOCK, OpenFlags::NONBLOCK);

            let (entered_tx, entered_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let barrier_source = anon_test_file(
                &current_task,
                Box::new(BarrierSourceFile {
                    entered_tx: Mutex::new(Some(entered_tx)),
                    resume_rx: Mutex::new(Some(resume_rx)),
                    payload: b"AAAA",
                }),
                OpenFlags::RDONLY,
            );

            let (ready_tx, ready_rx) = mpsc::channel();
            let pipe_in_unblock = pipe_in.clone();
            let (unblock_join, unblock_req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |_worker_task: &CurrentTask| {
                    ready_rx.recv().expect("wait until non-blocking write starts");
                    while pipe_in_unblock
                        .downcast_file::<PipeFileObject>()
                        .unwrap()
                        .pipe
                        .lock()
                        .waiters
                        .is_empty()
                    {
                        std::thread::yield_now();
                    }
                    resume_tx.send(()).expect("allow splice_from read to complete");
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(unblock_req);

            let pipe_in_clone = pipe_in.clone();
            let (writer_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    entered_rx.recv().expect("wait until splice_from enters read");
                    std::mem::drop(pipe_out);
                    worker_task.trigger_delayed_releaser();
                    ready_tx.send(()).expect("signal unblocker");
                    let err = pipe_in_clone
                        .write(worker_task, &mut VecInputBuffer::new(b"BBBB"))
                        .unwrap_err();
                    let has_sigpipe = worker_task.write().signals_mut().has_queued(SIGPIPE);
                    (err, has_sigpipe)
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            let splice_err = pipe_in
                .downcast_file::<PipeFileObject>()
                .unwrap()
                .splice_from(&current_task, &pipe_in, &barrier_source, None, 4, true)
                .unwrap_err();
            assert_eq!(splice_err, errno!(EPIPE));
            unblock_join().unwrap();
            let (write_err, has_sigpipe) = writer_join().unwrap();
            assert_eq!(write_err, errno!(EPIPE));
            assert!(has_sigpipe);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_splice_pipes_concurrent_with_reader_preserves_fifo_without_duplicates() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            // Concurrent `splice_pipes` (`consume_source = true`) and `read` on `pipe_a` must
            // partition messages across `pipe_a` and `pipe_b` in FIFO order without duplicates.
            let (pipe_a_out, pipe_a_in) = new_pipe(&current_task).unwrap();
            let (pipe_b_out, pipe_b_in) = new_pipe(&current_task).unwrap();
            const NUM_MSGS: usize = 64;
            for i in 0..NUM_MSGS {
                let msg = format!("M{:03}", i);
                assert_eq!(
                    pipe_a_in
                        .write(&current_task, &mut VecInputBuffer::new(msg.as_bytes()))
                        .unwrap(),
                    4
                );
            }
            std::mem::drop(pipe_a_in);
            current_task.trigger_delayed_releaser();

            let pipe_a_out_clone = pipe_a_out.clone();
            let pipe_b_in_clone = pipe_b_in.clone();
            let (splicer_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    let pipe_a_ops = pipe_a_out_clone.downcast_file::<PipeFileObject>().unwrap();
                    let pipe_b_ops = pipe_b_in_clone.downcast_file::<PipeFileObject>().unwrap();
                    loop {
                        let n = PipeFileObject::splice_pipes(
                            worker_task,
                            &pipe_a_out_clone,
                            &pipe_a_ops,
                            &pipe_b_in_clone,
                            &pipe_b_ops,
                            4,
                            false,
                        )
                        .unwrap();
                        if n == 0 {
                            break;
                        }
                    }
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            let mut read_msgs = Vec::new();
            loop {
                let mut buf = VecOutputBuffer::new(4);
                let n = pipe_a_out.read(&current_task, &mut buf).unwrap();
                if n == 0 {
                    break;
                }
                assert_eq!(n, 4);
                read_msgs.push(String::from_utf8(buf.data().to_vec()).unwrap());
            }
            splicer_join().unwrap();
            std::mem::drop(pipe_b_in);
            current_task.trigger_delayed_releaser();

            let mut spliced_msgs = Vec::new();
            loop {
                let mut buf = VecOutputBuffer::new(4);
                let n = pipe_b_out.read(&current_task, &mut buf).unwrap();
                if n == 0 {
                    break;
                }
                assert_eq!(n, 4);
                spliced_msgs.push(String::from_utf8(buf.data().to_vec()).unwrap());
            }

            // Both streams must be strictly increasing, and their disjoint union must be M000..M063.
            assert!(read_msgs.windows(2).all(|w| w[0] < w[1]));
            assert!(spliced_msgs.windows(2).all(|w| w[0] < w[1]));
            let mut combined = [read_msgs, spliced_msgs].concat();
            combined.sort();
            let expected: Vec<String> = (0..NUM_MSGS).map(|i| format!("M{:03}", i)).collect();
            assert_eq!(combined, expected);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_tee_concurrent_with_reader_preserves_fifo() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            // Concurrent `tee` (`consume_source = false`) and `read` on `pipe_c` must never
            // reorder messages into `pipe_d`.
            let (pipe_c_out, pipe_c_in) = new_pipe(&current_task).unwrap();
            let (pipe_d_out, pipe_d_in) = new_pipe(&current_task).unwrap();
            const NUM_MSGS: usize = 64;
            for i in 0..NUM_MSGS {
                let msg = format!("T{:03}", i);
                assert_eq!(
                    pipe_c_in
                        .write(&current_task, &mut VecInputBuffer::new(msg.as_bytes()))
                        .unwrap(),
                    4
                );
            }
            std::mem::drop(pipe_c_in);
            current_task.trigger_delayed_releaser();

            let pipe_c_out_clone = pipe_c_out.clone();
            let (tee_join, tee_req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    loop {
                        let n = PipeFileObject::tee(
                            worker_task,
                            &pipe_c_out_clone,
                            &pipe_d_in,
                            4,
                            false,
                        )
                        .unwrap();
                        if n == 0 {
                            break;
                        }
                    }
                    std::mem::drop(pipe_d_in);
                    worker_task.trigger_delayed_releaser();
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(tee_req);

            loop {
                let mut buf = VecOutputBuffer::new(4);
                if pipe_c_out.read(&current_task, &mut buf).unwrap() == 0 {
                    break;
                }
            }

            // Drain `pipe_d_out` before joining `worker_task` so a `tee` call blocked on a full
            // `pipe_d` can wake, observe EOF on `pipe_c`, and close `pipe_d_in`.
            let mut teed_msgs = Vec::new();
            loop {
                let mut buf = VecOutputBuffer::new(4);
                let n = pipe_d_out.read(&current_task, &mut buf).unwrap();
                if n == 0 {
                    break;
                }
                assert_eq!(n, 4);
                teed_msgs.push(String::from_utf8(buf.data().to_vec()).unwrap());
            }
            tee_join().unwrap();
            assert!(teed_msgs.windows(2).all(|w| w[0] <= w[1]));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_tee_blocked_on_full_destination_wakes_on_source_eof() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            // Deterministic regression check for https://fxbug.dev/578116361: force blocking
            // `tee` to fill the destination pipe and sleep on `PIPE_WRITE_SPACE_WAIT_KEY` before
            // the source pipe is drained to EOF, then verify draining the destination wakes the
            // worker and terminates cleanly.
            const MSG_LEN: usize = 4;
            let (full_source_out, full_source_in) = new_pipe(&current_task).unwrap();
            let (full_dest_out, full_dest_in) = new_pipe(&current_task).unwrap();
            let dest_capacity = (*PAGE_SIZE as usize) * 2;
            assert_eq!(
                full_dest_in.fcntl(&current_task, F_SETPIPE_SZ, dest_capacity as u64).unwrap(),
                dest_capacity.into()
            );
            assert_eq!(
                full_source_in.write(&current_task, &mut VecInputBuffer::new(b"T000")).unwrap(),
                MSG_LEN
            );
            std::mem::drop(full_source_in);
            current_task.trigger_delayed_releaser();

            let full_dest_ops = full_dest_out.downcast_file::<PipeFileObject>().unwrap();
            let full_source_out_clone = full_source_out.clone();
            let (blocked_tee_join, blocked_tee_req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    loop {
                        let count = PipeFileObject::tee(
                            worker_task,
                            &full_source_out_clone,
                            &full_dest_in,
                            MSG_LEN,
                            false,
                        )
                        .unwrap();
                        if count == 0 {
                            break;
                        }
                    }
                    std::mem::drop(full_dest_in);
                    worker_task.trigger_delayed_releaser();
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(blocked_tee_req);

            while full_dest_ops.pipe.lock().waiters.is_empty() {
                std::thread::yield_now();
            }

            let mut source_buf = VecOutputBuffer::new(MSG_LEN);
            assert_eq!(full_source_out.read(&current_task, &mut source_buf).unwrap(), MSG_LEN);
            assert_eq!(source_buf.data(), b"T000");
            assert_eq!(
                full_source_out.read(&current_task, &mut VecOutputBuffer::new(MSG_LEN)).unwrap(),
                0
            );

            let mut drained_bytes = 0;
            loop {
                let mut dest_buf = VecOutputBuffer::new(MSG_LEN);
                let count = full_dest_out.read(&current_task, &mut dest_buf).unwrap();
                if count == 0 {
                    break;
                }
                assert_eq!(count, MSG_LEN);
                assert_eq!(dest_buf.data(), b"T000");
                drained_bytes += count;
            }
            blocked_tee_join().unwrap();
            assert_eq!(drained_bytes, dest_capacity);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_copy_pipes_empty_input_and_readerless_output_matches_linux_ipipe_prep_order() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            // 1. When `pipe_in` is empty (with open writers) and `pipe_out` has no readers,
            // non-blocking `splice_pipes` and `tee` return `EAGAIN` without `SIGPIPE`.
            let (pipe_a_out, pipe_a_in) = new_pipe(&current_task).unwrap();
            let (pipe_b_out, pipe_b_in) = new_pipe(&current_task).unwrap();
            std::mem::drop(pipe_b_out);
            current_task.trigger_delayed_releaser();

            let pipe_a_ops = pipe_a_out.downcast_file::<PipeFileObject>().unwrap();
            let pipe_b_ops = pipe_b_in.downcast_file::<PipeFileObject>().unwrap();
            assert_eq!(
                PipeFileObject::splice_pipes(
                    &current_task,
                    &pipe_a_out,
                    &pipe_a_ops,
                    &pipe_b_in,
                    &pipe_b_ops,
                    4,
                    true,
                )
                .unwrap_err(),
                errno!(EAGAIN)
            );
            assert!(!current_task.write().signals_mut().has_queued(SIGPIPE));
            assert_eq!(
                PipeFileObject::tee(&current_task, &pipe_a_out, &pipe_b_in, 4, true).unwrap_err(),
                errno!(EAGAIN)
            );
            assert!(!current_task.write().signals_mut().has_queued(SIGPIPE));

            // 2. Blocking `splice_pipes` on empty `pipe_in` + readerless `pipe_out` blocks on
            // `pipe_in` first, then returns `EPIPE` + `SIGPIPE` once `pipe_in` becomes readable.
            let pipe_a_out_clone = pipe_a_out.clone();
            let pipe_b_in_clone = pipe_b_in.clone();
            let (splice_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    let a_ops = pipe_a_out_clone.downcast_file::<PipeFileObject>().unwrap();
                    let b_ops = pipe_b_in_clone.downcast_file::<PipeFileObject>().unwrap();
                    let err = PipeFileObject::splice_pipes(
                        worker_task,
                        &pipe_a_out_clone,
                        &a_ops,
                        &pipe_b_in_clone,
                        &b_ops,
                        4,
                        false,
                    )
                    .unwrap_err();
                    let has_sigpipe = worker_task.write().signals_mut().has_queued(SIGPIPE);
                    (err, has_sigpipe)
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            while pipe_a_ops.pipe.lock().waiters.is_empty() {
                std::thread::yield_now();
            }
            assert_eq!(
                pipe_a_in.write(&current_task, &mut VecInputBuffer::new(b"DATA")).unwrap(),
                4
            );
            let (err, has_sigpipe) = splice_join().unwrap();
            assert_eq!(err, errno!(EPIPE));
            assert!(has_sigpipe);
            std::mem::drop(pipe_a_in);

            // 3. Same-pipe `copy_pipes` returns `EINVAL` without deadlocking `ordered_lock`.
            let (same_out, same_in) = new_pipe(&current_task).unwrap();
            let same_out_ops = same_out.downcast_file::<PipeFileObject>().unwrap();
            let same_in_ops = same_in.downcast_file::<PipeFileObject>().unwrap();
            assert_eq!(
                PipeFileObject::splice_pipes(
                    &current_task,
                    &same_out,
                    &same_out_ops,
                    &same_in,
                    &same_in_ops,
                    4,
                    true,
                )
                .unwrap_err(),
                errno!(EINVAL)
            );
        })
        .await;
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "LockDep: Self-deadlock detected")]
    fn test_unordered_nested_pipe_lock_panics_in_lockdep() {
        let pipe1 = Pipe::new(*PAGE_SIZE as usize);
        let pipe2 = Pipe::new(*PAGE_SIZE as usize);
        let _guard1 = pipe1.lock();
        // Attempting to acquire a second pipe lock while holding one must panic in LockDep.
        let _guard2 = pipe2.lock();
    }

    #[::fuchsia::test]
    async fn test_f_setpipe_sz_power_of_two_rounding_and_grow_wakes_blocked_writer() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            register_pipe_fs(kernel.expando.get::<FsRegistry>().as_ref());

            let (_pipe_out, pipe_in) = new_pipe(&current_task).unwrap();
            let page_size = *PAGE_SIZE as usize;

            // Negative sizes must be rejected with `EINVAL`.
            assert_eq!(
                pipe_in.fcntl(&current_task, F_SETPIPE_SZ, (-1i32 as u32) as u64).unwrap_err(),
                errno!(EINVAL)
            );

            // Non-power-of-two sizes above PAGE_SIZE round up to the next power of two.
            let rounded = pipe_in
                .fcntl(&current_task, F_SETPIPE_SZ, (page_size * 3) as u64)
                .expect("F_SETPIPE_SZ rounds to power of two");
            assert_eq!(rounded, (page_size * 4).into());
            assert_eq!(
                pipe_in.fcntl(&current_task, F_GETPIPE_SZ, 0).unwrap(),
                (page_size * 4).into()
            );

            // Fill the pipe to capacity (`page_size * 4`).
            let fill = vec![b'F'; page_size * 4];
            assert_eq!(
                pipe_in.write(&current_task, &mut VecInputBuffer::new(&fill)).unwrap(),
                page_size * 4
            );

            let pipe_in_clone = pipe_in.clone();
            let (writer_join, req) = SpawnRequestBuilder::new()
                .with_sync_closure(move |worker_task: &CurrentTask| {
                    pipe_in_clone
                        .write(worker_task, &mut VecInputBuffer::new(b"GROW"))
                        .expect("blocked writer wakes when F_SETPIPE_SZ grows pipe")
                })
                .build_with_sync_result();
            kernel.kthreads.spawner().spawn_from_request(req);

            while pipe_in.downcast_file::<PipeFileObject>().unwrap().pipe.lock().waiters.is_empty()
            {
                std::thread::yield_now();
            }

            let grown = pipe_in
                .fcntl(&current_task, F_SETPIPE_SZ, (page_size * 8) as u64)
                .expect("F_SETPIPE_SZ grows capacity");
            assert_eq!(grown, (page_size * 8).into());
            assert_eq!(writer_join().unwrap(), 4);
        })
        .await;
    }
}
