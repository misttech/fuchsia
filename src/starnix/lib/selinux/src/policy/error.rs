// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bstr::BString;
use thiserror::Error;

use super::bitmap::MAP_NODE_BITS;
use super::metadata::{
    CONFIG_MLS_FLAG, POLICYDB_SIGNATURE, POLICYDB_VERSION_MAX, POLICYDB_VERSION_MIN, SELINUX_MAGIC,
};
use super::rules::{RuleKey, RuleKind};
use crate::InitialSid;

/// Errors that may be encountered parsing a binary policy.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("expected MLS-enabled flag ({CONFIG_MLS_FLAG:#032b}), but found {found_config:#032b}")]
    ConfigMissingMlsFlag { found_config: u32 },
    #[error("invalid configuration flags: {flags:#032b}")]
    InvalidConfigFlags { flags: u32 },
    #[error("invalid constraint operand type: {value:#x}")]
    InvalidConstraintOperandType { value: u32 },
    #[error("invalid constraint term type: {value}")]
    InvalidConstraintTermType { value: u32 },
    #[error("invalid enum value for {enum_name}: {value}")]
    InvalidEnumValue { enum_name: &'static str, value: u64 },
    #[error("expected extensible bitmap high bit to be {expected}, but found {found}")]
    InvalidExtensibleBitmapHighBit { expected: u32, found: u32 },
    #[error("invalid extensible bitmap item start bit: {found_start_bit}")]
    InvalidExtensibleBitmapItemStartBit { found_start_bit: u32 },
    #[error(
        "expected extensible bitmap item size to be exactly {MAP_NODE_BITS}, but found {found_size}"
    )]
    InvalidExtensibleBitmapItemSize { found_size: u32 },
    #[error("invalid ID value: {value}")]
    InvalidId { value: u32 },
    #[error("expected primary names count <= {expected_at_most}, but found {found}")]
    InvalidPrimaryNamesCount { expected_at_most: u32, found: u32 },
    #[error("expected data item of type {type_name} ({type_size} bytes), but found {num_bytes}")]
    MissingData { type_name: &'static str, type_size: usize, num_bytes: usize },
    #[error("expected end of policy, but found {num_bytes} additional bytes")]
    TrailingBytes { num_bytes: usize },
    #[error("unexpected non-empty type set in constraint")]
    UnexpectedConstraintTypeSet,
    #[error("duplicate rule for key {key:?}, kind: {kind:?}")]
    DuplicateAccessVectorRule { key: RuleKey, kind: RuleKind },
    #[error(
        "duplicate filename transition for target_type {target_type:?}, target_class {target_class:?}, filename {filename:?}"
    )]
    DuplicateFilenameTransition {
        target_type: super::TypeId,
        target_class: super::ClassId,
        filename: BString,
    },
    #[error("duplicate ID: {id}")]
    DuplicateId { id: u32 },
    #[error("duplicate name: {name:?}")]
    DuplicateName { name: BString },
    #[error("index out of range: {index} (maximum supported is {max})")]
    IndexOutOfRange { index: usize, max: usize },
    #[error("observed size {observed} exceeds limit {limit}")]
    UnsupportedlyLarge { observed: usize, limit: usize },
}

/// Errors that may be encountered validating a binary policy.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ValidateError {
    #[error("invalid sequence of terms in constraint expression")]
    InvalidConstraintTermSequence,
    #[error("invalid extended permissions type: {value}")]
    InvalidExtendedPermissionsType { value: u8 },
    #[error("expected extensible bitmap items to have at least one bit set")]
    InvalidExtensibleBitmapItem,
    #[error("invalid ID index {index} in IdSet")]
    InvalidIdSetIndex { index: u32 },
    #[error("expected selinux magic value {SELINUX_MAGIC:#x}, but found {found_magic:#x}")]
    InvalidMagic { found_magic: u32 },
    #[error("invalid MLS range: high level does not dominate low level")]
    InvalidMlsRange,
    #[error(
        "expected policy version in range [{POLICYDB_VERSION_MIN}, {POLICYDB_VERSION_MAX}], but found {found_policy_version}"
    )]
    InvalidPolicyVersion { found_policy_version: u32 },
    #[error("expected primary names count <= {expected_at_most}, but found {found}")]
    InvalidPrimaryNamesCount { expected_at_most: u32, found: u32 },
    #[error("expected signature {POLICYDB_SIGNATURE:?}, but found {found_signature:?}")]
    InvalidSignature { found_signature: BString },
    #[error(
        "expected extensible bitmap item start bit to be multiple of item size {found_size}, but found {found_start_bit}"
    )]
    MisalignedExtensibleBitmapItemStartBit { found_start_bit: u32, found_size: u32 },
    #[error("missing initial SID {initial_sid:?}")]
    MissingInitialSid { initial_sid: InitialSid },
    #[error("missing mandatory 'object_r' role")]
    MissingObjectRRole,
    #[error("expected UTF-8 string, but found {name:?}")]
    NonUtf8String { name: BString },
    #[error(
        "expected extensible bitmap items to be in sorted order, but found item starting at {found_start_bit} after item that ends at {min_start}"
    )]
    OutOfOrderExtensibleBitmapItems { found_start_bit: u32, min_start: u32 },
    #[error("referenced common symbol {name:?} is not defined")]
    UndefinedCommonSymbol { name: BString },
    #[error("undefined {kind} Id value {id}")]
    UnknownId { kind: &'static str, id: u32 },
}

/// Errors that may be encountered serializing a binary policy.
pub type SerializeError = std::convert::Infallible;
