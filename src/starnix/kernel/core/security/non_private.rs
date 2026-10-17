// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{Auditable, common_cap, selinux_hooks};
use crate::vfs::socket::{Socket, SocketFile, SocketHandle};
use crate::vfs::{DirEntryHandle, DowncastedFile, FsNode};
use std::ops::Deref;

/// Opaque structure used by [`crate::vfs::FsNode`] to store security state.
///
/// Its inner state is only accessible via [`NonPrivateFsNode`], guaranteeing at
/// compile time that kernel-private [`FsNode`]s never have their security state
/// read or mutated.
#[derive(Debug, Default)]
pub struct FsNodeState {
    selinux: selinux_hooks::FsNodeState,
    common_cap: common_cap::FsNodeState,
}

impl FsNodeState {
    #[cfg(test)]
    pub(super) fn selinux_for_test(&self) -> &selinux_hooks::FsNodeState {
        &self.selinux
    }
}

/// Borrowed reference to an [`FsNode`] verified not to be kernel-private.
#[derive(Clone, Copy, Debug)]
pub(super) struct NonPrivateFsNode<'a> {
    fs_node: &'a FsNode,
}

impl<'a> NonPrivateFsNode<'a> {
    /// Returns `Some` if `fs_node` is not marked kernel-private.
    pub(super) fn new(fs_node: &'a FsNode) -> Option<Self> {
        (!fs_node.is_private()).then_some(Self { fs_node })
    }

    /// Asserts in debug builds that `fs_node` is not kernel-private, returning `None`
    /// if the invariant is violated in release builds so callers can fail-safe.
    pub(super) fn assert_from(fs_node: &'a FsNode) -> Option<Self> {
        debug_assert!(!fs_node.is_private());
        Self::new(fs_node)
    }

    /// Returns the underlying [`FsNode`] reference.
    pub(super) fn as_fs_node(self) -> &'a FsNode {
        self.fs_node
    }

    /// Returns the SELinux [`selinux_hooks::FsNodeState`] for this non-private node.
    pub(super) fn selinux_state(self) -> &'a selinux_hooks::FsNodeState {
        &self.fs_node.security_state.selinux
    }

    /// Returns the common capabilities [`common_cap::FsNodeState`] for this non-private node.
    pub(super) fn common_cap_state(self) -> &'a common_cap::FsNodeState {
        &self.fs_node.security_state.common_cap
    }
}

impl Deref for NonPrivateFsNode<'_> {
    type Target = FsNode;

    fn deref(&self) -> &Self::Target {
        self.fs_node
    }
}

impl<'a> From<NonPrivateFsNode<'a>> for Auditable<'a> {
    fn from(value: NonPrivateFsNode<'a>) -> Self {
        Auditable::FsNode(value.fs_node)
    }
}

/// Borrowed reference to a [`DirEntryHandle`] whose [`FsNode`] is verified not to be
/// kernel-private.
#[derive(Clone, Copy, Debug)]
pub(super) struct NonPrivateDirEntry<'a> {
    dir_entry: &'a DirEntryHandle,
    fs_node: NonPrivateFsNode<'a>,
}

impl<'a> NonPrivateDirEntry<'a> {
    /// Returns `Some` if `dir_entry.node` is not marked kernel-private.
    pub(super) fn new(dir_entry: &'a DirEntryHandle) -> Option<Self> {
        let fs_node = NonPrivateFsNode::new(&dir_entry.node)?;
        Some(Self { dir_entry, fs_node })
    }

    /// Asserts in debug builds that `dir_entry.node` is not kernel-private, returning `None`
    /// if the invariant is violated in release builds so callers can fail-safe.
    pub(super) fn assert_from(dir_entry: &'a DirEntryHandle) -> Option<Self> {
        let fs_node = NonPrivateFsNode::assert_from(&dir_entry.node)?;
        Some(Self { dir_entry, fs_node })
    }

    /// Returns the underlying [`DirEntryHandle`] reference.
    pub(super) fn as_dir_entry(self) -> &'a DirEntryHandle {
        self.dir_entry
    }

    /// Returns the verified [`NonPrivateFsNode`] for this directory entry.
    pub(super) fn node(self) -> NonPrivateFsNode<'a> {
        self.fs_node
    }
}

impl Deref for NonPrivateDirEntry<'_> {
    type Target = DirEntryHandle;

    fn deref(&self) -> &Self::Target {
        self.dir_entry
    }
}

impl<'a> From<NonPrivateDirEntry<'a>> for Auditable<'a> {
    fn from(value: NonPrivateDirEntry<'a>) -> Self {
        value.dir_entry.into()
    }
}

/// Borrowed reference to a [`Socket`] verified not to be backed by a kernel-private [`FsNode`].
#[derive(Clone, Copy)]
pub(super) struct NonPrivateSocket<'a> {
    socket: &'a Socket,
    fs_node: Option<NonPrivateFsNode<'a>>,
}

impl<'a> NonPrivateSocket<'a> {
    /// Returns `Some` if `socket` is not backed by a kernel-private [`FsNode`].
    pub(super) fn new(socket: &'a Socket) -> Option<Self> {
        let fs_node = match socket.fs_node() {
            Some(node) => Some(NonPrivateFsNode::new(node)?),
            None => None,
        };
        Some(Self { socket, fs_node })
    }

    /// Returns the socket's [`NonPrivateFsNode`], if an [`FsNode`] has been attached.
    pub(super) fn fs_node(self) -> Option<NonPrivateFsNode<'a>> {
        self.fs_node
    }
}

impl Deref for NonPrivateSocket<'_> {
    type Target = Socket;

    fn deref(&self) -> &Self::Target {
        self.socket
    }
}

/// Borrowed reference to a [`DowncastedFile<SocketFile>`] whose [`FsNode`] is verified not to
/// be kernel-private.
#[derive(Clone, Copy)]
pub(super) struct NonPrivateSocketFile<'a> {
    socket_file: DowncastedFile<'a, SocketFile>,
    fs_node: NonPrivateFsNode<'a>,
}

impl<'a> NonPrivateSocketFile<'a> {
    /// Returns `Some` if `socket_file`'s [`FsNode`] is not marked kernel-private.
    pub(super) fn new(socket_file: DowncastedFile<'a, SocketFile>) -> Option<Self> {
        let fs_node = NonPrivateFsNode::new(socket_file.file().node())?;
        Some(Self { socket_file, fs_node })
    }

    /// Asserts in debug builds that `socket_file`'s [`FsNode`] is not kernel-private,
    /// returning `None` if the invariant is violated in release builds so callers can
    /// fail-safe.
    pub(super) fn assert_from(socket_file: DowncastedFile<'a, SocketFile>) -> Option<Self> {
        let fs_node = NonPrivateFsNode::assert_from(socket_file.file().node())?;
        Some(Self { socket_file, fs_node })
    }

    /// Returns the underlying [`SocketHandle`].
    pub(super) fn socket(self) -> &'a SocketHandle {
        self.socket_file.socket()
    }

    /// Returns the verified [`NonPrivateFsNode`] for this socket file.
    pub(super) fn node(self) -> NonPrivateFsNode<'a> {
        self.fs_node
    }
}
