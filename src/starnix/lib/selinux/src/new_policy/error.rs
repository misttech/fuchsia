// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::metadata::{
    CONFIG_MLS_FLAG, POLICYDB_SIGNATURE, POLICYDB_STRING_MAX_LENGTH, POLICYDB_VERSION_MAX,
    POLICYDB_VERSION_MIN, SELINUX_MAGIC,
};
use thiserror::Error;

/// Errors that may be encountered parsing a binary policy.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ParseError {
    #[error("expected MLS-enabled flag ({CONFIG_MLS_FLAG:#032b}), but found {found_config:#032b}")]
    ConfigMissingMlsFlag { found_config: u32 },
    #[error("invalid configuration flags: {flags:#032b}")]
    InvalidConfigFlags { flags: u32 },
    #[error("expected data item of type {type_name} ({type_size} bytes), but found {num_bytes}")]
    MissingData { type_name: &'static str, type_size: usize, num_bytes: usize },
    #[error("expected end of policy, but found {num_bytes} additional bytes")]
    TrailingBytes { num_bytes: usize },
}

/// Errors that may be encountered validating a binary policy.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ValidateError {
    #[error("expected selinux magic value {SELINUX_MAGIC:#x}, but found {found_magic:#x}")]
    InvalidMagic { found_magic: u32 },
    #[error(
        "expected policy version in range [{POLICYDB_VERSION_MIN}, {POLICYDB_VERSION_MAX}], but found {found_policy_version}"
    )]
    InvalidPolicyVersion { found_policy_version: u32 },
    #[error("expected signature {POLICYDB_SIGNATURE:?}, but found {:?}", bstr::BStr::new(found_signature.as_slice()))]
    InvalidSignature { found_signature: Vec<u8> },
    #[error(
        "expected signature length in range [0, {POLICYDB_STRING_MAX_LENGTH}], but found {found_length}"
    )]
    InvalidSignatureLength { found_length: u32 },
    #[error("undefined {kind} Id value {id}")]
    UnknownId { kind: &'static str, id: u32 },
}

/// Errors that may be encountered serializing a binary policy.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum SerializeError {
    #[error("unknown serialization error")]
    Unknown,
}
