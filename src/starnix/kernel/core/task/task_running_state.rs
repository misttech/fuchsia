// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mm::MemoryManager;
use crate::task::{AbstractUnixSocketNamespace, AbstractVsockSocketNamespace};
use crate::vfs::{FdTable, FsContext, FsNodeHandle};
use fuchsia_rcu::{RcuArc, RcuOptionArc, RcuOptionBox};
use starnix_sync::RwLock;
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use std::ops::Deref;
use std::sync::Arc;

/// The running state of a task.
///
/// This structure contains the state of a task that is only relevant while the task is running. It
/// is dropped when the task enters an exited state.
pub struct TaskRunningState {
    /// A handle to the underlying Zircon thread object.
    ///
    /// Some tasks lack an underlying Zircon thread. These tasks are used internally by the
    /// Starnix kernel to track background work, typically on a `kthread`.
    pub thread: RwLock<ZirconThread>,

    /// The file descriptor table for this task.
    ///
    /// This table can be share by many tasks.
    pub files: FdTable,

    /// The memory manager for this task.  This is `None` only for system tasks.
    pub mm: RcuOptionArc<MemoryManager>,

    /// The file system for this task.
    pub fs: RcuArc<FsContext>,

    /// The namespace for abstract AF_UNIX sockets for this task.
    pub abstract_socket_namespace: Arc<AbstractUnixSocketNamespace>,

    /// The namespace for AF_VSOCK for this task.
    pub abstract_vsock_namespace: Arc<AbstractVsockSocketNamespace>,

    /// The pid directory, so it doesn't have to be generated and thrown away on every access.
    /// See https://fxbug.dev/291962828 for details.
    pub proc_pid_directory_cache: RcuOptionBox<FsNodeHandle>,
}

impl TaskRunningState {
    pub fn mm(&self) -> Result<Arc<MemoryManager>, Errno> {
        self.mm.to_option_arc().ok_or_else(|| errno!(EINVAL))
    }

    pub fn fs(&self) -> Arc<FsContext> {
        self.fs.to_arc()
    }
}

/// A synchronized container for an optional Zircon thread and its cached KOID.
#[derive(Debug)]
pub struct ZirconThread {
    thread: Option<Arc<zx::Thread>>,
    koid: Option<zx::Koid>,
}

impl ZirconThread {
    pub fn new(thread: Option<Arc<zx::Thread>>) -> Self {
        let koid = thread.as_ref().and_then(|t| t.koid().ok());
        Self { thread, koid }
    }

    pub fn set(&mut self, thread: Arc<zx::Thread>) {
        self.koid = thread.koid().ok();
        self.thread = Some(thread);
    }

    pub fn koid(&self) -> Option<zx::Koid> {
        self.koid
    }
}

impl Deref for ZirconThread {
    type Target = Option<Arc<zx::Thread>>;
    fn deref(&self) -> &Self::Target {
        &self.thread
    }
}
