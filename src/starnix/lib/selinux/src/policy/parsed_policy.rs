// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::arrays::{
    ACCESS_VECTOR_RULE_TYPE_ALLOW, ACCESS_VECTOR_RULE_TYPE_ALLOWXPERM,
    ACCESS_VECTOR_RULE_TYPE_AUDITALLOW, ACCESS_VECTOR_RULE_TYPE_AUDITALLOWXPERM,
    ACCESS_VECTOR_RULE_TYPE_DONTAUDIT, ACCESS_VECTOR_RULE_TYPE_DONTAUDITXPERM, AccessVectorRule,
    AccessVectorRuleMetadata, ConditionalNode, Context, DeprecatedFilenameTransition,
    ExtendedPermissions, FilenameTransition, FilenameTransitionList, FsUse, GenericFsContext,
    IPv6Node, InfinitiBandEndPort, InfinitiBandPartitionKey, InitialSid,
    MIN_POLICY_VERSION_FOR_INFINITIBAND_PARTITION_KEY, NamedContextPair, Node, Port,
    RangeTransition, RoleAllow, RoleAllows, RoleTransition, RoleTransitions, SimpleArray,
    XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES, XPERMS_TYPE_IOCTL_PREFIXES, XPERMS_TYPE_NLMSG,
    XpermsBitmap,
};
use super::error::{ParseError, ValidateError};
use super::extensible_bitmap::ExtensibleBitmap;

use super::parser::{PolicyCursor, PolicyData};
use super::security_context::SecurityContext;
use super::symbols::{
    Category, CategoryIndex, ConditionalBoolean, Role, Sensitivity, SymbolList, Type, TypeIndex,
    User,
};
use super::view::{Hashable, HashedArrayView};
use super::{
    AccessDecision, AccessVector, CategoryId, ClassId, MlsLevel, Parse, PolicyValidationContext,
    RoleId, SELINUX_AVD_FLAGS_PERMISSIVE, SensitivityId, TypeId, UserId, Validate,
    XpermsAccessDecision, XpermsKind,
};

use crate::new_policy::traits::{HasPolicyId, PolicyId};
use crate::new_policy::{Class, NewPolicy};
use crate::policy::arrays::FsContext;
use crate::policy::view::CustomKeyHashedView;
use crate::{NullessByteStr, PolicyCap};
use std::ops::Deref;
use std::sync::Arc;

use anyhow::Context as _;
use itertools::Itertools;
use std::collections::HashSet;
use std::fmt::Debug;
use std::hash::Hash;
use std::iter::Iterator;
use zerocopy::little_endian as le;

// As of 2026-01-30, more than five times larger than any policy seen in production or tests.
const MAXIMUM_POLICY_SIZE: usize = 1 << 24;

/// Parsed binary policy.
#[derive(Debug)]
pub struct ParsedPolicy {
    /// Raw policy data (remaining).
    data: PolicyData,

    /// [`NewPolicy`] that handles the header and base tables.
    new_policy: Arc<NewPolicy>,

    /// The set of roles referenced by this policy.
    roles: SymbolList<Role>,
    /// The set of types referenced by this policy.
    types: TypeIndex,
    /// The set of users referenced by this policy.
    users: SymbolList<User>,
    /// The set of dynamically adjustable booleans referenced by this policy.
    conditional_booleans: SymbolList<ConditionalBoolean>,
    /// The set of sensitivity levels referenced by this policy.
    sensitivities: SymbolList<Sensitivity>,
    /// The set of categories referenced by this policy.
    categories: CategoryIndex,
    /// The set of access vector rules referenced by this policy.
    access_vector_rules: HashedArrayView<AccessVectorRule>,
    conditional_lists: SimpleArray<ConditionalNode>,
    /// The set of role transitions to apply when instantiating new objects.
    role_transitions: RoleTransitions,
    /// The set of role transitions allowed by policy.
    role_allowlist: RoleAllows,
    filename_transition_list: FilenameTransitionList,
    initial_sids: SimpleArray<InitialSid>,
    filesystems: SimpleArray<NamedContextPair>,
    ports: SimpleArray<Port>,
    network_interfaces: SimpleArray<NamedContextPair>,
    nodes: SimpleArray<Node>,
    fs_uses: SimpleArray<FsUse>,
    ipv6_nodes: SimpleArray<IPv6Node>,
    infinitiband_partition_keys: Option<SimpleArray<InfinitiBandPartitionKey>>,
    infinitiband_end_ports: Option<SimpleArray<InfinitiBandEndPort>>,
    /// A set of labeling statements to apply to given filesystems and/or their subdirectories.
    /// Corresponds to the `genfscon` labeling statement in the policy.
    generic_fs_contexts: CustomKeyHashedView<GenericFsContext>,
    range_transitions: SimpleArray<RangeTransition>,
    /// Extensible bitmaps that encode associations between types and attributes.
    attribute_maps: Vec<ExtensibleBitmap>,
}

