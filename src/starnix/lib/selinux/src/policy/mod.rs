// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod access_vector;
mod bitmap;
mod booleans;
mod classes;
mod common_symbols;
mod compute;
mod constraints;
mod error;
mod filename_transitions;
mod id_type;
mod indexed;
mod initial_sids;
mod metadata;
mod mls;
mod object_contexts;
mod parser;
mod permissions;
mod policy_cap;
mod roles;
mod rules;
mod security_context;
#[cfg(test)]
mod tests;
mod traits;
mod types;
mod u24_index;
mod users;

use std::num::NonZeroU16;

use selinux_policy_derive::{Parse, Serialize, Validate};

use crate::InitialSid;
pub use access_vector::AccessVector;
use bitmap::IdSpan;
use booleans::{ConditionalBoolean, ConditionalBooleanId};
pub(crate) use classes::Class;
pub use classes::ClassId;
use classes::{ClassDefault, ClassDefaultRange};
use common_symbols::CommonSymbol;
pub use compute::XpermsKind;
pub(crate) use compute::{FsUseLabelAndType, XpermsAccessDecision};
use error::{ParseError, SerializeError, ValidateError};
use filename_transitions::FilenameTransitions;
use indexed::IdAndNameIndexed;
use initial_sids::InitialSids;
pub(crate) use metadata::HandleUnknown;
pub use metadata::POLICYDB_VERSION_MAX;
use metadata::{Config, Counts, Magic, PolicyVersion, Signature};
use mls::{
    Category, CategoryId, CategorySet, CategorySetBuilder, RangeTransition, Sensitivity,
    SensitivityId,
};
pub use object_contexts::FsUseType;
use object_contexts::{GenfsCon, GenfsConPath, ObjectContexts};
pub use parser::PolicyData;
use parser::{Array, PolicyCursor, PolicyWriter, SymbolArray};
pub use permissions::PermissionId;
pub use policy_cap::PolicyCap;
use policy_cap::PolicyCapSet;
pub(crate) use roles::RoleId;
use roles::{Role, RoleAllow, RoleSet, RoleTransition};
pub use rules::AccessDecision;
use rules::{ConditionalNode, IndexedAccessVectorRules};
pub(crate) use rules::{SELINUX_AVD_FLAGS_PERMISSIVE, XpermsBitmap};
use security_context::{MlsLevel, MlsRange};
pub(crate) use security_context::{SecurityContext, SecurityContextError};
pub use traits::PolicyId;
pub(crate) use traits::{HasName, HasPolicyId};
use traits::{Serialize, Validate};
pub(crate) use types::TypeId;
use types::{PermissiveTypeSet, TypeAttributeMaps, TypeSet, Types};
use u24_index::U24Index;
use users::{User, UserId};

/// Maximum supported binary policy size in bytes (`U24Index` limit).
///
/// As of 2026-01-30, this is more than five times larger than any policy seen in production or
/// tests.
const MAXIMUM_POLICY_SIZE: usize = (1 << 24) - 1;

/// Mandatory role name and ID assigned to non-process/non-socket SELinux objects by default.
const OBJECT_R_ROLE_NAME: &[u8] = b"object_r";
const OBJECT_R_ROLE_ID: RoleId = RoleId::new(NonZeroU16::MIN);

/// Top-level parsed SELinux binary policy.
#[derive(Debug, Parse, Serialize, Validate)]
pub(crate) struct Policy {
    magic: Magic,
    signature: Signature,
    version: PolicyVersion,
    config: Config,
    counts: Counts,
    policy_capabilities: PolicyCapSet,
    permissive_map: PermissiveTypeSet,
    common_symbols: IdAndNameIndexed<SymbolArray<CommonSymbol>>,
    classes: IdAndNameIndexed<SymbolArray<Class>>,
    roles: IdAndNameIndexed<SymbolArray<Role>>,
    types: Types,
    users: IdAndNameIndexed<SymbolArray<User>>,
    conditional_booleans: IdAndNameIndexed<SymbolArray<ConditionalBoolean>>,
    sensitivities: IdAndNameIndexed<SymbolArray<Sensitivity>>,
    categories: IdAndNameIndexed<SymbolArray<Category>>,
    access_vector_rules: IndexedAccessVectorRules,
    conditional_nodes: Array<ConditionalNode>,
    role_transitions: Array<RoleTransition>,
    role_allowlist: Array<RoleAllow>,
    filename_transitions: FilenameTransitions,
    initial_sids: InitialSids,
    object_contexts: ObjectContexts,
    generic_fs_contexts: Array<GenfsCon>,
    range_transitions: Array<RangeTransition>,
    type_attribute_maps: TypeAttributeMaps,
}

