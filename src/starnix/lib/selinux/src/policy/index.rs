// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::arrays::{FsContext, FsUseType};
use super::metadata::HandleUnknown;
use super::security_context::{SecurityContext, SecurityLevel};
use super::symbols::{
    Class, ClassDefault, ClassDefaultRange, Classes, CommonSymbol, CommonSymbols, Permission,
};
use super::{ClassId, ParsedPolicy, RoleId, TypeId};

use crate::{ClassPermission as _, NullessByteStr};
use std::collections::HashMap;

/// The [`SecurityContext`] and [`FsUseType`] derived from some `fs_use_*` line of the policy.
pub struct FsUseLabelAndType {
    pub context: SecurityContext,
    pub use_type: FsUseType,
}

/// An index for facilitating fast lookup of common abstractions inside parsed binary policy data
/// structures. Typically, data is indexed by an enum that describes a well-known value and the
/// index stores the offset of the data in the binary policy to avoid scanning a collection to find
/// an element that contains a matching string. For example, the policy contains a collection of
/// classes that are identified by string names included in each collection entry. However,
/// `policy_index.classes(KernelClass::Process).unwrap()` yields the offset in the policy's
/// collection of classes where the "process" class resides.
#[derive(Debug)]
pub(super) struct PolicyIndex {
    /// Map from object class Ids to their offset in the associate policy's
    /// [`crate::symbols::Classes`] collection. The map includes mappings from both the Ids used
    /// internally for kernel object classes, and from the policy-defined Id for each policy-
    /// defined class - if an object class is not found in this map then it is not defined by the
    /// policy.
    classes: HashMap<crate::ObjectClass, usize>,
    /// Map from well-known permissions to their class's associated [`crate::symbols::Permissions`]
    /// collection.
    permissions: HashMap<crate::KernelPermission, PermissionIndex>,
    /// The parsed binary policy.
    parsed_policy: ParsedPolicy,
    /// The "object_r" role used as a fallback for new file context transitions.
    cached_object_r_role: RoleId,
}

impl PolicyIndex {
    /// Constructs a [`PolicyIndex`] that indexes over well-known policy elements.
    ///
    /// [`Class`]es and [`Permission`]s used by the kernel are amongst the indexed elements.
    /// The policy's `handle_unknown()` configuration determines whether the policy can be loaded even
    /// if it omits classes or permissions expected by the kernel, and whether to allow or deny those
    /// permissions if so.
    pub fn new(parsed_policy: ParsedPolicy) -> Result<Self, anyhow::Error> {
        let policy_classes = parsed_policy.classes();
        let common_symbols = parsed_policy.common_symbols();

        // Accumulate classes indexed by `crate::ObjectClass`. Capacity for twice as many entries as
        // the policy defines allows each class to be indexed by policy-defined Id, and also by the
        // kernel object class enum Id.
        let mut classes = HashMap::with_capacity(policy_classes.len() * 2);

        // Insert elements for each kernel object class. If the policy defines that unknown
        // kernel classes should cause rejection then return an error describing the missing
        // element.
        for known_class in crate::KernelClass::all_variants() {
            match get_class_index_by_name(policy_classes, known_class.name()) {
                Some(class_index) => {
                    classes.insert(known_class.into(), class_index);
                }
                None => {
                    if parsed_policy.handle_unknown() == HandleUnknown::Reject {
                        return Err(anyhow::anyhow!("missing object class {:?}", known_class,));
                    }
                }
            }
        }

        // Insert an element for each class, by its policy-defined Id.
        for index in 0..policy_classes.len() {
            let class = &policy_classes[index];
            classes.insert(class.id().into(), index);
        }

        // Allow unused space in the classes map to be released.
        classes.shrink_to_fit();

        // Accumulate permissions indexed by kernel permission enum. If the policy defines that
        // unknown permissions or classes should cause rejection then return an error describing the
        // missing element.
        let mut permissions =
            HashMap::with_capacity(crate::KernelPermission::all_variants().count());
        for known_permission in crate::KernelPermission::all_variants() {
            let object_class = known_permission.class();
            if let Some(class_index) = classes.get(&object_class.into()) {
                let class = &policy_classes[*class_index];
                if let Some(permission_index) =
                    get_permission_index_by_name(common_symbols, class, known_permission.name())
                {
                    permissions.insert(known_permission, permission_index);
                } else if parsed_policy.handle_unknown() == HandleUnknown::Reject {
                    return Err(anyhow::anyhow!(
                        "missing permission {:?}:{:?}",
                        object_class.name(),
                        known_permission.name(),
                    ));
                }
            }
        }
        permissions.shrink_to_fit();

        // Locate the "object_r" role.
        let cached_object_r_role = parsed_policy
            .role_by_name("object_r".into())
            .ok_or_else(|| anyhow::anyhow!("missing 'object_r' role"))?
            .id();

        let index = Self { classes, permissions, parsed_policy, cached_object_r_role };

        // Verify that the initial Security Contexts are all defined, and valid.
        for initial_sids in crate::InitialSid::all_variants() {
            index.resolve_initial_context(*initial_sids);
        }

        // Validate the contexts used in fs_use statements.
        for fs_use in index.parsed_policy.fs_uses() {
            SecurityContext::new_from_policy_context(fs_use.context());
        }

        Ok(index)
    }

