// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::{HidePid, ProcFs};
use itertools::Itertools;
use regex_lite::Regex;
use starnix_core::mm::{
    MemoryAccessor, MemoryAccessorExt, MemoryManager, MemoryStats, PAGE_SIZE, ProcMapsFile,
    ProcPagemapFile, ProcSmapsFile, ProcSmapsRollupFile,
};
use starnix_core::security;
use starnix_core::task::{
    CurrentTask, Pid, Task, TaskContainer, TaskEntryScope, TaskStateCode, path_from_root,
};
use starnix_core::vfs::buffers::{InputBuffer, OutputBuffer};
use starnix_core::vfs::pseudo::dynamic_file::{DynamicFile, DynamicFileBuf, DynamicFileSource};
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectory;
use starnix_core::vfs::pseudo::simple_file::{
    BytesFile, BytesFileOps, SimpleFileNode, parse_i32_file, parse_unsigned_file,
    serialize_for_file,
};
use starnix_core::vfs::pseudo::stub_empty_file::StubEmptyFile;
use starnix_core::vfs::pseudo::vec_directory::{VecDirectory, VecDirectoryEntry};
use starnix_core::vfs::{
    CallbackSymlinkNode, CheckAccessReason, CloseFreeSafe, DirEntry, DirEntryOps,
    DirectoryEntryType, DirentSink, FdNumber, FileObject, FileOps, FileSystemHandle, FsNode,
    FsNodeHandle, FsNodeInfo, FsNodeOps, FsStr, FsString, ProcMountinfoFile, ProcMountsFile,
    SeekTarget, StatxFlags, SymlinkTarget, default_seek, emit_dotdot, fileops_impl_directory,
    fileops_impl_noop_sync, fileops_impl_seekable, fileops_impl_unbounded_seek,
    fs_node_impl_dir_readonly, fs_node_impl_dir_readonly_ops,
};
use starnix_logging::{bug_ref, track_stub};
use starnix_sync::{DynamicLockDepRwLock, LockDepReadGuard};

use starnix_task_command::TaskCommand;
use starnix_types::time::duration_to_scheduler_clock;
use starnix_uapi::auth::{
    CAP_SYS_NICE, CAP_SYS_RESOURCE, PTRACE_MODE_ATTACH_FSCREDS, PTRACE_MODE_NOAUDIT,
    PTRACE_MODE_READ_FSCREDS, PtraceAccessMode,
};
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::{Access, FileMode, mode};
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::resource_limits::Resource;
use starnix_uapi::user_address::UserAddress;
use starnix_uapi::{
    OOM_ADJUST_MIN, OOM_DISABLE, OOM_SCORE_ADJ_MIN, RLIM_INFINITY, errno, error, ino_t, off_t,
    pid_t, uapi,
};
use std::borrow::Cow;
use std::ops::{Deref, Range};
use std::sync::{Arc, LazyLock, Weak};

/// Static table of entries in `/proc/<pid>` and `/proc/<pid>/task/<tid>`.
const TASK_ENTRIES: &[(&[u8], FileMode)] = &[
    // NOTE: keep entries in sync with `TaskDirectoryNode::lookup()`.
    (b"cgroup", mode!(IFREG, 0o444)),
    (b"cwd", mode!(IFLNK, 0o777)),
    (b"exe", mode!(IFLNK, 0o777)),
    (b"fd", mode!(IFDIR, 0o500)),
    (b"fdinfo", mode!(IFDIR, 0o555)),
    (b"io", mode!(IFREG, 0o400)),
    (b"limits", mode!(IFREG, 0o444)),
    (b"maps", mode!(IFREG, 0o444)),
    (b"mem", mode!(IFREG, 0o600)),
    (b"root", mode!(IFLNK, 0o777)),
    (b"sched", mode!(IFREG, 0o644)),
    (b"schedstat", mode!(IFREG, 0o444)),
    (b"smaps", mode!(IFREG, 0o444)),
    (b"smaps_rollup", mode!(IFREG, 0o444)),
    (b"stat", mode!(IFREG, 0o444)),
    (b"statm", mode!(IFREG, 0o444)),
    (b"status", mode!(IFREG, 0o444)),
    (b"cmdline", mode!(IFREG, 0o444)),
    (b"environ", mode!(IFREG, 0o400)),
    (b"auxv", mode!(IFREG, 0o400)),
    (b"comm", mode!(IFREG, 0o644)),
    (b"attr", mode!(IFDIR, 0o555)),
    (b"ns", mode!(IFDIR, 0o511)),
    (b"mountinfo", mode!(IFREG, 0o444)),
    (b"mounts", mode!(IFREG, 0o444)),
    (b"oom_adj", mode!(IFREG, 0o744)),
    (b"oom_score", mode!(IFREG, 0o444)),
    (b"oom_score_adj", mode!(IFREG, 0o744)),
    (b"timerslack_ns", mode!(IFREG, 0o666)),
    (b"wchan", mode!(IFREG, 0o444)),
    (b"clear_refs", mode!(IFREG, 0o200)),
    (b"pagemap", mode!(IFREG, 0o400)),
    // "task" must be last so we can dynamically include it for ThreadGroups.
    (b"task", mode!(IFDIR, 0o555)),
];

/// Returns entries for the `scope` of a task.
fn task_entries(scope: TaskEntryScope) -> &'static [(&'static [u8], FileMode)] {
    match scope {
        TaskEntryScope::Task => &TASK_ENTRIES[..TASK_ENTRIES.len() - 1],
        TaskEntryScope::ThreadGroup => TASK_ENTRIES,
    }
}

/// Represents a directory node for either `/proc/<pid>` or `/proc/<pid>/task/<tid>`.
///
/// This directory lazily creates its child entries to save memory.
///
/// It pre-allocates a range of inode numbers (`inode_range`) for all its child entries to mark
/// them as unchanged when re-accessed.
/// The `creds` stored within is applied to the directory node itself and child entries.
pub struct TaskDirectory {
    target: TaskContainer,
    inode_range: Range<ino_t>,
}

#[derive(Clone)]
struct TaskDirectoryNode {
    task_directory: Arc<TaskDirectory>,
}

impl Deref for TaskDirectoryNode {
    type Target = TaskDirectory;

    fn deref(&self) -> &Self::Target {
        &self.task_directory
    }
}

impl TaskDirectory {
    fn new(fs: &FileSystemHandle, task: &Arc<Task>, target: TaskContainer) -> FsNodeHandle {
        let creds = task.real_creds().euid_as_fscred();
        let scope = target.scope;
        fs.create_node_and_allocate_node_id(
            TaskDirectoryNode {
                task_directory: Arc::new(TaskDirectory {
                    target,
                    inode_range: fs.allocate_ino_range(task_entries(scope).len()),
                }),
            },
            FsNodeInfo::new(mode!(IFDIR, 0o555), creds),
        )
    }
}

struct TaskDirectoryDirEntryOps {
    target: TaskContainer,
}

