// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::tracing_directory::TraceMarkerFile;
use starnix_core::perf::{TraceEventQueue, TraceEventQueueList};
use starnix_core::task::CurrentTask;
use starnix_core::vfs::pseudo::dynamic_file::ConstFile;
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectory;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps, SimpleFileNode};
use starnix_core::vfs::{
    CacheMode, FileObject, FileOps, FileSystem, FileSystemHandle, FileSystemOps, FileSystemOptions,
    FsNodeOps, FsStr, OutputBuffer,
};
use starnix_core::{fileops_impl_nonseekable, fileops_impl_noop_sync};
use starnix_logging::{CATEGORY_TRACE_META, track_stub};
use starnix_sync::{LockDepMutex, TraceFsDataLock};
use starnix_types::PAGE_SIZE;
use starnix_types::vfs::default_statfs;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::{TRACEFS_MAGIC, errno, error, statfs};
use std::borrow::Cow;
use std::sync::{Arc, LazyLock};

pub fn trace_fs(
    current_task: &CurrentTask,
    options: FileSystemOptions,
) -> Result<FileSystemHandle, Errno> {
    struct TraceFsHandle(FileSystemHandle);

    let handle = current_task.kernel().expando.get_or_init(|| {
        TraceFsHandle(
            TraceFs::new_fs(current_task, options).expect("tracefs constructed with valid options"),
        )
    });
    Ok(handle.0.clone())
}

pub struct TraceFs;

impl FileSystemOps for TraceFs {
    fn statfs(&self, _fs: &FileSystem, _current_task: &CurrentTask) -> Result<statfs, Errno> {
        Ok(default_statfs(TRACEFS_MAGIC))
    }

    fn name(&self) -> &'static FsStr {
        "tracefs".into()
    }

    fn manages_timestamps(&self) -> bool {
        true
    }
}

impl TraceFs {
    pub fn new_fs(
        current_task: &CurrentTask,
        options: FileSystemOptions,
    ) -> Result<FileSystemHandle, Errno> {
        let kernel = current_task.kernel();
        let event_queue_collection = TraceEventQueueList::from(kernel);
        let fs = FileSystem::new(kernel, CacheMode::Uncached, TraceFs, options)?;
        let dir = SimpleDirectory::new();
        dir.edit(&fs, |dir| {
            dir.entry("trace", TraceFile::new_node(), mode!(IFREG, 0o755));
            dir.subdir("per_cpu", 0o755, |dir| {
                /// A name for each cpu directory, cached to provide a 'static lifetime.
                static CPU_DIR_NAMES: LazyLock<Vec<String>> = LazyLock::new(|| {
                    (0..zx::system_get_num_cpus()).map(|cpu| format!("cpu{}", cpu)).collect()
                });
                for (cpu, dir_name) in CPU_DIR_NAMES.iter().map(|s| s.as_str()).enumerate() {
                    dir.subdir(dir_name, 0o755, |dir| {
                        let ops: Box<dyn FsNodeOps> = Box::new(TraceRawFile::new_node(
                            event_queue_collection.queues[cpu].clone(),
                        ));
                        dir.entry("trace_pipe_raw", ops, mode!(IFREG, 0o444));
                        dir.entry("trace", TraceFile::new_node(), mode!(IFREG, 0o444));
                    });
                }
            });
            dir.subdir("events", 0o755, |dir| {
                dir.entry("header_page", EventsHeaderPage::new_node(), mode!(IFREG, 0o444));
                dir.subdir("ftrace", 0o755, |dir| {
                    dir.subdir("print", 0o755, |dir| {
                        dir.entry("format", FtracePrintFormatFile::new_node(), mode!(IFREG, 0o444));
                    });
                    if kernel.features.selinux_test_suite {
                        // Necessary for the perf_event SELinux testsuite case.
                        // See https://fxbug.dev/398663320.
                        dir.subdir("function", 0o755, |dir| {
                            dir.entry(
                                "id",
                                BytesFile::new_node(b"1\n".to_vec()),
                                mode!(IFREG, 0o444),
                            );
                        });
                    }
                });
                dir.entry("enable", TraceBytesFile::new_node(), mode!(IFREG, 0o755));
            });
            dir.subdir("options", 0o755, |dir| {
                dir.entry("overwrite", TraceBytesFile::new_node(), mode!(IFREG, 0o444));
            });
            dir.entry(
                "tracing_on",
                TracingOnFile::new_node(event_queue_collection.clone()),
                mode!(IFREG, 0o666),
            );
            dir.entry("current_tracer", ConstFile::new_node("nop".into()), mode!(IFREG, 0o755));
            dir.entry(
                "trace_marker",
                TraceMarkerFile::new_node(event_queue_collection.clone()),
                mode!(IFREG, 0o222),
            );
            dir.entry("printk_formats", TraceBytesFile::new_node(), mode!(IFREG, 0o755));
            dir.entry(
                "trace_clock",
                ConstFile::new_node(
                    "[local] global counter uptime perf mono mono_raw boot tai x86-tsc ".into(),
                ),
                mode!(IFREG, 0o755),
            );
            dir.entry(
                "buffer_size_kb",
                BufferSizeKbFile::new_node(event_queue_collection.clone()),
                mode!(IFREG, 0o666),
            );
        });

        let root_ino = fs.allocate_ino();
        fs.create_root(root_ino, dir);
        Ok(fs)
    }
}

