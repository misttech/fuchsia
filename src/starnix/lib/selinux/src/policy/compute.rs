// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::rules::{
    ExtendedPermissions, HasRuleKey, RuleKind, XPERMS_TYPE_IOCTL_PREFIX_AND_POSTFIXES,
    XPERMS_TYPE_IOCTL_PREFIXES, XPERMS_TYPE_NLMSG, XpermsBitmap,
};
use super::traits::{HasName, HasPolicyId};
use super::{
    AccessDecision, AccessVector, CategorySet, Class, ClassDefault, ClassDefaultRange, ClassId,
    FsUseType, GenfsConPath, MlsLevel, OBJECT_R_ROLE_NAME, PermissionId, Policy, PolicyCap, RoleId,
    SELINUX_AVD_FLAGS_PERMISSIVE, SecurityContext, SecurityContextError, TypeId,
};
use crate::{InitialSid, NullessByteStr};

/// Kind of extended permission, corresponding to the base permission that triggers a check of an
/// extended permission.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum XpermsKind {
    Ioctl,
    Nlmsg,
}

/// Result of an extended permissions calculation between source and target domains for a specific
/// class, kind of extended permissions, and xperm prefix byte.
///
/// Decisions describe which 16-bit xperms are allowed, and whether xperms should be audit-logged
/// when allowed and when denied.
#[derive(Debug, Clone, PartialEq)]
pub struct XpermsAccessDecision {
    pub allow: XpermsBitmap,
    pub auditallow: XpermsBitmap,
    pub auditdeny: XpermsBitmap,
}

impl XpermsAccessDecision {
    pub const DENY_ALL: Self = Self {
        allow: XpermsBitmap::NONE,
        auditallow: XpermsBitmap::NONE,
        auditdeny: XpermsBitmap::ALL,
    };
    pub const ALLOW_ALL: Self = Self {
        allow: XpermsBitmap::ALL,
        auditallow: XpermsBitmap::NONE,
        auditdeny: XpermsBitmap::ALL,
    };
}

/// [`SecurityContext`] and [`FsUseType`] derived from an `fs_use_*` line of the policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsUseLabelAndType {
    pub context: SecurityContext,
    pub use_type: FsUseType,
}

impl Policy {
    /// Returns true if the specified capability is in the policy's enabled capabilities set.
    pub fn has_policycap(&self, policy_cap: PolicyCap) -> bool {
        self.policy_capabilities().contains(policy_cap)
    }

    /// Returns true if `bounded_type` is bounded by `parent_type` in the policy.
    pub fn is_bounded_by(&self, bounded_type: TypeId, parent_type: TypeId) -> bool {
        self.types().get_by_id(bounded_type).unwrap().bounded_by() == Some(parent_type)
    }

    /// Returns true if the policy has marked the type/domain for permissive checks.
    pub fn is_permissive(&self, type_: TypeId) -> bool {
        self.permissive_map().contains(type_)
    }

    /// Returns the [`RoleId`] of the `"object_r"` role within the policy, for use when validating
    /// and computing Security Context fields.
    ///
    /// # Panics
    ///
    /// Panics if [`Self::validate`] has not verified the presence of `"object_r"`.
    pub(crate) fn object_role(&self) -> RoleId {
        self.roles()
            .get_by_name(OBJECT_R_ROLE_NAME)
            .expect("validated policy must contain object_r role")
            .id()
    }

    /// Returns the [`SecurityContext`] defined by this policy for the specified [`InitialSid`].
    ///
    /// # Panics
    ///
    /// Panics if [`Self::validate`] has not verified that all required initial SIDs are present.
    pub fn initial_context(&self, mut id: InitialSid) -> SecurityContext {
        let need_init_sid = self.has_policycap(PolicyCap::UserspaceInitialContext);
        if id == InitialSid::Init && !need_init_sid {
            id = InitialSid::Kernel;
        }
        let context = self
            .initial_sids()
            .get_by_id(id as u32)
            .expect("initial SID must be present in validated policy");
        SecurityContext::from_policy_context(context)
    }

    /// Returns a [`SecurityContext`] with fields parsed from the supplied Security Context string.
    pub fn parse_security_context(
        &self,
        security_context: NullessByteStr<'_>,
    ) -> Result<SecurityContext, SecurityContextError> {
        SecurityContext::from_string(self, security_context)
    }