impl DirEntryOps for TaskDirectoryDirEntryOps {
    fn revalidate(&self, current_task: &CurrentTask, dir_entry: &DirEntry) -> Result<bool, Errno> {
        let Ok(target_task) = self.target.get_task() else {
            return Ok(false);
        };
        if let Some(proc_fs) = dir_entry.node.fs().downcast_ops::<ProcFs>() {
            if !proc_fs.has_pid_permissions(current_task, &target_task, HidePid::Invisible) {
                return error!(ENOENT);
            }
        }
        Ok(true)
    }
}

macro_rules! task_dir_impl_readonly {
    () => {
        fn create_dir_entry_ops(&self) -> Box<dyn DirEntryOps> {
            Box::new(TaskDirectoryDirEntryOps { target: self.target.clone() })
        }

        fn check_access(
            &self,
            node: &FsNode,
            current_task: &CurrentTask,
            permission_flags: security::PermissionFlags,
            info: &DynamicLockDepRwLock<FsNodeInfo>,
            reason: CheckAccessReason,
            audit_context: security::Auditable<'_>,
        ) -> Result<(), Errno> {
            if let Some(proc_fs) = node.fs().downcast_ops::<ProcFs>() {
                proc_fs.check_pid_access(current_task, &self.target, HidePid::NoAccess)?;
            }
            if permission_flags.as_access().contains(Access::WRITE) {
                return error!(EROFS, format!("check_access failed: read-only directory"));
            }
            node.default_check_access_impl(
                current_task,
                permission_flags,
                reason,
                info.read(),
                audit_context,
            )
        }

        fn fetch_and_refresh_info<'a>(
            &self,
            node: &FsNode,
            current_task: &CurrentTask,
            info: &'a DynamicLockDepRwLock<FsNodeInfo>,
            _flags: StatxFlags,
        ) -> Result<LockDepReadGuard<'a, FsNodeInfo>, Errno> {
            if let (Some(proc_fs), Ok(target_task)) =
                (node.fs().downcast_ops::<ProcFs>(), self.target.get_task())
            {
                if !proc_fs.has_pid_permissions(current_task, &target_task, HidePid::Invisible) {
                    return error!(ENOENT);
                }
            }
            Ok(info.read())
        }

        fs_node_impl_dir_readonly_ops!();
    };
}

impl FsNodeOps for TaskDirectoryNode {
    task_dir_impl_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(self.clone()))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        _current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        let _task = self.target.get_task()?;
        let target = self.target.clone();
        let creds = entry.node.info().cred();
        let fs = entry.node.fs();
        let (mode, ino) = task_entries(self.target.scope)
            .iter()
            .enumerate()
            .find_map(|(index, (n, mode))| {
                if name == *n {
                    Some((*mode, self.inode_range.start + index as ino_t))
                } else {
                    None
                }
            })
            .ok_or_else(|| errno!(ENOENT))?;

        // NOTE: keep entries in sync with `task_entries()`.
        let ops: Box<dyn FsNodeOps> = match &**name {
            b"cgroup" => Box::new(CgroupFile::new_node(target)),
            b"cwd" => Box::new(CallbackSymlinkNode::new(move || {
                Ok(SymlinkTarget::Node(target.get_task()?.fs()?.cwd()))
            })),
            b"exe" => Box::new(CallbackSymlinkNode::new(move || {
                let task = target.get_task()?;
                if let Some(node) = task.mm().ok().and_then(|mm| mm.executable_node()) {
                    Ok(SymlinkTarget::Node(node))
                } else {
                    error!(ENOENT)
                }
            })),
            b"fd" => Box::new(FdDirectory::new(target)),
            b"fdinfo" => Box::new(FdInfoDirectory::new(target)),
            b"io" => Box::new(IoFile::new_node()),
            b"limits" => Box::new(LimitsFile::new_node(target)),
            b"maps" => {
                let target_for_file = target.clone();
                Box::new(PtraceCheckedNode::new_node(
                    target,
                    PTRACE_MODE_READ_FSCREDS,
                    move |task| Ok(ProcMapsFile::new(task, target_for_file.clone())),
                ))
            }
            b"mem" => Box::new(MemFile::new_node(target)),
            b"root" => Box::new(CallbackSymlinkNode::new(move || {
                Ok(SymlinkTarget::Node(target.get_task()?.fs()?.root()))
            })),
            b"sched" => Box::new(StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/322893980"))),
            b"schedstat" => {
                Box::new(StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/322894256")))
            }
            b"smaps" => {
                let target_for_file = target.clone();
                Box::new(PtraceCheckedNode::new_node(
                    target,
                    PTRACE_MODE_READ_FSCREDS,
                    move |task| Ok(ProcSmapsFile::new(task, target_for_file.clone())),
                ))
            }
            b"smaps_rollup" => {
                let target_for_file = target.clone();
                Box::new(PtraceCheckedNode::new_node(
                    target,
                    PTRACE_MODE_READ_FSCREDS,
                    move |task| Ok(ProcSmapsRollupFile::new(task, target_for_file.clone())),
                ))
            }
            b"stat" => Box::new(StatFile::new_node(target)),
            b"statm" => Box::new(StatmFile::new_node(target)),
            b"status" => Box::new(StatusFile::new_node(target)),
            b"cmdline" => Box::new(CmdlineFile::new_node(target)),
            b"environ" => Box::new(EnvironFile::new_node(target)),
            b"auxv" => Box::new(AuxvFile::new_node(target)),
            b"comm" => Box::new(CommFile::new_node(target)),
            b"attr" => {
                let dir = SimpleDirectory::new();
                dir.edit(&fs, |dir| {
                    for (attr, name) in [
                        (security::ProcAttr::Current, "current"),
                        (security::ProcAttr::Exec, "exec"),
                        (security::ProcAttr::FsCreate, "fscreate"),
                        (security::ProcAttr::KeyCreate, "keycreate"),
                        (security::ProcAttr::SockCreate, "sockcreate"),
                    ] {
                        dir.entry_etc(
                            name.into(),
                            AttrNode::new(target.clone(), attr),
                            mode!(IFREG, 0o666),
                            DeviceId::NONE,
                            creds,
                        );
                    }
                    dir.entry_etc(
                        "prev".into(),
                        AttrNode::new(target, security::ProcAttr::Previous),
                        mode!(IFREG, 0o444),
                        DeviceId::NONE,
                        creds,
                    );
                });
                Box::new(dir)
            }
            b"ns" => Box::new(NsDirectory::new(target)),
            b"mountinfo" => Box::new(ProcMountinfoFile::new_node(target)),
            b"mounts" => Box::new(ProcMountsFile::new_node(target)),
            b"oom_adj" => Box::new(OomAdjFile::new_node(target)),
            b"oom_score" => Box::new(OomScoreFile::new_node(target)),
            b"oom_score_adj" => Box::new(OomScoreAdjFile::new_node(target)),
            b"timerslack_ns" => Box::new(TimerslackNsFile::new_node(target)),
            b"wchan" => Box::new(BytesFile::new_node(b"0".to_vec())),
            b"clear_refs" => Box::new(ClearRefsFile::new_node(target)),
            b"pagemap" => Box::new(PtraceCheckedNode::new_node_with_current_task(
                target,
                PTRACE_MODE_READ_FSCREDS,
                |current_task, task| Ok(ProcPagemapFile::new(current_task, &task)),
            )),
            b"task" => Box::new(TaskListDirectory::new_node(target.pid)),
            name => unreachable!(
                "entry \"{:?}\" should be supported to keep in sync with task_entries()",
                name
            ),
        };

        Ok(fs.create_node(ino, ops, FsNodeInfo::new(mode, creds)))
    }
}

