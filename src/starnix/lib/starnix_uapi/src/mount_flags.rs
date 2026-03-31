// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::uapi;
use atomic_bitflags::atomic_bitflags;

atomic_bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct MountFlags: u32 {
        // per-mountpoint flags
        const RDONLY = uapi::MS_RDONLY;
        const NOEXEC = uapi::MS_NOEXEC;
        const NOSUID = uapi::MS_NOSUID;
        const NODEV = uapi::MS_NODEV;
        const NOATIME = uapi::MS_NOATIME;
        const NODIRATIME = uapi::MS_NODIRATIME;
        const RELATIME = uapi::MS_RELATIME;
        const STRICTATIME = uapi::MS_STRICTATIME;

        // per-superblock flags
        const SILENT = uapi::MS_SILENT;
        const LAZYTIME = uapi::MS_LAZYTIME;
        const SYNCHRONOUS = uapi::MS_SYNCHRONOUS;
        const DIRSYNC = uapi::MS_DIRSYNC;
        const MANDLOCK = uapi::MS_MANDLOCK;

        // mount() control flags
        const REMOUNT = uapi::MS_REMOUNT;
        const BIND = uapi::MS_BIND;
        const REC = uapi::MS_REC;
        const DOWNSTREAM = uapi::MS_SLAVE;
        const SHARED = uapi::MS_SHARED;
        const PRIVATE = uapi::MS_PRIVATE;

        /// Flags that change be changed with REMOUNT.
        ///
        /// MS_DIRSYNC and MS_SILENT cannot be changed with REMOUNT.
        const CHANGEABLE_WITH_REMOUNT = MountpointFlags::all().bits() |
            Self::MANDLOCK.bits() | Self::LAZYTIME.bits() | Self::SYNCHRONOUS.bits();
    }
}

atomic_bitflags! {
    /// Subset of `MountFlags` that allow the behaviours of different mountpoints to the same
    /// underlying `FileSystem` to be independently configured.
    /// Note that
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct MountpointFlags: u32 {
        // Flags stored for each mountpoint to configure its behaviour.
        const RDONLY = MountFlags::RDONLY.bits();
        const NOEXEC = MountFlags::NOEXEC.bits();
        const NOSUID = MountFlags::NOSUID.bits();
        const NODEV = MountFlags::NODEV.bits();
        const NOATIME = MountFlags::NOATIME.bits();
        const NODIRATIME = MountFlags::NODIRATIME.bits();
        const RELATIME = MountFlags::RELATIME.bits();

        const STORED_ON_MOUNT = Self::RDONLY.bits() | Self::NOEXEC.bits() | Self::NOSUID.bits() |
            Self::NODEV.bits() | Self::NOATIME.bits() | Self::NODIRATIME.bits() | Self::RELATIME.bits();

        // Flags affecting the behaviour of a single operation on a mountpoint.
        const STRICTATIME = MountFlags::STRICTATIME.bits();
        const REC = MountFlags::REC.bits();
    }
}

impl From<MountpointFlags> for MountFlags {
    fn from(flags: MountpointFlags) -> Self {
        // MountpointFlags is defined using only bits that are valid for MountFlags.
        Self::from_bits_retain(flags.bits())
    }
}

atomic_bitflags! {
    /// Subset of `MountFlags` that affect `FileSystem` behaviour.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct FileSystemFlags: u32 {
        const RDONLY = MountFlags::RDONLY.bits();
        const DIRSYNC = MountFlags::DIRSYNC.bits();
        const LAZYTIME = MountFlags::LAZYTIME.bits();
        const MANDLOCK = MountFlags::MANDLOCK.bits();
        const SILENT = MountFlags::SILENT.bits();
        const SYNCHRONOUS = MountFlags::SYNCHRONOUS.bits();
    }
}

impl From<FileSystemFlags> for MountFlags {
    fn from(flags: FileSystemFlags) -> Self {
        // FileSystemFlags is defined using only bits that are valid for MountFlags.
        Self::from_bits_retain(flags.bits())
    }
}

impl MountFlags {
    pub fn mountpoint_flags(&self) -> MountpointFlags {
        MountpointFlags::from_bits_truncate(self.bits())
    }

    pub fn file_system_flags(&self) -> FileSystemFlags {
        FileSystemFlags::from_bits_truncate(self.bits())
    }
}

impl std::fmt::Display for MountpointFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        MountFlags::from(*self).fmt(f)
    }
}

impl std::fmt::Display for FileSystemFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        MountFlags::from(*self).fmt(f)
    }
}

impl std::fmt::Display for MountFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", if self.contains(Self::RDONLY) { "ro" } else { "rw" })?;
        if self.contains(Self::NOEXEC) {
            write!(f, ",noexec")?;
        }
        if self.contains(Self::NOSUID) {
            write!(f, ",nosuid")?;
        }
        if self.contains(Self::NODEV) {
            write!(f, ",nodev")?
        }
        if self.contains(Self::NOATIME) {
            write!(f, ",noatime")?;
        }
        if self.contains(Self::SILENT) {
            write!(f, ",silent")?;
        }
        if self.contains(Self::BIND) {
            write!(f, ",bind")?;
        }
        if self.contains(Self::LAZYTIME) {
            write!(f, ",lazytime")?;
        }
        Ok(())
    }
}