    /// Validates a [`SecurityContext`] against this policy's constraints.
    pub fn validate_security_context(
        &self,
        security_context: &SecurityContext,
    ) -> Result<(), SecurityContextError> {
        security_context.validate(self)
    }

    /// Returns a byte string describing the supplied [`SecurityContext`].
    pub fn serialize_security_context(&self, security_context: &SecurityContext) -> Vec<u8> {
        security_context.to_string(self)
    }

    /// Returns the set of permissions for the given class, including both the explicitly owned
    /// permissions and the inherited ones from common symbols. Each permission is a tuple of the
    /// [`PermissionId`] (in the scope of the given class) and the permission name.
    pub fn find_class_permissions_by_name(
        &self,
        class_name: &str,
    ) -> Result<Vec<(PermissionId, Vec<u8>)>, ()> {
        let classes = self.classes();
        let class = classes.get_by_name(class_name.as_bytes()).ok_or(())?;
        let owned_permissions = class.permissions();

        let mut result: Vec<_> = owned_permissions
            .iter()
            .map(|permission| (permission.id(), permission.name().to_vec()))
            .collect();

        // common_name() is empty when the class doesn't inherit from a CommonSymbol.
        if class.common_name().is_empty() {
            return Ok(result);
        }

        let common_symbol_permissions =
            self.common_symbols().get_by_name(class.common_name()).ok_or(())?.permissions();

        result.extend(
            common_symbol_permissions
                .iter()
                .map(|permission| (permission.id(), permission.name().to_vec())),
        );

        Ok(result)
    }

    /// Returns the [`PermissionId`] of the specified permission for the specified security
    /// [`Class`], looking up the permission in the class's common symbol, if any.
    pub(crate) fn find_permission_id_by_name(
        &self,
        class: &Class,
        name: &str,
    ) -> Option<PermissionId> {
        let name = name.as_bytes();
        if let Some(permission) = class.permissions().get_by_name(name) {
            return Some(permission.id());
        }
        let common_name = class.common_name();
        if !common_name.is_empty() {
            let common_symbol = self.common_symbols().get_by_name(common_name)?;
            let permission = common_symbol.permissions().get_by_name(name)?;
            return Some(permission.id());
        }
        None
    }

    /// If there is an `fs_use` statement for the given filesystem type, returns the associated
    /// [`SecurityContext`] and [`FsUseType`].
    pub fn fs_use_label_and_type(&self, fs_type: NullessByteStr<'_>) -> Option<FsUseLabelAndType> {
        self.object_contexts()
            .fs_uses()
            .iter()
            .find(|fs_use| fs_use.fs_type() == fs_type.as_bytes())
            .map(|fs_use| FsUseLabelAndType {
                context: SecurityContext::from_policy_context(fs_use.context()),
                use_type: fs_use.behavior(),
            })
    }