impl Policy {
    /// Parses a [`Policy`] from the raw binary data.
    pub fn parse(data: &[u8]) -> Result<Self, ParseError> {
        let policy_size = data.len();
        if policy_size > MAXIMUM_POLICY_SIZE {
            return Err(ParseError::UnsupportedlyLarge {
                observed: policy_size,
                limit: MAXIMUM_POLICY_SIZE,
            });
        }
        let mut cursor = PolicyCursor::new(data);
        let policy = cursor.parse()?;
        if cursor.offset() < data.len() {
            return Err(ParseError::TrailingBytes { num_bytes: data.len() - cursor.offset() });
        }
        Ok(policy)
    }

    /// Validates the parsed policy.
    pub fn validate(&self) -> Result<(), ValidateError> {
        Validate::validate(self, self)?;

        // Validate that all kernel-required initial SIDs are present in the policy.
        let need_init_sid = self.has_policycap(PolicyCap::UserspaceInitialContext);
        for initial_sid in InitialSid::all_variants() {
            if *initial_sid == InitialSid::Init && !need_init_sid {
                continue;
            }
            self.initial_sids()
                .get_by_id(*initial_sid as u32)
                .ok_or(ValidateError::MissingInitialSid { initial_sid: *initial_sid })?;
        }

        // Validate that the well-known "object_r" role is defined at role ID 1.
        if self.roles().get_by_id(OBJECT_R_ROLE_ID).map(|role| role.name())
            != Some(OBJECT_R_ROLE_NAME)
        {
            return Err(ValidateError::MissingObjectRRole);
        }

        // TODO: https://fxbug.dev/575728936 - Validate that `typebounds` hierarchies do not
        // contain cycles, exceed maximum depth, or reference attributes.

        Ok(())
    }

    /// Parses and validates a [`Policy`] from raw binary data for tests.
    #[cfg(test)]
    pub(crate) fn parse_and_validate(data: &[u8]) -> Self {
        let policy = Self::parse(data).expect("parse policy");
        policy.validate().expect("validate policy");
        policy
    }

