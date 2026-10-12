// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod error;
pub mod parsed_policy;
pub mod parser;

use crate::kernel::PolicyIndex;
pub use crate::new_policy::traits::{HasName, HasPolicyId, PolicyId};
pub use crate::new_policy::{
    AccessDecision, AccessVector, AccessVectorRules, CategoryId, ClassId, FsUseLabelAndType,
    FsUseType, HandleUnknown, IndexedAccessVectorRules, MlsLevel, MlsRange, POLICYDB_VERSION_MAX,
    PermissionId, RoleId, SELINUX_AVD_FLAGS_PERMISSIVE, SecurityContext, SecurityContextError,
    SensitivityId, TypeId, User, UserId, XpermsAccessDecision, XpermsBitmap, XpermsKind,
};
use parsed_policy::ParsedPolicy;
pub use parser::PolicyCursor;
use parser::PolicyData;

use anyhow::Context as _;
use std::fmt::Debug;
use std::ops::Deref;

use std::sync::Arc;
use zerocopy::little_endian as le;

/// Parses `binary_policy` by value; that is, copies underlying binary data out in addition to
/// building up parser output structures. This function returns
/// `(unvalidated_parser_output, binary_policy)` on success, or an error if parsing failed. Note
/// that the second component of the success case contains precisely the same bytes as the input.
/// This function depends on a uniformity of interface between the "by value" and "by reference"
/// strategies, but also requires an `unvalidated_parser_output` type that is independent of the
/// `binary_policy` lifetime. Taken together, these requirements demand the "move-in + move-out"
/// interface for `binary_policy`.
pub fn parse_policy_by_value(binary_policy: Vec<u8>) -> Result<Unvalidated, anyhow::Error> {
    let policy_data: PolicyData = Arc::from(binary_policy);
    let policy = ParsedPolicy::parse(policy_data).context("parsing policy")?;
    Ok(Unvalidated(policy))
}

#[derive(Debug)]
pub struct Policy(PolicyIndex);

impl Deref for Policy {
    type Target = PolicyIndex;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Policy {
    /// Serializes the policy back into [`PolicyData`].
    pub fn serialize(&self) -> PolicyData {
        let mut bytes = Vec::new();
        self.0.serialize(&mut bytes).expect("serialization of new_policy should succeed");
        std::sync::Arc::from(bytes)
    }

    pub fn conditional_booleans<'a>(&'a self) -> Vec<(&'a [u8], bool)> {
        self.0
            .conditional_booleans()
            .iter()
            .map(|boolean| (boolean.name(), boolean.active()))
            .collect()
    }
}

/// A [`Policy`] that has been successfully parsed, but not validated.
pub struct Unvalidated(ParsedPolicy);

impl Unvalidated {
    pub fn validate(self) -> Result<Policy, anyhow::Error> {
        self.0.validate().context("validating parsed policy")?;
        let index = PolicyIndex::new(self.0).context("building index")?;
        Ok(Policy(index))
    }
}

/// A data structure that can be parsed as a part of a binary policy.
pub trait Parse: Sized {
    /// The type of error that may be returned from `parse()`, usually [`ParseError`] or
    /// [`anyhow::Error`].
    type Error: Into<anyhow::Error>;

    /// Parses a `Self` from `bytes`, returning the `Self` and trailing bytes, or an error if
    /// bytes corresponding to a `Self` are malformed.
    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error>;
}

/// Treat a type as metadata that contains a count of subsequent data.
impl Parse for le::U32 {
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        bytes.parse::<le::U32>().map_err(anyhow::Error::from)
    }
}

impl<T: crate::new_policy::traits::Parse> Parse for T {
    type Error = anyhow::Error;

    fn parse<'a>(bytes: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let offset = bytes.offset() as usize;
        let slice = &bytes.data().as_ref()[offset..];
        let mut new_cursor = crate::new_policy::parser::PolicyCursor::new(slice);
        let item = <T as crate::new_policy::traits::Parse>::parse(&mut new_cursor)
            .map_err(|e| anyhow::anyhow!("Parse error: {:?}", e))?;
        let new_offset = bytes.offset() + new_cursor.offset() as u32;
        Ok((item, PolicyCursor::new_at(bytes.data(), new_offset)))
    }
}
