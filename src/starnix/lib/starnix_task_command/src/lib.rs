// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![warn(missing_docs)]

//! The `TaskCommand` type and associated functions.

use flyweights::FlyByteStr;

/// The command for a task.
///
/// Linux task commands are limited to 15 bytes, but Fuchsia allows longer names in places. It's
/// useful to store longer names diagnostics and debugging information.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskCommand(FlyByteStr);

impl TaskCommand {
    /// Create a new `TaskCommand` from a byte slice. The byte slice is truncated at the first null
    /// byte if any.
    pub fn new(name: &[u8]) -> Self {
        let name = if let Some(idx) = memchr::memchr(b'\0', name) { &name[..idx] } else { name };
        Self(FlyByteStr::new(name))
    }

    /// Create a new `TaskCommand` from a path. The basename of the path is used as the name.
    pub fn from_path_bytes(path: &[u8]) -> Self {
        let basename =
            if let Some(idx) = memchr::memrchr(b'/', path) { &path[idx + 1..] } else { path };
        Self::new(basename)
    }

    /// Returns the name truncated to 15 bytes.
    pub fn comm_name(&self) -> &[u8] {
        let bytes = self.as_bytes();
        &bytes[..std::cmp::min(bytes.len(), 15)]
    }

    /// Returns the name as a 16-byte array, null-terminated if shorter than 16 bytes,
    /// as expected by `prctl(PR_GET_NAME)`.
    pub fn prctl_name(&self) -> [u8; 16] {
        let mut name = [0u8; 16];
        let comm = self.comm_name();
        name[..comm.len()].copy_from_slice(comm);
        name
    }

    /// Returns the entire name as a byte slice.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl Default for TaskCommand {
    fn default() -> Self {
        Self::new(b"")
    }
}

impl std::fmt::Debug for TaskCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Display for TaskCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        assert_eq!(TaskCommand::new(b"foo").as_bytes(), b"foo");
        assert_eq!(TaskCommand::new(b"foo\0bar").as_bytes(), b"foo");
    }

    #[test]
    fn test_from_path_bytes() {
        assert_eq!(TaskCommand::from_path_bytes(b"/foo/bar").as_bytes(), b"bar");
        assert_eq!(TaskCommand::from_path_bytes(b"bar").as_bytes(), b"bar");
        assert_eq!(TaskCommand::from_path_bytes(b"/bar").as_bytes(), b"bar");
    }

    #[test]
    fn test_comm_name() {
        assert_eq!(TaskCommand::new(b"short").comm_name(), b"short");
        assert_eq!(TaskCommand::new(b"0123456789abcdef").comm_name(), b"0123456789abcde");
        assert_eq!(TaskCommand::new(b"0123456789abcdefg").comm_name(), b"0123456789abcde");
    }

    #[test]
    fn test_prctl_name() {
        assert_eq!(TaskCommand::new(b"short").prctl_name(), *b"short\0\0\0\0\0\0\0\0\0\0\0");
        assert_eq!(TaskCommand::new(b"0123456789abcdef").prctl_name(), *b"0123456789abcde\0");
        assert_eq!(TaskCommand::new(b"0123456789abcdefg").prctl_name(), *b"0123456789abcde\0");
    }

    #[test]
    fn test_debug() {
        assert_eq!(format!("{:?}", TaskCommand::new(b"foo")), "\"foo\"");
    }

    #[test]
    fn test_display() {
        assert_eq!(TaskCommand::new(b"foo").to_string(), "foo");
    }
}
