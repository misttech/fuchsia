// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::num::NonZeroU16;

use super::NewPolicy;
use super::constraints::{Constraint, ConstraintTerm};
use super::error::{ParseError, SerializeError, ValidateError};
use super::id_type::IdType;
use super::indexed::IdAndNameIndexed;
use super::parser::{Array, PolicyCursor};
use super::permissions::Permission;
use super::traits::{HasName, Parse, PolicyId, Serialize, Validate};

use selinux_policy_derive::{HasName, HasPolicyId, Parse, Serialize, Validate};

/// Tag type for type safety of policy class identifiers.
#[derive(Copy, Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct ClassTag;

/// Identifies a class within a policy.
pub type ClassId = IdType<NonZeroU16, ClassTag>;

#[derive(Parse, Serialize)]
struct BinaryClassMetadata {
    key_length: u32,
    common_key_length: u32,
    id: u32,
    /// Included in the policy to allow allocation of index structures to be optimized.
    permission_primary_names_count: u32,
    permission_count: u32,
    constraint_count: u32,
}

/// Rule for computing default user, role, or type when creating an object of a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Parse, Serialize, Validate)]
#[policy(wire_type = u32)]
pub enum ClassDefault {
    Unspecified = 0,
    Source = 1,
    Target = 2,
}

/// Rule for computing default MLS range when creating an object of a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Parse, Serialize, Validate)]
#[policy(wire_type = u32)]
pub enum ClassDefaultRange {
    Unspecified = 0,
    SourceLow = 1,
    SourceHigh = 2,
    SourceLowHigh = 3,
    TargetLow = 4,
    TargetHigh = 5,
    TargetLowHigh = 6,
    UnknownUsedValue = 7,
}

/// Set of rules for computing default security context fields for a class.
#[derive(Debug, Clone, PartialEq, Eq, Parse, Serialize, Validate)]
pub struct ClassDefaults {
    default_user: ClassDefault,
    default_role: ClassDefault,
    default_range: ClassDefaultRange,
    default_type: ClassDefault,
}

