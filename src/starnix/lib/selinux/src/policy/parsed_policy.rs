// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::error::ParseError;
use super::parser::PolicyData;
use super::{ClassId, TypeId};
use crate::new_policy::{GenfsConPath, NewPolicy};
use std::ops::Deref;
use std::sync::Arc;

use anyhow::Context as _;
use std::iter::Iterator;

// As of 2026-01-30, more than five times larger than any policy seen in production or tests.
const MAXIMUM_POLICY_SIZE: usize = 1 << 24;

/// Parsed binary policy.
#[derive(Debug)]
pub struct ParsedPolicy {
    /// [`NewPolicy`] that handles the policy tables and verification.
    new_policy: Arc<NewPolicy>,
}

impl Deref for ParsedPolicy {
    type Target = NewPolicy;
    fn deref(&self) -> &Self::Target {
        &self.new_policy
    }
}

impl ParsedPolicy {
    pub(crate) fn genfscon_find_all<'a>(
        &'a self,
        fs_type: &'a [u8],
    ) -> impl Iterator<Item = &'a GenfsConPath> {
        self.generic_fs_contexts()
            .iter()
            .filter(move |entry| entry.fs_type() == fs_type)
            .flat_map(|entry| entry.paths().iter())
    }

    pub(crate) fn compute_filename_transition(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        class: ClassId,
        name: &[u8],
    ) -> Option<TypeId> {
        self.new_policy.filename_transitions().compute_filename_transition(
            source_type,
            target_type,
            class,
            name,
        )
    }
}

impl ParsedPolicy {
    /// Parses the binary policy stored in `bytes`. It is an error for `bytes` to have trailing
    /// bytes after policy parsing completes.
    pub(super) fn parse(data: PolicyData) -> Result<Self, anyhow::Error> {
        let policy_size = data.len();
        if MAXIMUM_POLICY_SIZE <= policy_size {
            return Err(anyhow::Error::from(ParseError::UnsupportedlyLarge {
                observed: policy_size,
                limit: MAXIMUM_POLICY_SIZE,
            }));
        }
        let new_policy =
            NewPolicy::parse(&data).map_err(|e| anyhow::anyhow!("new parser failed: {:?}", e))?;
        new_policy.validate().context("validating new policy structure")?;

        Ok(ParsedPolicy { new_policy: Arc::new(new_policy) })
    }
}

impl ParsedPolicy {
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_policy::TypeSet;
    use crate::new_policy::traits::PolicyId;
    use crate::policy::Parse;
    use crate::policy::parser::PolicyCursor;
    use std::sync::Arc;

    #[test]
    fn test_id_set_parse_compatibility() {
        let bytes = [
            64, 0, 0, 0, // map_item_size_bits = 64
            128, 0, 0, 0, // high_bit = 128
            2, 0, 0, 0, // count = 2
            // Item 1
            0, 0, 0, 0, // start_bit = 0
            5, 0, 0, 0, 0, 0, 0, 0, // map = 5 (bits 0 and 2 set)
            // Item 2
            64, 0, 0, 0, // start_bit = 64
            2, 0, 0, 0, 0, 0, 0, 0, // map = 2 (bit 65 set)
        ];
        let data: PolicyData = Arc::from(bytes);
        let cursor = PolicyCursor::new(&data);
        let (id_set, tail) = TypeSet::parse(cursor).unwrap();
        assert_eq!(tail.offset(), bytes.len() as u32);
        assert!(id_set.contains(TypeId::from_u32(1).unwrap()));
        assert!(!id_set.contains(TypeId::from_u32(2).unwrap()));
        assert!(id_set.contains(TypeId::from_u32(3).unwrap()));
        assert!(id_set.contains(TypeId::from_u32(66).unwrap()));
    }
}