/// `TaskDirectory` doesn't implement the `close` method.
impl CloseFreeSafe for TaskDirectory {}
impl FileOps for TaskDirectory {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();

    fn readdir(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        let _task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        emit_dotdot(file, sink)?;

        // Skip through the entries until the current offset is reached.
        // Subtract 2 from the offset to account for `.` and `..`.
        for (index, (name, mode)) in
            task_entries(self.target.scope).iter().enumerate().skip(sink.offset() as usize - 2)
        {
            sink.add(
                self.inode_range.start + index as ino_t,
                sink.offset() + 1,
                DirectoryEntryType::from_mode(*mode),
                (*name).into(),
            )?;
        }
        Ok(())
    }

    fn as_pid(&self, _file: &FileObject) -> Result<Pid, Errno> {
        Ok(self.target.pid.clone())
    }
}

/// Creates an [`FsNode`] that represents the `/proc/<pid>` directory for `task`.
pub fn pid_directory(
    current_task: &CurrentTask,
    fs: &FileSystemHandle,
    task: &Arc<Task>,
    tid: Pid,
) -> FsNodeHandle {
    // proc(5): "The files inside each /proc/pid directory are normally
    // owned by the effective user and effective group ID of the process."
    let fs_node =
        TaskDirectory::new(fs, task, TaskContainer::from_thread_group(task.pid.clone(), tid));

    security::task_to_fs_node(current_task, task, &fs_node);
    fs_node
}

/// Creates an [`FsNode`] that represents the `/proc/<pid>/task/<tid>` directory for `task`.
fn tid_directory(fs: &FileSystemHandle, task: &Arc<Task>) -> FsNodeHandle {
    TaskDirectory::new(fs, task, TaskContainer::from_task(task))
}

/// `FdDirectory` implements the directory listing operations for a `proc/<pid>/fd` directory.
///
/// Reading the directory returns a list of all the currently open file descriptors for the
/// associated task.
struct FdDirectory {
    target: TaskContainer,
}

impl FdDirectory {
    fn new(target: TaskContainer) -> Self {
        Self { target }
    }
}

impl FsNodeOps for FdDirectory {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let task = self.target.get_task()?;
        let fds = task.files().map_or_else(|_| Vec::new(), |files| files.get_all_fds());
        Ok(VecDirectory::new_file(fds_to_directory_entries(fds)))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        _current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        let fd = FdNumber::from_fs_str(name).map_err(|_| errno!(ENOENT))?;
        let task = self.target.get_task()?;
        // Make sure that the file descriptor exists before creating the node.
        let file = task.files()?.get_allowing_opath(fd).map_err(|_| errno!(ENOENT))?;
        // Derive the symlink's mode from the mode in which the file was opened.
        // Readable and/or writable file descriptors also set execute permission on the symlink
        // (0500 for O_RDONLY, 0300 for O_WRONLY, 0700 for O_RDWR, and 0000 for O_PATH).
        let mut access = Access::from_open_flags(file.flags());
        if access.is_nontrivial() {
            access |= Access::EXEC;
        }
        let mode = FileMode::IFLNK | access.user_mode();
        let target = self.target.clone();
        Ok(entry.node.fs().create_node_and_allocate_node_id(
            CallbackSymlinkNode::new(move || {
                let task = target.get_task()?;
                let file = task.files()?.get_allowing_opath(fd).map_err(|_| errno!(ENOENT))?;
                Ok(SymlinkTarget::Node(file.name.to_passive()))
            }),
            FsNodeInfo::new(mode, task.real_fscred()),
        ))
    }
}

const NS_ENTRIES: &[&str] = &[
    "cgroup",
    "ipc",
    "mnt",
    "net",
    "pid",
    "pid_for_children",
    "time",
    "time_for_children",
    "user",
    "uts",
];

/// /proc/<pid>/attr directory entry.
struct AttrNode {
    attr: security::ProcAttr,
    target: TaskContainer,
}

impl AttrNode {
    fn new(target: TaskContainer, attr: security::ProcAttr) -> impl FsNodeOps {
        SimpleFileNode::new(move |_| Ok(AttrNode { attr, target: target.clone() }))
    }
}

impl FileOps for AttrNode {
    fileops_impl_seekable!();
    fileops_impl_noop_sync!();

    fn writes_update_seek_offset(&self) -> bool {
        false
    }

    fn read(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        let task = self.target.get_task()?;
        let response = security::get_procattr(current_task, &task, self.attr)?;
        data.write(&response[offset..])
    }

    fn write(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let task = self.target.get_task()?;

        // If the current task is not the target then writes are not allowed.
        if current_task.task != task {
            return error!(EPERM);
        }
        if offset != 0 {
            return error!(EINVAL);
        }

        let data = data.read_all()?;
        let data_len = data.len();
        security::set_procattr(current_task, self.attr, data.as_slice())?;
        Ok(data_len)
    }
}

/// /proc/[pid]/ns directory
struct NsDirectory {
    target: TaskContainer,
}

impl NsDirectory {
    fn new(target: TaskContainer) -> Self {
        Self { target }
    }
}

impl FsNodeOps for NsDirectory {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        // For each namespace, this contains a link to the current identifier of the given namespace
        // for the current task.
        Ok(VecDirectory::new_file(
            NS_ENTRIES
                .iter()
                .map(|&name| VecDirectoryEntry {
                    entry_type: DirectoryEntryType::LNK,
                    name: FsString::from(name),
                    inode: None,
                })
                .collect(),
        ))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        // If name is a given namespace, link to the current identifier of the that namespace for
        // the current task.
        // If name is {namespace}:[id], get a file descriptor for the given namespace.

        let name = String::from_utf8(name.to_vec()).map_err(|_| errno!(ENOENT))?;
        let mut elements = name.split(':');
        let ns = elements.next().expect("name must not be empty");
        // The name doesn't starts with a known namespace.
        if !NS_ENTRIES.contains(&ns) {
            return error!(ENOENT);
        }

