// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::kernel::{AccessVectorCache, KernelAccessDecision, Query};
use crate::policy::{AccessVector, SELINUX_AVD_FLAGS_PERMISSIVE, XpermsKind};
use crate::security_server::{PolicySeqNo, SecurityServer};
use crate::{
    ClassPermission, FdPermission, KernelClass, KernelPermission, KernelPermissionSet, SecurityId,
};

use std::num::NonZeroU32;

pub use crate::kernel::PerThreadCache;

/// Result of a permission lookup between two Security Contexts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermissionCheckResult {
    /// True if all of the specified permissions are granted by policy.
    pub granted: bool,

    /// Subset of the requested permissions to be audit logged, as kernel permission-Id bits of the
    /// permissions' class (see [`KernelPermissionSet`]), or `AccessVector::NONE` if the check
    /// should not be audited.
    ///
    /// If any requested permissions are denied, this holds the denied permissions except those
    /// marked `dontaudit`. Otherwise, this holds the requested permissions marked `auditallow`.
    pub audit: AccessVector,

    /// True if the access should be granted because either the security server is running in
    /// permissive mode, or the subject domain is marked as permissive.
    pub permissive: bool,

    /// If the [`KernelAccessDecision`] indicates that permission denials should not be enforced then
    /// [`PermissionCheckResult::permit`] will be true, and this field will hold the Id of the bug to
    /// reference in audit logging.
    pub todo_bug: Option<NonZeroU32>,
}

impl PermissionCheckResult {
    /// Returns true if the request was granted, or was made in permissive mode.
    #[inline]
    pub fn permit(&self) -> bool {
        self.granted || self.permissive
    }

    /// Returns the permissions that should be audit logged, as a set of `class` permissions, or
    /// `None` if nothing should be audit logged.
    #[inline]
    pub fn audit_permissions(&self, class: KernelClass) -> Option<KernelPermissionSet> {
        (self.audit != AccessVector::NONE).then(|| KernelPermissionSet::new(class, self.audit))
    }
}

/// Implements the [`PermissionCheck::has_permissions`] API, based on supplied [`SecurityServer`] and
/// [`AccessVectorCache`] implementations.
// TODO: https://fxbug.dev/362699811 - Revise the traits to avoid direct dependencies on `SecurityServer`.
pub struct PermissionCheck<'a> {
    security_server: &'a SecurityServer,
    access_vector_cache: &'a AccessVectorCache,
    local_cache: &'a PerThreadCache,
}

impl<'a> PermissionCheck<'a> {
    pub(crate) fn new(
        security_server: &'a SecurityServer,
        access_vector_cache: &'a AccessVectorCache,
        local_cache: &'a PerThreadCache,
    ) -> Self {
        Self { security_server, access_vector_cache, local_cache }
    }

