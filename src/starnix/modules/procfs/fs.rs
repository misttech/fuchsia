// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::proc_directory::ProcDirectory;
use starnix_core::mm::PAGE_SIZE;
use starnix_core::task::{CurrentTask, Task, TaskContainer};
use starnix_core::vfs::fs_args::MountParams;
use starnix_core::vfs::{
    CacheMode, FileSystem, FileSystemHandle, FileSystemOps, FileSystemOptions, FsStr, FsString,
};
use starnix_types::vfs::default_statfs;
use starnix_uapi::auth::{PTRACE_MODE_NOAUDIT, PTRACE_MODE_READ_FSCREDS};
use starnix_uapi::errors::Errno;
use starnix_uapi::mount_flags::FileSystemFlags;
use starnix_uapi::{PROC_SUPER_MAGIC, error, gid_t, statfs};
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};

/// Modes for the `hidepid` mount option on `procfs`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum HidePid {
    #[default]
    Off = 0,
    NoAccess = 1,
    Invisible = 2,
    Ptraceable = 4,
}

impl HidePid {
    fn parse(s: &str) -> Result<Self, Errno> {
        match s {
            "0" | "off" => Ok(Self::Off),
            "1" | "noaccess" => Ok(Self::NoAccess),
            "2" | "invisible" => Ok(Self::Invisible),
            "4" | "ptraceable" => Ok(Self::Ptraceable),
            _ => error!(EINVAL),
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::NoAccess,
            2 => Self::Invisible,
            4 => Self::Ptraceable,
            _ => Self::Off,
        }
    }

    fn as_str(&self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::NoAccess => Some("noaccess"),
            Self::Invisible => Some("invisible"),
            Self::Ptraceable => Some("ptraceable"),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ProcMountParams {
    gid: Option<gid_t>,
    hide_pid: Option<HidePid>,
}

/// Returns a new procfs instance.
///
/// Each mount of procfs creates a new instance with its own mount options.
pub fn proc_fs(
    current_task: &CurrentTask,
    options: FileSystemOptions,
) -> Result<FileSystemHandle, Errno> {
    ProcFs::new_fs(current_task, options)
}

/// `ProcFs` is a filesystem that exposes runtime information about a `Kernel` instance.
#[derive(Debug, Default)]
pub struct ProcFs {
    /// Packed mount options: lower 32 bits store `pid_gid` (`gid_t`), bits 32..40 store `hide_pid`.
    mount_options: AtomicU64,
}

impl FileSystemOps for ProcFs {
    fn statfs(&self, _fs: &FileSystem, _current_task: &CurrentTask) -> Result<statfs, Errno> {
        Ok(default_statfs(PROC_SUPER_MAGIC))
    }

    fn name(&self) -> &'static FsStr {
        "proc".into()
    }

    fn reconfigure(
        &self,
        fs: &FileSystem,
        _current_task: &CurrentTask,
        new_flags: FileSystemFlags,
        params: &MountParams,
    ) -> Result<(), Errno> {
        self.apply_mount_params(params)?;
        fs.options.flags.store(new_flags, Ordering::Relaxed);
        Ok(())
    }

    fn show_options(&self, _fs: &FileSystem) -> Result<FsString, Errno> {
        let (pid_gid, hide_pid) = self.load_mount_options();
        let mut opts = String::new();
        if pid_gid != 0 {
            let _ = write!(&mut opts, ",gid={pid_gid}");
        }
        if let Some(hide_pid) = hide_pid.as_str() {
            let _ = write!(&mut opts, ",hidepid={hide_pid}");
        }
        Ok(opts.into())
    }

    fn default_blksize(&self) -> Option<usize> {
        Some(*PAGE_SIZE as usize)
    }
}

impl ProcFs {
    /// Creates a new instance of `ProcFs` for the given `kernel`.
    pub fn new_fs(
        current_task: &CurrentTask,
        options: FileSystemOptions,
    ) -> Result<FileSystemHandle, Errno> {
        let params = Self::parse_mount_params(&options.params)?;
        let proc_fs = ProcFs::default();
        proc_fs.set_mount_params(params);
        let kernel = current_task.kernel();
        let fs = FileSystem::new(kernel, CacheMode::Uncached, proc_fs, options)?;
        let root_ino = fs.allocate_ino();
        fs.create_root(root_ino, ProcDirectory::new(kernel, &fs));
        Ok(fs)
    }