        let task = self.target.get_task()?;
        if let Some(id) = elements.next() {
            // The name starts with {namespace}:, check that it matches {namespace}:[id]
            static NS_IDENTIFIER_RE: LazyLock<Regex> =
                LazyLock::new(|| Regex::new("^\\[[0-9]+\\]$").unwrap());
            if !NS_IDENTIFIER_RE.is_match(id) {
                return error!(ENOENT);
            }
            let node_info = || FsNodeInfo::new(mode!(IFREG, 0o444), task.real_fscred());
            let fallback = || {
                entry
                    .node
                    .fs()
                    .create_node_and_allocate_node_id(BytesFile::new_node(vec![]), node_info())
            };
            Ok(match ns {
                "cgroup" => {
                    track_stub!(TODO("https://fxbug.dev/297313673"), "cgroup namespaces");
                    fallback()
                }
                "ipc" => {
                    track_stub!(TODO("https://fxbug.dev/297313673"), "ipc namespaces");
                    fallback()
                }
                "mnt" => entry
                    .node
                    .fs()
                    .create_node_and_allocate_node_id(current_task.fs().namespace(), node_info()),
                "net" => {
                    track_stub!(TODO("https://fxbug.dev/297313673"), "net namespaces");
                    fallback()
                }
                "pid" => {
                    track_stub!(TODO("https://fxbug.dev/297313673"), "pid namespaces");
                    fallback()
                }
                "pid_for_children" => {
                    track_stub!(TODO("https://fxbug.dev/297313673"), "pid_for_children namespaces");
                    fallback()
                }
                "time" => {
                    track_stub!(TODO("https://fxbug.dev/297313673"), "time namespaces");
                    fallback()
                }
                "time_for_children" => {
                    track_stub!(
                        TODO("https://fxbug.dev/297313673"),
                        "time_for_children namespaces"
                    );
                    fallback()
                }
                "user" => {
                    track_stub!(TODO("https://fxbug.dev/297313673"), "user namespaces");
                    fallback()
                }
                "uts" => {
                    track_stub!(TODO("https://fxbug.dev/297313673"), "uts namespaces");
                    fallback()
                }
                _ => return error!(ENOENT),
            })
        } else {
            // The name is {namespace}, link to the correct one of the current task.
            let id = current_task.fs().namespace().id;
            Ok(entry.node.fs().create_node_and_allocate_node_id(
                CallbackSymlinkNode::new(move || {
                    Ok(SymlinkTarget::Path(format!("{name}:[{id}]").into()))
                }),
                FsNodeInfo::new(mode!(IFLNK, 0o7777), task.real_fscred()),
            ))
        }
    }
}

/// `FdInfoDirectory` implements the directory listing operations for a `proc/<pid>/fdinfo`
/// directory.
///
/// Reading the directory returns a list of all the currently open file descriptors for the
/// associated task.
struct FdInfoDirectory {
    target: TaskContainer,
}

impl FdInfoDirectory {
    fn new(target: TaskContainer) -> Self {
        Self { target }
    }
}

impl FsNodeOps for FdInfoDirectory {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let task = self.target.get_task()?;
        current_task
            .check_ptrace_access_mode(PTRACE_MODE_READ_FSCREDS, &task)
            .map_err(|_| errno!(EACCES))?;

        let fds = task.files().map_or_else(|_| Vec::new(), |files| files.get_all_fds());
        Ok(VecDirectory::new_file(fds_to_directory_entries(fds)))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        let task = self.target.get_task()?;
        let fd = FdNumber::from_fs_str(name).map_err(|_| errno!(ENOENT))?;
        let file = task.files()?.get_allowing_opath(fd).map_err(|_| errno!(ENOENT))?;
        let pos = file.offset.read();
        let flags = file.flags();
        let mut data = format!("pos:\t{}\nflags:\t0{:o}\n", pos, flags.bits()).into_bytes();
        if let Some(extra_fdinfo) = file.extra_fdinfo(current_task) {
            data.extend_from_slice(extra_fdinfo.as_slice());
        }
        Ok(entry.node.fs().create_node_and_allocate_node_id(
            BytesFile::new_node(data),
            FsNodeInfo::new(mode!(IFREG, 0o444), task.real_fscred()),
        ))
    }
}

fn fds_to_directory_entries(fds: Vec<FdNumber>) -> Vec<VecDirectoryEntry> {
    fds.into_iter()
        .map(|fd| VecDirectoryEntry {
            entry_type: DirectoryEntryType::DIR,
            name: fd.raw().to_string().into(),
            inode: None,
        })
        .collect()
}

/// Directory that lists the task IDs (tid) in a process. Located at `/proc/<pid>/task/`.
struct TaskListDirectory {
    target: TaskContainer,
}

struct TaskListFile {
    target: TaskContainer,
    inner: Box<dyn FileOps>,
}

impl FileOps for TaskListFile {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();

    fn readdir(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        let _task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        self.inner.readdir(file, current_task, sink)
    }
}

impl TaskListDirectory {
    fn new_node(pid: Pid) -> impl FsNodeOps {
        Self { target: TaskContainer::from_pid(pid) }
    }
}

impl FsNodeOps for TaskListDirectory {
    task_dir_impl_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        let inner = VecDirectory::new_file(
            task.thread_group()
                .read()
                .task_ids()
                .map(|tid| VecDirectoryEntry {
                    entry_type: DirectoryEntryType::DIR,
                    name: tid.to_string().into(),
                    inode: None,
                })
                .collect(),
        );
        Ok(Box::new(TaskListFile { target: self.target.clone(), inner }))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        let target_task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        let tid = std::str::from_utf8(name)
            .map_err(|_| errno!(ENOENT))?
            .parse::<pid_t>()
            .map_err(|_| errno!(ENOENT))?;

        let task = current_task.get_task(tid).map_err(|_| errno!(ENOENT))?;
        // Make sure the tid belongs to this process.
        if task.pid != target_task.thread_group().leader {
            return error!(ENOENT);
        }
        if let Some(proc_fs) = entry.node.fs().downcast_ops::<ProcFs>() {
            if !proc_fs.has_pid_permissions(current_task, &task, HidePid::Invisible) {
                return error!(ENOENT);
            }
        }

        Ok(tid_directory(&entry.node.fs(), &task))
    }
}

#[derive(Clone)]
struct CgroupFile {
    target: TaskContainer,
}
impl CgroupFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}
impl DynamicFileSource for CgroupFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let task = self.target.get_task()?;
        let cgroup1 = task.kernel().cgroups.cgroup1.lock();
        for (key, root) in &cgroup1.hierarchies {
            let mut parts: Vec<&str> = key.controllers.iter().map(|c| c.as_str()).collect();
            let name_storage;
            if let Some(name) = &key.name {
                name_storage = format!("name={}", name);
                parts.push(&name_storage);
            }
            let controller_str = parts.join(",");
            let cgroup = root.get_cgroup(&task.pid);
            let path = path_from_root(cgroup)?;
            sink.write(format!("{}:{}:{}\n", root.hierarchy_id, controller_str, path).as_bytes());
        }
        let cgroup = task.kernel().cgroups.cgroup2.get_cgroup(&task.pid);
        let path = path_from_root(cgroup)?;
        sink.write(format!("0::{}\n", path).as_bytes());
        Ok(())
    }
}

