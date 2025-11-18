// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module provides a subset of LSM hook implementations that check access based on the
//! Linux capability bits held by the caller.
//!
//! See https://fxbug.dev/440048727 for the full set of hooks that we expect the common capabilities
//! LSM to need to implement.
//!
//! The LSM hooks layer calls these hooks from the appropriate `security::` entrypoint, and the
//! SELinux LSM may also delegate to them.  They should never be called into directly.

use crate::task::CurrentTask;
use crate::vfs::{FsNode, FsStr, XattrOp};
use linux_uapi::XATTR_NAME_CAPS;
use starnix_uapi::auth::CAP_SETFCAP;
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;

/// Corresponds to the `capable()` LSM hook.
pub(super) fn capable(
    current_task: &CurrentTask,
    capability: starnix_uapi::auth::Capabilities,
) -> Result<(), Errno> {
    current_task
        .with_current_creds(|creds| creds.has_capability(capability))
        .then_some(())
        .ok_or_else(|| errno!(EPERM))
}

/// Corresponds to the `inode_setxattr()` LSM hook.
pub(super) fn fs_node_setxattr(
    current_task: &CurrentTask,
    _fs_node: &FsNode,
    name: &FsStr,
    _value: &FsStr,
    _op: XattrOp,
) -> Result<(), Errno> {
    if name == XATTR_NAME_CAPS.to_bytes() {
        return capable(current_task, CAP_SETFCAP);
    }
    Ok(())
}

/// Corresponds to the `inode_removexattr()` LSM hook.
pub(super) fn fs_node_removexattr(
    current_task: &CurrentTask,
    _fs_node: &FsNode,
    name: &FsStr,
) -> Result<(), Errno> {
    if name == XATTR_NAME_CAPS.to_bytes() {
        return capable(current_task, CAP_SETFCAP);
    }
    Ok(())
}