impl Deref for ParsedPolicy {
    type Target = NewPolicy;
    fn deref(&self) -> &Self::Target {
        &self.new_policy
    }
}

impl ParsedPolicy {
    /// Returns true if the specified capability is in the policy's enabled capabilities set.
    pub fn has_policycap(&self, policy_cap: PolicyCap) -> bool {
        self.new_policy.policy_capabilities().is_set(policy_cap as u32)
    }

    /// Computes the access granted to `source_type` on `target_type`, for the specified
    /// `target_class`. The result is a set of access vectors with bits set for each
    /// `target_class` permission, describing which permissions are allowed, and
    /// which should have access checks audit-logged when denied, or allowed.
    ///
    /// An [`AccessDecision`] is accumulated, starting from no permissions to be granted,
    /// nor audit-logged if allowed, and all permissions to be audit-logged if denied.
    /// Permissions that are explicitly `allow`ed, but that are subject to unsatisfied
    /// constraints, are removed from the allowed set. Matching policy statements then
    /// add permissions to the granted & audit-allow sets, or remove them from the
    /// audit-deny set.
    pub(super) fn compute_access_decision(
        &self,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        target_class: &Class,
    ) -> AccessDecision {
        let mut access_decision = self.compute_explicitly_allowed(
            source_context.type_(),
            target_context.type_(),
            target_class,
        );
        access_decision.allow -=
            self.compute_denied_by_constraints(source_context, target_context, target_class);
        access_decision
    }

    /// Computes the access granted to `source_type` on `target_type`, for the specified
    /// `target_class`. The result is a set of access vectors with bits set for each
    /// `target_class` permission, describing which permissions are explicitly allowed,
    /// and which should have access checks audit-logged when denied, or allowed.
    pub(super) fn compute_explicitly_allowed(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        target_class: &Class,
    ) -> AccessDecision {
        let target_class_id = target_class.id();

        let mut computed_access_vector = AccessVector::NONE;
        let mut computed_audit_allow = AccessVector::NONE;
        let mut computed_audit_deny = AccessVector::ALL;

        let source_attribute_bitmap: &ExtensibleBitmap =
            &self.attribute_maps[(source_type.as_u32() - 1) as usize];
        let target_attribute_bitmap: &ExtensibleBitmap =
            &self.attribute_maps[(target_type.as_u32() - 1) as usize];

        for (source_bit_index, target_bit_index) in Itertools::cartesian_product(
            source_attribute_bitmap.indices_of_set_bits(),
            target_attribute_bitmap.indices_of_set_bits(),
        ) {
            let source_id = TypeId::from_u32((source_bit_index + 1) as u32).unwrap();
            let target_id = TypeId::from_u32((target_bit_index + 1) as u32).unwrap();

            if let Some(allow_rule) = self.access_vector_rules_find(
                source_id,
                target_id,
                target_class_id,
                ACCESS_VECTOR_RULE_TYPE_ALLOW,
            ) {
                // `access_vector` has bits set for each permission allowed by this rule.
                computed_access_vector |= allow_rule.access_vector().unwrap();
            }
            if let Some(auditallow_rule) = self.access_vector_rules_find(
                source_id,
                target_id,
                target_class_id,
                ACCESS_VECTOR_RULE_TYPE_AUDITALLOW,
            ) {
                // `access_vector` has bits set for each permission to audit when allowed.
                computed_audit_allow |= auditallow_rule.access_vector().unwrap();
            }
            if let Some(dontaudit_rule) = self.access_vector_rules_find(
                source_id,
                target_id,
                target_class_id,
                ACCESS_VECTOR_RULE_TYPE_DONTAUDIT,
            ) {
                // `access_vector` has bits cleared for each permission not to audit on denial.
                computed_audit_deny &= dontaudit_rule.access_vector().unwrap();
            }
        }

        // If the `source_type` is bounded by some `parent_type` then bound the allowed permissions
        // to those available to the parent. Doing the calculation here ensures that type-bounds
        // take into account bounding ancestors, if any.
        if let Some(parent) = self.type_(source_type).bounded_by() {
            // If `source_type`==`target_type` then this is a "self" permission check, which should
            // be bounded to the parent domain's "self" permissions.
            let access = if source_type == target_type {
                self.compute_explicitly_allowed(parent, parent, target_class)
            } else {
                self.compute_explicitly_allowed(parent, target_type, target_class)
            };
            computed_access_vector &= access.allow;
        }

        let mut flags = 0;
        if self.permissive_map().contains(source_type) {
            flags |= SELINUX_AVD_FLAGS_PERMISSIVE;
        }
        AccessDecision {
            allow: computed_access_vector,
            auditallow: computed_audit_allow,
            auditdeny: computed_audit_deny,
            flags,
            todo_bug: None,
        }
    }

