// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use crate::vfs::FileHandle;
use starnix_uapi::errors::Errno;
use starnix_uapi::from_status_like_fdio;
use starnix_uapi::open_flags::OpenFlags;

mod remote;
mod remote_bundle;
mod remote_unix_domain_socket;
mod remote_volume;
mod syslog;
mod timer;

pub mod sync_file;
pub mod zxio;

pub use remote::*;
pub use remote_bundle::RemoteBundle;
pub use remote_unix_domain_socket::*;
pub use remote_volume::*;
pub use syslog::*;
pub use timer::*;

/// Creates a [`FileHandle`] from a [`zx::NullableHandle`].
///
/// Wraps [`zx::ObjectType::SOCKET`] handles (such as component stdio handles) in `pipefs`-backed
/// FIFO (`S_IFIFO`) nodes via [`create_fuchsia_pipe`], and uses [`new_remote_file`] for all other
/// handle types.
pub fn create_file_from_handle(
    current_task: &CurrentTask,
    handle: zx::NullableHandle,
) -> Result<FileHandle, Errno> {
    let info = handle.basic_info().map_err(|status| from_status_like_fdio!(status))?;
    if info.object_type == zx::ObjectType::SOCKET {
        create_fuchsia_pipe(current_task, zx::Socket::from(handle), OpenFlags::RDWR)
    } else {
        new_remote_file(current_task, handle, OpenFlags::RDWR)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::testing::*;

    #[fuchsia::test]
    async fn test_create_from_invalid_handle() {
        spawn_kernel_and_run(async |current_task| {
            assert!(create_file_from_handle(current_task, zx::NullableHandle::invalid()).is_err());
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_create_pipe_from_handle() {
        spawn_kernel_and_run(async |current_task| {
            let (left_handle, right_handle) = zx::Socket::create_stream();
            create_file_from_handle(current_task, left_handle.into_handle())
                .expect("failed to create left FileHandle");
            create_file_from_handle(current_task, right_handle.into_handle())
                .expect("failed to create right FileHandle");
        })
        .await;
    }
}
