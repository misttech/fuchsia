// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use smallvec::SmallVec;
use starnix_core::fileops_impl_nonseekable;
use starnix_core::perf::{TraceEvent, TraceEventQueueList};
use starnix_core::task::{CurrentTask, complete_syscall_trace};
use starnix_core::vfs::buffers::InputBuffer;
use starnix_core::vfs::pseudo::simple_file::SimpleFileNode;
use starnix_core::vfs::{FileObject, FileOps, FsNodeOps, OutputBuffer, fileops_impl_noop_sync};
use starnix_logging::CATEGORY_TRACE_META;

use starnix_types::PAGE_SIZE;
use starnix_uapi::errors::Errno;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const TRACE_MARKER_STACK_BUF_SIZE: usize = 128;

pub struct TraceMarkerFile {
    event_queue_collection: Arc<TraceEventQueueList>,
    num_cpus: usize,
    counter: AtomicUsize,
}
impl TraceMarkerFile {
    pub fn new_node(event_queue_collection: Arc<TraceEventQueueList>) -> impl FsNodeOps {
        let num_cpus = event_queue_collection.queues.len();
        SimpleFileNode::new(move |_| {
            Ok(Self {
                event_queue_collection: event_queue_collection.clone(),
                num_cpus,
                counter: AtomicUsize::new(0),
            })
        })
    }
}

impl FileOps for TraceMarkerFile {
    fileops_impl_noop_sync!();
    fileops_impl_nonseekable!();

    fn read(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        Ok(0)
    }

    fn write(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        if !self.event_queue_collection.is_enabled() {
            return Ok(data.drain());
        }
        //TODO(b/502606269): Get the current CPU index from the current task.
        let cpu_index = self.counter.fetch_add(1, Ordering::Relaxed) % self.num_cpus;
        let queue = &self.event_queue_collection.queues[cpu_index];
        let _guard = fuchsia_trace::async_enter!(
            queue.async_id_write,
            CATEGORY_TRACE_META,
            queue.write_track_name(),
            "tid" => current_task.get_tid()
        );
        let mut buf = SmallVec::<[u8; TRACE_MARKER_STACK_BUF_SIZE]>::with_capacity(
            data.available().min(*PAGE_SIZE as usize),
        );
        let bytes_read = data.read_each(&mut |chunk| {
            buf.extend_from_slice(chunk);
            Ok(chunk.len())
        })?;
        // The TraceEvent struct appends a new line to the trace data unconditionally, so
        // remove the trailing newline if here to avoid generating empty events when reading.
        let bytes = buf.strip_suffix(b"\n").unwrap_or(&buf);
        let trace_event = TraceEvent::new(
            // This pid is a Kernel pid (do not confuse with userspace pid aka tgid), so we use
            // the task thread id, the pid and tid are equal when the thread is the "main thread"
            // of the thread group/process.
            // It is used when CPU scheduling information is not available.
            current_task.get_tid(),
            bytes.len(),
        );
        // End the syscall's `CATEGORY_STARNIX` duration (e.g. `sys_write`) before reserving the
        // `ftrace/print` event in the ring buffer, so the syscall slice ends at or before the
        // `atrace` event's timestamp on the thread track instead of truncating the `atrace` slice.
        // Perfetto does the same for Linux `ftrace` traces
        // (`SyscallTracker::MaybeTruncateOngoingWriteSlice`).
        complete_syscall_trace();
        queue.push_event(trace_event, bytes)?;
        Ok(bytes_read) // Includes '\n', if present
    }
}