    /// A permission is denied if it matches at least one unsatisfied constraint.
    fn compute_denied_by_constraints(
        &self,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        target_class: &Class,
    ) -> AccessVector {
        let mut denied = AccessVector::NONE;
        for constraint in target_class.constraints() {
            match crate::policy::constraints::evaluate_constraint(
                constraint.constraint_expr(),
                source_context,
                target_context,
            ) {
                Err(err) => {
                    unreachable!("validated constraint expression failed to evaluate: {:?}", err)
                }
                Ok(false) => denied |= constraint.access_vector(),
                Ok(true) => {}
            }
        }
        denied
    }

    /// Computes the access decision for set of extended permissions of a given kind and with a
    /// given prefix byte, for a particular source and target context and target class.
    pub(super) fn compute_xperms_access_decision(
        &self,
        xperms_kind: XpermsKind,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        target_class: &Class,
        xperms_prefix: u8,
    ) -> XpermsAccessDecision {
        let target_class_id = target_class.id();

        let mut explicit_allow: Option<XpermsBitmap> = None;
        let mut auditallow = XpermsBitmap::NONE;
        let mut auditdeny = XpermsBitmap::ALL;

        let xperms_types = match xperms_kind {
            XpermsKind::Ioctl => {
                [XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES, XPERMS_TYPE_IOCTL_PREFIXES].as_slice()
            }
            XpermsKind::Nlmsg => [XPERMS_TYPE_NLMSG].as_slice(),
        };
        let bitmap_if_prefix_matches =
            |xperms_prefix: u8, xperms: &ExtendedPermissions| match xperms_kind {
                XpermsKind::Ioctl => match xperms.xperms_type {
                    XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES => (xperms.xperms_optional_prefix
                        == xperms_prefix)
                        .then_some(xperms.xperms_bitmap),
                    XPERMS_TYPE_IOCTL_PREFIXES => {
                        xperms.xperms_bitmap.contains(xperms_prefix).then_some(XpermsBitmap::ALL)
                    }
                    _ => None,
                },
                XpermsKind::Nlmsg => match xperms.xperms_type {
                    XPERMS_TYPE_NLMSG => (xperms.xperms_optional_prefix == xperms_prefix)
                        .then_some(xperms.xperms_bitmap),
                    _ => None,
                },
            };

        let source_attribute_bitmap: &ExtensibleBitmap =
            &self.attribute_maps[(source_context.type_().as_u32() - 1) as usize];
        let target_attribute_bitmap: &ExtensibleBitmap =
            &self.attribute_maps[(target_context.type_().as_u32() - 1) as usize];

        for (source_bit_index, target_bit_index) in Itertools::cartesian_product(
            source_attribute_bitmap.indices_of_set_bits(),
            target_attribute_bitmap.indices_of_set_bits(),
        ) {
            let source_id = TypeId::from_u32((source_bit_index + 1) as u32).unwrap();
            let target_id = TypeId::from_u32((target_bit_index + 1) as u32).unwrap();

            for xperms_allow_rule in self.access_vector_rules_find_all(
                source_id,
                target_id,
                target_class_id,
                ACCESS_VECTOR_RULE_TYPE_ALLOWXPERM,
            ) {
                let xperms = xperms_allow_rule.extended_permissions().unwrap();

                // Only filter xperms if there is at least one `allowxperm` rule for the relevant
                // kind of extended permission. If this condition is not satisfied by any
                // access vector rule, then all xperms of the relevant type are allowed.
                if xperms_types.contains(&xperms.xperms_type) {
                    explicit_allow.get_or_insert(XpermsBitmap::NONE);
                }

                if let Some(ref xperms_bitmap) = bitmap_if_prefix_matches(xperms_prefix, xperms) {
                    (*explicit_allow.get_or_insert(XpermsBitmap::NONE)) |= xperms_bitmap;
                }
            }

            for xperms_auditallow_rule in self.access_vector_rules_find_all(
                source_id,
                target_id,
                target_class_id,
                ACCESS_VECTOR_RULE_TYPE_AUDITALLOWXPERM,
            ) {
                let xperms = xperms_auditallow_rule.extended_permissions().unwrap();
                if let Some(ref xperms_bitmap) = bitmap_if_prefix_matches(xperms_prefix, xperms) {
                    auditallow |= xperms_bitmap;
                }
            }

            for xperms_dontaudit_rule in self.access_vector_rules_find_all(
                source_id,
                target_id,
                target_class_id,
                ACCESS_VECTOR_RULE_TYPE_DONTAUDITXPERM,
            ) {
                let xperms = xperms_dontaudit_rule.extended_permissions().unwrap();
                if let Some(ref xperms_bitmap) = bitmap_if_prefix_matches(xperms_prefix, xperms) {
                    auditdeny -= xperms_bitmap;
                }
            }
        }
        let allow = explicit_allow.unwrap_or(XpermsBitmap::ALL);
        XpermsAccessDecision { allow, auditallow, auditdeny }
    }

