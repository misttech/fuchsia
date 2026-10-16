// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mm::{
    MemoryAccessorExt, NumberOfElementsRead, PAGE_SIZE, TaskMemoryAccessor, read_to_vec,
};
use crate::security;
use crate::signals::{SignalInfo, send_standard_signal};
use crate::task::{CurrentTask, EventHandler, WaitCallback, WaitCanceler, WaitQueue, Waiter};
use crate::vfs::buffers::{
    Buffer, InputBuffer, InputBufferCallback, MessageData, MessageQueue, OutputBuffer,
    OutputBufferCallback, PeekBufferSegmentsCallback, PipeMessageData, UserBuffersOutputBuffer,
};
use crate::vfs::fs_registry::FsRegistry;
use crate::vfs::{
    CacheMode, FileHandle, FileObject, FileObjectState, FileOps, FileSystem, FileSystemHandle,
    FileSystemOps, FileSystemOptions, FsNodeInfo, FsStr, SpecialNode, default_fcntl,
    fileops_impl_nonseekable, fileops_impl_noop_sync,
};
use starnix_sync::{Mutex, MutexGuard};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_types::user_buffer::{UserBuffer, UserBuffers};
use starnix_types::vfs::default_statfs;
use starnix_uapi::auth::CAP_SYS_RESOURCE;
use starnix_uapi::errors::{EPIPE, Errno};
use starnix_uapi::file_mode::mode;
use starnix_uapi::fs_type::FileSystemTypeFlags;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::signals::SIGPIPE;
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    F_GETPIPE_SZ, F_SETPIPE_SZ, FIONREAD, PIPEFS_MAGIC, errno, error, statfs, uapi,
};
use std::cmp::Ordering;
use std::sync::Arc;

const ATOMIC_IO_BYTES: u16 = 4096;

/// The maximum size of a pipe, independent of task capabilities and sysctl limits.
const PIPE_MAX_SIZE: usize = 1 << 31;

fn round_up(value: usize, increment: usize) -> usize {
    (value + (increment - 1)) & !(increment - 1)
}

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
}

pub type PipeHandle = Arc<Mutex<Pipe>>;

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
            self.had_reader = false;
            self.had_writer = false;
            self.messages = MessageQueue::new(self.messages.capacity());
            self.waiters = WaitQueue::default();
        }
    }

    fn is_empty(&self) -> bool {
        self.messages.is_empty()
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
        if requested_capacity
            > current_task
                .kernel()
                .system_limits
                .pipe_max_size
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            security::check_task_capable(current_task, CAP_SYS_RESOURCE)?;
        }
        let page_size = *PAGE_SIZE as usize;
        if requested_capacity < page_size {
            requested_capacity = page_size;
        }
        requested_capacity = round_up(requested_capacity, page_size);
        self.messages.set_capacity(requested_capacity)
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

    pub fn read(&mut self, data: &mut dyn OutputBuffer) -> Result<usize, Errno> {
        // Return EAGAIN if the pipe is empty and still has at least one open writer so the caller
        // blocks; otherwise fall through and return 0 to indicate EOF.

        if !self.is_readable() {
            return error!(EAGAIN);
        }

        self.messages.read_stream(data).map(|(info, _)| info.bytes_read)
    }

    pub fn write(&mut self, data: &mut dyn InputBuffer) -> Result<usize, Errno> {
        if !self.had_reader {
            return error!(EAGAIN);
        }

        if self.reader_count == 0 {
            return error!(EPIPE);
        }

        if !self.is_writable(data.available()) {
            return error!(EAGAIN);
        }

        self.messages.write_stream(data, None, &mut vec![])
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
                self.set_capacity(current_task, arg as usize)?;
                Ok(self.capacity().into())
            }
            _ => default_fcntl(cmd),
        }
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
                let addr = UserRef::<i32>::new(user_addr);
                let value: i32 = self.messages.len().try_into().map_err(|_| errno!(EINVAL))?;
                current_task.write_object(addr, &value)?;
                Ok(SUCCESS)
            }
            _ => error!(ENOTTY),
        }
    }

    fn notify_fd_events(&self, events: FdEvents) {
        self.waiters.notify_fd_events(events);
    }

    /// Splice from the `from` pipe to the `to` pipe.
    pub fn splice(from: &mut Pipe, to: &mut Pipe, len: usize) -> Result<usize, Errno> {
        if len == 0 {
            return Ok(0);
        }
        let to_was_empty = to.is_empty();
        let mut bytes_transferred = 0;
        loop {
            let limit = std::cmp::min(len - bytes_transferred, to.messages.available_capacity());
            if limit == 0 {
                // We no longer want to transfer any bytes.
                break;
            }
            let Some(mut message) = from.messages.read_message() else {
                // The `from` pipe is empty.
                break;
            };
            if let Some(data) = MessageData::split_off(&mut message.data, limit) {
                // Some data is left in the message. Push it back.
                assert!(data.len() > 0);
                from.messages.write_front(data.into());
            }
            bytes_transferred += message.len();
            to.messages.write_message(message);
        }
        if bytes_transferred > 0 {
            if from.is_empty() {
                from.notify_fd_events(FdEvents::POLLOUT);
            }
            if to_was_empty {
                to.notify_fd_events(FdEvents::POLLIN);
            }
        }
        return Ok(bytes_transferred);
    }

    /// Tee from the `from` pipe to the `to` pipe.
    pub fn tee(from: &mut Pipe, to: &mut Pipe, len: usize) -> Result<usize, Errno> {
        if len == 0 {
            return Ok(0);
        }
        let to_was_empty = to.is_empty();
        let mut bytes_transferred = 0;
        for message in from.messages.peek_queue().iter() {
            let limit = std::cmp::min(len - bytes_transferred, to.messages.available_capacity());
            if limit == 0 {
                break;
            }
            let message = message.clone_at_most(limit);
            bytes_transferred += message.len();
            to.messages.write_message(message);
        }
        if bytes_transferred > 0 && to_was_empty {
            to.notify_fd_events(FdEvents::POLLIN);
        }
        return Ok(bytes_transferred);
    }
}

