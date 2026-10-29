// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bstr::BString;
use selinux_policy_derive::{Parse, Serialize, Validate};
use thiserror::Error;

use super::traits::{HasName, HasPolicyId};
use super::{MlsRange, Policy, RoleId, TypeId, UserId};
use crate::NullessByteStr;

/// Security context, a variable-length label associated with each SELinux object in the
/// system. Contains mandatory `user:role:type` components and an MLS `range` component.
///
/// Used both in parsed binary policy structures (such as initial SIDs, `fs_use_*`, and
/// `genfscon` rules) and at runtime when mapping userspace security context strings to
/// [`crate::SecurityId`]s.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Parse, Serialize, Validate)]
pub struct SecurityContext {
    user: UserId,
    role: RoleId,
    type_: TypeId,
    mls_range: MlsRange,
}

impl SecurityContext {
    /// Returns a new instance with the specified field values.
    ///
    /// Fields are not validated against policy constraints until explicitly validated via
    /// [`Policy::validate_security_context`], or implicitly via insertion into a
    /// [`crate::sid_table::SidTable`].
    pub(super) fn new(user: UserId, role: RoleId, type_: TypeId, mls_range: MlsRange) -> Self {
        Self { user, role, type_, mls_range }
    }

    /// Returns the user ID.
    pub(super) fn user(&self) -> UserId {
        self.user
    }

    /// Returns the role ID.
    pub fn role(&self) -> RoleId {
        self.role
    }

    /// Returns the type ID.
    pub fn type_(&self) -> TypeId {
        self.type_
    }

    /// Returns the MLS security range.
    pub(super) fn mls_range(&self) -> &MlsRange {
        &self.mls_range
    }
}

impl Policy {
    /// Parses a security context string into a [`SecurityContext`] structure, resolving user,
    /// role, type, sensitivity, and category names against this policy.
    ///
    /// Semantic consistency across the resolved fields (such as user-role and role-type
    /// authorizations or MLS range validity) is not checked here; call
    /// [`Self::validate_security_context`] to validate the resulting [`SecurityContext`] against
    /// policy constraints.
    ///
    /// Security Contexts in Multi-Level Security (MLS) and Multi-Category Security (MCS)
    /// policies take the form:
    ///   `context := <user>:<role>:<type>:<levels>`
    /// such that they always include user, role, type, and a range of
    /// security levels.
    ///
    /// The security levels part consists of a "low" value and optional "high"
    /// value, defining the range. In MCS policies each level may optionally be
    /// associated with a set of categories:
    ///   `levels := <level>[-<level>]`
    ///   `level := <sensitivity>[:<category_spec>[,<category_spec>]*]`
    ///
    /// Entries in the optional list of categories may specify individual
    /// categories, or ranges (from low to high):
    ///   `category_spec := <category>[.<category>]`
    ///
    /// e.g. `"u:r:t:s0"` has a single (low) sensitivity.
    /// e.g. `"u:r:t:s0-s1"` has a sensitivity range.
    /// e.g. `"u:r:t:s0:c1,c2,c3"` has a single sensitivity, with three categories.
    /// e.g. `"u:r:t:s0:c1-s1:c1,c2,c3"` has a sensitivity range, with categories
    ///      associated with both low and high ends.
    ///
    /// Returns an error if `security_context` is not a syntactically valid
    /// Security Context string, or if any field name is not defined by this policy.
    pub fn parse_security_context(
        &self,
        security_context: NullessByteStr<'_>,
    ) -> Result<SecurityContext, SecurityContextError> {
        let as_str = std::str::from_utf8(security_context.as_bytes())
            .map_err(|_| SecurityContextError::InvalidSyntax)?;

        // Parse the user, role, type and security level parts, to validate syntax.
        let mut items = as_str.splitn(4, ':');
        let user = items.next().ok_or(SecurityContextError::InvalidSyntax)?;
        let role = items.next().ok_or(SecurityContextError::InvalidSyntax)?;
        let type_ = items.next().ok_or(SecurityContextError::InvalidSyntax)?;
        let levels = items.next().ok_or(SecurityContextError::InvalidSyntax)?;

        // Resolve the user, role, type and security levels to identifiers.
        let user = self
            .users
            .get_by_name(user.as_bytes())
            .ok_or_else(|| SecurityContextError::UnknownUser { name: user.into() })?
            .id();
        let role = self
            .roles
            .get_by_name(role.as_bytes())
            .ok_or_else(|| SecurityContextError::UnknownRole { name: role.into() })?
            .id();
        let type_ = self
            .types
            .get_by_name(type_.as_bytes())
            .ok_or_else(|| SecurityContextError::UnknownType { name: type_.into() })?
            .id();
        let mls_range = MlsRange::parse_str(self, levels)?;

        Ok(SecurityContext::new(user, role, type_, mls_range))
    }