    /// Returns the policy entry for the specified initial Security Context.
    pub(super) fn initial_context(&self, mut id: crate::InitialSid) -> &Context {
        // If "userspace_initial_context" is not set then the "init" SID is treated as "kernel".
        if id == crate::InitialSid::Init && !self.has_policycap(PolicyCap::UserspaceInitialContext)
        {
            id = crate::InitialSid::Kernel
        }

        // [`InitialSids`] validates that all `InitialSid` values are defined by the policy.
        let id = le::U32::from(id as u32);
        &self.initial_sids.data.iter().find(|initial| initial.id() == id).unwrap().context()
    }

    /// Returns the `User` structure for the requested Id. Valid policies include definitions
    /// for all the Ids they refer to internally; supply some other Id will trigger a panic.
    pub(super) fn user(&self, id: UserId) -> &User {
        self.users.data.iter().find(|x| x.id() == id).unwrap()
    }

    /// Returns the named user, if present in the policy.
    pub(super) fn user_by_name(&self, name: &str) -> Option<&User> {
        self.users.data.iter().find(|x| x.name_bytes() == name.as_bytes())
    }

    /// Returns the `Role` structure for the requested Id. Valid policies include definitions
    /// for all the Ids they refer to internally; supply some other Id will trigger a panic.
    pub(super) fn role(&self, id: RoleId) -> &Role {
        self.roles.data.iter().find(|x| x.id() == id).unwrap()
    }

    /// Returns the named role, if present in the policy.
    pub(super) fn role_by_name(&self, name: &str) -> Option<&Role> {
        self.roles.data.iter().find(|x| x.name_bytes() == name.as_bytes())
    }

    /// Returns the `Type` structure for the requested Id. Valid policies include definitions
    /// for all the Ids they refer to internally; supply some other Id will trigger a panic.
    pub(super) fn type_(&self, id: TypeId) -> Type {
        self.types.type_by_type_id(id, &self.data)
    }

    /// Returns the [`TypeId`] of the [`Type`] with the given name, if present in the policy.
    pub(super) fn type_id_by_name(&self, name: &str) -> Option<TypeId> {
        self.types.type_id_by_name(name, &self.data)
    }

