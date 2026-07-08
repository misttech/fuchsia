// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use selinux_policy_derive::{HasName, HasPolicyId, Parse, Serialize};

use super::context::MlsLevel;
use super::error::{ParseError, SerializeError, ValidateError};
use super::parser::PolicyCursor;
use super::traits::{Parse, PolicyId, Serialize, Validate};
use super::{CategoryId, SensitivityId};

#[derive(Debug, Clone, HasName, HasPolicyId)]
pub struct Sensitivity {
    id: SensitivityId,
    name: Box<[u8]>,
    is_alias: bool,
    level: MlsLevel,
}

impl Sensitivity {
    pub fn is_alias(&self) -> bool {
        self.is_alias
    }

    pub fn level(&self) -> &MlsLevel {
        &self.level
    }
}

#[derive(Parse, Serialize)]
struct BinarySensitivityMetadata {
    length: u32,
    is_alias: u32,
}

impl Parse for Sensitivity {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let metadata = BinarySensitivityMetadata::parse(cursor)?;
        let name = Box::from(cursor.read_bytes(metadata.length as usize)?);
        let level = MlsLevel::parse(cursor)?;
        Ok(Self { id: level.sensitivity(), name, is_alias: metadata.is_alias != 0, level })
    }
}

impl Serialize for Sensitivity {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        let metadata = BinarySensitivityMetadata {
            length: self.name.len() as u32,
            is_alias: if self.is_alias { 1 } else { 0 },
        };
        metadata.serialize(writer)?;
        writer.extend_from_slice(&self.name);
        self.level.serialize(writer)?;
        Ok(())
    }
}

impl Validate for Sensitivity {
    fn validate(&self, policy: &super::NewPolicy) -> Result<(), ValidateError> {
        self.level.validate(policy)?;
        Ok(())
    }
}

impl Validate for SensitivityId {
    fn validate(&self, policy: &super::NewPolicy) -> Result<(), ValidateError> {
        policy
            .sensitivities()
            .get_by_id(*self)
            .map(|_| ())
            .ok_or_else(|| ValidateError::UnknownId { kind: "sensitivity", id: self.as_u32() })
    }
}

#[derive(Debug, Clone, HasName, HasPolicyId)]
pub struct Category {
    id: CategoryId,
    name: Box<[u8]>,
    is_alias: bool,
}

impl Category {
    pub fn is_alias(&self) -> bool {
        self.is_alias
    }
}

#[derive(Parse, Serialize)]
struct BinaryCategoryMetadata {
    length: u32,
    id: u32,
    is_alias: u32,
}

impl Parse for Category {
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let metadata = BinaryCategoryMetadata::parse(cursor)?;
        let name = Box::from(cursor.read_bytes(metadata.length as usize)?);
        let id = CategoryId::from_u32(metadata.id)
            .ok_or(ParseError::InvalidId { value: metadata.id })?;
        Ok(Self { id, name, is_alias: metadata.is_alias != 0 })
    }
}

impl Serialize for Category {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        let metadata = BinaryCategoryMetadata {
            length: self.name.len() as u32,
            id: self.id.as_u32(),
            is_alias: if self.is_alias { 1 } else { 0 },
        };
        metadata.serialize(writer)?;
        writer.extend_from_slice(&self.name);
        Ok(())
    }
}

impl Validate for Category {
    fn validate(&self, _policy: &super::NewPolicy) -> Result<(), ValidateError> {
        Ok(())
    }
}

impl Validate for CategoryId {
    fn validate(&self, policy: &super::NewPolicy) -> Result<(), ValidateError> {
        policy
            .categories()
            .get_by_id(*self)
            .map(|_| ())
            .ok_or_else(|| ValidateError::UnknownId { kind: "category", id: self.as_u32() })
    }
}