fn fill_buf_from_addr_range(
    task: &Task,
    range_start: UserAddress,
    range_end: UserAddress,
    sink: &mut DynamicFileBuf,
) -> Result<(), Errno> {
    #[allow(clippy::manual_saturating_arithmetic)]
    let len = range_end.ptr().checked_sub(range_start.ptr()).unwrap_or(0);
    // NB: If this is exercised in a hot-path, we can plumb the reading task
    // (`CurrentTask`) here to perform a copy without going through the VMO when
    // unified aspaces is enabled.
    let buf = task.read_memory_partial_to_vec(range_start, len)?;
    sink.write(&buf[..]);
    Ok(())
}

/// `CmdlineFile` implements `proc/<pid>/cmdline` file.
#[derive(Clone)]
pub struct CmdlineFile {
    target: TaskContainer,
}
impl CmdlineFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}
impl DynamicFileSource for CmdlineFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        // Opened cmdline file should still be functional once the task is a zombie.
        let Ok(task) = self.target.get_task() else {
            return Ok(());
        };
        // /proc/<pid>/cmdline is empty for kthreads.
        let Ok(mm) = task.mm() else {
            return Ok(());
        };
        let (start, end) = {
            let mm_state = mm.state.read();
            (mm_state.argv_start, mm_state.argv_end)
        };
        fill_buf_from_addr_range(&task, start, end, sink)
    }
}

struct PtraceCheckedNode {}

impl PtraceCheckedNode {
    pub fn new_node<F, O>(
        target: TaskContainer,
        mode: PtraceAccessMode,
        create_ops: F,
    ) -> impl FsNodeOps
    where
        F: Fn(Arc<Task>) -> Result<O, Errno> + Send + Sync + 'static,
        O: FileOps,
    {
        Self::new_node_with_current_task(target, mode, move |_current_task, task| create_ops(task))
    }

    pub fn new_node_with_current_task<F, O>(
        target: TaskContainer,
        mode: PtraceAccessMode,
        create_ops: F,
    ) -> impl FsNodeOps
    where
        F: Fn(&CurrentTask, Arc<Task>) -> Result<O, Errno> + Send + Sync + 'static,
        O: FileOps,
    {
        SimpleFileNode::new(move |current_task: &CurrentTask| {
            let task = target.get_task()?;
            // proc-pid nodes for kthreads do not require ptrace access checks.
            if task.mm().is_ok() || task.state_code() == TaskStateCode::Zombie {
                current_task.check_ptrace_access_mode(mode, &task).map_err(|_| errno!(EACCES))?;
            }
            create_ops(current_task, task)
        })
    }
}

/// `EnvironFile` implements `proc/<pid>/environ` file.
#[derive(Clone)]
pub struct EnvironFile {
    target: TaskContainer,
}
impl EnvironFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        let target_for_file = target.clone();
        PtraceCheckedNode::new_node(target, PTRACE_MODE_READ_FSCREDS, move |_| {
            Ok(DynamicFile::new(Self { target: target_for_file.clone() }))
        })
    }
}
impl DynamicFileSource for EnvironFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let task = self.target.get_task()?;
        // /proc/<pid>/environ is empty for kthreads.
        let Ok(mm) = task.mm() else {
            return Ok(());
        };
        let (start, end) = {
            let mm_state = mm.state.read();
            (mm_state.environ_start, mm_state.environ_end)
        };
        fill_buf_from_addr_range(&task, start, end, sink)
    }
}

/// `AuxvFile` implements `proc/<pid>/auxv` file.
#[derive(Clone)]
pub struct AuxvFile {
    target: TaskContainer,
}
impl AuxvFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        let target_for_file = target.clone();
        PtraceCheckedNode::new_node(target, PTRACE_MODE_READ_FSCREDS, move |_| {
            Ok(DynamicFile::new(Self { target: target_for_file.clone() }))
        })
    }
}
impl DynamicFileSource for AuxvFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let task = self.target.get_task()?;
        // /proc/<pid>/auxv is empty for kthreads.
        let Ok(mm) = task.mm() else {
            return Ok(());
        };
        let (start, end) = {
            let mm_state = mm.state.read();
            (mm_state.auxv_start, mm_state.auxv_end)
        };
        fill_buf_from_addr_range(&task, start, end, sink)
    }
}

/// `CommFile` implements `proc/<pid>/comm` file.
#[derive(Clone)]
pub struct CommFile {
    target: TaskContainer,
}
impl CommFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}

impl DynamicFileSource for CommFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let task = self.target.get_task()?;
        sink.write(task.persistent_info.command_guard().comm_name());
        sink.write(b"\n");
        Ok(())
    }

    fn write(
        &self,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let task = self.target.get_task()?;
        if !Arc::ptr_eq(&task.thread_group(), &current_task.thread_group()) {
            return error!(EINVAL);
        }
        // What happens if userspace writes to this file in multiple syscalls? We need more
        // detailed tests to see when the data is actually committed back to the task.
        let bytes = data.read_all()?;
        task.set_command_name(TaskCommand::new(&bytes));
        Ok(bytes.len())
    }
}

/// `IoFile` implements `proc/<pid>/io` file.
#[derive(Clone)]
pub struct IoFile {}
impl IoFile {
    pub fn new_node() -> impl FsNodeOps {
        DynamicFile::new_node(Self {})
    }
}
impl DynamicFileSource for IoFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        track_stub!(TODO("https://fxbug.dev/322874250"), "/proc/pid/io");
        sink.write(b"rchar: 0\n");
        sink.write(b"wchar: 0\n");
        sink.write(b"syscr: 0\n");
        sink.write(b"syscw: 0\n");
        sink.write(b"read_bytes: 0\n");
        sink.write(b"write_bytes: 0\n");
        sink.write(b"cancelled_write_bytes: 0\n");
        Ok(())
    }
}

/// `LimitsFile` implements `proc/<pid>/limits` file.
#[derive(Clone)]
pub struct LimitsFile {
    target: TaskContainer,
}
impl LimitsFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}
impl DynamicFileSource for LimitsFile {
    fn generate_locked(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let task = self.target.get_task()?;
        let limits = task.thread_group().limits.lock();

        let write_limit = |sink: &mut DynamicFileBuf, value| {
            if value == RLIM_INFINITY as u64 {
                sink.write(format!("{:<20}", "unlimited").as_bytes());
            } else {
                sink.write(format!("{:<20}", value).as_bytes());
            }
        };
        sink.write(
            format!("{:<25}{:<20}{:<20}{:<10}\n", "Limit", "Soft Limit", "Hard Limit", "Units")
                .as_bytes(),
        );
        for resource in Resource::ALL {
            let desc = resource.desc();
            let limit = limits.get(resource);
            sink.write(format!("{:<25}", desc.name).as_bytes());
            write_limit(sink, limit.rlim_cur);
            write_limit(sink, limit.rlim_max);
            if !desc.unit.is_empty() {
                sink.write(format!("{:<10}", desc.unit).as_bytes());
            }
            sink.write(b"\n");
        }
        Ok(())
    }
}

/// `MemFile` implements `proc/<pid>/mem` file.
pub struct MemFile {
    mm: Weak<MemoryManager>,