    fn pack_mount_options(pid_gid: gid_t, hide_pid: HidePid) -> u64 {
        (pid_gid as u64) | ((hide_pid as u64) << 32)
    }

    fn unpack_mount_options(raw: u64) -> (gid_t, HidePid) {
        let pid_gid = raw as gid_t;
        let hide_pid = HidePid::from_u8((raw >> 32) as u8);
        (pid_gid, hide_pid)
    }

    pub fn load_mount_options(&self) -> (gid_t, HidePid) {
        Self::unpack_mount_options(self.mount_options.load(Ordering::Relaxed))
    }

    fn parse_mount_params(params: &MountParams) -> Result<ProcMountParams, Errno> {
        let gid = params.get_as::<gid_t>(b"gid")?;
        let hide_pid = params.get_with(b"hidepid", HidePid::parse)?;
        Ok(ProcMountParams { gid, hide_pid })
    }

    fn set_mount_params(&self, params: ProcMountParams) {
        let _ = self.mount_options.try_update(Ordering::Relaxed, Ordering::Relaxed, |raw| {
            let (cur_gid, cur_hide_pid) = Self::unpack_mount_options(raw);
            let new_gid = params.gid.unwrap_or(cur_gid);
            let new_hide_pid = params.hide_pid.unwrap_or(cur_hide_pid);
            Some(Self::pack_mount_options(new_gid, new_hide_pid))
        });
    }

    fn apply_mount_params(&self, params: &MountParams) -> Result<(), Errno> {
        let parsed = Self::parse_mount_params(params)?;
        self.set_mount_params(parsed);
        Ok(())
    }

    pub fn has_pid_permissions_with_opts(
        &self,
        current_task: &CurrentTask,
        target_task: &Task,
        hide_pid_min: HidePid,
        pid_gid: gid_t,
        hide_pid: HidePid,
    ) -> bool {
        if current_task.pid == target_task.pid {
            return true;
        }
        if hide_pid != HidePid::Ptraceable {
            if hide_pid < hide_pid_min {
                return true;
            }
            let creds = current_task.current_creds();
            if creds.fsgid == pid_gid || creds.groups.contains(&pid_gid) {
                return true;
            }
        }
        current_task
            .check_ptrace_access_mode(PTRACE_MODE_READ_FSCREDS | PTRACE_MODE_NOAUDIT, target_task)
            .is_ok()
    }

    pub fn has_pid_permissions(
        &self,
        current_task: &CurrentTask,
        target_task: &Task,
        hide_pid_min: HidePid,
    ) -> bool {
        let (pid_gid, hide_pid) = self.load_mount_options();
        self.has_pid_permissions_with_opts(
            current_task,
            target_task,
            hide_pid_min,
            pid_gid,
            hide_pid,
        )
    }

    pub fn check_pid_access(
        &self,
        current_task: &CurrentTask,
        target: &TaskContainer,
        hide_pid_min: HidePid,
    ) -> Result<(), Errno> {
        let (pid_gid, hide_pid) = self.load_mount_options();
        let Ok(target_task) = target.get_task() else {
            if hide_pid >= HidePid::Invisible {
                return error!(ENOENT);
            }
            return error!(ESRCH);
        };
        if !self.has_pid_permissions_with_opts(
            current_task,
            &target_task,
            hide_pid_min,
            pid_gid,
            hide_pid,
        ) {
            if hide_pid >= HidePid::Invisible {
                return error!(ENOENT);
            }
            return error!(EPERM);
        }
        Ok(())
    }
}