#[derive(Clone)]
struct BufferSizeKbFile {
    event_queue_collection: Arc<TraceEventQueueList>,
}

impl BufferSizeKbFile {
    pub fn new_node(event_queue_collection: Arc<TraceEventQueueList>) -> impl FsNodeOps {
        BytesFile::new_node(Self { event_queue_collection })
    }
}

impl BytesFileOps for BufferSizeKbFile {
    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let text = std::str::from_utf8(&data).map_err(|_| errno!(EINVAL))?;
        let trimmed = text.trim();
        let size_kb: usize = trimmed.parse().map_err(|_| errno!(EINVAL))?;
        self.event_queue_collection.set_buffer_size_kb(size_kb)?;
        Ok(())
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let size_kb = self.event_queue_collection.buffer_size_kb();
        let s = format!("{}\n", size_kb);
        Ok(Cow::Owned(s.into_bytes()))
    }
}

#[derive(Default)]
struct TraceBytesFile {
    data: LockDepMutex<Vec<u8>, TraceFsDataLock>,
}

impl TraceBytesFile {
    #[track_caller]
    fn new_node() -> impl FsNodeOps {
        SimpleFileNode::new(move |_| Ok(BytesFile::new(Self::default())))
    }
}

impl BytesFileOps for TraceBytesFile {
    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        *self.data.lock() = data;
        Ok(())
    }
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let data = self.data.lock().clone();
        Ok(data.into())
    }
}

#[derive(Clone)]
struct TraceRawFile {
    queue: Arc<TraceEventQueue>,
}

impl TraceRawFile {
    pub fn new_node(queue: Arc<TraceEventQueue>) -> impl FsNodeOps {
        SimpleFileNode::new(move |_| Ok(Self { queue: queue.clone() }))
    }
}

impl FileOps for TraceRawFile {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        // TODO(b/505532201): Consider supporting larger buffers.
        if data.available() as u64 != *PAGE_SIZE {
            return error!(EINVAL);
        }
        let _guard = fuchsia_trace::async_enter!(
            self.queue.async_id_read,
            CATEGORY_TRACE_META,
            self.queue.read_track_name(),
            "tid" => current_task.tid.id
        );
        self.queue.read(data)
    }

    fn write(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn starnix_core::vfs::InputBuffer,
    ) -> Result<usize, Errno> {
        error!(ENOSYS)
    }
}

#[derive(Clone)]
struct TracingOnFile {
    event_queue_collection: Arc<TraceEventQueueList>,
}

impl TracingOnFile {
    pub fn new_node(event_queue_collection: Arc<TraceEventQueueList>) -> impl FsNodeOps {
        BytesFile::new_node(Self { event_queue_collection })
    }
}

impl BytesFileOps for TracingOnFile {
    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let state_str = std::str::from_utf8(&data).map_err(|_| errno!(EINVAL))?;
        let clean_state_str = state_str.split('\n').next().unwrap_or("");
        match clean_state_str {
            "0" => self.event_queue_collection.disable(),
            "1" => self.event_queue_collection.enable(),
            _ => error!(EINVAL),
        }
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        if self.event_queue_collection.is_enabled() {
            Ok(Cow::Borrowed(&b"1\n"[..]))
        } else {
            Ok(Cow::Borrowed(&b"0\n"[..]))
        }
    }
}

#[derive(Clone)]
struct EventsHeaderPage;
impl EventsHeaderPage {
    fn new_node() -> impl FsNodeOps {
        ConstFile::new_node(
            format!(
                "\tfield: u64 timestamp;\toffset:0;\tsize:8;\tsigned:0\n\
            \tfield: local_t commit;\toffset:8;\tsize:8;\tsigned:1\n\
            \tfield: int overwrite;\toffset:8;\tsize:1;\tsigned:1\n\
            \tfield: char data;\toffset:16;\tsize:{};\tsigned:0\n",
                *PAGE_SIZE - 16
            )
            .into_bytes(),
        )
    }
}