    /// Returns the policy entry for a class identified either by its well-known kernel object class
    /// enum value, or its policy-defined Id.
    pub fn class<'a>(&'a self, object_class: crate::ObjectClass) -> Option<&'a Class> {
        let index = self.classes.get(&object_class)?;
        Some(&self.parsed_policy.classes()[*index])
    }

    /// Returns the policy entry for a well-known kernel object class permission.
    pub fn permission<'a>(
        &'a self,
        permission: &crate::KernelPermission,
    ) -> Option<&'a Permission> {
        let target_class = self.class(permission.class().into())?;
        self.permissions.get(permission).map(|p| match p {
            PermissionIndex::Class { permission_index } => {
                &target_class.permissions()[*permission_index]
            }
            PermissionIndex::Common { common_symbol_index, permission_index } => {
                let common_symbol = &self.parsed_policy().common_symbols()[*common_symbol_index];
                &common_symbol.permissions()[*permission_index]
            }
        })
    }

    /// Returns the security context that should be applied to a newly created SELinux
    /// object according to `source` and `target` security contexts, as well as the new object's
    /// `class`.
    ///
    /// If no filename-transition rule matches the supplied arguments then `None` is returned, and
    /// the caller should fall-back to filename-independent labeling via
    /// [`compute_create_context()`]
    pub fn compute_create_context_with_name(
        &self,
        source: &SecurityContext,
        target: &SecurityContext,
        class: crate::ObjectClass,
        name: NullessByteStr<'_>,
    ) -> Option<SecurityContext> {
        let policy_class = self.class(class)?;
        let type_id = self.type_transition_new_type_with_name(
            source.type_(),
            target.type_(),
            policy_class,
            name,
        )?;
        Some(self.new_security_context_internal(
            source,
            target,
            class,
            // Override the "type" with the value specified by the filename-transition rules.
            Some(type_id),
        ))
    }

    /// Returns the security context that should be applied to a newly created SELinux
    /// object according to `source` and `target` security contexts, as well as the new object's
    /// `class`.
    ///
    /// Computation follows the "create" algorithm for labeling newly created objects:
    /// - user is taken from the `source`.
    /// - role, type and range are taken from the matching transition rules, if any.
    /// - role, type and range fall-back to the `source` or `target` values according to policy.
    ///
    /// If no transitions apply, and the policy does not explicitly specify defaults then the
    /// role, type and range values have defaults chosen based on the `class`:
    /// - For "process", and socket-like classes, role, type and range are taken from the `source`.
    /// - Otherwise role is "object_r", type is taken from `target` and range is set to the
    ///   low level of the `source` range.
    pub fn compute_create_context(
        &self,
        source: &SecurityContext,
        target: &SecurityContext,
        class: crate::ObjectClass,
    ) -> SecurityContext {
        self.new_security_context_internal(source, target, class, None)
    }

    /// Internal implementation used by `compute_create_context_with_name()` and
    /// `compute_create_context()` to implement the policy transition calculations.
    /// If `override_type` is specified then the supplied value will be applied rather than a value
    /// being calculated based on the policy; this is used by `compute_create_context_with_name()`
    /// to shortcut the default `type_transition` lookup.
    fn new_security_context_internal(
        &self,
        source: &SecurityContext,
        target: &SecurityContext,
        target_class: crate::ObjectClass,
        override_type: Option<TypeId>,
    ) -> SecurityContext {
        let Some(policy_class) = self.class(target_class) else {
            // If the class is not defined in the policy then there can be no transitions, nor
            // class-defined choice of defaults, so default to the non-process-or-socket behaviour.
            // TODO: https://fxbug.dev/361552580 - For `KernelClass`es, apply the kernel's notion
            // of whether the class is "process", or socket-like?
            return SecurityContext::new(
                source.user(),
                self.cached_object_r_role,
                target.type_(),
                source.low_level().clone(),
                None,
            );
        };

        let is_process_or_socket = policy_class.name_bytes() == b"process"
            || policy_class.common_name_bytes() == b"socket";
        let (unspecified_role, unspecified_type, unspecified_low, unspecified_high) =
            if is_process_or_socket {
                (source.role(), source.type_(), source.low_level(), source.high_level())
            } else {
                (self.cached_object_r_role, target.type_(), source.low_level(), None)
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
            match self.type_transition_new_type(source.type_(), target.type_(), policy_class) {
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
                },
            };

        // TODO(http://b/334968228): Validate domain & role transitions are allowed?
        SecurityContext::new(user, role, type_, low_level, high_level)
    }

    /// Returns the Id of the "object_r" role within the `parsed_policy`, for use when validating
    /// Security Context fields.
    pub(super) fn object_role(&self) -> RoleId {
        self.cached_object_r_role
    }

    pub(super) fn parsed_policy(&self) -> &ParsedPolicy {
        &self.parsed_policy
    }

    /// Returns the [`SecurityContext`] defined by this policy for the specified
    /// well-known (or "initial") Id.
    pub(super) fn initial_context(&self, id: crate::InitialSid) -> SecurityContext {
        // All [`InitialSid`] have already been verified as resolvable, by `new()`.
        self.resolve_initial_context(id)
    }

    /// If there is an fs_use statement for the given filesystem type, returns the associated
    /// [`SecurityContext`] and [`FsUseType`].
    pub(super) fn fs_use_label_and_type(
        &self,
        fs_type: NullessByteStr<'_>,
    ) -> Option<FsUseLabelAndType> {
        self.parsed_policy.fs_uses().find(|fs_use| fs_use.fs_type() == fs_type.as_bytes()).map(
            |fs_use| FsUseLabelAndType {
                context: SecurityContext::new_from_policy_context(fs_use.context()),
                use_type: fs_use.behavior(),
            },
        )
    }

    /// If there is a genfscon statement for the given filesystem type, returns the associated
    /// [`SecurityContext`], taking the `node_path` into account. `class_id` defines the type
    /// of the file in the given `node_path`. It can only be omitted when looking up the filesystem
    /// label.
    pub(super) fn genfscon_label_for_fs_and_path(
        &self,
        fs_type: NullessByteStr<'_>,
        node_path: NullessByteStr<'_>,
        class_id: Option<ClassId>,
    ) -> Option<SecurityContext> {
        let policy_data = &self.parsed_policy.data;
        // All contexts listed in the policy for the file system type.
        let found = self
            .parsed_policy
            .generic_fs_contexts()
            .find(|genfscon| genfscon.fs_type(policy_data) == fs_type.as_bytes())?;
        let fs_contexts = found.contexts(policy_data);

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
        let mut result: Option<FsContext> = None;
        for fs_context in fs_contexts {
            let partial_path = fs_context.partial_path(policy_data);
            if node_path.0.starts_with(partial_path) {
                if result.is_none()
                    || result.as_ref().unwrap().partial_path(policy_data).len() < partial_path.len()
                {
                    if class_id.is_none()
                        || fs_context
                            .class()
                            .map(|other| other == class_id.unwrap())
                            .unwrap_or(true)
                    {
                        result = Some(fs_context);
                    }
                }
            }
        }

        // The returned SecurityContext must be valid with respect to the policy, since otherwise
        // we'd have rejected the policy load.
        result.and_then(|fs_context| {
            Some(SecurityContext::new_from_policy_context(fs_context.context()))
        })
    }

    /// Helper used to construct and validate well-known [`SecurityContext`] values.
    fn resolve_initial_context(&self, id: crate::InitialSid) -> SecurityContext {
        SecurityContext::new_from_policy_context(&self.parsed_policy().initial_context(id))
    }

    fn role_transition_new_role(
        &self,
        current_role: RoleId,
        type_: TypeId,
        class: &Class,
    ) -> Option<RoleId> {
        self.parsed_policy
            .role_transitions()
            .iter()
            .find(|role_transition| {
                role_transition.current_role() == current_role
                    && role_transition.type_() == type_
                    && role_transition.class() == class.id()
            })
            .map(|x| x.new_role())
    }

    #[allow(dead_code)]
    // TODO(http://b/334968228): fn to be used again when checking role allow rules separately from
    // SID calculation.
    fn role_transition_is_explicitly_allowed(&self, source_role: RoleId, new_role: RoleId) -> bool {
        self.parsed_policy
            .role_allowlist()
            .iter()
            .find(|role_allow| {
                role_allow.source_role() == source_role && role_allow.new_role() == new_role
            })
            .is_some()
    }

    fn type_transition_new_type(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        class: &Class,
    ) -> Option<TypeId> {
        // Return first match. The `checkpolicy` tool will not compile a policy that has
        // multiple matches, so behavior on multiple matches is undefined.
        self.parsed_policy
            .access_vector_rules()
            .find(|access_vector_rule| {
                access_vector_rule.is_type_transition()
                    && access_vector_rule.source_type() == source_type
                    && access_vector_rule.target_type() == target_type
                    && access_vector_rule.target_class() == class.id()
            })
            .map(|x| x.new_type().unwrap())
    }

    fn type_transition_new_type_with_name(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        class: &Class,
        name: NullessByteStr<'_>,
    ) -> Option<TypeId> {
        self.parsed_policy.compute_filename_transition(source_type, target_type, class.id(), name)
    }

    fn range_transition_new_range(
        &self,
        source_type: TypeId,
        target_type: TypeId,
        class: &Class,
    ) -> Option<(SecurityLevel, Option<SecurityLevel>)> {
        for range_transition in self.parsed_policy.range_transitions() {
            if range_transition.source_type() == source_type
                && range_transition.target_type() == target_type
                && range_transition.target_class() == class.id()
            {
                let mls_range = range_transition.mls_range();
                let low_level = SecurityLevel::new_from_mls_level(mls_range.low());
                let high_level = mls_range
                    .high()
                    .as_ref()
                    .map(|high_level| SecurityLevel::new_from_mls_level(high_level));
                return Some((low_level, high_level));
            }
        }

        None
    }
}

