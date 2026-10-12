// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::cgroups::cgroups_file;
use crate::config_gz::ConfigFile;
use crate::cpuinfo::CpuinfoFile;
use crate::device_tree::DeviceTreeSymlink;
use crate::devices::DevicesFile;
use crate::filesystems::FilesystemsFile;
use crate::fs::{HidePid, ProcFs};
use crate::kmsg::kmsg_file;
use crate::loadavg::LoadavgFile;
use crate::meminfo::MeminfoFile;
use crate::misc::MiscFile;
use crate::mounts_symlink::MountsSymlink;
use crate::pid_directory::pid_directory;
use crate::pressure_directory::pressure_directory;
use crate::self_symlink::SelfSymlink;
use crate::stat::StatFile;
use crate::swaps::SwapsFile;
use crate::sysctl::{KptrRestrict, net_directory, sysctl_directory};
use crate::sysrq::SysRqNode;
use crate::thread_self::ThreadSelfSymlink;
use crate::uid_cputime::uid_cputime_directory;
use crate::uid_io::uid_io_directory;
use crate::uid_procstat::uid_procstat_directory;
use crate::uptime::UptimeFile;
use crate::vmstat::VmStatFile;
use crate::zoneinfo::ZoneInfoFile;
use maplit::btreemap;
use starnix_core::security;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::fs_registry::FsRegistry;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, SimpleFileNode};
use starnix_core::vfs::pseudo::stub_empty_file::StubEmptyFile;
use starnix_core::vfs::{
    CloseFreeSafe, DirEntry, DirectoryEntryType, DirentSink, FileObject, FileOps, FileSystemHandle,
    FsNode, FsNodeHandle, FsNodeInfo, FsNodeOps, FsStr, FsString, emit_dotdot,
    fileops_impl_directory, fileops_impl_noop_sync, fileops_impl_unbounded_seek,
    fs_node_impl_dir_readonly,
};
use starnix_logging::{BugRef, bug_ref, track_stub};

use starnix_uapi::auth::{CAP_SYSLOG, FsCred};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::version::{KERNEL_RELEASE, KERNEL_VERSION};
use starnix_uapi::{errno, error, pid_t};
use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::Arc;

/// `ProcDirectory` represents the top-level directory in `procfs`.
///
/// It contains, for example, a directory for each running task, named after the task's pid.
///
/// It also contains a special symlink, `self`, which targets the task directory for the task
/// that reads the symlink.
pub struct ProcDirectory {
    /// A map that stores all the nodes that aren't task directories.
    nodes: BTreeMap<&'static FsStr, FsNodeHandle>,
}

#[derive(Clone)]
pub struct ProcDirectoryNode(Arc<ProcDirectory>);