#[derive(Clone)]
struct FtracePrintFormatFile;
impl FtracePrintFormatFile {
    fn new_node() -> impl FsNodeOps {
        ConstFile::new_node(
            "name: print\n\
        ID: 5\n\
        format:\n\
        \tfield:unsigned short common_type;\toffset:0;\tsize:2;\tsigned:0;\n\
        \tfield:unsigned char common_flags;\toffset:2;\tsize:1;\tsigned:0;\n\
        \tfield:unsigned char common_preempt_count;\toffset:3;\tsize:1;\tsigned:0;\n\
        \tfield:int common_pid;\toffset:4;\tsize:4;\tsigned:1;\n\
        \n\
        \tfield:unsigned long ip;\toffset:8;\tsize:8;\tsigned:0;\n\
        \tfield:char buf[];\toffset:16;\tsize:0;\tsigned:0;\n\
        \n\
        print fmt: \"%ps: %s\", (void *)REC->ip, REC- >buf\n"
                .into(),
        )
    }
}

#[derive(Clone)]
struct TraceFile;
impl TraceFile {
    fn new_node() -> impl FsNodeOps {
        track_stub!(TODO("https://fxbug.dev/357665908"), "/sys/kernel/tracing/trace");
        ConstFile::new_node("# tracer: nop\n".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_core::vfs::buffers::{InputBuffer, VecInputBuffer, VecOutputBuffer};
    use starnix_core::vfs::{FsNodeInfo, NamespaceNode, OpenAccessCheck};
    use starnix_uapi::auth::FsCred;
    use starnix_uapi::file_mode::mode;
    use starnix_uapi::open_flags::OpenFlags;

    #[::fuchsia::test]
    async fn test_events_header_page_size() {
        spawn_kernel_and_run(async move |current_task| {
            let fs = current_task.fs().root().entry.node.fs();
            let info = FsNodeInfo::new(mode!(IFREG, 0o444), FsCred::root());
            let node = fs.create_node_and_allocate_node_id(EventsHeaderPage::new_node(), info);
            let ns_node = NamespaceNode::new_anonymous_unrooted(current_task, node);
            let file = ns_node
                .open(current_task, OpenAccessCheck::skip(OpenFlags::RDONLY))
                .expect("open header_page node");

            let mut buffer = VecOutputBuffer::new(*PAGE_SIZE as usize);
            let bytes_read = file.read(current_task, &mut buffer).expect("read header_page");
            let content = std::str::from_utf8(&buffer.data()[..bytes_read]).expect("valid utf8");

            let expected_content = format!(
                "\tfield: u64 timestamp;\toffset:0;\tsize:8;\tsigned:0\n\
            \tfield: local_t commit;\toffset:8;\tsize:8;\tsigned:1\n\
            \tfield: int overwrite;\toffset:8;\tsize:1;\tsigned:1\n\
            \tfield: char data;\toffset:16;\tsize:{};\tsigned:0\n",
                *PAGE_SIZE - 16
            );
            assert_eq!(
                content, expected_content,
                "Expected header_page to match exactly:\n{}\nGot:\n{}",
                expected_content, content
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_trace_marker_write_small_and_large_and_disabled() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let event_queue_collection = TraceEventQueueList::from(kernel);
            let fs = TraceFs::new_fs(current_task, FileSystemOptions::default())
                .expect("create tracefs");
            assert!(fs.manages_timestamps());
            let info = FsNodeInfo::new(mode!(IFREG, 0o222), FsCred::root());
            let node = fs.create_node_and_allocate_node_id(
                TraceMarkerFile::new_node(event_queue_collection.clone()),
                info,
            );
            let ns_node = NamespaceNode::new_anonymous_unrooted(current_task, node);
            let file = ns_node
                .open(current_task, OpenAccessCheck::skip(OpenFlags::WRONLY))
                .expect("open trace_marker node");
            let mtime_before = file.node().info().time_modify;

            // 1. When disabled, write should drain the input buffer and return the full byte count.
            event_queue_collection.disable().expect("disable queue");
            let mut disabled_buf = VecInputBuffer::new(b"B|1234|disabled_slice\n");
            let written = file.write(current_task, &mut disabled_buf).expect("write disabled");
            assert_eq!(written, b"B|1234|disabled_slice\n".len());
            assert_eq!(disabled_buf.available(), 0);
            assert_eq!(file.node().info().time_modify, mtime_before);

            // 2. When enabled, small writes (<= 128 bytes) use the inline SmallVec buffer,
            // and large writes (> 128 bytes) spill to the heap. Write enough events across
            // the per-CPU queues to overflow a full page on queue 0 so we can read it back.
            event_queue_collection.enable().expect("enable queue");
            let num_cpus = event_queue_collection.queues.len();
            for _ in 0..num_cpus {
                let mut small_buf = VecInputBuffer::new(b"B|1234|activityStart\n");
                let written = file.write(current_task, &mut small_buf).expect("write small");
                assert_eq!(written, b"B|1234|activityStart\n".len());
                assert_eq!(small_buf.available(), 0);
            }
            let large_payload = vec![b'x'; 768];
            for _ in 0..(6 * num_cpus) {
                let mut large_buf = VecInputBuffer::new(&large_payload);
                let written = file.write(current_task, &mut large_buf).expect("write large");
                assert_eq!(written, large_payload.len());
                assert_eq!(large_buf.available(), 0);
            }
            assert_eq!(file.node().info().time_modify, mtime_before);

            let mut read_buf = VecOutputBuffer::new(*PAGE_SIZE as usize);
            let bytes_read = event_queue_collection.queues[0]
                .read(&mut read_buf)
                .expect("read full page from queue 0");
            assert_eq!(bytes_read, *PAGE_SIZE as usize);
            let page_data = read_buf.data();
            assert!(
                page_data
                    .windows(b"B|1234|activityStart\n".len())
                    .any(|w| w == b"B|1234|activityStart\n")
            );
            assert!(page_data.windows(large_payload.len()).any(|w| w == large_payload.as_slice()));
            event_queue_collection.disable().expect("disable queue after test");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_buffer_size_kb_read_write() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let event_queue_collection = TraceEventQueueList::from(kernel);
            let fs = current_task.fs().root().entry.node.fs();
            let info = FsNodeInfo::new(mode!(IFREG, 0o666), FsCred::root());
            let node = fs.create_node_and_allocate_node_id(
                BufferSizeKbFile::new_node(event_queue_collection.clone()),
                info,
            );
            let ns_node = NamespaceNode::new_anonymous_unrooted(current_task, node);
            let file = ns_node
                .open(current_task, OpenAccessCheck::skip(OpenFlags::RDWR))
                .expect("open buffer_size_kb node");

            let mut buffer = VecOutputBuffer::new(64);
            let bytes_read =
                file.read_at(current_task, 0, &mut buffer).expect("read buffer_size_kb");
            assert_eq!(&buffer.data()[..bytes_read], b"2048\n");

            let mut input = starnix_core::vfs::buffers::VecInputBuffer::new(b"4096\n");
            let bytes_written = file.write(current_task, &mut input).expect("write buffer_size_kb");
            assert_eq!(bytes_written, 5);

            let mut buffer2 = VecOutputBuffer::new(64);
            let bytes_read2 =
                file.read_at(current_task, 0, &mut buffer2).expect("read updated buffer_size_kb");
            assert_eq!(&buffer2.data()[..bytes_read2], b"4096\n");

            // Writing smaller than 3 pages clamps to 12 KB
            let mut input3 = starnix_core::vfs::buffers::VecInputBuffer::new(b"4\n");
            file.write(current_task, &mut input3).expect("write small buffer_size_kb");
            let mut buffer3 = VecOutputBuffer::new(64);
            let bytes_read3 =
                file.read_at(current_task, 0, &mut buffer3).expect("read clamped buffer_size_kb");
            assert_eq!(&buffer3.data()[..bytes_read3], b"12\n");

            // Writing 0 returns EINVAL
            let mut input_zero = starnix_core::vfs::buffers::VecInputBuffer::new(b"0\n");
            assert_eq!(file.write(current_task, &mut input_zero), error!(EINVAL));

            // Writing non-numeric returns EINVAL
            let mut input_bad = starnix_core::vfs::buffers::VecInputBuffer::new(b"invalid\n");
            assert_eq!(file.write(current_task, &mut input_bad), error!(EINVAL));

            // Writing while enabled also succeeds
            event_queue_collection.enable().expect("enable tracing");
            let mut input_enabled = starnix_core::vfs::buffers::VecInputBuffer::new(b"2048\n");
            let bytes_written_enabled =
                file.write(current_task, &mut input_enabled).expect("write enabled buffer_size_kb");
            assert_eq!(bytes_written_enabled, 5);
            let mut buffer_enabled = VecOutputBuffer::new(64);
            let bytes_read_enabled = file
                .read_at(current_task, 0, &mut buffer_enabled)
                .expect("read buffer_size_kb while enabled");
            assert_eq!(&buffer_enabled.data()[..bytes_read_enabled], b"2048\n");
            event_queue_collection.disable().expect("disable tracing");
        })
        .await;
    }
}
