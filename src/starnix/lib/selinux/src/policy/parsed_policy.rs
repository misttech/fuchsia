// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::arrays::{
    Context, FsUse, GenericFsContext, IPv6Node, InfinitiBandEndPort, InfinitiBandPartitionKey,
    MIN_POLICY_VERSION_FOR_INFINITIBAND_PARTITION_KEY, NamedContextPair, Node, Port,
    RangeTransition, SimpleArray,
};
use super::error::{ParseError, ValidateError};
use crate::new_policy::TypeSet;
use crate::new_policy::bitmap::IdSet;

use super::constraints::evaluate_constraint;
use super::parser::{PolicyCursor, PolicyData};
use super::security_context::SecurityContext;
use super::view::Hashable;
use super::{
    AccessDecision, AccessVector, CategoryId, ClassId, MlsLevel, Parse, PolicyValidationContext,
    RoleId, SELINUX_AVD_FLAGS_PERMISSIVE, SensitivityId, TypeId, UserId, Validate,
    XpermsAccessDecision, XpermsKind,
};

use crate::PolicyCap;
use crate::new_policy::rules::{
    ExtendedPermissions, HasRuleKey, RuleKind, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES,
    XPERMS_TYPE_IOCTL_PREFIXES, XPERMS_TYPE_NLMSG, XpermsBitmap,
};
use crate::new_policy::traits::{HasPolicyId, PolicyId};
use crate::new_policy::{Class, NewPolicy};
use crate::policy::arrays::FsContext;
use crate::policy::view::CustomKeyHashedView;
use std::ops::Deref;
use std::sync::Arc;

use anyhow::Context as _;
use std::collections::HashSet;
use std::fmt::Debug;
use std::hash::Hash;
use std::iter::Iterator;

// As of 2026-01-30, more than five times larger than any policy seen in production or tests.
const MAXIMUM_POLICY_SIZE: usize = 1 << 24;

/// Parsed binary policy.
#[derive(Debug)]
pub struct ParsedPolicy {
    /// Raw policy data (remaining).
    data: PolicyData,

    /// [`NewPolicy`] that handles the header and base tables.
    new_policy: Arc<NewPolicy>,

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
    attribute_maps: Vec<TypeSet>,
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
        self.new_policy.policy_capabilities().contains(policy_cap)
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

        let source_attribute_set: &TypeSet =
            &self.attribute_maps[(source_type.as_u32() - 1) as usize];
        let target_attribute_set: &TypeSet =
            &self.attribute_maps[(target_type.as_u32() - 1) as usize];

        for source_id in source_attribute_set.iter() {
            for target_id in target_attribute_set.iter() {
                for rule in self.new_policy.access_vector_rules().find_av_rules(
                    source_id,
                    target_id,
                    target_class_id,
                ) {
                    match rule.kind() {
                        RuleKind::Allow => computed_access_vector |= rule.access_vector(),
                        RuleKind::AuditAllow => computed_audit_allow |= rule.access_vector(),
                        RuleKind::DontAudit => computed_audit_deny &= rule.access_vector(),
                        _ => {}
                    }
                }
            }
        }

        // If the `source_type` is bounded by some `parent_type` then bound the allowed permissions
        // to those available to the parent. Doing the calculation here ensures that type-bounds
        // take into account bounding ancestors, if any.
        if let Some(parent) = self.types().get_by_id(source_type).unwrap().bounded_by() {
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
            if !evaluate_constraint(constraint.constraint_expr(), source_context, target_context) {
                denied |= constraint.access_vector();
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
                XpermsKind::Ioctl => match xperms.xperms_type() {
                    XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES => (xperms.xperms_optional_prefix()
                        == xperms_prefix)
                        .then_some(*xperms.xperms_bitmap()),
                    XPERMS_TYPE_IOCTL_PREFIXES => {
                        xperms.xperms_bitmap().contains(xperms_prefix).then_some(XpermsBitmap::ALL)
                    }
                    _ => None,
                },
                XpermsKind::Nlmsg => match xperms.xperms_type() {
                    XPERMS_TYPE_NLMSG => (xperms.xperms_optional_prefix() == xperms_prefix)
                        .then_some(*xperms.xperms_bitmap()),
                    _ => None,
                },
            };

        let source_attribute_set: &TypeSet =
            &self.attribute_maps[(source_context.type_().as_u32() - 1) as usize];
        let target_attribute_set: &TypeSet =
            &self.attribute_maps[(target_context.type_().as_u32() - 1) as usize];

        for source_id in source_attribute_set.iter() {
            for target_id in target_attribute_set.iter() {
                for rule in self.new_policy.access_vector_rules().find_xperm_rules(
                    source_id,
                    target_id,
                    target_class_id,
                ) {
                    let xperms = rule.extended_permissions();
                    if rule.kind() == RuleKind::AllowXperm
                        && xperms_types.contains(&xperms.xperms_type())
                    {
                        explicit_allow.get_or_insert(XpermsBitmap::NONE);
                    }

                    if let Some(xperms_bitmap) = bitmap_if_prefix_matches(xperms_prefix, xperms) {
                        match rule.kind() {
                            RuleKind::AllowXperm => {
                                (*explicit_allow.get_or_insert(XpermsBitmap::NONE)) |=
                                    xperms_bitmap;
                            }
                            RuleKind::AuditAllowXperm => auditallow |= xperms_bitmap,
                            RuleKind::DontAuditXperm => auditdeny -= xperms_bitmap,
                            _ => {}
                        }
                    }
                }
            }
        }
        let allow = explicit_allow.unwrap_or(XpermsBitmap::ALL);
        XpermsAccessDecision { allow, auditallow, auditdeny }
    }