impl Deref for ProcDirectoryNode {
    type Target = ProcDirectory;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ProcDirectory {
    /// Returns a new `ProcDirectory` exposing information about `kernel`.
    pub fn new(kernel: &Kernel, fs: &FileSystemHandle) -> ProcDirectoryNode {
        // First add all the nodes that are always present in the top-level proc directory.
        let mut nodes = btreemap! {
            "asound".into() => stub_file(fs, bug_ref!("https://fxbug.dev/322893329")),
            "cgroups".into() => cgroups_file(fs),
            "cmdline".into() => {
                let mut cmdline = Vec::from(kernel.cmdline.clone());
                cmdline.push(b'\n');
                read_only_file(fs, BytesFile::new_node(cmdline))
            },
            "config.gz".into() => read_only_file(fs, ConfigFile::new_node()),
            "cpuinfo".into() => read_only_file(fs, CpuinfoFile::new_node()),
            "devices".into() => read_only_file(fs, DevicesFile::new_node()),
            "device-tree".into() => symlink_file(fs, DeviceTreeSymlink::new_node()),
            "diskstats".into() => stub_file(fs, bug_ref!("https://fxbug.dev/322893370")),
            "filesystems".into() => read_only_file(fs, FilesystemsFile::new_node(&kernel.expando.get::<FsRegistry>())),
            "kallsyms".into() => read_only_file(fs, SimpleFileNode::new(|current_task| {
                track_stub!(TODO("https://fxbug.dev/369067922"), "Provide a real /proc/kallsyms");
                let show_value = match current_task.kernel().expando.get::<KptrRestrict>().load() {
                    0 => true,
                    1 => security::is_task_capable_noaudit(current_task, CAP_SYSLOG),
                    _ => false,
                };
                let mut contents = String::new();
                for &(address, symbol_type, name) in KALLSYMS {
                    let display_address = if show_value { address } else { 0 };
                    let _ = std::fmt::Write::write_fmt(
                        &mut contents,
                        format_args!("{display_address:016x} {symbol_type} {name}\n"),
                    );
                }
                Ok(BytesFile::new(contents.into_bytes()))
            })),
            "kmsg".into() => kmsg_file(fs),
            "loadavg".into() => read_only_file(fs, LoadavgFile::new_node(kernel)),
            "meminfo".into() => read_only_file(fs, MeminfoFile::new_node(&kernel.stats)),
            "misc".into() => read_only_file(fs, MiscFile::new_node()),
            // Starnix does not support dynamically loading modules.
            // Instead, we pretend to have loaded a single module, ferris (named after
            // Rust's 🦀), to avoid breaking code that assumes the modules list is
            // non-empty.
            "modules".into() => bytes_file(fs, b"ferris 8192 0 - Live 0x0000000000000000\n".to_vec()),
            "mounts".into() => symlink_file(fs, MountsSymlink::new_node()),
            "net".into() => net_directory(fs),
            "pagetypeinfo".into() => stub_file(fs, bug_ref!("https://fxbug.dev/322894315")),
            "self".into() => symlink_file(fs, SelfSymlink::new_node()),
            "slabinfo".into() => stub_file(fs, bug_ref!("https://fxbug.dev/322894195")),
            "stat".into() => read_only_file(fs, StatFile::new_node(&kernel.stats)),
            "swaps".into() => read_only_file(fs, SwapsFile::new_node()),
            "sys".into() => sysctl_directory(fs),
            "sysrq-trigger".into() => root_writable_file(fs, SysRqNode::new()),
            "thread-self".into() => symlink_file(fs, ThreadSelfSymlink::new_node()),
            "uid_cputime".into() => uid_cputime_directory(fs),
            "uid_io".into() => uid_io_directory(fs),
            "uid_procstat".into() => uid_procstat_directory(fs),
            "uptime".into() => read_only_file(fs, UptimeFile::new_node(&kernel.stats)),
            "version".into() => {
                let release = KERNEL_RELEASE;
                let user = "build-user@build-host";
                let toolchain = "clang version HEAD, LLD HEAD";
                let version = KERNEL_VERSION;
                let version_string = format!("Linux version {} ({}) ({}) {}\n", release, user, toolchain, version);
                bytes_file(fs, version_string.into())
            },
            "vmallocinfo".into() => stub_file(fs, bug_ref!("https://fxbug.dev/322894183")),
            "vmstat".into() => read_only_file(fs, VmStatFile::new_node(&kernel.stats)),
            "zoneinfo".into() => read_only_file(fs, ZoneInfoFile::new_node(&kernel.stats)),
            "allocinfo".into() => stub_file(fs, bug_ref!("https://fxbug.dev/452096300")),
            "buddyinfo".into() => stub_file(fs, bug_ref!("https://fxbug.dev/452096300")),
            "locks".into() => stub_file(fs, bug_ref!("https://fxbug.dev/452096300")),
        };

        // Then optionally add the nodes that are only present in some configurations.
        if let Some(pressure_directory) = pressure_directory(kernel, fs) {
            nodes.insert("pressure".into(), pressure_directory);
        }

        ProcDirectoryNode(Arc::new(ProcDirectory { nodes }))
    }
}

const KALLSYMS: &[(u64, char, &str)] = &[
    (0xffffffff81000000, 'T', "_text"),
    (0xffffffff81000000, 'T', "_stext"),
    (0xffffffff81001000, 'T', "do_one_initcall"),
    (0xffffffff81002000, 'T', "schedule"),
    (0xffffffff81003000, 'T', "security_inode_copy_up"),
    (0xffffffff81100000, 'T', "_etext"),
    (0xffffffff81200000, 'T', "_sinittext"),
    (0xffffffff81210000, 'T', "_einittext"),
    (0xffffffff81400000, 'D', "_sdata"),
    (0xffffffff81500000, 'D', "_edata"),
    (0xffffffff81600000, 'B', "__bss_start"),
    (0xffffffff81700000, 'B', "__bss_stop"),
    (0xffffffff81800000, 'B', "_end"),
];

/// Creates a stub file that logs a message with the associated bug when it is accessed.
fn stub_file(fs: &FileSystemHandle, bug: BugRef) -> FsNodeHandle {
    read_only_file(fs, StubEmptyFile::new_node(bug))
}

/// Returns a new `BytesFile` containing the provided `bytes`.
fn bytes_file(fs: &FileSystemHandle, bytes: Vec<u8>) -> FsNodeHandle {
    read_only_file(fs, BytesFile::new_node(bytes))
}

/// Creates a standard read-only file suitable for use in `ProcDirectory`.
fn read_only_file(fs: &FileSystemHandle, ops: impl Into<Box<dyn FsNodeOps>>) -> FsNodeHandle {
    fs.create_node_and_allocate_node_id(ops, FsNodeInfo::new(mode!(IFREG, 0o444), FsCred::root()))
}

/// Creates a file that is only writable by root.
fn root_writable_file(fs: &FileSystemHandle, ops: impl Into<Box<dyn FsNodeOps>>) -> FsNodeHandle {
    fs.create_node_and_allocate_node_id(ops, FsNodeInfo::new(mode!(IFREG, 0o200), FsCred::root()))
}

fn symlink_file(fs: &FileSystemHandle, ops: impl Into<Box<dyn FsNodeOps>>) -> FsNodeHandle {
    fs.create_node_and_allocate_node_id(ops, FsNodeInfo::new(mode!(IFLNK, 0o777), FsCred::root()))
}

impl FsNodeOps for ProcDirectoryNode {
    fs_node_impl_dir_readonly!();

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
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        match self.nodes.get(name) {
            Some(node) => Ok(Arc::clone(node)),
            None => {
                let pid_string = std::str::from_utf8(name).map_err(|_| errno!(ENOENT))?;
                let pid = pid_string.parse::<pid_t>().map_err(|_| errno!(ENOENT))?;
                let pid_entry = current_task.kernel().pids.get(pid).map_err(|_| errno!(ENOENT))?;
                let task = pid_entry.get_process_task().ok_or_else(|| errno!(ENOENT))?;
                let fs = entry.node.fs();
                if let Some(proc_fs) = fs.downcast_ops::<ProcFs>() {
                    if !proc_fs.has_pid_permissions(current_task, &task, HidePid::Invisible) {
                        return error!(ENOENT);
                    }
                }
                let dir = match task.running_state() {
                    Ok(running_state) if task.tid == pid_entry => {
                        let cached = running_state.proc_pid_directory_cache.get_or_init(|| {
                            pid_directory(current_task, &fs, &task, pid_entry.clone())
                        });
                        if cached.try_fs().is_some_and(|cached_fs| Arc::ptr_eq(&cached_fs, &fs)) {
                            cached.clone()
                        } else {
                            pid_directory(current_task, &fs, &task, pid_entry)
                        }
                    }
                    _ => pid_directory(current_task, &fs, &task, pid_entry),
                };
                Ok(dir)
            }
        }
    }
}