/// Creates a new pipe between the two returned FileObjects.
///
/// The first FileObject is the read endpoint of the pipe. The second is the
/// write endpoint of the pipe. This order matches the order expected by
/// sys_pipe2().
pub fn new_pipe(current_task: &CurrentTask) -> Result<(FileHandle, FileHandle), Errno> {
    let fs = current_task
        .kernel()
        .expando
        .get::<FsRegistry>()
        .create(current_task, "pipefs".into(), FileSystemOptions::default())
        .ok_or_else(|| errno!(EINVAL))??;
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
    Ok(kernel
        .expando
        .get_or_init(|| {
            PipeFsHandle(
                FileSystem::new(kernel, CacheMode::Uncached, PipeFs, FileSystemOptions::default())
                    .expect("pipefs constructed with valid options"),
            )
        })
        .0
        .clone())
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
            let mut pipe = self.pipe.lock();
            let was_writable = pipe.is_writable(uapi::PIPE_BUF as usize);
            let actual = pipe.read(data)?;
            if !was_writable && pipe.is_writable(uapi::PIPE_BUF as usize) {
                pipe.notify_fd_events(FdEvents::POLLOUT);
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

        let result = file.blocking_op(current_task, FdEvents::POLLOUT, None, || {
            let mut pipe = self.pipe.lock();
            let offset_before = data.bytes_read();
            let bytes_written = match pipe.write(data) {
                Err(e) if e == EPIPE => {
                    std::mem::drop(pipe);
                    send_standard_signal(current_task, SignalInfo::kernel(SIGPIPE));
                    return Err(e);
                }
                result => result?,
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
        Some(self.pipe.lock().waiters.wait_async_fd_events(waiter, events, handler))
    }

    fn query_events(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        Ok(self.pipe.lock().query_events(file.flags()))
    }

    fn fcntl(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        cmd: u32,
        arg: u64,
    ) -> Result<SyscallResult, Errno> {
        self.pipe.lock().fcntl(file, current_task, cmd, arg)
    }

    fn ioctl(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        self.pipe.lock().ioctl(file, current_task, request, arg)
    }
}

/// An OutputBuffer that will write the data to `pipe`.
#[derive(Debug)]
struct SpliceOutputBuffer<'a> {
    pipe: &'a mut Pipe,
    len: usize,
    available: usize,
}

impl<'a> Buffer for SpliceOutputBuffer<'a> {
    fn segments_count(&self) -> Result<usize, Errno> {
        error!(ENOTSUP)
    }

    fn peek_each_segment(
        &mut self,
        _callback: &mut PeekBufferSegmentsCallback<'_>,
    ) -> Result<(), Errno> {
        error!(ENOTSUP)
    }
}

impl<'a> OutputBuffer for SpliceOutputBuffer<'a> {
    fn write_each(&mut self, callback: &mut OutputBufferCallback<'_>) -> Result<usize, Errno> {
        // SAFETY: `callback` returns the number of bytes read on success.
        let bytes = unsafe {
            read_to_vec::<u8, _>(self.available, |buf| callback(buf).map(NumberOfElementsRead))
        }?;
        let bytes_len = bytes.len();
        if bytes_len > 0 {
            let was_empty = self.pipe.is_empty();
            self.pipe.messages.write_message(PipeMessageData::from(bytes).into());
            if was_empty {
                self.pipe.notify_fd_events(FdEvents::POLLIN);
            }
            self.available -= bytes_len;
        }
        Ok(bytes_len)
    }

    fn available(&self) -> usize {
        self.available
    }

    fn bytes_written(&self) -> usize {
        self.len - self.available
    }

    fn zero(&mut self) -> Result<usize, Errno> {
        let bytes = vec![0; self.available];
        let len = bytes.len();
        if len > 0 {
            let was_empty = self.pipe.is_empty();
            self.pipe.messages.write_message(PipeMessageData::from(bytes).into());
            if was_empty {
                self.pipe.notify_fd_events(FdEvents::POLLIN);
            }
            self.available -= len;
        }
        Ok(len)
    }

    unsafe fn advance(&mut self, _length: usize) -> Result<(), Errno> {
        error!(ENOTSUP)
    }
}

/// An InputBuffer that will read the data from `pipe`.
#[derive(Debug)]
struct SpliceInputBuffer<'a> {
    pipe: &'a mut Pipe,
    len: usize,
    available: usize,
}

impl<'a> Buffer for SpliceInputBuffer<'a> {
    fn segments_count(&self) -> Result<usize, Errno> {
        Ok(self.pipe.messages.len())
    }

    fn peek_each_segment(
        &mut self,
        callback: &mut PeekBufferSegmentsCallback<'_>,
    ) -> Result<(), Errno> {
        let mut available = self.available;
        for message in self.pipe.messages.messages() {
            let to_read = std::cmp::min(available, message.len());
            callback(&UserBuffer {
                address: UserAddress::from(message.data.ptr()? as u64),
                length: to_read,
            });
            available -= to_read;
        }
        Ok(())
    }
}

impl<'a> InputBuffer for SpliceInputBuffer<'a> {
    fn peek_each(&mut self, callback: &mut InputBufferCallback<'_>) -> Result<usize, Errno> {
        let mut read = 0;
        let mut available = self.available;
        for message in self.pipe.messages.messages() {
            let to_read = std::cmp::min(available, message.len());
            let result = message.data.with_bytes(|bytes| callback(&bytes[0..to_read]))?;
            if result > to_read {
                return error!(EINVAL);
            }
            read += result;
            available -= result;
            if result != to_read {
                break;
            }
        }
        Ok(read)
    }

    fn available(&self) -> usize {
        self.available
    }

    fn bytes_read(&self) -> usize {
        self.len - self.available
    }

    fn drain(&mut self) -> usize {
        let result = self.available;
        self.available = 0;
        result
    }

    fn advance(&mut self, mut length: usize) -> Result<(), Errno> {
        if length == 0 {
            return Ok(());
        }
        if length > self.available {
            return error!(EINVAL);
        }
        self.available -= length;
        while let Some(mut message) = self.pipe.messages.read_message() {
            if let Some(data) = MessageData::split_off(&mut message.data, length) {
                // Some data is left in the message. Push it back.
                self.pipe.messages.write_front(data.into());
            }
            length -= message.len();
            if length == 0 {
                if self.pipe.is_empty() {
                    self.pipe.notify_fd_events(FdEvents::POLLOUT);
                }
                return Ok(());
            }
        }
        panic!();
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

    /// Returns the result of `pregen` and a lock on pipe, once `condition` returns true, ensuring
    /// `pregen` is run before the pipe is locked.
    ///
    /// This will wait on `events` if the file is opened in blocking mode. If the file is opened in
    /// not blocking mode and `condition` is not realized, this will return EAGAIN.
    fn wait_for_condition<'a, F, G, V>(
        &'a self,
        current_task: &CurrentTask,
        file: &FileHandle,
        condition: F,
        pregen: G,
        events: FdEvents,
    ) -> Result<(V, MutexGuard<'a, Pipe>), Errno>
    where
        F: Fn(&Pipe) -> bool,
        G: Fn() -> Result<V, Errno>,
    {
        file.blocking_op(current_task, events, None, || {
            let other = pregen()?;
            let pipe = self.pipe.lock();
            if condition(&pipe) { Ok((other, pipe)) } else { error!(EAGAIN) }
        })
    }

    /// Lock the pipe for reading, after having run `pregen`.
    fn lock_pipe_for_reading_with<'a, G, V>(
        &'a self,
        current_task: &CurrentTask,
        file: &FileHandle,
        pregen: G,
        non_blocking: bool,
    ) -> Result<(V, MutexGuard<'a, Pipe>), Errno>
    where
        G: Fn() -> Result<V, Errno>,
    {
        if non_blocking {
            let other = pregen()?;
            let pipe = self.pipe.lock();
            if !pipe.is_readable() {
                return error!(EAGAIN);
            }
            Ok((other, pipe))
        } else {
            self.wait_for_condition(
                current_task,
                file,
                |pipe| pipe.is_readable(),
                pregen,
                FdEvents::POLLIN | FdEvents::POLLHUP,
            )
        }
    }

    fn lock_pipe_for_reading<'a>(
        &'a self,
        current_task: &CurrentTask,
        file: &FileHandle,
        non_blocking: bool,
    ) -> Result<MutexGuard<'a, Pipe>, Errno> {
        self.lock_pipe_for_reading_with(current_task, file, || Ok(()), non_blocking).map(|(_, l)| l)
    }

    /// Lock the pipe for writing, after having run `pregen`.
    fn lock_pipe_for_writing_with<'a, G, V>(
        &'a self,
        current_task: &CurrentTask,
        file: &FileHandle,
        pregen: G,
        non_blocking: bool,
        len: usize,
    ) -> Result<(V, MutexGuard<'a, Pipe>), Errno>
    where
        G: Fn() -> Result<V, Errno>,
    {
        let (other, pipe) = if non_blocking {
            let other = pregen()?;
            let pipe = self.pipe.lock();
            if !pipe.is_writable(len) {
                return error!(EAGAIN);
            }
            (other, pipe)
        } else {
            self.wait_for_condition(
                current_task,
                file,
                |pipe| pipe.is_writable(len),
                pregen,
                FdEvents::POLLOUT,
            )?
        };
        if pipe.reader_count == 0 {
            std::mem::drop((other, pipe));
            send_standard_signal(current_task, SignalInfo::kernel(SIGPIPE));
            return error!(EPIPE);
        }
        Ok((other, pipe))
    }

    fn lock_pipe_for_writing<'a>(
        &'a self,
        current_task: &CurrentTask,
        file: &FileHandle,
        non_blocking: bool,
        len: usize,
    ) -> Result<MutexGuard<'a, Pipe>, Errno> {
        self.lock_pipe_for_writing_with(current_task, file, || Ok(()), non_blocking, len)
            .map(|(_, l)| l)
    }

    /// Splice from the given file handle to this pipe.
    ///
    /// The given file handle must not be a pipe. If you wish to splice between two pipes, use
    /// `lock_pipes` and `Pipe::splice`.
    pub fn splice_from(
        &self,
        current_task: &CurrentTask,
        self_file: &FileHandle,
        from: &FileHandle,
        maybe_offset: Option<usize>,
        len: usize,
        non_blocking: bool,
    ) -> Result<usize, Errno> {
        // If both ends are pipes, use `lock_pipes` and `Pipe::splice`.
        assert!(from.downcast_file::<PipeFileObject>().is_none());

        let mut pipe = self.lock_pipe_for_writing(current_task, self_file, non_blocking, len)?;
        let len = std::cmp::min(len, pipe.messages.available_capacity());
        let mut buffer = SpliceOutputBuffer { pipe: &mut pipe, len, available: len };
        let result = if let Some(offset) = maybe_offset {
            from.read_at(current_task, offset, &mut buffer)
        } else {
            from.read(current_task, &mut buffer)
        };
        std::mem::drop(pipe);
        if let Err(err) = &result
            && *err == EPIPE
        {
            send_standard_signal(current_task, SignalInfo::kernel(SIGPIPE));
        }
        result
    }

    /// Splice from this pipe to the given file handle.
    ///
    /// The given file handle must not be a pipe. If you wish to splice between two pipes, use
    /// `lock_pipes` and `Pipe::splice`.
    pub fn splice_to(
        &self,
        current_task: &CurrentTask,
        self_file: &FileHandle,
        to: &FileHandle,
        maybe_offset: Option<usize>,
        len: usize,
        non_blocking: bool,
    ) -> Result<usize, Errno> {
        // If both ends are pipes, use `lock_pipes` and `Pipe::splice`.
        assert!(to.downcast_file::<PipeFileObject>().is_none());

        let mut pipe = self.lock_pipe_for_reading(current_task, self_file, non_blocking)?;
        let len = std::cmp::min(len, pipe.messages.len());
        let mut buffer = SpliceInputBuffer { pipe: &mut pipe, len, available: len };
        if let Some(offset) = maybe_offset {
            to.write_at(current_task, offset, &mut buffer)
        } else {
            to.write(current_task, &mut buffer)
        }
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
        let mappings = current_task.mm()?.get_mappings_for_vmsplice(&iovec)?;

        let mut pipe =
            self.lock_pipe_for_writing(current_task, self_file, non_blocking, available)?;

        let mut remaining = std::cmp::min(available, pipe.messages.available_capacity());

        let mut bytes_transferred = 0;
        for mut mapping in mappings.into_iter() {
            mapping.truncate(remaining);
            let actual = mapping.len();

            pipe.messages.write_message(PipeMessageData::Vmspliced(mapping).into());
            remaining -= actual;
            bytes_transferred += actual;

            if remaining == 0 {
                break;
            }
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
        let mut pipe = self.lock_pipe_for_reading(current_task, self_file, non_blocking)?;
        let was_writable = pipe.is_writable(uapi::PIPE_BUF as usize);
        let actual = pipe.read(&mut data)?;
        if !was_writable && pipe.is_writable(uapi::PIPE_BUF as usize) {
            pipe.notify_fd_events(FdEvents::POLLOUT);
        }
        Ok(actual)
    }

    /// Obtain the pipe objects from the given file handles, if they are both pipes.
    ///
    /// Returns EINVAL if one (or both) of the given file handles is not a pipe.
    ///
    /// Obtains the locks on the pipes in the correct order to avoid deadlocks.
    pub fn lock_pipes<'a, 'b>(
        current_task: &CurrentTask,
        file_in: &'a FileHandle,
        file_out: &'b FileHandle,
        len: usize,
        non_blocking: bool,
    ) -> Result<PipeOperands<'a, 'b>, Errno> {
        let pipe_in = file_in.downcast_file::<PipeFileObject>().ok_or_else(|| errno!(EINVAL))?;
        let pipe_out = file_out.downcast_file::<PipeFileObject>().ok_or_else(|| errno!(EINVAL))?;

        let node_cmp =
            Arc::as_ptr(&file_in.name.entry.node).cmp(&Arc::as_ptr(&file_out.name.entry.node));

        match node_cmp {
            Ordering::Equal => error!(EINVAL),
            Ordering::Less => {
                let (write, read) = pipe_in.lock_pipe_for_reading_with(
                    current_task,
                    file_in,
                    || pipe_out.lock_pipe_for_writing(current_task, file_out, non_blocking, len),
                    non_blocking,
                )?;
                Ok(PipeOperands { read, write })
            }
            Ordering::Greater => {
                let (read, write) = pipe_out.lock_pipe_for_writing_with(
                    current_task,
                    file_out,
                    || pipe_in.lock_pipe_for_reading(current_task, file_in, non_blocking),
                    non_blocking,
                    len,
                )?;
                Ok(PipeOperands { read, write })
            }
        }
    }
}

pub struct PipeOperands<'a, 'b> {
    pub read: MutexGuard<'a, Pipe>,
    pub write: MutexGuard<'b, Pipe>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::dynamic_thread_spawner::SpawnRequestBuilder;
    use crate::testing::{anon_test_file, spawn_kernel_and_run};
    use crate::vfs::buffers::{VecInputBuffer, VecOutputBuffer};

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
}