    // TODO: https://fxbug.dev/442459337 - Tear-down MemoryManager internals on process exit, to
    // avoid extension of the MM lifetime prolonging access to memory via "/proc/pid/mem", etc
    // beyond that of the actual process/address-space.
    target: TaskContainer,
}

impl MemFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        let target_for_file = target.clone();
        PtraceCheckedNode::new_node(target, PTRACE_MODE_ATTACH_FSCREDS, move |task| {
            let mm = task.mm().ok().as_ref().map(Arc::downgrade).unwrap_or_default();
            Ok(Self { mm, target: target_for_file.clone() })
        })
    }
}

impl FileOps for MemFile {
    fileops_impl_noop_sync!();

    fn is_seekable(&self) -> bool {
        true
    }

    fn seek(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        current_offset: off_t,
        target: SeekTarget,
    ) -> Result<off_t, Errno> {
        default_seek(current_offset, target, || error!(EINVAL))
    }

    fn read(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        let Ok(_task) = self.target.get_task() else {
            return Ok(0);
        };
        let Some(mm) = self.mm.upgrade() else {
            return Ok(0);
        };
        let mut addr = UserAddress::from(offset as u64);
        data.write_each(&mut |bytes| {
            let read_bytes = if current_task.has_same_address_space(Some(&mm)) {
                current_task.read_memory_partial(addr, bytes)
            } else {
                mm.syscall_read_memory_partial(addr, bytes)
            }
            .map_err(|_| errno!(EIO))?;
            let actual = read_bytes.len();
            addr = (addr + actual)?;
            Ok(actual)
        })
    }

    fn write(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let Ok(_task) = self.target.get_task() else {
            return Ok(0);
        };
        let Some(mm) = self.mm.upgrade() else {
            return Ok(0);
        };
        let addr = UserAddress::from(offset as u64);
        let mut written = 0;
        let result = data.peek_each(&mut |bytes| {
            let actual = if current_task.has_same_address_space(Some(&mm)) {
                current_task.write_memory_partial((addr + written)?, bytes)
            } else {
                mm.syscall_write_memory_partial((addr + written)?, bytes)
            }
            .map_err(|_| errno!(EIO))?;
            written += actual;
            Ok(actual)
        });
        data.advance(written)?;
        result
    }
}

const STUBBED_MEM_BYTES: usize = 4096;

// Workaround for b/525059309: Zircon VMAR walks (`ZX_INFO_VMAR_MAPS`) are extremely
// slow and cause Perfetto's `traced_probes` watchdog timeouts when sweeping thread status
// files. We bypass them when the reader is `traced_probes` by returning a 1 page/KB stub.
fn should_skip_memory_stats(current_task: &CurrentTask) -> bool {
    current_task.persistent_info.command_guard().comm_name() == b"traced_probes"
}
fn stub_memory_stats() -> MemoryStats {
    MemoryStats {
        vm_size: STUBBED_MEM_BYTES,
        vm_rss: STUBBED_MEM_BYTES,
        vm_rss_hwm: STUBBED_MEM_BYTES,
        rss_anonymous: STUBBED_MEM_BYTES,
        rss_file: 0,
        rss_shared: 0,
        vm_data: 0,
        vm_stack: STUBBED_MEM_BYTES,
        vm_exe: STUBBED_MEM_BYTES,
        vm_swap: 0,
        vm_lck: 0,
    }
}

#[derive(Clone)]
pub struct StatFile {
    target: TaskContainer,
}

impl StatFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}
impl DynamicFileSource for StatFile {
    fn generate_locked(
        &self,
        current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let task = self.target.get_task()?;

        // All fields and their types as specified in the man page.
        // Unimplemented fields are set to 0 here.
        let pid: pid_t; // 1
        let comm: TaskCommand;
        let state: char;
        let ppid: pid_t;
        let pgrp: pid_t; // 5
        let session: pid_t;
        let tty_nr: i32;
        let tpgid: i32 = 0;
        let flags: u32 = 0;
        let minflt: u64 = 0; // 10
        let cminflt: u64 = 0;
        let majflt: u64 = 0;
        let cmajflt: u64 = 0;
        let utime: i64;
        let stime: i64; // 15
        let cutime: i64;
        let cstime: i64;
        let priority: i64 = 0;
        let nice: i64;
        let num_threads: i64; // 20
        let itrealvalue: i64 = 0;
        let mut starttime: u64 = 0;
        let mut vsize: usize = 0;
        let mut rss: usize = 0;
        let mut rsslim: u64 = 0; // 25
        let mut startcode: u64 = 0;
        let mut endcode: u64 = 0;
        let mut startstack: usize = 0;
        let mut kstkesp: u64 = 0;
        let mut kstkeip: u64 = 0; // 30
        let signal: u64 = 0;
        let blocked: u64 = 0;
        let siginore: u64 = 0;
        let sigcatch: u64 = 0;
        let mut wchan: u64 = 0; // 35
        let nswap: u64 = 0;
        let cnswap: u64 = 0;
        let exit_signal: i32 = 0;
        let processor: i32 = 0;
        let rt_priority: u32 = 0; // 40
        let policy: u32 = 0;
        let delayacct_blkio_ticks: u64 = 0;
        let guest_time: u64 = 0;
        let cguest_time: i64 = 0;
        let mut start_data: u64 = 0; // 45
        let mut end_data: u64 = 0;
        let mut start_brk: u64 = 0;
        let mut arg_start: usize = 0;
        let mut arg_end: usize = 0;
        let mut env_start: usize = 0; // 50
        let mut env_end: usize = 0;
        let mut exit_code: i32 = 0;

        pid = self.target.tid.id;
        comm = task.command();
        state = task.state_code().code_char();
        nice = task.scheduler_state.lock().normal_priority().as_nice() as i64;

        {
            let thread_group = task.thread_group();
            let tg_state = thread_group.read();
            ppid = tg_state.get_ppid();
            pgrp = thread_group.pgid().id;
            session = tg_state.session.id;

            // TTY device ID.
            tty_nr = tg_state
                .controlling_terminal
                .as_ref()
                .map(|t| t.terminal.device().bits())
                .unwrap_or(0) as i32;

            cutime = duration_to_scheduler_clock(tg_state.children_time_stats.user_time);
            cstime = duration_to_scheduler_clock(tg_state.children_time_stats.system_time);

            num_threads = std::cmp::max(1, tg_state.tasks_count()) as i64;
        }

        let time_stats = match self.target.scope {
            TaskEntryScope::Task => task.time_stats(),
            TaskEntryScope::ThreadGroup => task.thread_group().time_stats(),
        };
        utime = duration_to_scheduler_clock(time_stats.user_time);
        stime = duration_to_scheduler_clock(time_stats.system_time);

        if let Ok(info) = task.thread_group().process.info() {
            starttime =
                duration_to_scheduler_clock(info.start_time - zx::MonotonicInstant::ZERO) as u64;
        }

        if let Ok(mm) = task.mm() {
            // TODO(b/525059309): Bypassed for traced_probes due to VMAR walk slowness. Re-enable when optimized.
            let mem_stats = if should_skip_memory_stats(current_task) {
                stub_memory_stats()
            } else {
                mm.get_stats(current_task)
            };
            let page_size = *PAGE_SIZE as usize;
            vsize = mem_stats.vm_size;
            rss = mem_stats.vm_rss / page_size;
            rsslim = task.thread_group().limits.lock().get(Resource::RSS).rlim_max;

            {
                let mm_state = mm.state.read();
                startstack = mm_state.stack_start.ptr();
                arg_start = mm_state.argv_start.ptr();
                arg_end = mm_state.argv_end.ptr();
                env_start = mm_state.environ_start.ptr();
                env_end = mm_state.environ_end.ptr();
            }
        }

        // The man page describes that the following fields have "... values displayed as 0" if the
        // caller does not have ptrace read access to the target.
        // In practice the `startcode` and `endcode` fields appear to be displayed as 1.
        if !current_task
            .check_ptrace_access_mode(PTRACE_MODE_READ_FSCREDS | PTRACE_MODE_NOAUDIT, &task)
            .is_ok()
        {
            startcode = 1;
            endcode = 1;
            startstack = 0;
            kstkesp = 0;
            kstkeip = 0;
            wchan = 0;
            start_data = 0;
            end_data = 0;
            start_brk = 0;
            arg_start = 0;
            arg_end = 0;
            env_start = 0;
            env_end = 0;
            exit_code = 0;
        }

        write!(sink, "{pid} (")?;
        sink.write(comm.comm_name());
        writeln!(
            sink,
            ") {state} {ppid} {pgrp} {session} {tty_nr} {tpgid} {flags} {minflt} {cminflt} {majflt} {cmajflt} {utime} {stime} {cutime} {cstime} {priority} {nice} {num_threads} {itrealvalue} {starttime} {vsize} {rss} {rsslim} {startcode} {endcode} {startstack} {kstkesp} {kstkeip} {signal} {blocked} {siginore} {sigcatch} {wchan} {nswap} {cnswap} {exit_signal} {processor} {rt_priority} {policy} {delayacct_blkio_ticks} {guest_time} {cguest_time} {start_data} {end_data} {start_brk} {arg_start} {arg_end} {env_start} {env_end} {exit_code}"
        )?;

        Ok(())
    }
}