    /// Returns an iterator over all `genfscon` [`GenfsConPath`] entries for the given filesystem
    /// type.
    fn genfscon_find_all<'a>(
        &'a self,
        fs_type: &'a [u8],
    ) -> impl Iterator<Item = &'a GenfsConPath> {
        self.generic_fs_contexts()
            .iter()
            .filter(move |entry| entry.fs_type() == fs_type)
            .flat_map(|entry| entry.paths().iter())
    }

    /// If there is a `genfscon` statement for the given filesystem type, returns the associated
    /// [`SecurityContext`], taking the `node_path` into account. `class_id` defines the type of
    /// the file in the given `node_path`. It can only be omitted when looking up the filesystem
    /// label.
    pub fn genfscon_label_for_fs_and_path(
        &self,
        fs_type: NullessByteStr<'_>,
        node_path: NullessByteStr<'_>,
        class_id: Option<ClassId>,
    ) -> Option<SecurityContext> {
        // All contexts listed in the policy for the file system type.
        let fs_contexts = self.genfscon_find_all(fs_type.as_bytes());

        #[derive(PartialEq)]
        enum OrderType {
            Alphabetic,
            ByLength,
            Unknown,
        }
        // The correct match is the closest parent among the ones given in the policy file.
        // E.g. if in the policy we have
        //     genfscon foofs "/" label1
        //     genfscon foofs "/abc/" label2
        //     genfscon foofs "/abc/def" label3
        //
        // The correct label for a file "/abc/def/g/h/i" is label3, as "/abc/def" is the closest parent
        // among those defined.
        //
        // Partial paths are prefix-matched, so that "/abc/default" would also be assigned label3.
        //
        // TODO(372212126): Optimize the algorithm.
        let mut result: Option<&GenfsConPath> = None;
        let mut order_type = OrderType::Unknown;
        let mut prev_path_bytes: Option<Vec<u8>> = None;
        for fs_context in fs_contexts {
            // Determine the order type based on the first entries.
            let path = fs_context.partial_path();
            if order_type == OrderType::Unknown {
                if let Some(prev) = &prev_path_bytes {
                    if path.len() > prev.len() {
                        order_type = OrderType::Alphabetic;
                    } else if path < prev.as_slice() {
                        order_type = OrderType::ByLength;
                    }
                }
                prev_path_bytes = Some(path.to_vec());
            }

            // Check if the class matches.
            let class_matches = class_id.is_none()
                || fs_context
                    .class()
                    .map(|other| other == class_id.unwrap().into())
                    .unwrap_or(true);
            if !class_matches {
                continue;
            }

            if order_type == OrderType::Alphabetic
                && fs_context.partial_path() > node_path.as_bytes()
            {
                // We know that:
                // - We have alphabetic order,
                // - The current path is lexicographically greater than our target path.
                // We can infer that we have passed any potential prefixes in alphabetical order.
                break;
            }

            if node_path.as_bytes().starts_with(fs_context.partial_path()) {
                if result
                    .as_ref()
                    .map_or(true, |c| c.partial_path().len() < fs_context.partial_path().len())
                {
                    // The path matches, and it's the closest parent so far.
                    result = Some(fs_context);
                    if order_type == OrderType::ByLength {
                        break;
                    }
                }
            }
        }

        // The returned SecurityContext must be valid with respect to the policy, since otherwise
        // we'd have rejected the policy load.
        Some(SecurityContext::from_policy_context(result?.context()))
    }

    /// Returns the security context that should be applied to a newly created SELinux
    /// object according to `source_context` and `target_context`, as well as the new object's
    /// `target_class` and `name`.
    ///
    /// Computation follows the "create" algorithm for labeling newly created objects:
    /// - user is taken from the `source_context`.
    /// - role, type and range are taken from the matching transition rules, if any.
    /// - role, type and range fall-back to the `source_context` or `target_context` values
    ///   according to policy.
    ///
    /// Callers pass an empty slice (`&[]`) for `name` to express nameless transitions.
    /// When a non-empty `name` is provided, filename transition rules are checked first.
    /// If no transitions apply, and the policy does not explicitly specify defaults then the
    /// role, type and range values have defaults chosen based on the `target_class`:
    /// - For "process", and socket-like classes, role, type and range are taken from the
    ///   `source_context`.
    /// - Otherwise role is "object_r", type is taken from `target_context` and range is set to the
    ///   low level of the `source_context` range.
    pub fn compute_create_context(
        &self,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        target_class: Option<&Class>,
        name: &[u8],
    ) -> SecurityContext {
        let override_type = if !name.is_empty() {
            target_class.and_then(|policy_class| {
                self.type_transition_new_type_with_name(
                    source_context.type_(),
                    target_context.type_(),
                    policy_class,
                    name,
                )
            })
        } else {
            None
        };
        self.new_security_context_internal(
            source_context,
            target_context,
            target_class,
            override_type,
        )
    }

    /// Internal implementation used by [`Self::compute_create_context`] to implement the policy transition calculations.
    /// If `override_type` is specified then the supplied value will be applied rather than a value
    /// being calculated based on the policy; this is used by [`Self::compute_create_context`]
    /// when a filename transition matches to shortcut the default `type_transition` lookup.
    fn new_security_context_internal(
        &self,
        source: &SecurityContext,
        target: &SecurityContext,
        target_class: Option<&Class>,
        override_type: Option<TypeId>,
    ) -> SecurityContext {
        let Some(policy_class) = target_class else {
            // If the class is not defined in the policy then there can be no transitions, nor
            // class-defined choice of defaults, so default to the non-process-or-socket behaviour.
            return SecurityContext::new(
                source.user(),
                self.object_role(),
                target.type_(),
                source.low_level().clone(),
                None,
            );
        };

        let is_process_or_socket =
            policy_class.name() == b"process" || policy_class.common_name() == b"socket";
        let (unspecified_role, unspecified_type, unspecified_low, unspecified_high) =
            if is_process_or_socket {
                (source.role(), source.type_(), source.low_level(), source.high_level())
            } else {
                (self.object_role(), target.type_(), source.low_level(), None)
            };
        let class_defaults = policy_class.defaults();

        let user = match class_defaults.user() {
            ClassDefault::Source => source.user(),
            ClassDefault::Target => target.user(),
            ClassDefault::Unspecified => source.user(),
        };

        let role = match self.role_transition_new_role(source.role(), target.type_(), policy_class)
        {
            Some(new_role) => new_role,
            None => match class_defaults.role() {
                ClassDefault::Source => source.role(),
                ClassDefault::Target => target.role(),
                ClassDefault::Unspecified => unspecified_role,
            },
        };

        let type_ = override_type.unwrap_or_else(|| {
            let transition = self
                .access_vector_rules()
                .find_type_rules(source.type_(), target.type_(), policy_class.id())
                .find(|rule| rule.kind() == RuleKind::TypeTransition)
                .map(|rule| rule.new_type());
            match transition {
                Some(new_type) => new_type,
                None => match class_defaults.type_() {
                    ClassDefault::Source => source.type_(),
                    ClassDefault::Target => target.type_(),
                    ClassDefault::Unspecified => unspecified_type,
                },
            }
        });

        let (low_level, high_level) =
            match self.range_transition_new_range(source.type_(), target.type_(), policy_class) {
                Some((low_level, high_level)) => (low_level, high_level),
                None => match class_defaults.range() {
                    ClassDefaultRange::SourceLow => (source.low_level().clone(), None),
                    ClassDefaultRange::SourceHigh => {
                        (source.high_level().unwrap_or_else(|| source.low_level()).clone(), None)
                    }
                    ClassDefaultRange::SourceLowHigh => {
                        (source.low_level().clone(), source.high_level().cloned())
                    }
                    ClassDefaultRange::TargetLow => (target.low_level().clone(), None),
                    ClassDefaultRange::TargetHigh => {
                        (target.high_level().unwrap_or_else(|| target.low_level()).clone(), None)
                    }
                    ClassDefaultRange::TargetLowHigh => {
                        (target.low_level().clone(), target.high_level().cloned())
                    }
                    ClassDefaultRange::Unspecified => {
                        (unspecified_low.clone(), unspecified_high.cloned())
                    }
                    ClassDefaultRange::Glblub => glblub_range(source, target),
                },
            };

        // TODO(http://b/334968228): Validate domain & role transitions are allowed?
        SecurityContext::new(user, role, type_, low_level, high_level)
    }

    /// Computes the access granted to `source_context` on `target_context`, for the specified
    /// `target_class`. The result is a set of access vectors with bits set for each
    /// `target_class` permission, describing which permissions are allowed, and
    /// which should have access checks audit-logged when denied, or allowed.
    ///
    /// An [`AccessDecision`] is accumulated, starting from no permissions to be granted,
    /// nor audit-logged if allowed, and all permissions to be audit-logged if denied.
    /// Permissions that are explicitly `allow`ed, but that are subject to unsatisfied
    /// constraints, are removed from the allowed set. For the `process` class, cross-role
    /// `transition` and `dyntransition` permissions are also removed unless the role
    /// transition is explicitly allowed by policy.
    pub fn compute_access_decision(
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

        // Process domain transitions ("transition" and "dyntransition") across different roles
        // require explicit authorization in policy via a role allow rule ("allow old_role new_role;").
        if access_decision.allow != AccessVector::NONE
            && source_context.role() != target_context.role()
            && target_class.name() == b"process"
            && !self
                .role_transition_is_explicitly_allowed(source_context.role(), target_context.role())
        {
            let mut transition_permissions = AccessVector::NONE;
            if let Some(permission_id) = self.find_permission_id_by_name(target_class, "transition")
            {
                transition_permissions |= AccessVector::from(permission_id);
            }
            if let Some(permission_id) =
                self.find_permission_id_by_name(target_class, "dyntransition")
            {
                transition_permissions |= AccessVector::from(permission_id);
            }
            access_decision.allow -= transition_permissions;
        }

        access_decision
    }

    /// Computes the access granted to `source_type` on `target_type`, for the specified
    /// `target_class`. The result is a set of access vectors with bits set for each
    /// `target_class` permission, describing which permissions are explicitly allowed,
    /// and which should have access checks audit-logged when denied, or allowed.
    pub(crate) fn compute_explicitly_allowed(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        target_class: &Class,
    ) -> AccessDecision {
        let target_class_id = target_class.id();

        let mut computed_access_vector = AccessVector::NONE;
        let mut computed_audit_allow = AccessVector::NONE;
        let mut computed_audit_deny = AccessVector::ALL;

        let source_attribute_set = &self.type_attribute_maps()[source_type];
        let target_attribute_set = &self.type_attribute_maps()[target_type];

        for source_id in source_attribute_set.iter() {
            for target_id in target_attribute_set.iter() {
                for rule in
                    self.access_vector_rules().find_av_rules(source_id, target_id, target_class_id)
                {
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

    /// Permissions are denied if they match at least one unsatisfied constraint.
    fn compute_denied_by_constraints(
        &self,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        target_class: &Class,
    ) -> AccessVector {
        let mut denied = AccessVector::NONE;
        for constraint in target_class.constraints() {
            if !constraint.evaluate(source_context, target_context) {
                denied |= constraint.access_vector();
            }
        }
        denied
    }

    /// Computes the access decision for a set of extended permissions of a given kind and with a
    /// given prefix byte, for a particular source and target context and target class.
    pub fn compute_xperms_access_decision(
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

        let source_attribute_set = &self.type_attribute_maps()[source_context.type_()];
        let target_attribute_set = &self.type_attribute_maps()[target_context.type_()];

        for source_id in source_attribute_set.iter() {
            for target_id in target_attribute_set.iter() {
                for rule in self.access_vector_rules().find_xperm_rules(
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

    fn role_transition_new_role(
        &self,
        current_role: RoleId,
        type_: TypeId,
        class: &Class,
    ) -> Option<RoleId> {
        self.role_transitions()
            .iter()
            .find(|role_transition| {
                role_transition.current_role() == current_role
                    && role_transition.type_() == type_
                    && role_transition.class() == class.id().into()
            })
            .map(|x| x.new_role())
    }

    fn role_transition_is_explicitly_allowed(&self, source_role: RoleId, new_role: RoleId) -> bool {
        self.role_allowlist().iter().any(|role_allow| {
            role_allow.source_role() == source_role && role_allow.new_role() == new_role
        })
    }

    fn type_transition_new_type_with_name(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        class: &Class,
        name: &[u8],
    ) -> Option<TypeId> {
        self.compute_filename_transition(source_type, target_type, class.id().into(), name)
    }

    pub(crate) fn compute_filename_transition(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        class: ClassId,
        name: &[u8],
    ) -> Option<TypeId> {
        self.filename_transitions().compute_filename_transition(
            source_type,
            target_type,
            class,
            name,
        )
    }

    fn range_transition_new_range(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        class: &Class,
    ) -> Option<(MlsLevel, Option<MlsLevel>)> {
        for range_transition in self.range_transitions() {
            if range_transition.source_type() == source_type
                && range_transition.target_type() == target_type
                && range_transition.target_class() == class.id()
            {
                let mls_range = range_transition.mls_range();
                let low_level = mls_range.low().clone();
                let high_level = mls_range.high().clone();
                return Some((low_level, high_level));
            }
        }

        None
    }
}

/// Returns the set of categories present in both `left` and `right`.
fn intersect_categories(left: &MlsLevel, right: &MlsLevel) -> CategorySet {
    let right_categories = right.categories();
    CategorySet::from_ids(left.category_ids().filter(|id| right_categories.contains(*id)))
}

/// Returns the intersection of the `source` and `target` ranges.
///
/// If the two ranges do not overlap then the intersection is empty, and the returned range is
/// mis-ordered, i.e. its high level does not dominate its low level. Such a range is rejected
/// when the resulting Security Context is validated.
fn glblub_range(
    source: &SecurityContext,
    target: &SecurityContext,
) -> (MlsLevel, Option<MlsLevel>) {
    let source_low = source.low_level();
    let source_high = source.effective_high_level();
    let target_low = target.low_level();
    let target_high = target.effective_high_level();

    let low_level = MlsLevel::new(
        std::cmp::max(source_low.sensitivity(), target_low.sensitivity()),
        intersect_categories(source_low, target_low),
    );
    let high_level = MlsLevel::new(
        std::cmp::min(source_high.sensitivity(), target_high.sensitivity()),
        intersect_categories(source_high, target_high),
    );
    (low_level, Some(high_level))
}