    /// Returns the `Sensitivity` structure for the requested Id. Valid policies include definitions
    /// for all the Ids they refer to internally; supply some other Id will trigger a panic.
    pub(super) fn sensitivity(&self, id: SensitivityId) -> &Sensitivity {
        self.sensitivities.data.iter().find(|x| x.id() == id).unwrap()
    }

    /// Returns the named sensitivity level, if present in the policy.
    pub(super) fn sensitivity_by_name(&self, name: &str) -> Option<&Sensitivity> {
        self.sensitivities.data.iter().find(|x| x.name_bytes() == name.as_bytes())
    }

    /// Returns the `Category` structure for the requested Id. Valid policies include definitions
    /// for all the Ids they refer to internally; supplying some other Id will trigger a panic.
    pub(super) fn category(&self, id: CategoryId) -> Category {
        self.categories.category(&self.data, id)
    }

    /// Returns the named category, if present in the policy.
    pub(super) fn category_by_name(&self, name: &str) -> Option<Category> {
        self.categories.category_by_name(&self.data, name)
    }

    pub(super) fn class(&self, class_id: ClassId) -> Option<&Class> {
        self.classes().get_by_id(class_id)
    }

    pub(super) fn conditional_booleans(&self) -> &Vec<ConditionalBoolean> {
        &self.conditional_booleans.data
    }

    pub(super) fn fs_uses(&self) -> &[FsUse] {
        &self.fs_uses.data
    }

    pub(super) fn genfscon_find_all(&self, fs_type: &str) -> impl Iterator<Item = FsContext> {
        let query = GenericFsContext::for_query(fs_type);
        self.generic_fs_contexts.find_all(query, &self.data)
    }

    pub(super) fn role_allowlist(&self) -> &[RoleAllow] {
        &self.role_allowlist.data
    }

    pub(super) fn role_transitions(&self) -> &[RoleTransition] {
        &self.role_transitions.data
    }

    pub(super) fn range_transitions(&self) -> &[RangeTransition] {
        &self.range_transitions.data
    }

    pub(super) fn access_vector_rules_find(
        &self,
        source: TypeId,
        target: TypeId,
        class: ClassId,
        rule_type: u16,
    ) -> Option<AccessVectorRule> {
        let query = AccessVectorRuleMetadata::for_query(source, target, class, rule_type);
        self.access_vector_rules.find(query, &self.data)
    }

    pub(super) fn access_vector_rules_find_all(
        &self,
        source: TypeId,
        target: TypeId,
        class: ClassId,
        rule_type: u16,
    ) -> impl Iterator<Item = AccessVectorRule> {
        let query = AccessVectorRuleMetadata::for_query(source, target, class, rule_type);
        self.access_vector_rules.find_all(query, &self.data)
    }

    #[cfg(test)]
    pub(super) fn access_vector_rules_for_test(
        &self,
    ) -> impl Iterator<Item = AccessVectorRule> + use<'_> {
        use super::arrays::testing::access_vector_rule_ordering;
        use itertools::Itertools;