#[derive(Clone)]
pub struct StatmFile {
    target: TaskContainer,
}
impl StatmFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}
impl DynamicFileSource for StatmFile {
    fn generate(&self, current_task: &CurrentTask, sink: &mut DynamicFileBuf) -> Result<(), Errno> {
        // /proc/<pid>/statm reports zeroes for kthreads.
        let task = self.target.get_task()?;
        // TODO(b/525059309): Bypassed for traced_probes due to VMAR walk slowness. Re-enable when optimized.
        let mem_stats = if should_skip_memory_stats(current_task) {
            stub_memory_stats()
        } else {
            match task.mm() {
                Ok(mm) => mm.get_stats(current_task),
                Err(_) => Default::default(),
            }
        };
        let page_size = *PAGE_SIZE as usize;

        // 5th and 7th fields are deprecated and should be set to 0.
        writeln!(
            sink,
            "{} {} {} {} 0 {} 0",
            mem_stats.vm_size / page_size,
            mem_stats.vm_rss / page_size,
            mem_stats.rss_shared / page_size,
            mem_stats.vm_exe / page_size,
            (mem_stats.vm_data + mem_stats.vm_stack) / page_size
        )?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct StatusFile {
    target: TaskContainer,
}
impl StatusFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}
impl DynamicFileSource for StatusFile {
    fn generate(&self, current_task: &CurrentTask, sink: &mut DynamicFileBuf) -> Result<(), Errno> {
        let start_monotonic = zx::MonotonicInstant::get();
        let start_boot = zx::BootInstant::get();
        let task = self.target.get_task()?;
        let creds_string = {
            // Collect everything stored in info in this block.  There is a lock ordering
            // issue with the task lock acquired below, and cloning info is
            // expensive.
            write!(sink, "Name:\t")?;
            sink.write(task.persistent_info.command_guard().comm_name());
            let creds = task.persistent_info.real_creds();
            format!(
                "Uid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nGroups:\t{}",
                creds.uid,
                creds.euid,
                creds.saved_uid,
                creds.fsuid,
                creds.gid,
                creds.egid,
                creds.saved_gid,
                creds.fsgid,
                creds.groups.iter().map(|n| n.to_string()).join(" ")
            )
        };

        writeln!(sink)?;

        if let Ok(fs) = task.fs() {
            writeln!(sink, "Umask:\t0{:03o}", fs.umask().bits())?;
        }
        {
            let task_state = task.read();
            writeln!(sink, "SigBlk:\t{:016x}", task_state.signal_mask().0)?;
            writeln!(sink, "SigPnd:\t{:016x}", task_state.task_specific_pending_signals().0)?;
            writeln!(
                sink,
                "ShdPnd:\t{:x}",
                task.thread_group().pending_signals.lock().pending().0
            )?;
            writeln!(sink, "NoNewPrivs:\t{}", task_state.no_new_privs() as u8)?;
        }

        // Since version 3.8 all nonexistent capabilities are reported as not-enabled.
        let creds = task.real_creds();
        writeln!(sink, "CapInh:\t{:016x}", creds.cap_inheritable)?;
        writeln!(sink, "CapPrm:\t{:016x}", creds.cap_permitted)?;
        writeln!(sink, "CapEff:\t{:016x}", creds.cap_effective)?;
        writeln!(sink, "CapBnd:\t{:016x}", creds.cap_bounding)?;
        writeln!(sink, "CapAmb:\t{:016x}", creds.cap_ambient)?;

        let state_code = task.state_code();
        writeln!(sink, "State:\t{} ({})", state_code.code_char(), state_code.name())?;

        writeln!(sink, "Tgid:\t{}", task.pid)?;
        writeln!(sink, "Pid:\t{}", self.target.tid)?;
        let (ppid, threads, tracer_pid) = {
            let tracer_pid =
                task.read().ptrace.as_ref().map_or(0, |p| {
                    p.core_state.thread_group.upgrade().map_or(0, |tg| tg.leader.id)
                });
            let task_group = task.thread_group().read();
            (task_group.get_ppid(), task_group.tasks_count(), tracer_pid)
        };
        writeln!(sink, "PPid:\t{}", ppid)?;
        writeln!(sink, "TracerPid:\t{}", tracer_pid)?;

        writeln!(sink, "{}", creds_string)?;

        if let Ok(mm) = task.mm() {
            // TODO(b/525059309): Bypassed for traced_probes due to VMAR walk slowness. Re-enable when optimized.
            let mem_stats = if should_skip_memory_stats(current_task) {
                stub_memory_stats()
            } else {
                mm.get_stats(current_task)
            };
            writeln!(sink, "VmSize:\t{} kB", mem_stats.vm_size / 1024)?;
            writeln!(sink, "VmLck:\t{} kB", mem_stats.vm_lck / 1024)?;
            writeln!(sink, "VmRSS:\t{} kB", mem_stats.vm_rss / 1024)?;
            writeln!(sink, "RssAnon:\t{} kB", mem_stats.rss_anonymous / 1024)?;
            writeln!(sink, "RssFile:\t{} kB", mem_stats.rss_file / 1024)?;
            writeln!(sink, "RssShmem:\t{} kB", mem_stats.rss_shared / 1024)?;
            writeln!(sink, "VmData:\t{} kB", mem_stats.vm_data / 1024)?;
            writeln!(sink, "VmStk:\t{} kB", mem_stats.vm_stack / 1024)?;
            writeln!(sink, "VmExe:\t{} kB", mem_stats.vm_exe / 1024)?;
            writeln!(sink, "VmSwap:\t{} kB", mem_stats.vm_swap / 1024)?;
            writeln!(sink, "VmHWM:\t{} kB", mem_stats.vm_rss_hwm / 1024)?;
        }
        // Report seccomp filter status.
        let seccomp = task.seccomp_filter_state.get() as u8;
        writeln!(sink, "Seccomp:\t{}", seccomp)?;

        // There should be at least one thread in Zombie processes.
        writeln!(sink, "Threads:\t{}", std::cmp::max(1, threads))?;

        let elapsed_monotonic = zx::MonotonicInstant::get() - start_monotonic;
        let elapsed_boot = zx::BootInstant::get() - start_boot;
        if elapsed_monotonic > zx::MonotonicDuration::from_millis(100)
            || elapsed_boot > zx::BootDuration::from_seconds(1)
        {
            let target_pid = task.pid.id;
            let target_comm =
                String::from_utf8_lossy(task.persistent_info.command_guard().comm_name())
                    .into_owned();
            starnix_logging::log_warn!(
                "StatusFile::generate for task {} ({}) took {} ms (monotonic), {} ms (boot)",
                target_pid,
                target_comm,
                elapsed_monotonic.into_millis(),
                elapsed_boot.into_millis()
            );
        }

        Ok(())
    }
}

struct OomScoreFile {
    target: TaskContainer,
}

impl OomScoreFile {
    fn new_node(target: TaskContainer) -> impl FsNodeOps {
        BytesFile::new_node(Self { target })
    }
}

impl BytesFileOps for OomScoreFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let _task = self.target.get_task()?;
        track_stub!(TODO("https://fxbug.dev/322873459"), "/proc/pid/oom_score");
        Ok(serialize_for_file(0).into())
    }
}

