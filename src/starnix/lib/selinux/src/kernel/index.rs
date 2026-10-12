// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::permissions::{ClassPermission as _, FileClass, KernelClass, KernelPermission};
use super::{AccessVectorComputer, KernelAccessDecision, ObjectClass};
use crate::new_policy::rules::{HasRuleKey, RuleKind};
use crate::new_policy::traits::{HasName, HasPolicyId};
use crate::new_policy::{
    CategorySet, Class, ClassDefault, ClassDefaultRange, FsUseType, GenfsConPath, HandleUnknown,
    SELINUX_AVD_FLAGS_PERMISSIVE, XpermsAccessDecision, XpermsKind,
};
use crate::policy::parsed_policy::ParsedPolicy;
use crate::policy::{
    AccessDecision, AccessVector, ClassId, MlsLevel, PermissionId, RoleId, SecurityContext, TypeId,
};
use crate::{NullessByteStr, PolicyCap};

use std::collections::HashMap;
use std::ops::Deref;

use strum::VariantArray as _;

/// [`SecurityContext`] and [`FsUseType`] derived from some `fs_use_*` line of the policy.
pub struct FsUseLabelAndType {
    pub context: SecurityContext,
    pub use_type: FsUseType,
}

/// Array of [`PermissionId`] values for each of a kernel security class' permissions.
type KernelPermissionIdsArray = [Option<PermissionId>; 32];

/// Index for facilitating fast lookup of common abstractions inside parsed binary policy data
/// structures. Typically, data is indexed by an enum that describes a well-known value and the
/// index stores the ID or offset of the data in the binary policy to avoid scanning a collection to
/// find an element that contains a matching string. For example, the policy contains a collection
/// of classes that are identified by string names included in each collection entry. However,
/// `policy_index.class(KernelClass::Process.into()).unwrap()` yields the [`Class`] entry in the
/// policy's collection of classes without scanning by name.
#[derive(Debug)]
pub struct PolicyIndex {
    /// Map from [`KernelClass`]es to their corresponding [`ClassId`]s in the associated policy's
    /// classes collection.
    classes: HashMap<KernelClass, ClassId>,
    /// Index mapping kernel class permissions to their policy-specific [`AccessVector`] bit index.
    permissions: [KernelPermissionIdsArray; KernelClass::VARIANTS.len()],
    /// Parsed binary policy.
    parsed_policy: ParsedPolicy,
}

impl PolicyIndex {
    /// Constructs a [`PolicyIndex`] that indexes over well-known policy elements.
    ///
    /// [`Class`]es and permissions used by the kernel are amongst the indexed elements.
    /// The policy's `handle_unknown()` configuration determines whether the policy can be loaded even
    /// if it omits classes or permissions expected by the kernel, and whether to allow or deny those
    /// permissions if so.
    pub fn new(parsed_policy: ParsedPolicy) -> Result<Self, anyhow::Error> {
        let policy_classes = parsed_policy.classes();

        let mut classes = HashMap::with_capacity(KernelClass::VARIANTS.len());

        // Insert elements for each kernel object class. If the policy defines that unknown
        // kernel classes should cause rejection then return an error describing the missing
        // element.
        for known_class in KernelClass::VARIANTS {
            match policy_classes.get_by_name(known_class.name().as_bytes()) {
                Some(class) => {
                    classes.insert(*known_class, class.id());
                }
                None => {
                    if parsed_policy.handle_unknown() == HandleUnknown::Reject {
                        return Err(anyhow::anyhow!("missing object class {:?}", known_class,));
                    }
                }
            }
        }

        // Allow unused space in the classes map to be released.
        classes.shrink_to_fit();

        // Accumulate permissions indexed by kernel permission enum. If the policy defines that
        // unknown permissions or classes should cause rejection then return an error describing the
        // missing element.
        let mut permissions = [KernelPermissionIdsArray::default(); _];
        for kernel_permission in KernelPermission::all_variants() {
            let kernel_class_name = kernel_permission.class().name();
            if let Some(class) = policy_classes.get_by_name(kernel_class_name.as_bytes()) {
                if let Some(permission_id) =
                    parsed_policy.find_permission_id_by_name(class, kernel_permission.name())
                {
                    let kernel_class_id = kernel_permission.class() as usize;
                    let kernel_permission_id = kernel_permission.id() as usize;
                    permissions[kernel_class_id][kernel_permission_id] = Some(permission_id);
                } else if parsed_policy.handle_unknown() == HandleUnknown::Reject {
                    return Err(anyhow::anyhow!(
                        "missing permission {:?}:{:?}",
                        kernel_class_name,
                        kernel_permission.name(),
                    ));
                }
            }
        }

        Ok(Self { classes, permissions, parsed_policy })
    }