        self.access_vector_rules
            .iter(&self.data)
            .map(|view| view.parse(&self.data))
            .sorted_by(access_vector_rule_ordering)
    }

    pub(super) fn compute_filename_transition(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        class: ClassId,
        name: NullessByteStr<'_>,
    ) -> Option<TypeId> {
        match &self.filename_transition_list {
            FilenameTransitionList::PolicyVersionGeq33(list) => {
                let entry = list.data.iter().find(|transition| {
                    transition.target_type() == target_type
                        && transition.target_class() == class
                        && transition.name_bytes() == name.as_bytes()
                })?;
                entry
                    .outputs()
                    .iter()
                    .find(|entry| entry.has_source_type(source_type))
                    .map(|x| x.out_type())
            }
            FilenameTransitionList::PolicyVersionLeq32(list) => list
                .data
                .iter()
                .find(|transition| {
                    transition.target_class() == class
                        && transition.target_type() == target_type
                        && transition.source_type() == source_type
                        && transition.name_bytes() == name.as_bytes()
                })
                .map(|x| x.out_type()),
        }
    }

    // Validate an MLS range statement against sets of defined sensitivity and category
    // IDs:
    // - Verify that all sensitivity and category IDs referenced in the MLS levels are
    //   defined.
    // - Verify that the range is internally consistent; i.e., the high level (if any)
    //   dominates the low level.
    fn validate_mls_range(
        &self,
        low_level: &MlsLevel,
        high_level: &Option<MlsLevel>,
        sensitivity_ids: &HashSet<SensitivityId>,
        category_ids: &HashSet<CategoryId>,
    ) -> Result<(), anyhow::Error> {
        validate_id(sensitivity_ids, low_level.sensitivity(), "sensitivity")?;
        for id in low_level.category_ids() {
            validate_id(category_ids, id, "category")?;
        }
        if let Some(high) = high_level {
            validate_id(sensitivity_ids, high.sensitivity(), "sensitivity")?;
            for id in high.category_ids() {
                validate_id(category_ids, id, "category")?;
            }
            if !high.dominates(low_level) {
                return Err(ValidateError::InvalidMlsRange {
                    low: low_level.to_string(self).into(),
                    high: high.to_string(self).into(),
                }
                .into());
            }
        }
        Ok(())
    }

    fn validate_context(
        &self,
        context: &Context,
        user_ids: &HashSet<UserId>,
        role_ids: &HashSet<RoleId>,
        type_ids: &HashSet<TypeId>,
        sensitivity_ids: &HashSet<SensitivityId>,
        category_ids: &HashSet<CategoryId>,
    ) -> Result<(), anyhow::Error> {
        validate_id(user_ids, context.user_id(), "user")?;
        validate_id(role_ids, context.role_id(), "role")?;
        validate_id(type_ids, context.type_id(), "type")?;
        self.validate_mls_range(
            context.low_level(),
            context.high_level(),
            sensitivity_ids,
            category_ids,
        )?;
        Ok(())
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

        let rest_data = new_policy.rest_bytes();
        let (policy, excess_bytes) = parse_policy_remaining(new_policy, rest_data)?;
        if excess_bytes > 0 {
            return Err(anyhow::Error::from(ParseError::TrailingBytes { num_bytes: excess_bytes }));
        }
        Ok(policy)
    }
}