    /// Validates that `context`'s fields are consistent with this policy's constraints (e.g. that
    /// the role is valid for the user).
    pub fn validate_security_context(
        &self,
        context: &SecurityContext,
    ) -> Result<(), SecurityContextError> {
        let user = self.users.get_by_id(context.user()).unwrap();

        // Check that the security context's levels are internally consistent: i.e., that the
        // high level, if any, dominates the low level. This applies to every context, including
        // those labelled with the special "object_r" role.
        if !context.mls_range.is_consistent() {
            return Err(SecurityContextError::InvalidSecurityRange {
                low: context.mls_range.low().serialize_str(self).into(),
                high: context.mls_range.effective_high().serialize_str(self).into(),
            });
        }

        // Validation of the user/role/type relationships is skipped for the special "object_r"
        // role, which is applied by default to non-process/socket-like resources.
        if context.role() == self.object_role() {
            return Ok(());
        }

        // Validate that the selected role is valid for this user.
        if !user.roles().contains(context.role()) {
            return Err(SecurityContextError::InvalidRoleForUser {
                role: self.roles.get_by_id(context.role()).unwrap().name().into(),
                user: user.name().into(),
            });
        }

        // Validate that the selected type is valid for this role.
        let role = self.roles.get_by_id(context.role()).unwrap();
        if !role.types().contains(context.type_()) {
            return Err(SecurityContextError::InvalidTypeForRole {
                type_: self.types.get_by_id(context.type_()).unwrap().name().into(),
                role: role.name().into(),
            });
        }

        // Check that the security context's MLS range is valid for the user.
        let user_range = user.mls_range();

        // 1. Check that the security context's low level is in the valid range for the user.
        if !user_range.contains_level(context.mls_range.low()) {
            return Err(SecurityContextError::InvalidLevelForUser {
                level: context.mls_range.low().serialize_str(self).into(),
                user: user.name().into(),
            });
        }

        // 2. Check that the security context's high level is in the valid range for the user.
        if let Some(high_level) = context.mls_range.high() {
            if !user_range.contains_level(high_level) {
                return Err(SecurityContextError::InvalidLevelForUser {
                    level: high_level.serialize_str(self).into(),
                    user: user.name().into(),
                });
            }
        }

        Ok(())
    }

    /// Serializes `context` to a canonical Security Context byte string.
    pub fn serialize_security_context(&self, context: &SecurityContext) -> Vec<u8> {
        let levels = context.mls_range().serialize_str(self);
        let type_ = self.types.get_by_id(context.type_()).unwrap();
        let parts: [&[u8]; 4] = [
            self.users.get_by_id(context.user()).unwrap().name(),
            self.roles.get_by_id(context.role()).unwrap().name(),
            type_.name(),
            levels.as_slice(),
        ];
        parts.join(b":".as_ref())
    }
}

/// Errors that may be returned when attempting to parse or validate a security context.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SecurityContextError {
    #[error("security context syntax is invalid")]
    InvalidSyntax,
    #[error("sensitivity {name:?} not defined by policy")]
    UnknownSensitivity { name: BString },
    #[error("category {name:?} not defined by policy")]
    UnknownCategory { name: BString },
    #[error("user {name:?} not defined by policy")]
    UnknownUser { name: BString },
    #[error("role {name:?} not defined by policy")]
    UnknownRole { name: BString },
    #[error("type {name:?} not defined by policy")]
    UnknownType { name: BString },
    #[error("role {role:?} not valid for {user:?}")]
    InvalidRoleForUser { role: BString, user: BString },
    #[error("type {type_:?} not valid for {role:?}")]
    InvalidTypeForRole { role: BString, type_: BString },
    #[error("security level {level:?} not valid for {user:?}")]
    InvalidLevelForUser { level: BString, user: BString },
    #[error("high security level {high:?} lower than low level {low:?}")]
    InvalidSecurityRange { low: BString, high: BString },
}

