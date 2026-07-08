// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::num::NonZeroU16;

use super::NewPolicy;
use super::bitmap::IdSet;
use super::error::ValidateError;
use super::id_type::IdType;
use super::traits::Validate;

/// Tag type for type safety of policy type identifiers.
#[derive(Copy, Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct TypeTag;

/// Identifies a type (or type attribute) within a policy.
pub type TypeId = IdType<NonZeroU16, TypeTag>;

/// Set of types that are marked permissive.
pub type PermissiveTypeSet = IdSet<TypeId, true>;

/// Set of [`TypeId`]s.
pub type TypeSet = IdSet<TypeId>;

impl Validate for TypeId {
    fn validate(&self, _policy: &NewPolicy) -> Result<(), ValidateError> {
        // TODO: Validate against types table when integrated
        Ok(())
    }
}