    /// Returns whether the `source_sid` has the specified `permission` on `target_sid`.
    /// The result indicates both whether `permission` is `permit`ted, and whether the caller
    /// should `audit` log the query.
    #[inline]
    pub fn has_permission<P: ClassPermission + Into<KernelPermission> + 'static>(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        permission: P,
    ) -> PermissionCheckResult {
        self.has_permissions(source_sid, target_sid, permission)
    }

    /// Returns whether the `source_sid` has all of the specified `permissions` on `target_sid`.
    /// The result indicates both whether the `permissions` are `permit`ted, and which of them the
    /// caller should `audit` log.
    ///
    /// `permissions` may be a single permission, a typed [`PermissionSet`](crate::PermissionSet),
    /// or a [`KernelPermissionSet`] of permissions of the same class, which are checked against a
    /// single access decision.
    #[inline]
    pub fn has_permissions(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        permissions: impl Into<KernelPermissionSet>,
    ) -> PermissionCheckResult {
        let policy_seqno = self.security_server.policy_seqno();
        let result = has_permissions(
            self.local_cache,
            self.access_vector_cache,
            policy_seqno,
            source_sid,
            target_sid,
            permissions.into(),
        );
        self.apply_enforcement(result)
    }

    /// Returns whether the `source_sid` has both a base permission (i.e. `ioctl` or `nlmsg`) and
    /// the specified extended permission on `target_sid`, and whether the decision should be
    /// audited.
    ///
    /// A request is allowed if the base permission is `allow`ed and either the numeric extended
    /// permission of this `xperms_kind` is included in an `allowxperm` statement, or extended
    /// permissions of this kind are not filtered for this domain.
    ///
    /// A granted request is audited if the base permission is `auditallow` and the extended
    /// permission is `auditallowxperm`.
    ///
    /// A denied request is audited if the base permission is `dontaudit` or the extended
    /// permission is `dontauditxperm`.
    pub fn has_extended_permission<P: ClassPermission + Into<KernelPermission> + 'static>(
        &self,
        xperms_kind: XpermsKind,
        source_sid: SecurityId,
        target_sid: SecurityId,
        permission: P,
        xperm: u16,
    ) -> PermissionCheckResult {
        let permission: KernelPermission = permission.into();
        let policy_seqno = self.security_server.policy_seqno();
        let result = self.local_cache.check_xperm(
            policy_seqno,
            xperms_kind,
            source_sid,
            target_sid,
            permission,
            xperm,
            move || {
                has_extended_permission(
                    self.access_vector_cache,
                    xperms_kind,
                    source_sid,
                    target_sid,
                    permission,
                    xperm,
                )
            },
        );
        self.apply_enforcement(result)
    }

    #[inline]
    fn apply_enforcement(&self, mut result: PermissionCheckResult) -> PermissionCheckResult {
        if !result.granted {
            if !self.security_server.is_enforcing() {
                result.permissive = true;
                result.todo_bug = None;
            } else if result.todo_bug.is_some() {
                result.granted = true;
            }
        } else {
            result.todo_bug = None;
        }
        result
    }

    // TODO: https://fxbug.dev/362699811 - Remove this once `SecurityServer` APIs such as `sid_to_security_context()`
    // are exposed via a trait rather than directly by that implementation.
    pub fn security_server(&self) -> &SecurityServer {
        self.security_server
    }

    /// Returns the SID with which to label a new `target_class` instance created by `source_sid`
    /// in a container labeled `target_sid`, taking into account role, type, and optional filename
    /// transition rules.
    /// Callers pass an empty slice (`&[]`) for `name` to express nameless transitions.
    pub fn compute_create_sid(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: KernelClass,
        name: &[u8],
    ) -> Result<SecurityId, anyhow::Error> {
        self.access_vector_cache.compute_create_sid(source_sid, target_sid, target_class, name)
    }

    /// Returns the raw [`KernelAccessDecision`] for a specified source, target and class.
    pub fn compute_access_decision(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: KernelClass,
    ) -> KernelAccessDecision {
        self.local_cache.lookup_access_decision(
            self.security_server.policy_seqno(),
            source_sid,
            target_sid,
            target_class,
            move || {
                self.access_vector_cache.compute_access_decision(
                    source_sid,
                    target_sid,
                    target_class,
                )
            },
        )
    }
}

/// Internal implementation of the [`PermissionCheck::has_permissions`] API, in terms of the [`Query`] trait.
#[inline]
fn has_permissions(
    local_cache: &PerThreadCache,
    query: &impl Query,
    policy_seqno: PolicySeqNo,
    source_sid: SecurityId,
    target_sid: SecurityId,
    permissions: KernelPermissionSet,
) -> PermissionCheckResult {
    let class = permissions.class();
    let requested = permissions.access_vector();

    if permissions == KernelPermissionSet::from(FdPermission::Use) {
        // fd use checks are cached separately.
        return local_cache.lookup_fd_use(policy_seqno, source_sid, target_sid, move || {
            let decision = query.compute_access_decision(source_sid, target_sid, KernelClass::Fd);
            access_decision_to_permission_check_result(requested, decision)
        });
    }

    let decision = local_cache.lookup_access_decision(
        policy_seqno,
        source_sid,
        target_sid,
        class,
        move || query.compute_access_decision(source_sid, target_sid, class),
    );
    access_decision_to_permission_check_result(requested, decision)
}

#[inline]
fn access_decision_to_permission_check_result(
    requested: AccessVector,
    decision: KernelAccessDecision,
) -> PermissionCheckResult {
    let permissive = decision.flags & SELINUX_AVD_FLAGS_PERMISSIVE != 0;
    let denied = requested - decision.allow;
    let granted = denied == AccessVector::NONE;

    // If any `requested` permissions are denied then the denied permissions are audited, unless
    // they are "dontaudit". Otherwise the permissions are audited only if they are "auditallow".
    let audited = if granted { requested } else { denied };
    let audit = audited & decision.audit;

    PermissionCheckResult { granted, audit, permissive, todo_bug: decision.todo_bug }
}

