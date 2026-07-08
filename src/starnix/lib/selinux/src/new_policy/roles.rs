// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::bitmap::IdSet;
use super::error::{ParseError, SerializeError, ValidateError};
use super::id_type::IdType;
use super::parser::PolicyCursor;
use super::traits::{Parse, PolicyId, Serialize, Validate};
use super::{NewPolicy, TypeSet};

use selinux_policy_derive::{HasName, HasPolicyId, Parse, Serialize, Validate};

/// Tag type for type safety of policy role identifiers.
#[derive(Copy, Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct RoleTag;

/// Identifies a role within a policy.
pub type RoleId = IdType<std::num::NonZeroU16, RoleTag>;

/// Set of [`RoleId`]s.
pub type RoleSet = IdSet<RoleId>;

#[derive(Parse, Serialize)]
struct BinaryRoleMetadata {
    key_length: u32,
    id: u32,
    bounds: u32,
}

/// Parsed SELinux [`Role`] definition.
#[derive(Debug, Clone, PartialEq, Eq, Validate, HasName, HasPolicyId)]
pub struct Role {
    id: RoleId,
    name: Box<[u8]>,
    bounds: Option<RoleId>,
    dominates: RoleSet,
    types: TypeSet,
}

impl Role {
    pub fn bounds(&self) -> Option<RoleId> {
        self.bounds
    }

    pub fn dominates(&self) -> &RoleSet {
        &self.dominates
    }

    pub fn types(&self) -> &TypeSet {
        &self.types
    }
}

impl Parse for Role {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let metadata = BinaryRoleMetadata::parse(cursor)?;
        let name = Box::from(cursor.read_bytes(metadata.key_length as usize)?);
        let dominates = RoleSet::parse(cursor)?;
        let types = TypeSet::parse(cursor)?;

        let bounds = RoleId::from_u32(metadata.bounds);
        let id =
            RoleId::from_u32(metadata.id).ok_or(ParseError::InvalidId { value: metadata.id })?;

        Ok(Self { id, name, bounds, dominates, types })
    }
}

impl Serialize for Role {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        let metadata = BinaryRoleMetadata {
            key_length: self.name.len() as u32,
            id: self.id.as_u32(),
            bounds: self.bounds.map_or(0, |id| id.as_u32()),
        };
        metadata.serialize(writer)?;
        writer.extend_from_slice(&self.name);
        self.dominates.serialize(writer)?;
        self.types.serialize(writer)?;
        Ok(())
    }
}
impl Validate for RoleId {
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError> {
        policy
            .roles()
            .get_by_id(*self)
            .map(|_| ())
            .ok_or_else(|| ValidateError::UnknownId { kind: "role", id: self.as_u32() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_policy::traits::{HasName, HasPolicyId};

    #[test]
    fn test_role_parse_and_serialize() {
        let data = [
            // BinaryRoleMetadata
            4, 0, 0, 0, // key_length = 4
            1, 0, 0, 0, // id = 1
            0, 0, 0, 0, // bounds = 0
            // name: "test"
            b't', b'e', b's', b't', // dominates (empty ExtensibleBitmap)
            64, 0, 0, 0, // map_item_size_bits = 64
            0, 0, 0, 0, // high_bit = 0
            0, 0, 0, 0, // items_count = 0
            // types (empty ExtensibleBitmap)
            64, 0, 0, 0, // map_item_size_bits = 64
            0, 0, 0, 0, // high_bit = 0
            0, 0, 0, 0, // items_count = 0
        ];
        let mut cursor = PolicyCursor::new(&data);
        let role = Role::parse(&mut cursor).unwrap();
        assert_eq!(role.id(), RoleId::from_u32(1).unwrap());
        assert_eq!(role.name(), b"test");
        assert!(role.bounds().is_none());
        // dominates and types are empty dynamically validated by round-trip

        let mut writer = Vec::new();
        role.serialize(&mut writer).unwrap();
        assert_eq!(writer, data);
    }
}
