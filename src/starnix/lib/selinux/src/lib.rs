// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![warn(variant_size_differences)]

mod exceptions_config;
mod kernel;
pub mod permission_check;
pub mod policy;
pub mod security_server;
mod sid_table;
mod sync;

pub use kernel::*;
pub use policy::{ClassId, FsUseType, PolicyCap};
pub use security_server::{PolicySeqNo, SecurityServer};
pub use starnix_uapi::selinux::{InitialSid, ReferenceInitialSid, SecurityId, TaskAttrs};

/// Hash table distribution statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HashStats {
    /// Total number of active entries across all buckets.
    pub entries: usize,
    /// Number of buckets containing at least one entry.
    pub buckets_used: usize,
    /// Total number of buckets in the table.
    pub total_buckets: usize,
    /// Maximum number of entries in any single bucket.
    pub longest_chain: usize,
}

impl std::ops::Add for &HashStats {
    type Output = HashStats;

    fn add(self, other: &HashStats) -> HashStats {
        HashStats {
            entries: self.entries + other.entries,
            buckets_used: self.buckets_used + other.buckets_used,
            total_buckets: self.total_buckets + other.total_buckets,
            longest_chain: self.longest_chain.max(other.longest_chain),
        }
    }
}

/// A borrowed byte slice that contains no `NUL` characters by truncating the input slice at the
/// first `NUL` (if any) upon construction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NullessByteStr<'a>(&'a [u8]);

impl<'a> NullessByteStr<'a> {
    /// Returns a non-null-terminated representation of the security context string.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl<'a, S: AsRef<[u8]> + ?Sized> From<&'a S> for NullessByteStr<'a> {
    /// Any `AsRef<[u8]>` can be processed into a [`NullessByteStr`]. The [`NullessByteStr`] will
    /// retain everything up to (but not including) a null character, or else the complete byte
    /// string.
    fn from(s: &'a S) -> Self {
        let value = s.as_ref();
        match value.iter().position(|c| *c == 0) {
            Some(end) => Self(&value[..end]),
            None => Self(value),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileSystemMountSids {
    pub context: Option<SecurityId>,
    pub fs_context: Option<SecurityId>,
    pub def_context: Option<SecurityId>,
    pub root_context: Option<SecurityId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileSystemLabel {
    pub sid: SecurityId,
    pub scheme: FileSystemLabelingScheme,
    // Sids obtained by parsing the mount options of the FileSystem.
    pub mount_sids: FileSystemMountSids,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FileSystemLabelingScheme {
    /// This filesystem was mounted with "context=".
    Mountpoint { sid: SecurityId },
    /// This filesystem has an "fs_use_xattr", "fs_use_task", or "fs_use_trans" entry in the
    /// policy. If the `fs_use_type` is "fs_use_xattr" then the `default_sid` specifies the SID
    /// with which to label `FsNode`s of files that do not have the "security.selinux" xattr.
    FsUse { fs_use_type: FsUseType, default_sid: SecurityId },
    /// This filesystem has one or more "genfscon" statements associated with it in the policy.
    /// If `supports_seclabel` is true then nodes in the filesystem may be dynamically relabeled.
    GenFsCon { supports_seclabel: bool },
    /// This filesystem has no labeling scheme specified in the policy.
    None,
}

/// SELinux security context-related filesystem mount options. These options are documented in the
/// `context=context, fscontext=context, defcontext=context, and rootcontext=context` section of
/// the `mount(8)` manpage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileSystemMountOptions {
    /// Specifies the effective security context to use for all nodes in the filesystem, and the
    /// filesystem itself. If the filesystem already contains security attributes then these are
    /// ignored. May not be combined with any of the other options.
    pub context: Option<Vec<u8>>,
    /// Specifies an effective security context to use for un-labeled nodes in the filesystem,
    /// rather than falling-back to the policy-defined "file" context.
    pub def_context: Option<Vec<u8>>,
    /// The value of the `fscontext=[security-context]` mount option. This option is used to
    /// label the filesystem (superblock) itself.
    pub fs_context: Option<Vec<u8>>,
    /// The value of the `rootcontext=[security-context]` mount option. This option is used to
    /// (re)label the inode located at the filesystem mountpoint.
    pub root_context: Option<Vec<u8>>,
}

/// Status information parameter for the [`SeLinuxStatusPublisher`] interface.
pub struct SeLinuxStatus {
    /// SELinux-wide enforcing vs. permissive mode bit.
    pub is_enforcing: bool,
    /// Number of times the policy has been changed since SELinux started.
    pub change_count: u32,
    /// Bit indicating whether operations unknown SELinux abstractions will be denied.
    pub deny_unknown: bool,
}

/// Interface for security server to interact with selinuxfs status file.
pub trait SeLinuxStatusPublisher: Send + Sync {
    /// Sets the value part of the associated selinuxfs status file.
    fn set_status(&mut self, policy_status: SeLinuxStatus);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nulless_byte_str_equivalence() {
        let unterminated: NullessByteStr<'_> = b"u:object_r:test_valid_t:s0".into();
        let nul_terminated: NullessByteStr<'_> = b"u:object_r:test_valid_t:s0\0".into();
        let nul_containing: NullessByteStr<'_> =
            b"u:object_r:test_valid_t:s0\0IGNORE THIS\0!\0".into();

        for context in [nul_terminated, nul_containing] {
            assert_eq!(unterminated, context);
            assert_eq!(unterminated.as_bytes(), context.as_bytes());
        }
    }
}
