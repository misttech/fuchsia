// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::permissions::{ClassPermission as _, FileClass, KernelClass, KernelPermission};
use super::{AccessVectorComputer, KernelAccessDecision, ObjectClass};
use crate::NullessByteStr;
use crate::policy::{
    AccessDecision, AccessVector, Class, ClassId, HandleUnknown, HasPolicyId as _, PermissionId,
    Policy, PolicyCap, SELINUX_AVD_FLAGS_PERMISSIVE, SecurityContext, XpermsAccessDecision,
    XpermsKind,
};

use std::collections::HashMap;
use std::ops::Deref;
use strum::VariantArray as _;

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
pub(crate) struct PolicyIndex {
    /// Map from [`KernelClass`]es to their corresponding [`ClassId`]s in the associated policy's
    /// classes collection.
    classes: HashMap<KernelClass, ClassId>,
    /// Index mapping kernel class permissions to their policy-specific [`AccessVector`] bit index.
    permissions: [KernelPermissionIdsArray; KernelClass::VARIANTS.len()],
    /// Parsed binary policy.
    policy: Policy,
}

impl PolicyIndex {
    /// Constructs a [`PolicyIndex`] that indexes over well-known policy elements.
    ///
    /// [`Class`]es and permissions used by the kernel are amongst the indexed elements.
    /// The policy's `handle_unknown()` configuration determines whether the policy can be loaded
    /// even if it omits classes or permissions expected by the kernel, and whether to allow or
    /// deny those permissions if so.
    pub fn new(policy: Policy) -> Result<Self, anyhow::Error> {
        let policy_classes = policy.classes();

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
                    if policy.handle_unknown() == HandleUnknown::Reject {
                        return Err(anyhow::anyhow!("missing object class {:?}", known_class));
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
                    policy.find_permission_id_by_name(class, kernel_permission.name())
                {
                    let kernel_class_id = kernel_permission.class() as usize;
                    let kernel_permission_id = kernel_permission.id() as usize;
                    permissions[kernel_class_id][kernel_permission_id] = Some(permission_id);
                } else if policy.handle_unknown() == HandleUnknown::Reject {
                    return Err(anyhow::anyhow!(
                        "missing permission {:?}:{:?}",
                        kernel_class_name,
                        kernel_permission.name(),
                    ));
                }
            }
        }

        Ok(Self { classes, permissions, policy })
    }

    /// Returns the policy entry for a class identified either by its well-known kernel object
    /// class enum value, or its policy-defined Id.
    pub fn class(&self, object_class: ObjectClass) -> Option<&Class> {
        match object_class {
            ObjectClass::Kernel(kernel_class) => {
                let &class_id = self.classes.get(&kernel_class)?;
                self.policy.classes().get_by_id(class_id)
            }
            ObjectClass::ClassId(class_id) => self.policy.classes().get_by_id(class_id),
        }
    }

    /// Returns the policy [`AccessVector`] for a well-known kernel object class permission.
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

    /// Returns the [`SecurityContext`] that should be applied to a newly created SELinux object
    /// according to `source_context` and `target_context`, as well as the new object's
    /// `object_class` and `name`.
    pub fn compute_create_context(
        &self,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        object_class: impl Into<ObjectClass>,
        name: &[u8],
    ) -> SecurityContext {
        // TODO: https://fxbug.dev/361552580 - For `KernelClass`es, apply the kernel's notion
        // of whether the class is "process", or socket-like when `target_class` is `None`?
        let target_class = self.class(object_class.into());
        self.policy.compute_create_context(source_context, target_context, target_class, name)
    }

    /// Computes the access decision for `source_context` and `target_context` on `object_class`.
    pub fn compute_access_decision(
        &self,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        object_class: impl Into<ObjectClass>,
    ) -> AccessDecision {
        if let Some(target_class) = self.class(object_class.into()) {
            self.policy.compute_access_decision(source_context, target_context, target_class)
        } else {
            let mut decision = AccessDecision::allow(AccessVector::NONE);
            if self.policy.is_permissive(source_context.type_()) {
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
            self.policy.compute_xperms_access_decision(
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

    /// If there is a `genfscon` statement for the given filesystem type, returns the associated
    /// [`SecurityContext`], taking the `node_path` and optional `class` into account.
    pub fn genfscon_label_for_fs_and_path(
        &self,
        fs_type: NullessByteStr<'_>,
        node_path: NullessByteStr<'_>,
        class: Option<KernelClass>,
    ) -> Option<SecurityContext> {
        let node_path = if class == Some(FileClass::LnkFile.into())
            && !self.policy.has_policycap(PolicyCap::GenfsSeclabelSymlinks)
        {
            // Symlinks receive the filesystem root label by default, rather than a label dependent on
            // the `node_path`. Path based labels may be enabled with the "genfs_seclabel_symlinks"
            // policy capability.
            "/".into()
        } else {
            node_path
        };

        let class_id = class.and_then(|class| self.class(class.into())).map(|class| class.id());
        self.policy.genfscon_label_for_fs_and_path(fs_type, node_path, class_id)
    }
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
        if self.policy.handle_unknown() == HandleUnknown::Allow {
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
    type Target = Policy;

    fn deref(&self) -> &Self::Target {
        &self.policy
    }
}
