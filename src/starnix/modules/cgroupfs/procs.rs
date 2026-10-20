// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of `cgroup.procs` file.
//!
//! Reading cgroup.procs produces all processes IDs that currently belong to the cgroup.
//! Writing a process ID to this file will move the process into this cgroup.
//!
//! Full details at https://docs.kernel.org/admin-guide/cgroup-v2.html#core-interface-files

use starnix_core::fs_node_impl_not_dir;
use starnix_core::task::{CgroupOps, CurrentTask, Kernel};
use starnix_core::vfs::pseudo::dynamic_file::{DynamicFile, DynamicFileBuf, DynamicFileSource};
use starnix_core::vfs::{AppendLockWriteGuard, FileOps, FsNode, FsNodeOps, InputBuffer};

use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{errno, pid_t};
use std::sync::{Arc, Weak};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlGroupFileKind {
    /// `cgroup.procs`: writing a PID or TID migrates the entire thread group (process).
    Procs,
    /// `tasks` (cgroup v1): writing a TID migrates that task (thread).
    Tasks,
}

pub struct ControlGroupNode {
    cgroup: Weak<dyn CgroupOps>,
    kind: ControlGroupFileKind,
}

impl ControlGroupNode {
    pub fn new(cgroup: Weak<dyn CgroupOps>) -> Self {
        ControlGroupNode { cgroup, kind: ControlGroupFileKind::Procs }
    }

    pub fn new_tasks(cgroup: Weak<dyn CgroupOps>) -> Self {
        ControlGroupNode { cgroup, kind: ControlGroupFileKind::Tasks }
    }
}

impl FsNodeOps for ControlGroupNode {
    fs_node_impl_not_dir!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(ControlGroupFile::new(current_task.kernel(), self.cgroup.clone(), self.kind)))
    }

    fn truncate(
        &self,
        _guard: &AppendLockWriteGuard<'_>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _length: u64,
    ) -> Result<(), Errno> {
        Ok(())
    }
}

impl DynamicFileSource for ControlGroupFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let cgroup = self.cgroup()?;
        for pid in cgroup.get_pids(self.kernel()?.as_ref()) {
            write!(sink, "{pid}\n")?;
        }
        Ok(())
    }

    fn write(
        &self,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let bytes = data.read_all()?;
        let pid_string = std::str::from_utf8(&bytes).map_err(|_| errno!(EINVAL))?;
        let pid = pid_string.trim().parse::<pid_t>().map_err(|_| errno!(EINVAL))?;

        let cgroup = self.cgroup()?;
        match self.kind {
            ControlGroupFileKind::Tasks => match pid {
                0 => cgroup.add_task(current_task)?,
                tid => {
                    let task = current_task.kernel().pids.get(tid)?.get_task()?;
                    cgroup.add_task(&task)?;
                }
            },
            ControlGroupFileKind::Procs => match pid {
                0 => cgroup.add_process(current_task.thread_group())?,
                pid => {
                    let pid_entry = current_task.kernel().pids.get(pid)?;
                    let thread_group = match pid_entry.get_thread_group() {
                        Ok(tg) => tg,
                        Err(_) => Arc::clone(pid_entry.get_task()?.thread_group()),
                    };
                    cgroup.add_process(&thread_group)?;
                }
            },
        }

        Ok(bytes.len())
    }
}

/// A `ControlGroupFile` represents either the `cgroup.procs` (`ControlGroupFileKind::Procs`) or
/// cgroup v1 `tasks` (`ControlGroupFileKind::Tasks`) interface file for a control group.
///
/// Writing a PID/TID (or `0` for the caller) to `cgroup.procs` moves the entire thread group into
/// the cgroup, whereas writing a TID (or `0` for the calling thread) to `tasks` invokes
/// `CgroupOps::add_task` for per-thread migration.
pub struct ControlGroupFile {
    kernel: Weak<Kernel>,
    cgroup: Weak<dyn CgroupOps>,
    kind: ControlGroupFileKind,
}

impl ControlGroupFile {
    fn new(
        kernel: &Kernel,
        cgroup: Weak<dyn CgroupOps>,
        kind: ControlGroupFileKind,
    ) -> impl FileOps {
        DynamicFile::new(Self { kernel: kernel.weak_self.clone(), cgroup: cgroup.clone(), kind })
    }

    fn kernel(&self) -> Result<Arc<Kernel>, Errno> {
        self.kernel.upgrade().ok_or_else(|| errno!(ENODEV))
    }

    fn cgroup(&self) -> Result<Arc<dyn CgroupOps>, Errno> {
        self.cgroup.upgrade().ok_or_else(|| errno!(ENODEV))
    }
}