/// Permissions may be stored in their associated [`Class`], or on the class's associated
/// [`CommonSymbol`]. This is a consequence of a limited form of inheritance supported for SELinux
/// policy classes. Classes may inherit from zero or one `common`. For example:
///
/// ```config
/// common file { ioctl read write create [...] }
/// class file inherits file { execute_no_trans entrypoint }
/// ```
///
/// In the above example, the "ioctl" permission for the "file" `class` is stored as a permission
/// on the "file" `common`, whereas the permission "execute_no_trans" is stored as a permission on
/// the "file" `class`.
#[derive(Debug)]
enum PermissionIndex {
    /// Permission is located at `Class::permissions()[permission_index]`.
    Class { permission_index: usize },
    /// Permission is located at
    /// `ParsedPolicy::common_symbols()[common_symbol_index].permissions()[permission_index]`.
    Common { common_symbol_index: usize, permission_index: usize },
}

fn get_class_index_by_name<'a>(classes: &'a Classes, name: &str) -> Option<usize> {
    let name_bytes = name.as_bytes();
    for i in 0..classes.len() {
        if classes[i].name_bytes() == name_bytes {
            return Some(i);
        }
    }

    None
}

fn get_common_symbol_index_by_name_bytes<'a>(
    common_symbols: &'a CommonSymbols,
    name_bytes: &[u8],
) -> Option<usize> {
    for i in 0..common_symbols.len() {
        if common_symbols[i].name_bytes() == name_bytes {
            return Some(i);
        }
    }

    None
}