#[cfg(test)]
mod tests {
    use super::super::mls::{CategoryId, CategorySpan, MlsLevel, SensitivityId};
    use super::*;

    fn test_policy() -> Policy {
        const TEST_POLICY: &[u8] =
            include_bytes!("../../testdata/micro_policies/security_context_tests_policy");
        let policy = Policy::parse(TEST_POLICY).unwrap();
        policy.validate().unwrap();
        policy
    }

    // CategoryItem helper for tests.
    #[derive(Debug, Eq, PartialEq)]
    struct CategoryItem {
        low: String,
        high: String,
    }

    fn user_name(policy: &Policy, id: UserId) -> &str {
        std::str::from_utf8(policy.users.get_by_id(id).unwrap().name()).unwrap()
    }

    fn role_name(policy: &Policy, id: RoleId) -> &str {
        std::str::from_utf8(policy.roles.get_by_id(id).unwrap().name()).unwrap()
    }

    fn type_name(policy: &Policy, id: TypeId) -> &str {
        std::str::from_utf8(policy.types.get_by_id(id).unwrap().name()).unwrap()
    }

    fn sensitivity_name(policy: &Policy, id: SensitivityId) -> &str {
        std::str::from_utf8(policy.sensitivities.get_by_id(id).unwrap().name()).unwrap()
    }

    fn category_name(policy: &Policy, id: CategoryId) -> &str {
        std::str::from_utf8(policy.categories.get_by_id(id).unwrap().name()).unwrap()
    }

    fn category_span(policy: &Policy, category: &CategorySpan) -> CategoryItem {
        CategoryItem {
            low: category_name(policy, category.low()).into(),
            high: category_name(policy, category.high()).into(),
        }
    }

    fn category_spans(policy: &Policy, level: &MlsLevel) -> Vec<CategoryItem> {
        level.categories().spans().map(|x| category_span(policy, &x)).collect()
    }

    #[test]
    fn parse_security_context_single_sensitivity() {
        let policy = test_policy();
        let security_context = policy
            .parse_security_context(b"user0:object_r:type0:s0".into())
            .expect("creating security context should succeed");
        assert_eq!(user_name(&policy, security_context.user()), "user0");
        assert_eq!(role_name(&policy, security_context.role()), "object_r");
        assert_eq!(type_name(&policy, security_context.type_()), "type0");
        assert_eq!(
            sensitivity_name(&policy, security_context.mls_range().low().sensitivity()),
            "s0"
        );
        assert!(category_spans(&policy, security_context.mls_range().low()).is_empty());
        assert_eq!(security_context.mls_range().high(), None);
    }

    #[test]
    fn parse_security_context_with_sensitivity_range() {
        let policy = test_policy();
        let security_context = policy
            .parse_security_context(b"user0:object_r:type0:s0-s1".into())
            .expect("creating security context should succeed");
        assert_eq!(user_name(&policy, security_context.user()), "user0");
        assert_eq!(role_name(&policy, security_context.role()), "object_r");
        assert_eq!(type_name(&policy, security_context.type_()), "type0");
        assert_eq!(
            sensitivity_name(&policy, security_context.mls_range().low().sensitivity()),
            "s0"
        );
        assert!(category_spans(&policy, security_context.mls_range().low()).is_empty());
        let high_level = security_context.mls_range().high().unwrap();
        assert_eq!(sensitivity_name(&policy, high_level.sensitivity()), "s1");
        assert!(category_spans(&policy, high_level).is_empty());
    }

