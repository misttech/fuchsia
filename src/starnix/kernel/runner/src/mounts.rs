// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, anyhow, bail};
use fidl_fuchsia_io as fio;
use starnix_core::fs::fuchsia::{RemoteBundle, new_remotefs_in_root};
use starnix_core::fs::tmpfs::TmpFs;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::fs_args::MountParams;
use starnix_core::vfs::{FileSystemHandle, FileSystemOptions, FsString};
use starnix_logging::log_info;

use starnix_uapi::mount_flags::{MountFlags, MountpointFlags};

pub struct MountAction {
    pub path: FsString,
    pub fs: FileSystemHandle,
    pub flags: MountpointFlags,
}

impl MountAction {
    pub fn new_for_root(
        kernel: &Kernel,
        pkg: &fio::DirectorySynchronousProxy,
        spec: &str,
    ) -> Result<MountAction, Error> {
        let (spec, options) = MountSpec::parse(spec)?;
        assert_eq!(spec.mount_point.as_slice(), b"/");
        let rights = fio::PERM_READABLE | fio::PERM_EXECUTABLE;

        // We only support mounting these file systems at the root.
        // The root file system needs to be creatable without a task because we mount the root
        // file system before creating the initial task.
        let fs = match spec.fs_type.as_slice() {
            b"remote_bundle" => RemoteBundle::new_fs_in_base(kernel, pkg, options, rights)?,
            b"remote_pkg_subdir" => new_remotefs_in_root(kernel, pkg, options, rights)?,
            b"tmpfs" => TmpFs::new_fs_with_options(kernel, options)?,
            _ => bail!("unsupported root file system: {}", spec.fs_type),
        };

        Ok(spec.into_action(fs))
    }

    pub fn from_spec(
        current_task: &CurrentTask,
        pkg: &fio::DirectorySynchronousProxy,
        spec_str: &str,
    ) -> Result<Option<MountAction>, Error> {
        let (spec, options) = MountSpec::parse(spec_str)?;
        let rights = fio::PERM_READABLE | fio::PERM_EXECUTABLE;

        let fs = match spec.fs_type.as_slice() {
            // The remote_bundle file system is available only via the mounts declaration in CML.
            b"remote_bundle" => {
                RemoteBundle::new_fs_in_base(current_task.kernel(), pkg, options, rights)
            }

            // Mounts a subdirectory of the container's `/pkg`.
            b"remote_pkg_subdir" => {
                new_remotefs_in_root(current_task.kernel(), pkg, options, rights)
                    .map_err(Error::from)
            }

            _ => {
                current_task.create_filesystem(spec.fs_type.as_ref(), options).map_err(Error::from)
            }
        };

        let fs = match fs {
            Ok(fs) => fs,
            Err(err) if spec.nofail => {
                log_info!("Skipping mount {spec_str:?} due to nofail: {err:#}");
                return Ok(None);
            }
            Err(err) => return Err(err),
        };

        Ok(Some(spec.into_action(fs)))
    }
}

struct MountSpec {
    mount_point: FsString,
    fs_type: FsString,
    flags: MountFlags,
    nofail: bool,
}

impl MountSpec {
    fn parse(spec: &str) -> Result<(MountSpec, FileSystemOptions), Error> {
        let mut iter = spec.splitn(4, ':');
        let mount_point =
            iter.next().ok_or_else(|| anyhow!("mount point is missing from {:?}", spec))?;
        let fs_type = iter.next().ok_or_else(|| anyhow!("fs type is missing from {:?}", spec))?;
        let fs_src = match iter.next() {
            Some(src) if !src.is_empty() => src,
            _ => ".",
        };

        let mut params = MountParams::parse(iter.next().unwrap_or_default().into())?;
        let flags = params.remove_mount_flags();
        let nofail = params.remove(b"nofail").is_some();

        Ok((
            MountSpec { fs_type: fs_type.into(), mount_point: mount_point.into(), flags, nofail },
            FileSystemOptions {
                source: fs_src.into(),
                flags: flags.file_system_flags().into(),
                params,
            },
        ))
    }

    fn into_action(self, fs: FileSystemHandle) -> MountAction {
        MountAction { path: self.mount_point, fs, flags: self.flags.mountpoint_flags() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::fuchsia::test]
    fn test_parse_mount_spec_nofail() {
        let (spec, options) =
            MountSpec::parse("/product:remote_bundle:data/product:nodev,relatime,nofail").unwrap();
        assert_eq!(spec.mount_point, "/product");
        assert_eq!(spec.fs_type, "remote_bundle");
        assert_eq!(spec.flags, MountFlags::NODEV | MountFlags::RELATIME);
        assert!(spec.nofail);
        assert_eq!(options.source, "data/product");
        assert!(options.params.is_empty());

        let (spec, options) =
            MountSpec::parse("/vendor:remote_bundle:data/vendor:nodev,relatime").unwrap();
        assert_eq!(spec.mount_point, "/vendor");
        assert_eq!(spec.fs_type, "remote_bundle");
        assert_eq!(spec.flags, MountFlags::NODEV | MountFlags::RELATIME);
        assert!(!spec.nofail);
        assert_eq!(options.source, "data/vendor");
        assert!(options.params.is_empty());
    }
}
