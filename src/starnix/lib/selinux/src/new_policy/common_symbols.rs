// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::error::{ParseError, SerializeError};
use super::id_type::IdType;
use super::indexed::IdAndNameIndexed;
use super::parser::PolicyCursor;
use super::permissions::Permission;
use super::traits::{Parse, PolicyId, Serialize};
use selinux_policy_derive::{HasName, HasPolicyId, Parse, Serialize, Validate};

/// Tag type for type safety of policy common symbol identifiers.
#[derive(Copy, Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct CommonSymbolTag;

/// Identifies a common symbol within a policy.
pub type CommonSymbolId = IdType<std::num::NonZeroU16, CommonSymbolTag>;

/// Parsed SELinux common symbol table entry (e.g. `common file { ... }`).
#[derive(Debug, Clone, PartialEq, Eq, Validate, HasName, HasPolicyId)]
pub struct CommonSymbol {
    id: CommonSymbolId,
    name: Box<[u8]>,
    primary_names_count: u32,
    permissions: IdAndNameIndexed<Box<[Permission]>>,
}

impl CommonSymbol {
    /// Returns the name of this common symbol as a byte slice.
    pub fn name_bytes(&self) -> &[u8] {
        &self.name
    }

    /// Returns the permissions associated with this common symbol.
    pub fn permissions(&self) -> &[Permission] {
        &self.permissions
    }
}

#[derive(Parse, Serialize)]
struct BinaryCommonSymbolHeader {
    name_len: u32,
    id: u32,
    primary_names_count: u32,
    permissions_count: u32,
}

impl Parse for CommonSymbol {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let header = BinaryCommonSymbolHeader::parse(cursor)?;

        let name = Box::from(cursor.read_bytes(header.name_len as usize)?);

        let mut permissions_vec = Vec::with_capacity(header.permissions_count as usize);
        for _ in 0..header.permissions_count {
            permissions_vec.push(Permission::parse(cursor)?);
        }
        let permissions = IdAndNameIndexed::new(permissions_vec.into_boxed_slice());

        let id = CommonSymbolId::from_u32(header.id)
            .ok_or(ParseError::InvalidId { value: header.id })?;

        Ok(Self { id, name, primary_names_count: header.primary_names_count, permissions })
    }
}

impl Serialize for CommonSymbol {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        let header = BinaryCommonSymbolHeader {
            name_len: self.name.len() as u32,
            id: self.id.as_u32(),
            primary_names_count: self.primary_names_count,
            permissions_count: self.permissions.len() as u32,
        };
        header.serialize(writer)?;

        writer.extend_from_slice(&self.name);
        for permission in self.permissions.iter() {
            permission.serialize(writer)?;
        }
        Ok(())
    }
}
