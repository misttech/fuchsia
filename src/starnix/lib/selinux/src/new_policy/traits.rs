// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::NewPolicy;
use super::error::{ParseError, SerializeError, ValidateError};
use super::parser::PolicyCursor;

/// Trait for types that can be parsed from a [`PolicyCursor`].
pub trait Parse: Sized {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError>;
}

/// Trait for types that can be serialized into a byte vector.
pub trait Serialize {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError>;
}

/// Trait for types that can be validated against the parsed policy.
pub trait Validate {
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError>;
}

/// Trait for strongly-typed policy identifiers.
///
/// Types implementing [`PolicyId`] can be parsed from and serialized to `u32` values
/// in the binary policy database, but are represented as strongly-typed integers
/// (often wrapping `NonZeroU16` or `NonZeroU32`) in the logical domain model.
pub trait PolicyId:
    Copy + Clone + std::fmt::Debug + Eq + std::hash::Hash + Ord + PartialOrd
{
    /// Returns the raw `u32` value of the ID.
    fn as_u32(&self) -> u32;

    /// Constructs an instance of [`Self`] from a raw `u32` value, returning [`None`]
    /// if the value is invalid (e.g. zero for a non-optional ID, or out of range).
    fn from_u32(value: u32) -> Option<Self>;
}

// Blanket implementations for all strongly-typed IDs
impl<T> Parse for T
where
    T: PolicyId,
{
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let value = u32::parse(cursor)?;
        T::from_u32(value).ok_or(ParseError::InvalidId { value })
    }
}

impl<T> Serialize for T
where
    T: PolicyId,
{
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        self.as_u32().serialize(writer)
    }
}

/// Trait for policy elements with a byte slice name.
pub trait HasName {
    fn name(&self) -> &[u8];
}

/// Trait for policy elements that have a strongly-typed policy identifier.
pub trait HasPolicyId {
    type Id: PolicyId;
    fn id(&self) -> Self::Id;
}

impl Validate for Box<[u8]> {
    fn validate(&self, _policy: &NewPolicy) -> Result<(), ValidateError> {
        Ok(())
    }
}

impl<T: Validate> Validate for Option<T> {
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError> {
        if let Some(value) = self {
            value.validate(policy)?;
        }
        Ok(())
    }
}