    /// Serializes the policy to binary representation.
    pub fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        let mut policy_writer = PolicyWriter::new(self.version, writer);
        Serialize::serialize(self, &mut policy_writer)
    }

    /// Returns the policy version.
    #[cfg(test)]
    fn version(&self) -> PolicyVersion {
        self.version
    }

    /// Returns the [`HandleUnknown`] configuration.
    pub fn handle_unknown(&self) -> HandleUnknown {
        self.config.handle_unknown()
    }

    /// Returns the policy capabilities set.
    fn policy_capabilities(&self) -> &PolicyCapSet {
        &self.policy_capabilities
    }

    /// Returns the permissive types set.
    fn permissive_map(&self) -> &PermissiveTypeSet {
        &self.permissive_map
    }

    /// Returns the common symbols table.
    fn common_symbols(&self) -> &IdAndNameIndexed<SymbolArray<CommonSymbol>> {
        &self.common_symbols
    }

    /// Returns the object classes table.
    pub fn classes(&self) -> &IdAndNameIndexed<SymbolArray<Class>> {
        &self.classes
    }

    /// Returns the roles table.
    fn roles(&self) -> &IdAndNameIndexed<SymbolArray<Role>> {
        &self.roles
    }

    /// Returns the types table.
    pub fn types(&self) -> &Types {
        &self.types
    }

    /// Returns the users table.
    fn users(&self) -> &IdAndNameIndexed<SymbolArray<User>> {
        &self.users
    }

    /// Returns the conditional booleans table.
    pub fn conditional_booleans(&self) -> &IdAndNameIndexed<SymbolArray<ConditionalBoolean>> {
        &self.conditional_booleans
    }

    /// Returns the sensitivities table.
    fn sensitivities(&self) -> &IdAndNameIndexed<SymbolArray<Sensitivity>> {
        &self.sensitivities
    }

    /// Returns the categories table.
    fn categories(&self) -> &IdAndNameIndexed<SymbolArray<Category>> {
        &self.categories
    }

    /// Returns the access vector rules table.
    fn access_vector_rules(&self) -> &IndexedAccessVectorRules {
        &self.access_vector_rules
    }

    /// Returns the role transitions rules array.
    fn role_transitions(&self) -> &[RoleTransition] {
        self.role_transitions.as_ref()
    }

    /// Returns the role allow rules array.
    fn role_allowlist(&self) -> &[RoleAllow] {
        self.role_allowlist.as_ref()
    }

    /// Returns the filename transitions table.
    fn filename_transitions(&self) -> &FilenameTransitions {
        &self.filename_transitions
    }

    /// Returns the initial SIDs table.
    fn initial_sids(&self) -> &InitialSids {
        &self.initial_sids
    }

    /// Returns the object contexts table.
    pub fn object_contexts(&self) -> &ObjectContexts {
        &self.object_contexts
    }

    /// Returns generic filesystem labeling statements (`genfscon`).
    fn generic_fs_contexts(&self) -> &[GenfsCon] {
        &self.generic_fs_contexts
    }

    /// Returns the MLS range transitions array.
    fn range_transitions(&self) -> &[RangeTransition] {
        &self.range_transitions
    }

    /// Returns the type attribute maps table.
    fn type_attribute_maps(&self) -> &TypeAttributeMaps {
        &self.type_attribute_maps
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;
    use crate::policy::traits::{Parse, Serialize};

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
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut writer);
        TestEnum::ValueOne.serialize(&mut policy_writer).unwrap();
        assert_eq!(writer, vec![1, 0, 0, 0]);

        let mut writer = Vec::new();
        let mut policy_writer = PolicyWriter::new(PolicyVersion::V33, &mut writer);
        TestEnum::ValueTwo.serialize(&mut policy_writer).unwrap();
        assert_eq!(writer, vec![2, 0, 0, 0]);

        let policy_bytes = include_bytes!("../../testdata/policies/selinux_testsuite");
        let policy = Policy::parse(policy_bytes).unwrap();
        TestEnum::ValueOne.validate(&policy).unwrap();
    }

    const TEST_POLICIES: &[(&str, &[u8])] = &[
        ("selinux_testsuite", include_bytes!("../../testdata/policies/selinux_testsuite")),
        ("emulator", include_bytes!("../../testdata/policies/emulator")),
        (
            "conditional_policy",
            include_bytes!("../../testdata/composite_policies/compiled/conditional_policy"),
        ),
        (
            "minimal_policy",
            include_bytes!("../../testdata/composite_policies/compiled/minimal_policy"),
        ),
        (
            "allow_fork_policy",
            include_bytes!("../../testdata/composite_policies/compiled/allow_fork_policy"),
        ),
        (
            "class_defaults_policy",
            include_bytes!("../../testdata/composite_policies/compiled/class_defaults_policy"),
        ),
        (
            "genfscon_policy",
            include_bytes!("../../testdata/composite_policies/compiled/genfscon_policy"),
        ),
        (
            "handle_unknown_policy-allow",
            include_bytes!(
                "../../testdata/composite_policies/compiled/handle_unknown_policy-allow"
            ),
        ),
        (
            "handle_unknown_policy-deny",
            include_bytes!("../../testdata/composite_policies/compiled/handle_unknown_policy-deny"),
        ),
        (
            "handle_unknown_policy-reject",
            include_bytes!(
                "../../testdata/composite_policies/compiled/handle_unknown_policy-reject"
            ),
        ),
        (
            "range_transition_policy",
            include_bytes!("../../testdata/composite_policies/compiled/range_transition_policy"),
        ),
        (
            "role_transition_policy",
            include_bytes!("../../testdata/composite_policies/compiled/role_transition_policy"),
        ),
        (
            "type_transition_policy",
            include_bytes!("../../testdata/composite_policies/compiled/type_transition_policy"),
        ),
    ];

    #[test]
    fn test_all_compiled_policies_roundtrip() {
        for (name, policy_bytes) in TEST_POLICIES {
            let policy = Policy::parse(policy_bytes)
                .unwrap_or_else(|e| panic!("Failed to parse {name}: {e:?}"));
            policy.validate().unwrap_or_else(|e| panic!("Failed to validate {name}: {e:?}"));

            if policy.version() >= PolicyVersion::V33 {
                let mut serialized = Vec::new();
                policy
                    .serialize(&mut serialized)
                    .unwrap_or_else(|e| panic!("Failed to serialize {name}: {e:?}"));
                assert_bytes_eq(&serialized, policy_bytes);
            }
        }
    }

    #[test]
    fn test_initial_sids_policy_elements() {
        let policy_bytes =
            include_bytes!("../../testdata/composite_policies/compiled/minimal_policy");
        let policy = Policy::parse(policy_bytes).expect("parse minimal policy");
        policy.validate().expect("validate minimal policy");

        assert!(!policy.initial_sids().is_empty());
        assert!(policy.initial_sids().len() >= crate::InitialSid::all_variants().len() - 1);

        let kernel_context =
            policy.initial_sids().get_by_id(crate::InitialSid::Kernel as u32).unwrap();
        assert!(policy.users().get_by_id(kernel_context.user()).is_some());
        assert!(policy.roles().get_by_id(kernel_context.role()).is_some());

        let unlabeled_context =
            policy.initial_sids().get_by_id(crate::InitialSid::Unlabeled as u32).unwrap();
        assert!(policy.users().get_by_id(unlabeled_context.user()).is_some());
        assert!(policy.roles().get_by_id(unlabeled_context.role()).is_some());
    }

    #[test]
    fn test_object_r_role_id_validation() {
        let policy_bytes =
            include_bytes!("../../testdata/composite_policies/compiled/minimal_policy");
        let mut policy = Policy::parse_and_validate(policy_bytes);

        // Swap the role ID of "object_r" (ID 1) with another role so that role ID 1 still exists
        // in the policy, but is no longer assigned to "object_r".
        let roles = policy.roles.as_mut_slice();
        let object_r_index =
            roles.iter().position(|role| role.name() == OBJECT_R_ROLE_NAME).unwrap();
        let other_index = roles.iter().position(|role| role.name() != OBJECT_R_ROLE_NAME).unwrap();
        let object_r_id = roles[object_r_index].id();
        let other_id = roles[other_index].id();
        roles[object_r_index].set_id(other_id);
        roles[other_index].set_id(object_r_id);

        let mut serialized = Vec::new();
        policy.serialize(&mut serialized).expect("serialize modified policy");

        let reparsed = Policy::parse(&serialized).expect("re-parse modified policy");
        assert_eq!(reparsed.validate(), Err(ValidateError::MissingObjectRRole));
    }
}

#[cfg(test)]
fn assert_bytes_eq(left: &[u8], right: &[u8]) {
    if left != right {
        let min_len = std::cmp::min(left.len(), right.len());
        for i in 0..min_len {
            if left[i] != right[i] {
                let start = i.saturating_sub(8);
                let end = std::cmp::min(i + 16, min_len);
                panic!(
                    "Byte mismatch at offset {i} (0x{i:x}): actual=0x{:02x} vs expected=0x{:02x}.\nActual   [{start}..{end}]: {:02x?}\nExpected [{start}..{end}]: {:02x?}",
                    left[i],
                    right[i],
                    &left[start..end],
                    &right[start..end]
                );
            }
        }
        panic!("Length mismatch: actual={}, expected={}", left.len(), right.len());
    }
}