// Redefine these constants as i32 to avoid conversions below.
const OOM_ADJUST_MAX: i32 = uapi::OOM_ADJUST_MAX as i32;
const OOM_SCORE_ADJ_MAX: i32 = uapi::OOM_SCORE_ADJ_MAX as i32;

struct OomAdjFile {
    target: TaskContainer,
}
impl OomAdjFile {
    fn new_node(target: TaskContainer) -> impl FsNodeOps {
        BytesFile::new_node(Self { target })
    }
}

impl BytesFileOps for OomAdjFile {
    fn write(&self, current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let value = parse_i32_file(&data)?;
        let oom_score_adj = if value == OOM_DISABLE {
            OOM_SCORE_ADJ_MIN
        } else {
            if !(OOM_ADJUST_MIN..=OOM_ADJUST_MAX).contains(&value) {
                return error!(EINVAL);
            }
            let fraction = (value - OOM_ADJUST_MIN) / (OOM_ADJUST_MAX - OOM_ADJUST_MIN);
            fraction * (OOM_SCORE_ADJ_MAX - OOM_SCORE_ADJ_MIN) + OOM_SCORE_ADJ_MIN
        };
        security::check_task_capable(current_task, CAP_SYS_RESOURCE)?;
        let task = self.target.get_task()?;
        task.write().oom_score_adj = oom_score_adj;
        Ok(())
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let task = self.target.get_task()?;
        let oom_score_adj = task.read().oom_score_adj;
        let oom_adj = if oom_score_adj == OOM_SCORE_ADJ_MIN {
            OOM_DISABLE
        } else {
            let fraction =
                (oom_score_adj - OOM_SCORE_ADJ_MIN) / (OOM_SCORE_ADJ_MAX - OOM_SCORE_ADJ_MIN);
            fraction * (OOM_ADJUST_MAX - OOM_ADJUST_MIN) + OOM_ADJUST_MIN
        };
        Ok(serialize_for_file(oom_adj).into())
    }
}

struct OomScoreAdjFile {
    target: TaskContainer,
}

impl OomScoreAdjFile {
    fn new_node(target: TaskContainer) -> impl FsNodeOps {
        BytesFile::new_node(Self { target })
    }
}

impl BytesFileOps for OomScoreAdjFile {
    fn write(&self, current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let value = parse_i32_file(&data)?;
        if !(OOM_SCORE_ADJ_MIN..=OOM_SCORE_ADJ_MAX).contains(&value) {
            return error!(EINVAL);
        }
        security::check_task_capable(current_task, CAP_SYS_RESOURCE)?;
        let task = self.target.get_task()?;
        task.write().oom_score_adj = value;
        Ok(())
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let task = self.target.get_task()?;
        let oom_score_adj = task.read().oom_score_adj;
        Ok(serialize_for_file(oom_score_adj).into())
    }
}

struct TimerslackNsFile {
    target: TaskContainer,
}

impl TimerslackNsFile {
    fn new_node(target: TaskContainer) -> impl FsNodeOps {
        BytesFile::new_node(Self { target })
    }
}

impl BytesFileOps for TimerslackNsFile {
    fn write(&self, current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let target_task = self.target.get_task()?;
        let same_task = current_task.task.pid == target_task.pid;
        if !same_task {
            security::check_task_capable(current_task, CAP_SYS_NICE)?;
            security::check_task_setscheduler_access(current_task, &target_task)?;
        };

        let value = parse_unsigned_file(&data)?;
        target_task.write().set_timerslack_ns(value);
        Ok(())
    }

    fn read(&self, current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let target_task = self.target.get_task()?;
        let same_task = current_task.task.pid == target_task.pid;
        if !same_task {
            security::check_task_capable(current_task, CAP_SYS_NICE)?;
            security::check_task_getscheduler_access(current_task, &target_task)?;
        };

        let timerslack_ns = target_task.read().timerslack_ns;
        Ok(serialize_for_file(timerslack_ns).into())
    }
}

struct ClearRefsFile {
    target: TaskContainer,
}

impl ClearRefsFile {
    fn new_node(target: TaskContainer) -> impl FsNodeOps {
        BytesFile::new_node(Self { target })
    }
}

impl BytesFileOps for ClearRefsFile {
    fn write(&self, _current_task: &CurrentTask, _data: Vec<u8>) -> Result<(), Errno> {
        let _task = self.target.get_task()?;
        track_stub!(TODO("https://fxbug.dev/396221597"), "/proc/pid/clear_refs");
        Ok(())
    }
}