    /// Returns the policy entry for a class identified either by its well-known kernel object class
    /// enum value, or its policy-defined Id.
    pub fn class(&self, object_class: ObjectClass) -> Option<&Class> {
        match object_class {
            ObjectClass::Kernel(kernel_class) => {
                let &class_id = self.classes.get(&kernel_class)?;
                self.classes().get_by_id(class_id)
            }
            ObjectClass::ClassId(class_id) => self.classes().get_by_id(class_id),
        }
    }

    /// Returns the policy entry for a well-known kernel object class permission.
    pub fn kernel_permission_to_access_vector<P: Into<KernelPermission>>(
        &self,
        permission: P,
    ) -> Option<AccessVector> {
        let permission = permission.into();
        let class_index = permission.class() as usize;
        let permission_index = permission.id() as usize;
        let permission_id = self.permissions[class_index][permission_index]?;
        Some(permission_id.into())
    }

    /// Returns the security context that should be applied to a newly created SELinux
    /// object according to `source_context` and `target_context`, as well as the new object's
    /// `object_class` and `name`.
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
    /// role, type and range values have defaults chosen based on the `object_class`:
    /// - For "process", and socket-like classes, role, type and range are taken from the
    ///   `source_context`.
    /// - Otherwise role is "object_r", type is taken from `target_context` and range is set to the
    ///   low level of the `source_context` range.
    pub fn compute_create_context(
        &self,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        object_class: ObjectClass,
        name: &[u8],
    ) -> SecurityContext {
        let override_type = if !name.is_empty() {
            self.class(object_class).and_then(|policy_class| {
                self.type_transition_new_type_with_name(
                    source_context.type_(),
                    target_context.type_(),
                    &policy_class,
                    name,
                )
            })
        } else {
            None
        };
        self.new_security_context_internal(
            source_context,
            target_context,
            object_class,
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
        target_class: ObjectClass,
        override_type: Option<TypeId>,
    ) -> SecurityContext {
        let Some(policy_class) = self.class(target_class) else {
            // If the class is not defined in the policy then there can be no transitions, nor
            // class-defined choice of defaults, so default to the non-process-or-socket behaviour.
            // TODO: https://fxbug.dev/361552580 - For `KernelClass`es, apply the kernel's notion
            // of whether the class is "process", or socket-like?
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

        let role = match self.role_transition_new_role(source.role(), target.type_(), &policy_class)
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
            match self.range_transition_new_range(source.type_(), target.type_(), &policy_class) {
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

    /// Computes the access decision for `source_context` and `target_context` on `object_class`.
    pub fn compute_access_decision(
        &self,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        object_class: impl Into<ObjectClass>,
    ) -> AccessDecision {
        if let Some(target_class) = self.class(object_class.into()) {
            self.parsed_policy.compute_access_decision(source_context, target_context, target_class)
        } else {
            let mut decision = AccessDecision::allow(AccessVector::NONE);
            if self.parsed_policy.is_permissive(source_context.type_()) {
                decision.flags |= SELINUX_AVD_FLAGS_PERMISSIVE;
            }
            decision
        }
    }

    /// Computes the extended permissions access decision for `source_context` and `target_context`
    /// on `object_class` for the given `xperms_kind` and `xperms_prefix`.
    pub fn compute_xperms_access_decision(
        &self,
        xperms_kind: XpermsKind,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        object_class: impl Into<ObjectClass>,
        xperms_prefix: u8,
    ) -> XpermsAccessDecision {
        if let Some(target_class) = self.class(object_class.into()) {
            self.parsed_policy.compute_xperms_access_decision(
                xperms_kind,
                source_context,
                target_context,
                target_class,
                xperms_prefix,
            )
        } else {
            XpermsAccessDecision::DENY_ALL
        }
    }

    /// If there is an fs_use statement for the given filesystem type, returns the associated
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

    /// If there is a genfscon statement for the given filesystem type, returns the associated
    /// [`SecurityContext`], taking the `node_path` into account. `class_id` defines the type
    /// of the file in the given `node_path`. It can only be omitted when looking up the filesystem
    /// label.
    pub fn genfscon_label_for_fs_and_path(
        &self,
        fs_type: NullessByteStr<'_>,
        node_path: NullessByteStr<'_>,
        class: Option<KernelClass>,
    ) -> Option<SecurityContext> {
        let node_path = if class == Some(FileClass::LnkFile.into())
            && !self.has_policycap(PolicyCap::GenfsSeclabelSymlinks)
        {
            // Symlinks receive the filesystem root label by default, rather than a label dependent on
            // the `node_path`. Path based labels may be enabled with the "genfs_seclabel_symlinks"
            // policy capability.
            "/".into()
        } else {
            node_path
        };

        let class_id = class.and_then(|class| self.class(class.into())).map(|class| class.id());

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

            if order_type == OrderType::Alphabetic && fs_context.partial_path() > node_path.0 {
                // We know that:
                // - We have alphabetic order,
                // - The current path is lexicographically greater than our target path.
                // We can infer that we have passed any potential prefixes in alphabetical order.
                break;
            }

            if node_path.0.starts_with(fs_context.partial_path()) {
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

    fn type_transition_new_type_with_name(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        class: &Class,
        name: &[u8],
    ) -> Option<TypeId> {
        self.compute_filename_transition(source_type, target_type, class.id().into(), name)
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

impl AccessVectorComputer for PolicyIndex {
    fn access_decision_to_kernel_access_decision(
        &self,
        class: KernelClass,
        access_decision: AccessDecision,
    ) -> KernelAccessDecision {
        let mut kernel_allow;
        let mut kernel_audit;
        // Set the default values of the bits as appropriate for the policy's handle_unknown value.
        // Bits corresponding to policy-known permissions will be overwritten.
        if self.handle_unknown() == HandleUnknown::Allow {
            // If we allow unknown permissions, a bit will be by default allowed and not audited.
            kernel_allow = u32::MAX;
            kernel_audit = 0u32;
        } else {
            // Otherwise, a bit is by default audited and not allowed.
            kernel_allow = 0u32;
            kernel_audit = u32::MAX;
        }

        let decision_allow = access_decision.allow;
        let decision_audit = (access_decision.allow & access_decision.auditallow)
            | (!access_decision.allow & access_decision.auditdeny);
        for permission in class.permissions() {
            if let Some(permission_access_vector) =
                self.kernel_permission_to_access_vector(*permission)
            {
                // If the permission is known, set the corresponding bit according to
                // `decision_allow` and `decision_audit`.
                let bit = 1 << permission.id();
                let allow = decision_allow & permission_access_vector == permission_access_vector;
                let audit = decision_audit & permission_access_vector == permission_access_vector;
                kernel_allow = (kernel_allow & !bit) | ((allow as u32) << permission.id());
                kernel_audit = (kernel_audit & !bit) | ((audit as u32) << permission.id());
            }
        }
        KernelAccessDecision {
            allow: AccessVector::from(kernel_allow),
            audit: AccessVector::from(kernel_audit),
            flags: access_decision.flags,
            todo_bug: access_decision.todo_bug,
        }
    }
}

impl Deref for PolicyIndex {
    type Target = ParsedPolicy;

    fn deref(&self) -> &Self::Target {
        &self.parsed_policy
    }
}
