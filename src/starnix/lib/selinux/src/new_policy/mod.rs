// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub(super) mod access_vector;
pub(super) mod bitmap;
pub(super) mod common_symbols;
pub(super) mod context;
pub(super) mod error;
pub(super) mod id_type;
pub(super) mod indexed;
pub(super) mod metadata;
pub(super) mod parser;
pub(super) mod permissions;
pub(super) mod traits;

use selinux_policy_derive::{Parse, Serialize, Validate};

use error::{ParseError, ValidateError};
use metadata::{Config, Counts, Magic, PolicyVersion, Signature};
pub use metadata::{HandleUnknown, POLICYDB_VERSION_MAX};
use parser::{PolicyCursor, RemainingBytes};
use traits::Validate;

pub(super) mod types;

pub use access_vector::AccessVector;
pub use bitmap::{ExtensibleBitmap, IdSpan};
pub use common_symbols::CommonSymbol;
pub use context::{Context, MlsLevel, MlsRange};
pub use id_type::IdType;
pub use parser::SymbolArray;
pub use permissions::PermissionId;
pub use types::*;

/// Tag type for type safety of policy user identifiers.
#[derive(Copy, Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct UserTag;

/// Identifies a user within a policy.
pub type UserId = IdType<std::num::NonZeroU16, UserTag>;

/// Tag type for type safety of policy role identifiers.
#[derive(Copy, Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct RoleTag;

/// Identifies a role within a policy.
pub type RoleId = IdType<std::num::NonZeroU16, RoleTag>;

/// Tag type for type safety of policy class identifiers.
#[derive(Copy, Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct ClassTag;

/// Identifies a class within a policy.
pub type ClassId = IdType<std::num::NonZeroU16, ClassTag>;

/// Top-level [`NewPolicy`] structure that parses the first few fields
/// and stores the rest in [`Self::rest`] to allow round-trip testing.
#[derive(Debug, Clone, Parse, Serialize, Validate)]
pub struct NewPolicy {
    magic: Magic,
    signature: Signature,
    version: PolicyVersion,
    config: Config,
    counts: Counts,
    policy_capabilities: ExtensibleBitmap,
    permissive_map: PermissiveTypeSet,
    common_symbols: SymbolArray<CommonSymbol>,
    rest: RemainingBytes,
}

impl NewPolicy {
    /// Parses a [`NewPolicy`] from the raw binary data.
    pub fn parse(data: &[u8]) -> Result<Self, ParseError> {
        let mut cursor = PolicyCursor::new(data);
        cursor.parse()
    }

    /// Validates the parsed policy.
    pub fn validate(&self) -> Result<(), ValidateError> {
        Validate::validate(self, self)
    }

    /// Returns the policy version.
    pub fn policy_version(&self) -> u32 {
        self.version.get()
    }

    /// Returns the [`HandleUnknown`] configuration.
    pub fn handle_unknown(&self) -> HandleUnknown {
        self.config.handle_unknown()
    }

    /// Returns the policy capabilities bitmap.
    pub fn policy_capabilities(&self) -> &ExtensibleBitmap {
        &self.policy_capabilities
    }

    /// Returns the permissive types set.
    pub fn permissive_map(&self) -> &PermissiveTypeSet {
        &self.permissive_map
    }

    /// Returns the common symbols table.
    pub fn common_symbols(&self) -> &SymbolArray<CommonSymbol> {
        &self.common_symbols
    }

    /// Returns the remaining unparsed bytes.
    pub fn rest_bytes(&self) -> std::sync::Arc<[u8]> {
        self.rest.bytes.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_policy::traits::{Parse, Serialize};

    #[derive(Copy, Clone, Debug, Eq, PartialEq, Parse, Serialize, Validate)]
    #[policy(wire_type = u32)]
    enum TestEnum {
        ValueOne = 1,
        ValueTwo = 2,
    }

    #[test]
    fn test_enum_derive() {
        let mut cursor = PolicyCursor::new(&[1, 0, 0, 0]);
        let parsed = TestEnum::parse(&mut cursor).unwrap();
        assert_eq!(parsed, TestEnum::ValueOne);

        let mut cursor = PolicyCursor::new(&[2, 0, 0, 0]);
        let parsed = TestEnum::parse(&mut cursor).unwrap();
        assert_eq!(parsed, TestEnum::ValueTwo);

        let mut cursor = PolicyCursor::new(&[3, 0, 0, 0]);
        let err = TestEnum::parse(&mut cursor).unwrap_err();
        assert!(matches!(err, ParseError::InvalidEnumValue { enum_name: "TestEnum", value: 3 }));

        let mut writer = Vec::new();
        TestEnum::ValueOne.serialize(&mut writer).unwrap();
        assert_eq!(writer, vec![1, 0, 0, 0]);

        let mut writer = Vec::new();
        TestEnum::ValueTwo.serialize(&mut writer).unwrap();
        assert_eq!(writer, vec![2, 0, 0, 0]);

        let policy_bytes = include_bytes!("../../testdata/policies/selinux_testsuite");
        let policy = NewPolicy::parse(policy_bytes).unwrap();
        TestEnum::ValueOne.validate(&policy).unwrap();
    }

    #[test]
    fn test_real_policy_roundtrip() {
        let policy_bytes = include_bytes!("../../testdata/policies/selinux_testsuite");
        let new_policy = NewPolicy::parse(policy_bytes).unwrap();
        new_policy.validate().unwrap();

        // Verify metadata basics
        assert!(new_policy.policy_version() >= 30);
        assert_eq!(new_policy.handle_unknown(), HandleUnknown::Allow);

        // Verify that we can query policy capabilities and permissive map
        // (even if they are empty or have specific values in the test policy,
        // we just verify the APIs exist and don't panic).
        let _caps = new_policy.policy_capabilities();
        let _permissive = new_policy.permissive_map();

        // Verify common symbols are parsed
        assert!(!new_policy.common_symbols().is_empty());
        let common = &new_policy.common_symbols()[0];
        assert!(!common.name_bytes().is_empty());
        assert!(!common.permissions().is_empty());

        // Verify 100% byte-for-byte roundtrip fidelity
        let mut serialized = Vec::new();
        new_policy.serialize(&mut serialized).unwrap();
        assert_eq!(serialized.len(), policy_bytes.len());
        assert_eq!(serialized, policy_bytes);
    }
}