/// Internal implementation of the [`PermissionCheck::has_extended_permission`] API, in terms of the [`Query`] trait.
fn has_extended_permission(
    query: &impl Query,
    xperms_kind: XpermsKind,
    source_sid: SecurityId,
    target_sid: SecurityId,
    permission: KernelPermission,
    xperm: u16,
) -> PermissionCheckResult {
    let [xperms_postfix, xperms_prefix] = xperm.to_le_bytes();
    let xperms_decision = query.compute_xperms_access_decision(
        xperms_kind,
        source_sid,
        target_sid,
        permission,
        xperms_prefix,
    );

    let granted = xperms_decision.allow.contains(xperms_postfix);
    let audit = if xperms_decision.audit.contains(xperms_postfix) {
        permission.as_access_vector()
    } else {
        AccessVector::NONE
    };
    let permissive = xperms_decision.permissive;
    let mut result = PermissionCheckResult { granted, audit, permissive, todo_bug: None };

    if !result.permit() && xperms_decision.has_todo {
        // A todo_bug applies to this entry. Look up the base decision for details.
        // This will re-compute the base decision if it is not cached.
        let base_decision =
            query.compute_access_decision(source_sid, target_sid, permission.class());
        result.todo_bug = base_decision.todo_bug;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::KernelXpermsAccessDecision;
    use crate::policy::{AccessVector, XpermsBitmap};
    use crate::{
        CommonFsNodePermission, FdPermissionSet, FileClass, ForClass, KernelClass,
        ProcessPermission,
    };

    use std::num::NonZeroU32;
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// SID to use where any value will do.
    static A_TEST_SID: LazyLock<SecurityId> = LazyLock::new(unique_sid);

    /// Returns a new [`SecurityId`] with unique id.
    fn unique_sid() -> SecurityId {
        static NEXT_ID: AtomicU32 = AtomicU32::new(1000);
        SecurityId(NonZeroU32::new(NEXT_ID.fetch_add(1, Ordering::AcqRel)).unwrap())
    }

    /// [`Query`] that denies all [`AccessVector`]s.
    #[derive(Default)]
    struct DenyAllPermissions;

    impl Query for DenyAllPermissions {
        fn compute_access_decision(
            &self,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _target_class: KernelClass,
        ) -> KernelAccessDecision {
            KernelAccessDecision {
                allow: AccessVector::NONE,
                audit: AccessVector::ALL,
                flags: 0,
                todo_bug: None,
            }
        }

        fn compute_create_sid(
            &self,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _target_class: KernelClass,
            _name: &[u8],
        ) -> Result<SecurityId, anyhow::Error> {
            unreachable!();
        }

        fn compute_xperms_access_decision(
            &self,
            _xperms_kind: XpermsKind,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _permission: KernelPermission,
            _xperms_prefix: u8,
        ) -> KernelXpermsAccessDecision {
            KernelXpermsAccessDecision {
                allow: XpermsBitmap::NONE,
                audit: XpermsBitmap::ALL,
                permissive: false,
                has_todo: false,
            }
        }
    }

    /// [`Query`] that permits all [`AccessVector`]s.
    #[derive(Default)]
    struct AllowAllPermissions;

    impl Query for AllowAllPermissions {
        fn compute_access_decision(
            &self,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _target_class: KernelClass,
        ) -> KernelAccessDecision {
            KernelAccessDecision {
                allow: AccessVector::ALL,
                audit: AccessVector::NONE,
                flags: 0,
                todo_bug: None,
            }
        }

        fn compute_create_sid(
            &self,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _target_class: KernelClass,
            _name: &[u8],
        ) -> Result<SecurityId, anyhow::Error> {
            unreachable!();
        }

        fn compute_xperms_access_decision(
            &self,
            _xperms_kind: XpermsKind,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _permission: KernelPermission,
            _xperms_prefix: u8,
        ) -> KernelXpermsAccessDecision {
            KernelXpermsAccessDecision {
                allow: XpermsBitmap::ALL,
                audit: XpermsBitmap::NONE,
                permissive: false,
                has_todo: false,
            }
        }
    }

    /// [`Query`] that returns the same [`KernelAccessDecision`] for all source, target and class
    /// combinations.
    struct FixedAccessDecision(KernelAccessDecision);

    impl Query for FixedAccessDecision {
        fn compute_access_decision(
            &self,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _target_class: KernelClass,
        ) -> KernelAccessDecision {
            self.0
        }

        fn compute_create_sid(
            &self,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _target_class: KernelClass,
            _name: &[u8],
        ) -> Result<SecurityId, anyhow::Error> {
            unreachable!();
        }

        fn compute_xperms_access_decision(
            &self,
            _xperms_kind: XpermsKind,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _permission: KernelPermission,
            _xperms_prefix: u8,
        ) -> KernelXpermsAccessDecision {
            unreachable!();
        }
    }

    #[test]
    fn has_permissions_both() {
        let deny_all = DenyAllPermissions::default();
        let allow_all = AllowAllPermissions::default();

        // Use permissions that are mapped to access vector bits in
        // `access_vector_from_permission`.
        let permissions = [ProcessPermission::Fork, ProcessPermission::Transition];
        for permission in permissions {
            let local_cache1 = PerThreadCache::default();
            // DenyAllPermissions denies.
            let result = has_permissions(
                &local_cache1,
                &deny_all,
                PolicySeqNo::INITIAL,
                *A_TEST_SID,
                *A_TEST_SID,
                permission.into(),
            );
            assert_eq!(
                result,
                PermissionCheckResult {
                    granted: false,
                    audit: permission.as_access_vector(),
                    permissive: false,
                    todo_bug: None
                }
            );
            assert!(!result.permit());

            let local_cache2 = PerThreadCache::default();
            // AllowAllPermissions allows.
            let result = has_permissions(
                &local_cache2,
                &allow_all,
                PolicySeqNo::INITIAL,
                *A_TEST_SID,
                *A_TEST_SID,
                permission.into(),
            );
            assert_eq!(
                result,
                PermissionCheckResult {
                    granted: true,
                    audit: AccessVector::NONE,
                    permissive: false,
                    todo_bug: None
                }
            );
            assert!(result.permit());
        }
    }

    #[test]
    fn has_permissions_audits_only_denied_permissions() {
        // Allow only `fork`, and audit all permissions, whether allowed or denied.
        let query = FixedAccessDecision(KernelAccessDecision {
            allow: ProcessPermission::Fork.as_access_vector(),
            audit: AccessVector::ALL,
            flags: 0,
            todo_bug: None,
        });
        let local_cache = PerThreadCache::default();

        // Only the denied permissions are audited, although the allowed one is "auditallow".
        let requested =
            ProcessPermission::Fork | ProcessPermission::GetSched | ProcessPermission::SetSched;
        let expected_audit = ProcessPermission::GetSched | ProcessPermission::SetSched;
        let result = has_permissions(
            &local_cache,
            &query,
            PolicySeqNo::INITIAL,
            *A_TEST_SID,
            *A_TEST_SID,
            requested.into(),
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: expected_audit.access_vector(),
                permissive: false,
                todo_bug: None
            }
        );
        assert_eq!(result.audit_permissions(KernelClass::Process), Some(expected_audit.into()));
        assert!(!result.permit());
    }

    #[test]
    fn has_permissions_permissive_domain_audits_only_denied_permissions() {
        // Allow only `fork`, audit all permissions, and mark the source domain as permissive.
        let query = FixedAccessDecision(KernelAccessDecision {
            allow: ProcessPermission::Fork.as_access_vector(),
            audit: AccessVector::ALL,
            flags: SELINUX_AVD_FLAGS_PERMISSIVE,
            todo_bug: None,
        });
        let local_cache = PerThreadCache::default();

        // Only the denied permissions are audited, and the check is permitted nonetheless.
        let requested =
            ProcessPermission::Fork | ProcessPermission::GetSched | ProcessPermission::SetSched;
        let expected_audit = ProcessPermission::GetSched | ProcessPermission::SetSched;
        let result = has_permissions(
            &local_cache,
            &query,
            PolicySeqNo::INITIAL,
            *A_TEST_SID,
            *A_TEST_SID,
            requested.into(),
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: expected_audit.access_vector(),
                permissive: true,
                todo_bug: None
            }
        );
        assert_eq!(result.audit_permissions(KernelClass::Process), Some(expected_audit.into()));
        assert!(result.permit());
    }

    #[test]
    fn has_permissions_audits_auditallow_permissions_if_granted() {
        // Allow all permissions, and audit `fork` and `getsched`.
        let expected_audit = ProcessPermission::Fork | ProcessPermission::GetSched;
        let query = FixedAccessDecision(KernelAccessDecision {
            allow: AccessVector::ALL,
            audit: expected_audit.access_vector(),
            flags: 0,
            todo_bug: None,
        });
        let local_cache = PerThreadCache::default();

        // Only the requested "auditallow" permissions are audited.
        let requested =
            ProcessPermission::Fork | ProcessPermission::GetSched | ProcessPermission::SetSched;
        let result = has_permissions(
            &local_cache,
            &query,
            PolicySeqNo::INITIAL,
            *A_TEST_SID,
            *A_TEST_SID,
            requested.into(),
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: expected_audit.access_vector(),
                permissive: false,
                todo_bug: None
            }
        );
        assert_eq!(result.audit_permissions(KernelClass::Process), Some(expected_audit.into()));
        assert!(result.permit());
    }

    #[test]
    fn has_permissions_empty_fd_set_does_not_affect_fd_use_cache() {
        let deny_all = DenyAllPermissions::default();
        let local_cache = PerThreadCache::default();

        // An empty set of permissions is trivially granted.
        let result = has_permissions(
            &local_cache,
            &deny_all,
            PolicySeqNo::INITIAL,
            *A_TEST_SID,
            *A_TEST_SID,
            FdPermissionSet::empty().into(),
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: AccessVector::NONE,
                permissive: false,
                todo_bug: None
            }
        );

        // `fd { use }` is still denied, since the granted empty set was not cached as an fd use.
        let result = has_permissions(
            &local_cache,
            &deny_all,
            PolicySeqNo::INITIAL,
            *A_TEST_SID,
            *A_TEST_SID,
            FdPermission::Use.into(),
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: FdPermission::Use.as_access_vector(),
                permissive: false,
                todo_bug: None
            }
        );
    }

    #[test]
    fn has_ioctl_permission_enforcing() {
        let deny_all = DenyAllPermissions::default();
        let allow_all = AllowAllPermissions::default();
        let permission = CommonFsNodePermission::Ioctl.for_class(FileClass::File);

        // DenyAllPermissions denies.
        let result = has_extended_permission(
            &deny_all,
            XpermsKind::Ioctl,
            *A_TEST_SID,
            *A_TEST_SID,
            permission.into(),
            0xabcd,
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: permission.as_access_vector(),
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());

        // AllowAllPermissions allows.
        let result = has_extended_permission(
            &allow_all,
            XpermsKind::Ioctl,
            *A_TEST_SID,
            *A_TEST_SID,
            permission.into(),
            0xabcd,
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: AccessVector::NONE,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(result.permit());
    }

    fn security_server_with_tests_policy() -> std::sync::Arc<SecurityServer> {
        const POLICY: &[u8] =
            include_bytes!("../testdata/micro_policies/security_server_tests_policy");
        let security_server = SecurityServer::new_default();
        assert!(security_server.load_policy(POLICY.into()).is_ok());
        security_server
    }

    #[test]
    fn has_ioctl_permission_not_enforcing() {
        let security_server = security_server_with_tests_policy();
        let enforcing_values = [true, false];
        for enforcing in enforcing_values {
            security_server.set_enforcing(enforcing);

            let sid =
                security_server.security_context_to_sid("user0:object_r:type0:s0".into()).unwrap();
            let local_cache = PerThreadCache::default();
            let permission_check = security_server.as_permission_check(&local_cache);

            let permission = CommonFsNodePermission::Ioctl.for_class(FileClass::File);

            // The test policy does not grant the permission, but when the security server
            // is not in enforcing mode the permission will still be granted.
            // Because the permission was not granted by policy, the check will be audit logged.
            let result = permission_check.has_extended_permission(
                XpermsKind::Ioctl,
                sid,
                sid,
                permission,
                0xabcd,
            );
            assert_eq!(
                result,
                PermissionCheckResult {
                    granted: false,
                    audit: permission.as_access_vector(),
                    permissive: !enforcing,
                    todo_bug: None
                }
            );
            assert_eq!(result.permit(), !enforcing);
        }
    }
}
