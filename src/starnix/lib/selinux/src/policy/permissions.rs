// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::num::NonZeroU8;

use fuchsia_rcu::RcuDroppable;
use selinux_policy_derive::{HasName, HasPolicyId, Validate};

use super::Policy;
use super::error::{ParseError, SerializeError, ValidateError};
use super::id_type::IdType;
use super::parser::{PolicyCursor, PolicyWriter};
use super::traits::{Parse, PolicyId, Serialize, Validate};

/// Tag type for type safety of policy permission identifiers.
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, RcuDroppable)]
pub struct PermissionTag;

/// Identifies a permission within an object class (class-relative, 1-indexed).
pub type PermissionId = IdType<NonZeroU8, PermissionTag>;

/// Parsed SELinux permission, containing a type-safe ID and a name.
#[derive(Debug, Validate, HasName, HasPolicyId)]
pub struct Permission {
    id: PermissionId,
    name: Box<[u8]>,
}

impl Parse for Permission {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let length = u32::parse(cursor)? as usize;
        let id_val = u32::parse(cursor)?;

        // Validate permission ID range (1..=32) during parsing.
        let id = PermissionId::from_u32(id_val)
            .filter(|id| id.as_u32() <= 32)
            .ok_or(ParseError::InvalidId { value: id_val })?;

        let name = Box::from(cursor.read_bytes(length)?);

        Ok(Self { id, name })
    }
}

impl Serialize for Permission {
    fn serialize(&self, writer: &mut PolicyWriter<'_>) -> Result<(), SerializeError> {
        let length = self.name.len() as u32;
        length.serialize(writer)?;
        self.id.as_u32().serialize(writer)?;
        writer.write_bytes(&self.name);
        Ok(())
    }
}

impl Validate for PermissionId {
    fn validate(&self, _policy: &Policy) -> Result<(), ValidateError> {
        Ok(())
    }
}