/// Parses the remaining parts of the policy from `rest_data` to construct a [`ParsedPolicy`].
fn parse_policy_remaining(
    new_policy: NewPolicy,
    rest_data: PolicyData,
) -> Result<(ParsedPolicy, usize), anyhow::Error> {
    let tail = PolicyCursor::new(&rest_data);

    let (roles, tail) = SymbolList::<Role>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing roles")?;

    let (types, tail) =
        TypeIndex::parse(tail).map_err(anyhow::Error::from).context("parsing types")?;

    let (users, tail) = SymbolList::<User>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing users")?;

    let (conditional_booleans, tail) = SymbolList::<ConditionalBoolean>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing conditional booleans")?;

    let (sensitivities, tail) = SymbolList::<Sensitivity>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing sensitivites")?;

    let (categories, tail) = CategoryIndex::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing categories")?;

    let (access_vector_rules, tail) = HashedArrayView::<AccessVectorRule>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing access vector rules")?;

    let (conditional_lists, tail) = SimpleArray::<ConditionalNode>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing conditional lists")?;

    let (role_transitions, tail) = RoleTransitions::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing role transitions")?;

    let (role_allowlist, tail) = RoleAllows::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing role allow rules")?;

    let (filename_transition_list, tail) = if new_policy.policy_version() >= 33 {
        let (filename_transition_list, tail) = SimpleArray::<FilenameTransition>::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .context("parsing standard filename transitions")?;
        (FilenameTransitionList::PolicyVersionGeq33(filename_transition_list), tail)
    } else {
        let (filename_transition_list, tail) =
            SimpleArray::<DeprecatedFilenameTransition>::parse(tail)
                .map_err(Into::<anyhow::Error>::into)
                .context("parsing deprecated filename transitions")?;
        (FilenameTransitionList::PolicyVersionLeq32(filename_transition_list), tail)
    };

    let (initial_sids, tail) = SimpleArray::<InitialSid>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing initial sids")?;

    let (filesystems, tail) = SimpleArray::<NamedContextPair>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing filesystem contexts")?;

    let (ports, tail) = SimpleArray::<Port>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing ports")?;

    let (network_interfaces, tail) = SimpleArray::<NamedContextPair>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing network interfaces")?;

    let (nodes, tail) = SimpleArray::<Node>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing nodes")?;

    let (fs_uses, tail) = SimpleArray::<FsUse>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing fs uses")?;

    let (ipv6_nodes, tail) = SimpleArray::<IPv6Node>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing ipv6 nodes")?;

    let (infinitiband_partition_keys, infinitiband_end_ports, tail) =
        if new_policy.policy_version() >= MIN_POLICY_VERSION_FOR_INFINITIBAND_PARTITION_KEY {
            let (infinity_band_partition_keys, tail) =
                SimpleArray::<InfinitiBandPartitionKey>::parse(tail)
                    .map_err(Into::<anyhow::Error>::into)
                    .context("parsing infiniti band partition keys")?;
            let (infinitiband_end_ports, tail) = SimpleArray::<InfinitiBandEndPort>::parse(tail)
                .map_err(Into::<anyhow::Error>::into)
                .context("parsing infiniti band end ports")?;
            (Some(infinity_band_partition_keys), Some(infinitiband_end_ports), tail)
        } else {
            (None, None, tail)
        };

    let (generic_fs_contexts, tail) = CustomKeyHashedView::<GenericFsContext>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing generic filesystem contexts")?;

    let (range_transitions, tail) = SimpleArray::<RangeTransition>::parse(tail)
        .map_err(Into::<anyhow::Error>::into)
        .context("parsing range transitions")?;

    let primary_names_count = types.primary_names_count();
    let mut attribute_maps = Vec::with_capacity(primary_names_count as usize);
    let mut tail = tail;

    for i in 0..primary_names_count {
        let (item, next_tail) = ExtensibleBitmap::parse(tail)
            .map_err(Into::<anyhow::Error>::into)
            .with_context(|| format!("parsing {}th attribute map", i))?;
        attribute_maps.push(item);
        tail = next_tail;
    }
    let tail = tail;
    let attribute_maps = attribute_maps;

    let excess_bytes = rest_data.len() - tail.offset() as usize;

    Ok((
        ParsedPolicy {
            data: rest_data,
            new_policy: Arc::new(new_policy),
            roles,
            types,
            users,
            conditional_booleans,
            sensitivities,
            categories,
            access_vector_rules,
            conditional_lists,
            role_transitions,
            role_allowlist,
            filename_transition_list,
            initial_sids,
            filesystems,
            ports,
            network_interfaces,
            nodes,
            fs_uses,
            ipv6_nodes,
            infinitiband_partition_keys,
            infinitiband_end_ports,
            generic_fs_contexts,
            range_transitions,
            attribute_maps,
        },
        excess_bytes,
    ))
}

impl ParsedPolicy {
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        let need_init_sid = self.has_policycap(PolicyCap::UserspaceInitialContext);
        let context = PolicyValidationContext {
            data: self.data.clone(),
            need_init_sid,
            new_policy: self.new_policy.clone(),
        };

        self.roles
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating roles")?;
        self.types
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating types")?;
        self.users
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating users")?;
        self.conditional_booleans
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating conditional_booleans")?;
        self.sensitivities
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating sensitivities")?;
        self.categories
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating categories")?;
        self.access_vector_rules
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating access_vector_rules")?;
        self.conditional_lists
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating conditional_lists")?;
        self.role_transitions
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating role_transitions")?;
        self.role_allowlist
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating role_allowlist")?;
        self.filename_transition_list
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating filename_transition_list")?;
        self.initial_sids
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating initial_sids")?;
        self.filesystems
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating filesystems")?;
        self.ports
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating ports")?;
        self.network_interfaces
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating network_interfaces")?;
        self.nodes
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating nodes")?;
        self.fs_uses
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating fs_uses")?;
        self.ipv6_nodes
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating ipv6 nodes")?;
        self.infinitiband_partition_keys
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating infinitiband_partition_keys")?;
        self.infinitiband_end_ports
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating infinitiband_end_ports")?;
        self.generic_fs_contexts
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating generic_fs_contexts")?;
        self.range_transitions
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating range_transitions")?;
        self.attribute_maps
            .validate(&context)
            .map_err(Into::<anyhow::Error>::into)
            .context("validating attribute_maps")?;