fn get_permission_index_by_name<'a>(
    common_symbols: &'a CommonSymbols,
    class: &'a Class,
    name: &str,
) -> Option<PermissionIndex> {
    if let Some(permission_index) = get_class_permission_index_by_name(class, name) {
        Some(PermissionIndex::Class { permission_index })
    } else if let Some(common_symbol_index) =
        get_common_symbol_index_by_name_bytes(common_symbols, class.common_name_bytes())
    {
        let common_symbol = &common_symbols[common_symbol_index];
        if let Some(permission_index) = get_common_permission_index_by_name(common_symbol, name) {
            Some(PermissionIndex::Common { common_symbol_index, permission_index })
        } else {
            None
        }
    } else {
        None
    }
}

fn get_class_permission_index_by_name<'a>(class: &'a Class, name: &str) -> Option<usize> {
    let name_bytes = name.as_bytes();
    let permissions = class.permissions();
    for i in 0..permissions.len() {
        if permissions[i].name_bytes() == name_bytes {
            return Some(i);
        }
    }

    None
}

fn get_common_permission_index_by_name<'a>(
    common_symbol: &'a CommonSymbol,
    name: &str,
) -> Option<usize> {
    let name_bytes = name.as_bytes();
    let permissions = common_symbol.permissions();
    for i in 0..permissions.len() {
        if permissions[i].name_bytes() == name_bytes {
            return Some(i);
        }
    }

    None
}