    pub(super) fn fs_uses(&self) -> &[FsUse] {
        &self.fs_uses.data
    }

    pub(super) fn genfscon_find_all(&self, fs_type: &str) -> impl Iterator<Item = FsContext> {
        let query = GenericFsContext::for_query(fs_type);
        self.generic_fs_contexts.find_all(query, &self.data)
    }

    pub(super) fn range_transitions(&self) -> &[RangeTransition] {
        &self.range_transitions.data
    }

    pub(super) fn compute_filename_transition(
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

    pub(super) fn initial_context(&self, mut id: crate::InitialSid) -> &crate::new_policy::Context {
        let need_init_sid = self.has_policycap(PolicyCap::UserspaceInitialContext);
        if id == crate::InitialSid::Init && !need_init_sid {
            id = crate::InitialSid::Kernel;
        }
        self.new_policy
            .initial_sids()
            .get_by_id(id as u32)
            .expect("initial SID must be present in validated policy")
    }

    // Validate that all sensitivity and category IDs referenced in the MLS level are
    // defined.
    fn validate_mls_level(
        &self,
        level: &MlsLevel,
        sensitivity_ids: &HashSet<SensitivityId>,
        category_ids: &HashSet<CategoryId>,
    ) -> Result<(), anyhow::Error> {
        validate_id(sensitivity_ids, level.sensitivity(), "sensitivity")?;
        for id in level.category_ids() {
            validate_id(category_ids, id, "category")?;
        }
        Ok(())
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
        self.validate_mls_level(low_level, sensitivity_ids, category_ids)?;
        if let Some(high) = high_level {
            self.validate_mls_level(high, sensitivity_ids, category_ids)?;
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
        if new_policy.version().get() >= MIN_POLICY_VERSION_FOR_INFINITIBAND_PARTITION_KEY {
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

    let primary_names_count = new_policy.types().primary_names_count();
    let mut attribute_maps = Vec::with_capacity(primary_names_count as usize);
    let mut tail = tail;

    for i in 0..primary_names_count {
        let (item, next_tail) = TypeSet::parse(tail)
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
        let context = PolicyValidationContext {
            data: self.data.clone(),
            new_policy: self.new_policy.clone(),
        };

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
        let user_ids: HashSet<UserId> = self.new_policy.users().iter().map(|x| x.id()).collect();
        let role_ids: HashSet<RoleId> = self.roles().iter().map(|x| x.id()).collect();
        let type_ids: HashSet<TypeId> = self.new_policy.types().iter().map(|t| t.id()).collect();
        let sensitivity_ids: HashSet<SensitivityId> =
            self.new_policy.sensitivities().iter().map(|x| x.id()).collect();
        let category_ids: HashSet<CategoryId> =
            self.new_policy.categories().iter().map(|x| x.id()).collect();

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

        // Validate that all kernel-required initial SIDs are present in the policy.
        let need_init_sid = self.has_policycap(PolicyCap::UserspaceInitialContext);
        for initial_sid in crate::InitialSid::all_variants() {
            if *initial_sid == crate::InitialSid::Init && !need_init_sid {
                continue;
            }
            self.new_policy
                .initial_sids()
                .get_by_id(*initial_sid as u32)
                .ok_or(ValidateError::MissingInitialSid { initial_sid: *initial_sid })?;
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

impl<T: PolicyId, const WITH_ID_ZERO: bool> Parse for IdSet<T, WITH_ID_ZERO> {
    type Error = anyhow::Error;

    fn parse<'a>(cursor: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let offset = cursor.offset() as usize;
        let slice = &cursor.data().as_ref()[offset..];
        let mut new_cursor = crate::new_policy::parser::PolicyCursor::new(slice);
        let id_set = <Self as crate::new_policy::traits::Parse>::parse(&mut new_cursor)
            .map_err(|e| anyhow::anyhow!("Parse error: {:?}", e))?;
        let bytes_parsed = new_cursor.offset();
        let new_offset = cursor.offset() + bytes_parsed as u32;
        Ok((id_set, PolicyCursor::new_at(cursor.data(), new_offset)))
    }
}

impl<T: PolicyId + crate::new_policy::traits::Validate, const WITH_ID_ZERO: bool> Validate
    for IdSet<T, WITH_ID_ZERO>
{
    type Error = anyhow::Error;

    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        crate::new_policy::traits::Validate::validate(self, &context.new_policy).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