        // Collate the sets of user, role, type, sensitivity and category Ids.
        let user_ids: HashSet<UserId> = self.users.data.iter().map(|x| x.id()).collect();
        let role_ids: HashSet<RoleId> = self.roles.data.iter().map(|x| x.id()).collect();
        let class_ids: HashSet<ClassId> = self.classes().iter().map(|x| x.id()).collect();
        let type_ids: HashSet<TypeId> = self.types.all_type_ids().collect();
        let sensitivity_ids: HashSet<SensitivityId> =
            self.sensitivities.data.iter().map(|x| x.id()).collect();
        let category_ids: HashSet<CategoryId> =
            self.categories.categories(&self.data).map(|x| x.id()).collect();

        // Validate that users use only defined sensitivities and categories, and that
        // each user's MLS levels are internally consistent (i.e., the high level
        // dominates the low level).
        for user in &self.users.data {
            self.validate_mls_range(
                user.mls_range().low(),
                user.mls_range().high(),
                &sensitivity_ids,
                &category_ids,
            )?;
        }

        // Validate that initial contexts use only defined user, role, type, etc Ids.
        // Check that all sensitivity and category IDs are defined and that MLS levels
        // are internally consistent.
        for initial_sid in &self.initial_sids.data {
            self.validate_context(
                initial_sid.context(),
                &user_ids,
                &role_ids,
                &type_ids,
                &sensitivity_ids,
                &category_ids,
            )?;
        }

        // Validate that contexts specified in filesystem labeling rules only use
        // policy-defined Ids for their fields. Check that MLS levels are internally
        // consistent.
        for fs_use in &self.fs_uses.data {
            self.validate_context(
                fs_use.context(),
                &user_ids,
                &role_ids,
                &type_ids,
                &sensitivity_ids,
                &category_ids,
            )?;
        }

        // Validate that contexts specified in genfscon rules only use
        // policy-defined Ids for their fields. Check that MLS levels are internally
        // consistent.
        for entry in self.generic_fs_contexts.iter(&self.data) {
            let entry = entry?;
            for fs_context_view in entry.values().data().iter(&self.data) {
                let fs_context = fs_context_view.parse(&self.data);
                self.validate_context(
                    fs_context.context(),
                    &user_ids,
                    &role_ids,
                    &type_ids,
                    &sensitivity_ids,
                    &category_ids,
                )?;
            }
        }

        // Validate that roles output by role- transitions & allows are defined.
        for transition in &self.role_transitions.data {
            validate_id(&role_ids, transition.current_role(), "current_role")?;
            validate_id(&type_ids, transition.type_(), "type")?;
            validate_id(&class_ids, transition.class(), "class")?;
            validate_id(&role_ids, transition.new_role(), "new_role")?;
        }
        for allow in &self.role_allowlist.data {
            validate_id(&role_ids, allow.source_role(), "source_role")?;
            validate_id(&role_ids, allow.new_role(), "new_role")?;
        }

        // Validate that types output by access vector rules are defined.
        for access_vector_rule_view in self.access_vector_rules.iter(&self.data) {
            let access_vector_rule = access_vector_rule_view.parse(&self.data);
            if let Some(type_id) = access_vector_rule.new_type() {
                validate_id(&type_ids, type_id, "new_type")?;
            }
        }

        // Validate that constraints are well-formed by evaluating against
        // a source and target security context.
        let initial_context = SecurityContext::new_from_policy_context(
            self.initial_context(crate::InitialSid::Kernel),
        );
        for class in self.classes().iter() {
            for constraint in class.constraints() {
                crate::policy::constraints::evaluate_constraint(
                    constraint.constraint_expr(),
                    &initial_context,
                    &initial_context,
                )
                .map_err(Into::<anyhow::Error>::into)
                .context("validating constraints")?;
            }
        }

        // To-do comments for cross-policy validations yet to be implemented go here.
        // TODO(b/356569876): Determine which "bounds" should be verified for correctness here.

        Ok(())
    }
}

fn validate_id<IdType: Debug + Eq + Hash>(
    id_set: &HashSet<IdType>,
    id: IdType,
    debug_kind: &'static str,
) -> Result<(), anyhow::Error> {
    if !id_set.contains(&id) {
        return Err(ValidateError::UnknownId { kind: debug_kind, id: format!("{:?}", id) }.into());
    }
    Ok(())
}
