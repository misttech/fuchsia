// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::num::{NonZeroU16, NonZeroUsize};

use hashbrown::HashTable;
use selinux_policy_derive::{HasPolicyId, Parse, Serialize};

use super::bitmap::IdSet;
use super::error::{ParseError, SerializeError, ValidateError};
use super::id_type::IdType;
use super::indexed::hash_name;
use super::parser::{Array, PolicyCursor};
use super::traits::{HasName, Parse, PolicyId, Serialize, Validate};
use super::NewPolicy;

/// Tag type for type safety of policy type identifiers.
#[derive(Copy, Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct TypeTag;

/// Identifies a type (or type attribute) within a policy.
pub type TypeId = IdType<NonZeroU16, TypeTag>;

/// Set of types that are marked permissive.
pub type PermissiveTypeSet = IdSet<TypeId, true>;

/// Set of [`TypeId`]s.
pub type TypeSet = IdSet<TypeId>;

/// Wrapper for [`usize`] that cannot be [`usize::MAX`].
///
/// Allows [`Option`]<[`NonMaxUsize`]> to have the same size in memory as a [`usize`]
/// by using [`usize::MAX`] as a niche.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NonMaxUsize {
    value: NonZeroUsize,
}

impl NonMaxUsize {
    pub fn new(value: usize) -> Option<Self> {
        NonZeroUsize::new(value.wrapping_add(1)).map(|value| Self { value })
    }

    pub fn get(self) -> usize {
        self.value.get().wrapping_sub(1)
    }
}

impl Validate for TypeId {
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError> {
        policy
            .types()
            .get_by_id(*self)
            .map(|_| ())
            .ok_or_else(|| ValidateError::UnknownId { kind: "type", id: self.as_u32() })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TypeKind {
    Alias,
    Type,
    Attribute,
}

impl TypeKind {
    pub const ALIAS: u32 = 0;
    pub const TYPE: u32 = 1;
    pub const ATTRIBUTE: u32 = 3;
}

/// Parsed SELinux type, containing an ID, a name, properties, and optional bounds.
#[derive(Debug, Clone, PartialEq, Eq, HasPolicyId)]
pub struct Type {
    pub(super) id: TypeId,
    pub(super) name: Box<[u8]>,
    pub(super) properties: TypeKind,
    pub(super) bounds: Option<TypeId>,
}

impl HasName for Type {
    fn name(&self) -> &[u8] {
        &self.name
    }
}

impl Type {
    pub fn bounded_by(&self) -> Option<TypeId> {
        self.bounds
    }
}

#[derive(Parse, Serialize)]
struct BinaryTypeMetadata {
    length: u32,
    id: u32,
    properties: u32,
    bounds: u32,
}

impl Parse for Type {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let metadata = BinaryTypeMetadata::parse(cursor)?;
        let name = cursor.read_bytes(metadata.length as usize)?.to_vec().into_boxed_slice();

        let properties_val = metadata.properties;
        let properties = match properties_val {
            TypeKind::ALIAS => TypeKind::Alias,
            TypeKind::TYPE => TypeKind::Type,
            TypeKind::ATTRIBUTE => TypeKind::Attribute,
            v => {
                return Err(ParseError::InvalidEnumValue {
                    enum_name: "TypeKind",
                    value: v as u64,
                });
            }
        };

        let bounds = TypeId::from_u32(metadata.bounds);
        let id =
            TypeId::from_u32(metadata.id).ok_or(ParseError::InvalidId { value: metadata.id })?;

        Ok(Self { id, name, properties, bounds })
    }
}

impl Serialize for Type {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        let properties_val = match self.properties {
            TypeKind::Alias => TypeKind::ALIAS,
            TypeKind::Type => TypeKind::TYPE,
            TypeKind::Attribute => TypeKind::ATTRIBUTE,
        };
        let metadata = BinaryTypeMetadata {
            length: self.name.len() as u32,
            id: self.id.as_u32(),
            properties: properties_val,
            bounds: self.bounds.map_or(0, |id| id.as_u32()),
        };
        metadata.serialize(writer)?;
        writer.extend_from_slice(&self.name);
        Ok(())
    }
}

impl Validate for Type {
    fn validate(&self, _policy: &NewPolicy) -> Result<(), ValidateError> {
        // Structural validation is done during parsing.
        Ok(())
    }
}

/// Container for all types in the policy, providing indices for fast lookup by ID and Name.
#[derive(Debug, Clone)]
pub struct Types {
    pub primary_names_count: u32,
    /// In-order list of all types, attributes, and aliases.
    pub ordered: Array<Type>,

    /// Maps TypeId -> index in `ordered`. Only contains Types and Attributes.
    /// Index is `TypeId - 1`.
    by_id: Box<[Option<NonMaxUsize>]>,

    /// Maps name -> index in `ordered`. Only contains Types and Aliases.
    /// Stores only u32 index to avoid keeping copies of type names.
    by_name: HashTable<u32>,
    hasher: rapidhash::RapidBuildHasher,
}

impl PartialEq for Types {
    fn eq(&self, other: &Self) -> bool {
        self.primary_names_count == other.primary_names_count && self.ordered == other.ordered
    }
}

impl Eq for Types {}

impl Parse for Types {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let primary_names_count = u32::parse(cursor)?;
        let ordered = Array::<Type>::parse(cursor)?;

        // Build indices
        let mut by_id = Vec::new();
        let hasher = rapidhash::RapidBuildHasher::default();
        let mut by_name = HashTable::new();

        for (index, t) in ordered.iter().enumerate() {
            if t.properties == TypeKind::Type || t.properties == TypeKind::Attribute {
                let id = t.id.as_u32() as usize;
                if id > by_id.len() {
                    by_id.resize(id, None);
                }
                by_id[id - 1] = Some(NonMaxUsize::new(index).expect("index overflow"));
            }
            if t.properties == TypeKind::Type || t.properties == TypeKind::Alias {
                let hash = hash_name(&hasher, t.name.as_ref());
                by_name.insert_unique(hash, index as u32, |&idx| {
                    hash_name(&hasher, ordered[idx as usize].name.as_ref())
                });
            }
        }

        Ok(Self { primary_names_count, ordered, by_id: by_id.into_boxed_slice(), by_name, hasher })
    }
}

impl Serialize for Types {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        self.primary_names_count.serialize(writer)?;
        self.ordered.serialize(writer)
    }
}

impl Validate for Types {
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError> {
        for t in self.ordered.iter() {
            t.validate(policy)?;
        }
        Ok(())
    }
}

impl Types {
    pub fn primary_names_count(&self) -> u32 {
        self.primary_names_count
    }

    pub fn get_by_id(&self, id: TypeId) -> Option<&Type> {
        let index = self.by_id.get((id.as_u32() - 1) as usize)?.as_ref()?;
        Some(&self.ordered[index.get()])
    }

    pub fn get_by_name(&self, name: &[u8]) -> Option<&Type> {
        let hash = hash_name(&self.hasher, name);
        let idx =
            self.by_name.find(hash, |&idx| self.ordered[idx as usize].name.as_ref() == name)?;
        Some(&self.ordered[*idx as usize])
    }

    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Type> {
        self.ordered.iter()
    }
}