impl ClassDefaults {
    #[cfg_attr(not(test), expect(dead_code))]
    pub fn user(&self) -> ClassDefault {
        self.default_user
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn role(&self) -> ClassDefault {
        self.default_role
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn range(&self) -> ClassDefaultRange {
        self.default_range
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn type_(&self) -> ClassDefault {
        self.default_type
    }
}

/// Parsed SELinux object class definition, including permissions and constraints.
#[derive(Debug, Clone, PartialEq, Eq, HasName, HasPolicyId)]
pub struct Class {
    id: ClassId,
    name: Box<[u8]>,
    common_name: Box<[u8]>,
    /// Included in the policy to allow allocation of index structures to be optimized.
    permission_primary_names_count: u32,
    permissions: IdAndNameIndexed<Box<[Permission]>>,
    constraints: Box<[Constraint]>,
    validate_transitions: Array<ConstraintTerm>,
    defaults: ClassDefaults,
}

impl Class {
    /// Name of the `common` from which this class inherits.
    ///
    /// For example, `common file { common_file_perm }` and
    /// `class file inherits file { file_perm }` yields a `Class` object
    /// for `file` with `self.common_name() == b"file"`.
    #[cfg_attr(not(test), expect(dead_code))]
    pub fn common_name(&self) -> &[u8] {
        &self.common_name
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn permissions(&self) -> &IdAndNameIndexed<Box<[Permission]>> {
        &self.permissions
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn constraints(&self) -> &[Constraint] {
        &self.constraints
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn validate_transitions(&self) -> &[ConstraintTerm] {
        &self.validate_transitions
    }

    #[expect(dead_code)]
    pub fn defaults(&self) -> &ClassDefaults {
        &self.defaults
    }
}

impl Parse for Class {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let metadata = BinaryClassMetadata::parse(cursor)?;

        let id_val = metadata.id;
        let id = ClassId::from_u32(id_val).ok_or(ParseError::InvalidId { value: id_val })?;

        let name_len = metadata.key_length as usize;
        let name = Box::from(cursor.read_bytes(name_len)?);

        let common_name_len = metadata.common_key_length as usize;
        let common_name = Box::from(cursor.read_bytes(common_name_len)?);

        let permissions_count = metadata.permission_count as usize;
        let mut permissions = Vec::with_capacity(permissions_count);
        for _ in 0..permissions_count {
            permissions.push(Permission::parse(cursor)?);
        }
        let permissions = IdAndNameIndexed::new(permissions.into_boxed_slice());

        let constraint_count = metadata.constraint_count as usize;
        let mut constraints = Vec::with_capacity(constraint_count);
        for _ in 0..constraint_count {
            constraints.push(Constraint::parse(cursor)?);
        }
        let constraints = constraints.into_boxed_slice();

        let validate_transitions = Array::<ConstraintTerm>::parse(cursor)?;
        let defaults = ClassDefaults::parse(cursor)?;

        Ok(Self {
            id,
            name,
            common_name,
            permission_primary_names_count: metadata.permission_primary_names_count,
            permissions,
            constraints,
            validate_transitions,
            defaults,
        })
    }
}

impl Serialize for Class {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        let metadata = BinaryClassMetadata {
            key_length: self.name.len() as u32,
            common_key_length: self.common_name.len() as u32,
            id: self.id.as_u32(),
            permission_primary_names_count: self.permission_primary_names_count,
            permission_count: self.permissions.len() as u32,
            constraint_count: self.constraints.len() as u32,
        };
        metadata.serialize(writer)?;

        writer.extend_from_slice(&self.name);
        writer.extend_from_slice(&self.common_name);

        for permission in self.permissions.iter() {
            permission.serialize(writer)?;
        }

        for constraint in self.constraints.iter() {
            constraint.serialize(writer)?;
        }

        self.validate_transitions.serialize(writer)?;
        self.defaults.serialize(writer)?;
        Ok(())
    }
}

impl Validate for Class {
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError> {
        self.permissions.validate(policy)?;
        for constraint in self.constraints.iter() {
            constraint.validate(policy)?;
        }
        self.validate_transitions.validate(policy)?;
        self.defaults.validate(policy)?;

        if self.permission_primary_names_count > self.permissions.len() as u32 {
            return Err(ValidateError::InvalidPrimaryNamesCount {
                expected_at_most: self.permissions.len() as u32,
                found: self.permission_primary_names_count,
            });
        }

        // Validate that inherited common symbol exists in policy
        if !self.common_name.is_empty() {
            policy.common_symbols().iter().find(|cs| cs.name() == &*self.common_name).ok_or_else(
                || ValidateError::UndefinedCommonSymbol { name: self.common_name.to_vec() },
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::traits::{HasName, HasPolicyId};
    use super::{PolicyCursor, *};

    #[test]
    fn test_class_defaults_parse_and_serialize() {
        let data = [
            1, 0, 0, 0, // default_user = 1 (Source)
            2, 0, 0, 0, // default_role = 2 (Target)
            6, 0, 0, 0, // default_range = 6 (TargetLowHigh)
            1, 0, 0, 0, // default_type = 1 (Source)
        ];
        let mut cursor = PolicyCursor::new(&data);
        let defaults = ClassDefaults::parse(&mut cursor).unwrap();
        assert_eq!(defaults.user(), ClassDefault::Source);
        assert_eq!(defaults.role(), ClassDefault::Target);
        assert_eq!(defaults.range(), ClassDefaultRange::TargetLowHigh);
        assert_eq!(defaults.type_(), ClassDefault::Source);

        let mut writer = Vec::new();
        defaults.serialize(&mut writer).unwrap();
        assert_eq!(writer, data);
    }

    #[test]
    fn test_minimal_class_parse_and_serialize() {
        let data = [
            // BinaryClassMetadata
            4, 0, 0, 0, // key_length = 4
            0, 0, 0, 0, // common_key_length = 0
            1, 0, 0, 0, // id = 1
            0, 0, 0, 0, // permission_primary_names_count = 0
            0, 0, 0, 0, // permission_count = 0
            0, 0, 0, 0, // constraint_count = 0
            116, 101, 115, 116, // name: "test"
            0, 0, 0, 0, // validate_transitions (Array count = 0)
            // defaults (all Unspecified = 0)
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let mut cursor = PolicyCursor::new(&data);
        let class = Class::parse(&mut cursor).unwrap();
        assert_eq!(class.id(), ClassId::from_u32(1).unwrap());
        assert_eq!(class.name(), b"test");
        assert!(class.common_name().is_empty());
        assert!(class.permissions().is_empty());
        assert!(class.constraints().is_empty());
        assert!(class.validate_transitions().is_empty());

        let mut writer = Vec::new();
        class.serialize(&mut writer).unwrap();
        assert_eq!(writer, data);
    }
}