/// `ProcDirectory` doesn't implement the `close` method.
impl CloseFreeSafe for ProcDirectory {}
impl FileOps for ProcDirectory {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();

    fn readdir(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        emit_dotdot(file, sink)?;

        // Iterate through all the named entries (i.e., non "task directories") and add them to
        // the sink. Subtract 2 from the offset, to account for `.` and `..`.
        for (name, node) in self.nodes.iter().skip((sink.offset() - 2) as usize) {
            let mode = node.info().mode;
            sink.add(node.ino, sink.offset() + 1, DirectoryEntryType::from_mode(mode), name)?;
        }

        // Add 2 to the number of non-"task directories", to account for `.` and `..`.
        let pid_offset = (self.nodes.len() + 2) as i32;

        // Adjust the offset to account for the other nodes in the directory.
        let adjusted_offset = (sink.offset() - pid_offset as i64) as usize;
        // Sort the pids, to keep the traversal order consistent.
        let mut pids = current_task.kernel().pids.process_ids();
        pids.sort();

        let proc_fs = file.fs.downcast_ops::<ProcFs>();
        let mount_opts = proc_fs.map(|fs| fs.load_mount_options());

        // The adjusted offset is used to figure out which task directories are to be listed.
        if let Some(start) = pids.iter().position(|pid| *pid as usize >= adjusted_offset) {
            for pid in &pids[start..] {
                if let (Some(proc_fs), Some((pid_gid, hide_pid))) = (proc_fs, mount_opts) {
                    if hide_pid >= HidePid::Invisible {
                        let Some(task) = current_task
                            .kernel()
                            .pids
                            .get(*pid)
                            .ok()
                            .and_then(|p| p.get_process_task())
                        else {
                            continue;
                        };
                        if !proc_fs.has_pid_permissions_with_opts(
                            current_task,
                            &task,
                            HidePid::Invisible,
                            pid_gid,
                            hide_pid,
                        ) {
                            continue;
                        }
                    }
                }
                // TODO: Figure out if this inode number is fine, given the content of the task
                // directories.
                let inode_num = file.fs.allocate_ino();
                let name = FsString::from(format!("{pid}"));

                // The + 1 is to set the offset to the next possible pid for subsequent reads.
                let next_offset = (*pid + pid_offset + 1) as i64;
                sink.add(inode_num, next_offset, DirectoryEntryType::DIR, name.as_ref())?;
            }
        }

        Ok(())
    }
}