    #[test]
    fn parse_security_context_with_single_sensitivity_and_categories_interval() {
        let policy = test_policy();
        let security_context = policy
            .parse_security_context(b"user0:object_r:type0:s1:c0.c4".into())
            .expect("creating security context should succeed");
        assert_eq!(user_name(&policy, security_context.user()), "user0");
        assert_eq!(role_name(&policy, security_context.role()), "object_r");
        assert_eq!(type_name(&policy, security_context.type_()), "type0");
        assert_eq!(
            sensitivity_name(&policy, security_context.mls_range().low().sensitivity()),
            "s1"
        );
        assert_eq!(
            category_spans(&policy, security_context.mls_range().low()),
            [CategoryItem { low: "c0".to_string(), high: "c4".to_string() }]
        );
        assert_eq!(security_context.mls_range().high(), None);
    }

    #[test]
    fn parse_security_context_and_normalize_categories() {
        let policy = &test_policy();
        let normalize = {
            |security_context: &str| -> String {
                String::from_utf8(
                    policy.serialize_security_context(
                        &policy
                            .parse_security_context(security_context.into())
                            .expect("creating security context should succeed"),
                    ),
                )
                .unwrap()
            }
        };
        // Overlapping category ranges are merged.
        assert_eq!(normalize("user0:object_r:type0:s1:c0.c1,c1"), "user0:object_r:type0:s1:c0,c1");
        assert_eq!(
            normalize("user0:object_r:type0:s1:c0.c2,c1.c2"),
            "user0:object_r:type0:s1:c0.c2"
        );
        assert_eq!(
            normalize("user0:object_r:type0:s1:c0.c2,c1.c3"),
            "user0:object_r:type0:s1:c0.c3"
        );
        // Adjacent category ranges are merged.
        assert_eq!(normalize("user0:object_r:type0:s1:c0.c1,c2"), "user0:object_r:type0:s1:c0.c2");
        // Category ranges are ordered by first element.
        assert_eq!(
            normalize("user0:object_r:type0:s1:c2.c3,c0"),
            "user0:object_r:type0:s1:c0,c2,c3"
        );
    }

    #[test]
    fn parse_security_context_with_sensitivity_range_and_category_interval() {
        let policy = test_policy();
        let security_context = policy
            .parse_security_context(b"user0:object_r:type0:s0-s1:c0.c4".into())
            .expect("creating security context should succeed");
        assert_eq!(user_name(&policy, security_context.user()), "user0");
        assert_eq!(role_name(&policy, security_context.role()), "object_r");
        assert_eq!(type_name(&policy, security_context.type_()), "type0");
        assert_eq!(
            sensitivity_name(&policy, security_context.mls_range().low().sensitivity()),
            "s0"
        );
        assert!(category_spans(&policy, security_context.mls_range().low()).is_empty());
        let high_level = security_context.mls_range().high().unwrap();
        assert_eq!(sensitivity_name(&policy, high_level.sensitivity()), "s1");
        assert_eq!(
            category_spans(&policy, high_level),
            [CategoryItem { low: "c0".to_string(), high: "c4".to_string() }]
        );
    }

    #[test]
    fn parse_security_context_with_sensitivity_range_with_categories() {
        let policy = test_policy();
        let security_context = policy
            .parse_security_context(b"user0:object_r:type0:s0:c0-s1:c0.c4".into())
            .expect("creating security context should succeed");
        assert_eq!(user_name(&policy, security_context.user()), "user0");
        assert_eq!(role_name(&policy, security_context.role()), "object_r");
        assert_eq!(type_name(&policy, security_context.type_()), "type0");
        assert_eq!(
            sensitivity_name(&policy, security_context.mls_range().low().sensitivity()),
            "s0"
        );
        assert_eq!(
            category_spans(&policy, security_context.mls_range().low()),
            [CategoryItem { low: "c0".to_string(), high: "c0".to_string() }]
        );

        let high_level = security_context.mls_range().high().unwrap();
        assert_eq!(sensitivity_name(&policy, high_level.sensitivity()), "s1");
        assert_eq!(
            category_spans(&policy, high_level),
            [CategoryItem { low: "c0".to_string(), high: "c4".to_string() }]
        );
    }

    #[test]
    fn parse_security_context_with_single_sensitivity_and_category_list() {
        let policy = test_policy();
        let security_context = policy
            .parse_security_context(b"user0:object_r:type0:s1:c0,c4".into())
            .expect("creating security context should succeed");
        assert_eq!(user_name(&policy, security_context.user()), "user0");
        assert_eq!(role_name(&policy, security_context.role()), "object_r");
        assert_eq!(type_name(&policy, security_context.type_()), "type0");
        assert_eq!(
            sensitivity_name(&policy, security_context.mls_range().low().sensitivity()),
            "s1"
        );
        assert_eq!(
            category_spans(&policy, security_context.mls_range().low()),
            [
                CategoryItem { low: "c0".to_string(), high: "c0".to_string() },
                CategoryItem { low: "c4".to_string(), high: "c4".to_string() }
            ]
        );
        assert_eq!(security_context.mls_range().high(), None);
    }

    #[test]
    fn parse_security_context_with_single_sensitivity_and_category_list_and_range() {
        let policy = test_policy();
        let security_context = policy
            .parse_security_context(b"user0:object_r:type0:s1:c0,c3.c4".into())
            .expect("creating security context should succeed");
        assert_eq!(user_name(&policy, security_context.user()), "user0");
        assert_eq!(role_name(&policy, security_context.role()), "object_r");
        assert_eq!(type_name(&policy, security_context.type_()), "type0");
        assert_eq!(
            sensitivity_name(&policy, security_context.mls_range().low().sensitivity()),
            "s1"
        );
        assert_eq!(
            category_spans(&policy, security_context.mls_range().low()),
            [
                CategoryItem { low: "c0".to_string(), high: "c0".to_string() },
                CategoryItem { low: "c3".to_string(), high: "c4".to_string() }
            ]
        );
        assert_eq!(security_context.mls_range().high(), None);
    }

    #[test]
    fn parse_invalid_syntax() {
        let policy = test_policy();
        for invalid_label in [
            "user0",
            "user0:object_r",
            "user0:object_r:type0",
            "user0:object_r:type0:s0-",
            "user0:object_r:type0:s0:s0:s0",
            "user0:object_r:type0:s0:c0.c0", // Category upper bound is equal to lower bound.
            "user0:object_r:type0:s0:c1.c0", // Category upper bound is less than lower bound.
        ] {
            assert_eq!(
                policy.parse_security_context(invalid_label.as_bytes().into()),
                Err(SecurityContextError::InvalidSyntax),
                "validating {:?}",
                invalid_label
            );
        }
    }

    #[test]
    fn parse_invalid_sensitivity() {
        let policy = test_policy();
        for invalid_label in ["user0:object_r:type0:s_invalid", "user0:object_r:type0:s0-s_invalid"]
        {
            assert_eq!(
                policy.parse_security_context(invalid_label.as_bytes().into()),
                Err(SecurityContextError::UnknownSensitivity { name: "s_invalid".into() }),
                "validating {:?}",
                invalid_label
            );
        }
    }

    #[test]
    fn parse_invalid_category() {
        let policy = test_policy();
        for invalid_label in
            ["user0:object_r:type0:s1:c_invalid", "user0:object_r:type0:s1:c0.c_invalid"]
        {
            assert_eq!(
                policy.parse_security_context(invalid_label.as_bytes().into()),
                Err(SecurityContextError::UnknownCategory { name: "c_invalid".into() }),
                "validating {:?}",
                invalid_label
            );
        }
    }

    #[test]
    fn invalid_security_context_fields() {
        let policy = test_policy();

        // Fails validation because the security context's high level does not dominate its
        // low level: the low level has categories that the high level does not.
        let context = policy
            .parse_security_context(b"user0:object_r:type0:s1:c0,c3.c4-s1".into())
            .expect("successfully parsed");
        assert_eq!(
            policy.validate_security_context(&context),
            Err(SecurityContextError::InvalidSecurityRange {
                low: "s1:c0,c3,c4".into(),
                high: "s1".into()
            })
        );

        // Fails validation because the security context's high level does not dominate its
        // low level: the category sets of the high level and low level are not comparable.
        let context = policy
            .parse_security_context(b"user0:object_r:type0:s1:c0-s1:c1".into())
            .expect("successfully parsed");
        assert_eq!(
            policy.validate_security_context(&context),
            Err(SecurityContextError::InvalidSecurityRange {
                low: "s1:c0".into(),
                high: "s1:c1".into()
            })
        );

        // Fails validation because the security context's high level does not dominate its
        // low level: the sensitivity of the high level is lower than that of the low level.
        let context = policy
            .parse_security_context(b"user0:object_r:type0:s1:c0-s0:c0.c1".into())
            .expect("successfully parsed");
        assert_eq!(
            policy.validate_security_context(&context),
            Err(SecurityContextError::InvalidSecurityRange {
                low: "s1:c0".into(),
                high: "s0:c0,c1".into()
            })
        );

        // Fails validation because the policy's high level does not dominate the
        // security context's high level: the security context's high level has categories
        // that the policy's high level does not.
        let context = policy
            .parse_security_context(b"user1:subject_r:type0:s1-s1:c3".into())
            .expect("successfully parsed");
        assert_eq!(
            policy.validate_security_context(&context),
            Err(SecurityContextError::InvalidLevelForUser {
                level: "s1:c3".into(),
                user: "user1".into(),
            })
        );

        // Fails validation because the security context's low level does not dominate
        // the policy's low level: the security context's low level has a lower sensitivity
        // than the policy's low level.
        let context = policy
            .parse_security_context(b"user1:subject_r:type0:s0".into())
            .expect("successfully parsed");
        assert_eq!(
            policy.validate_security_context(&context),
            Err(SecurityContextError::InvalidLevelForUser {
                level: "s0".into(),
                user: "user1".into(),
            })
        );

        // Passes validation even though the level is outside the user's range, because the
        // special "object_r" role is exempt from the user MLS range check.
        let context = policy
            .parse_security_context(b"user1:object_r:type0:s0".into())
            .expect("successfully parsed");
        assert!(policy.validate_security_context(&context).is_ok());

        // Fails validation because the role is not valid for the user.
        let context = policy
            .parse_security_context(b"user0:subject_r:type0:s0".into())
            .expect("successfully parsed");
        assert!(policy.validate_security_context(&context).is_err());

        // Fails validation because the type is not valid for the role.
        let context = policy
            .parse_security_context(b"user1:subject_r:non_subject_t:s1".into())
            .expect("successfully parsed");
        assert!(policy.validate_security_context(&context).is_err());

        // Passes validation even though the role is not explicitly allowed for the user,
        // because it is the special "object_r" role, used when labelling resources.
        let context = policy
            .parse_security_context(b"user1:object_r:type0:s1".into())
            .expect("successfully parsed");
        assert!(policy.validate_security_context(&context).is_ok());
    }

    #[test]
    fn format_security_contexts() {
        let policy = test_policy();
        for label in [
            "user0:object_r:type0:s0",
            "user0:object_r:type0:s0-s1",
            "user0:object_r:type0:s1:c0.c4",
            "user0:object_r:type0:s0-s1:c0.c4",
            "user0:object_r:type0:s1:c0,c3",
            "user0:object_r:type0:s0-s1:c0,c2,c4",
            "user0:object_r:type0:s1:c0,c3,c4-s1:c0,c2.c4",
        ] {
            let security_context =
                policy.parse_security_context(label.as_bytes().into()).expect("should succeed");
            assert_eq!(policy.serialize_security_context(&security_context), label.as_bytes());
        }
    }

    #[test]
    fn normalize_equal_low_and_high_levels() {
        let policy = test_policy();
        let single_level =
            policy.parse_security_context(b"user0:object_r:type0:s0:c0".into()).unwrap();
        let equal_range =
            policy.parse_security_context(b"user0:object_r:type0:s0:c0-s0:c0".into()).unwrap();
        assert_eq!(equal_range.mls_range().high(), None);
        assert_eq!(equal_range, single_level);
        assert_eq!(policy.serialize_security_context(&equal_range), b"user0:object_r:type0:s0:c0");
    }
}
