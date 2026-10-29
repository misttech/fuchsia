// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::exceptions_config::ExceptionsConfig;
use crate::kernel::{
    AccessVectorCache, CacheStats, KernelAccessDecision, KernelXpermsAccessDecision, PolicyIndex,
    Query,
};
use crate::permission_check::{PerThreadCache, PermissionCheck};
use crate::policy::{
    AccessDecision, AccessVector, ClassId, FsUseLabelAndType, HandleUnknown, HasName as _,
    HasPolicyId as _, PermissionId, Policy, PolicyData, SELINUX_AVD_FLAGS_PERMISSIVE,
    SecurityContext, XpermsBitmap, XpermsKind,
};
use crate::sid_table::{PortableSidTable, SidTable};
use crate::sync::{LockDepMutex, SeLinuxSecurityServerStateLock, SeLinuxStatusLock};
use crate::{
    ClassPermission, FileSystemLabel, FileSystemLabelingScheme, FileSystemMountOptions,
    FileSystemMountSids, InitialSid, KernelClass, KernelPermission, NullessByteStr, ObjectClass,
    PolicyCap, SeLinuxStatus, SeLinuxStatusPublisher, SecurityId,
};
use anyhow::Context as _;
use fuchsia_rcu::{RcuDroppable, RcuOptionArc, RcuReadScope};
use starnix_logging::log_info;
use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::DerefMut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const ROOT_PATH: &'static str = "/";

#[derive(RcuDroppable)]
struct ActivePolicy {
    /// Parsed policy structure.
    parsed: PolicyIndex,

    /// Allocates and maintains the mapping between `SecurityId`s (SIDs) and Security Contexts.
    sid_table: SidTable,

    /// Describes access checks that should be granted, with associated bug Ids.
    exceptions: ExceptionsConfig,

    /// Size in bytes of the loaded binary policy.
    binary_policy_size: usize,
}

impl ActivePolicy {
    fn compute_access_decision_raw(
        &self,
        scope: &RcuReadScope,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: ObjectClass,
    ) -> AccessDecision {
        let (source_context, target_context) =
            self.sid_table.sids_to_security_contexts(scope, source_sid, target_sid);
        self.compute_access_decision_for_contexts(source_context, target_context, target_class)
    }

    fn compute_access_decision_for_contexts(
        &self,
        source_context: &SecurityContext,
        target_context: &SecurityContext,
        target_class: ObjectClass,
    ) -> AccessDecision {
        let mut decision =
            self.parsed.compute_access_decision(source_context, target_context, target_class);

        decision.todo_bug =
            self.exceptions.lookup(source_context.type_(), target_context.type_(), target_class);

        decision
    }
}

#[derive(Default)]
struct SeLinuxBooleans {
    /// Active values for all of the booleans defined by the policy.
    /// Entries are created at policy load for each policy-defined conditional.
    active: HashMap<String, bool>,
    /// Pending values for any booleans modified since the last commit.
    pending: HashMap<String, bool>,
}

impl SeLinuxBooleans {
    fn reset(&mut self, booleans: Vec<(String, bool)>) {
        self.active = HashMap::from_iter(booleans);
        self.pending.clear();
    }
    fn names(&self) -> Vec<String> {
        self.active.keys().cloned().collect()
    }
    fn set_pending(&mut self, name: &str, value: bool) -> Result<(), ()> {
        if !self.active.contains_key(name) {
            return Err(());
        }
        self.pending.insert(name.into(), value);
        Ok(())
    }
    fn get(&self, name: &str) -> Result<(bool, bool), ()> {
        let active = self.active.get(name).ok_or(())?;
        let pending = self.pending.get(name).unwrap_or(active);
        Ok((*active, *pending))
    }
    fn commit_pending(&mut self) {
        self.active.extend(self.pending.drain());
    }
}

#[derive(Default)]
struct SecurityServerState {
    /// Holds active and pending states for each boolean defined by policy.
    booleans: SeLinuxBooleans,

    /// Write-only interface to the data stored in the selinuxfs status file.
    status_publisher: Option<Box<dyn SeLinuxStatusPublisher>>,
}

pub(crate) struct SecurityServerBackend {
    /// Currently active policy and its per-policy SID table, read lock-free via RCU.
    active_policy: RcuOptionArc<ActivePolicy>,

    /// Canonical strings for all dynamic SIDs allocated across policy loads.
    ///
    /// Locked when minting new SIDs into `active_policy.sid_table` or replacing `active_policy`.
    portable_sids: LockDepMutex<PortableSidTable, SeLinuxSecurityServerStateLock>,
}

/// An opaque identifier for the policy version, which can be used to detect if the policy has changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PolicySeqNo(u32);

impl PolicySeqNo {
    pub const INITIAL: Self = Self(0);

    #[cfg(test)]
    pub const OTHER: Self = Self(1);
}

pub struct SecurityServer {
    /// Serializes policy loads, boolean changes, enforcing mode changes, AVC resets, and status
    /// publication.
    state: LockDepMutex<SecurityServerState, SeLinuxStatusLock>,

    /// The access vector cache that is shared between threads subject to access control by this
    /// security server.
    access_vector_cache: AccessVectorCache,

    /// A shared reference to the security server's policy and SID state.
    backend: Arc<SecurityServerBackend>,

    /// True if the security server is enforcing, rather than permissive.
    /// Only modified with the `state` lock taken.
    is_enforcing: AtomicBool,

    /// Count of changes to the active policy.  Changes include both loads
    /// of complete new policies, and modifications to a previously loaded
    /// policy, e.g. by committing new values to conditional booleans in it.
    /// Only modified with the `state` lock taken.
    policy_change_count: AtomicU32,

    /// Optional set of exceptions to apply to access checks, via `ExceptionsConfig`.
    exceptions: Vec<String>,

    /// Indicates whether `allow_unimplemented` mode is enabled.
    allow_unimplemented: bool,
}

impl SecurityServer {
    /// Returns an instance with default configuration and no exceptions.
    pub fn new_default() -> Arc<Self> {
        Self::new(String::new(), Vec::new())
    }

    /// Returns an instance with the specified options and exceptions configured.
    pub fn new(options: String, exceptions: Vec<String>) -> Arc<Self> {
        let mut allow_unimplemented = false;
        for option in options.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match option {
                "allow_unimplemented" => allow_unimplemented = true,
                _ => {}
            }
        }

        let backend = Arc::new(SecurityServerBackend {
            active_policy: RcuOptionArc::default(),
            portable_sids: LockDepMutex::new(PortableSidTable::default()),
        });

        let access_vector_cache = AccessVectorCache::new(backend.clone());

        Arc::new(Self {
            state: LockDepMutex::new(SecurityServerState::default()),
            access_vector_cache,
            backend,
            is_enforcing: AtomicBool::new(false),
            policy_change_count: AtomicU32::new(0),
            exceptions,
            allow_unimplemented,
        })
    }

    /// Converts a shared pointer to [`SecurityServer`] to a [`PermissionCheck`] without consuming
    /// the pointer.
    pub fn as_permission_check<'a>(
        self: &'a Self,
        local_cache: &'a PerThreadCache,
    ) -> PermissionCheck<'a> {
        PermissionCheck::new(self, &self.access_vector_cache, local_cache)
    }

    /// Returns the security ID mapped to `security_context`, creating it if it does not exist.
    ///
    /// All objects with the same security context will have the same SID associated.
    pub fn security_context_to_sid(
        &self,
        security_context: NullessByteStr<'_>,
    ) -> Result<SecurityId, anyhow::Error> {
        self.backend.compute_sid(|_scope, active_policy| {
            active_policy
                .parsed
                .parse_security_context(security_context)
                .map(Cow::Owned)
                .map_err(anyhow::Error::from)
        })
    }

    /// Returns the Security Context string for the requested `sid`.
    ///
    /// This is used where Contexts need to be stringified to expose to userspace, as
    /// is the case for e.g. the `/proc/*/attr/` filesystem and `security.selinux` extended
    /// attribute values, and for audit logging. If `sid` was invalidated by a policy
    /// reload then the "unlabeled" context is returned instead.
    pub fn sid_to_security_context(&self, sid: SecurityId) -> Option<Vec<u8>> {
        let scope = RcuReadScope::new();
        let active_policy = self.backend.active_policy.as_ref(&scope)?;
        let context = active_policy.sid_table.sid_to_security_context(&scope, sid);
        Some(active_policy.parsed.serialize_security_context(context))
    }

    /// Returns the Security Context for the requested `sid` with a terminating NUL.
    pub fn sid_to_security_context_with_nul(&self, sid: SecurityId) -> Option<Vec<u8>> {
        self.sid_to_security_context(sid).map(|mut context| {
            context.push(0u8);
            context
        })
    }

    /// Applies the supplied policy to the security server.
    pub fn load_policy(&self, binary_policy: Vec<u8>) -> Result<(), anyhow::Error> {
        // Parse the supplied policy, and reject the load operation if it is
        // malformed or invalid.
        let binary_policy_size = binary_policy.len();
        let policy = Policy::parse(&binary_policy)?;
        policy.validate()?;
        if !self.allow_unimplemented {
            validate_no_unsupported_policy_features_used(&policy)?;
        }

        let exceptions = self.exceptions.iter().map(String::as_str).collect::<Vec<&str>>();
        let exceptions = ExceptionsConfig::new(&policy, &exceptions)?;
        let parsed = PolicyIndex::new(policy)?;

        log_info!(
            "SELinux:  {} users, {} roles, {} types, {} bools, {} sens, {} cats",
            parsed.users().len(),
            parsed.roles().len(),
            parsed.types().primary_names_count(),
            parsed.conditional_booleans().len(),
            parsed.sensitivities().len(),
            parsed.categories().len(),
        );
        log_info!(
            "SELinux:  {} classes, {} rules",
            parsed.classes().len(),
            parsed.access_vector_rules().rules_count(),
        );

        // Replace any existing policy and push update to `state.status_publisher`.
        self.with_mut_state_and_reset_cache(|state| {
            // TODO(b/324265752): Determine whether SELinux booleans need to be retained across
            // policy (re)loads.
            state.booleans.reset(
                parsed
                    .conditional_booleans()
                    .iter()
                    // TODO(b/324392507): Relax the UTF8 requirement on policy strings.
                    .map(|boolean| {
                        (String::from_utf8(boolean.name().to_vec()).unwrap(), boolean.active())
                    })
                    .collect(),
            );

            let portable_sids = self.backend.portable_sids.lock();
            let sid_table = SidTable::new(&parsed, &portable_sids);
            self.backend.active_policy.update(Some(Arc::new(ActivePolicy {
                parsed,
                sid_table,
                exceptions,
                binary_policy_size,
            })));
        });

        Ok(())
    }

    /// Returns true if a policy has been loaded.
    pub fn has_policy(&self) -> bool {
        let scope = RcuReadScope::new();
        self.backend.active_policy.as_ref(&scope).is_some()
    }

    /// Returns the size in bytes of the active binary policy, or `None` if no policy is loaded.
    pub fn binary_policy_size(&self) -> Option<usize> {
        let scope = RcuReadScope::new();
        let active_policy = self.backend.active_policy.as_ref(&scope)?;
        Some(active_policy.binary_policy_size)
    }

    /// Returns the active policy in binary form, or `None` if no policy has yet been loaded.
    pub fn get_binary_policy(&self) -> Option<PolicyData> {
        let scope = RcuReadScope::new();
        let active_policy = self.backend.active_policy.as_ref(&scope)?;
        let mut bytes = Vec::with_capacity(active_policy.binary_policy_size);
        active_policy
            .parsed
            .serialize(&mut bytes)
            .expect("Serializing an ActivePolicy should not fail.");
        Some(Arc::from(bytes))
    }

    /// Set to enforcing mode if `enforce` is true, permissive mode otherwise.
    pub fn set_enforcing(&self, enforcing: bool) {
        self.with_mut_state_and_update_status(|_| {
            self.is_enforcing.store(enforcing, Ordering::Release);
        });
    }

    pub fn is_enforcing(&self) -> bool {
        self.is_enforcing.load(Ordering::Acquire)
    }

    /// Returns true if the policy requires unknown class / permissions to be
    /// denied. Defaults to true until a policy is loaded.
    pub fn deny_unknown(&self) -> bool {
        let scope = RcuReadScope::new();
        self.backend
            .active_policy
            .as_ref(&scope)
            .is_none_or(|p| p.parsed.handle_unknown() != HandleUnknown::Allow)
    }

    /// Returns true if the policy requires unknown class / permissions to be
    /// rejected. Defaults to false until a policy is loaded.
    pub fn reject_unknown(&self) -> bool {
        let scope = RcuReadScope::new();
        self.backend
            .active_policy
            .as_ref(&scope)
            .is_some_and(|p| p.parsed.handle_unknown() == HandleUnknown::Reject)
    }

    /// Returns the list of names of boolean conditionals defined by the
    /// loaded policy.
    pub fn conditional_booleans(&self) -> Vec<String> {
        self.state.lock().booleans.names()
    }

    /// Returns the active and pending values of a policy boolean, if it exists.
    pub fn get_boolean(&self, name: &str) -> Result<(bool, bool), ()> {
        self.state.lock().booleans.get(name)
    }

    /// Sets the pending value of a boolean, if it is defined in the policy.
    pub fn set_pending_boolean(&self, name: &str, value: bool) -> Result<(), ()> {
        self.state.lock().booleans.set_pending(name, value)
    }

    /// Commits all pending changes to conditional booleans.
    pub fn commit_pending_booleans(&self) {
        // TODO(b/324264149): Commit values into the stored policy itself.
        self.with_mut_state_and_reset_cache(|state| {
            state.booleans.commit_pending();
        });
    }

    /// Returns whether a standard policy capability is enabled in the loaded policy.
    pub fn is_policycap_enabled(&self, policy_cap: PolicyCap) -> bool {
        let scope = RcuReadScope::new();
        let Some(policy) = self.backend.active_policy.as_ref(&scope) else {
            return false;
        };
        policy.parsed.has_policycap(policy_cap)
    }

    /// Returns a snapshot of the AVC usage statistics.
    pub fn avc_cache_stats(&self) -> CacheStats {
        self.access_vector_cache.cache_stats()
    }

    /// Returns the current policy version.
    ///
    /// Uses `Ordering::Relaxed` to avoid an acquire barrier on every [`PerThreadCache`] hit;
    /// callers that reset cached state upon observing a changed [`PolicySeqNo`] execute an
    /// `Ordering::Acquire` fence before querying [`SecurityServerBackend`].
    pub fn policy_seqno(&self) -> PolicySeqNo {
        PolicySeqNo(self.policy_change_count.load(Ordering::Relaxed))
    }

    /// Returns the list of all class names.
    pub fn class_names(&self) -> Result<Vec<Vec<u8>>, ()> {
        let scope = RcuReadScope::new();
        let names = self
            .backend
            .expect_active_policy(&scope)
            .parsed
            .classes()
            .iter()
            .map(|class| class.name().to_vec())
            .collect();
        Ok(names)
    }

    /// Returns the class identifier of a class, if it exists.
    pub fn class_id_by_name(&self, name: &str) -> Result<ClassId, ()> {
        let scope = RcuReadScope::new();
        Ok(self
            .backend
            .expect_active_policy(&scope)
            .parsed
            .classes()
            .get_by_name(name.as_bytes())
            .ok_or(())?
            .id())
    }

    /// Returns the set of permissions associated with a class. Each permission
    /// is represented as a tuple of the permission ID (in the scope of its
    /// associated class) and the permission name.
    pub fn class_permissions_by_name(
        &self,
        name: &str,
    ) -> Result<Vec<(PermissionId, Vec<u8>)>, ()> {
        let scope = RcuReadScope::new();
        self.backend.expect_active_policy(&scope).parsed.find_class_permissions_by_name(name)
    }

    /// Determines the appropriate [`FileSystemLabel`] for a mounted filesystem given this security
    /// server's loaded policy, the name of the filesystem type ("ext4" or "tmpfs", for example),
    /// and the security-relevant mount options passed for the mount operation.
    pub fn resolve_fs_label(
        &self,
        fs_type: NullessByteStr<'_>,
        mount_options: &FileSystemMountOptions,
    ) -> Result<FileSystemLabel, anyhow::Error> {
        // Lock `portable_sids` upfront so that any new SIDs allocated for mount options or
        // filesystem labels are resolved against a single consistent `active_policy`.
        let mut portable_sids = self.backend.portable_sids.lock();
        let scope = RcuReadScope::new();
        let active_policy = self.backend.expect_active_policy(&scope);

        let mut resolve_sid =
            |context: Cow<'_, SecurityContext>| -> Result<SecurityId, anyhow::Error> {
                // SAFETY: `self.backend.portable_sids` is locked, ensuring exclusive writer access.
                unsafe {
                    active_policy
                        .sid_table
                        .security_context_to_sid(
                            &scope,
                            &active_policy.parsed,
                            &mut portable_sids,
                            context,
                        )
                        .map_err(anyhow::Error::from)
                }
            };

        let mut sid_from_mount_option =
            |mount_option: &Option<Vec<u8>>| -> Result<Option<SecurityId>, anyhow::Error> {
                let Some(label) = mount_option else {
                    return Ok(None);
                };
                let context = active_policy.parsed.parse_security_context(label.into())?;
                resolve_sid(Cow::Owned(context)).map(Some)
            };

        let mount_sids = FileSystemMountSids {
            context: sid_from_mount_option(&mount_options.context)?,
            fs_context: sid_from_mount_option(&mount_options.fs_context)?,
            def_context: sid_from_mount_option(&mount_options.def_context)?,
            root_context: sid_from_mount_option(&mount_options.root_context)?,
        };

        let label = if let Some(mountpoint_sid) = mount_sids.context {
            // `mount_options` has `context` set, so the file-system and the nodes it contains are
            // labeled with that value, which is not modifiable. The `fs_context` option, if set,
            // overrides the file-system label.
            FileSystemLabel {
                sid: mount_sids.fs_context.unwrap_or(mountpoint_sid),
                scheme: FileSystemLabelingScheme::Mountpoint { sid: mountpoint_sid },
                mount_sids,
            }
        } else if let Some(FsUseLabelAndType { context, use_type }) =
            active_policy.parsed.fs_use_label_and_type(fs_type)
        {
            // There is an `fs_use` statement for this file-system type in the policy.
            let fs_sid_from_policy = resolve_sid(Cow::Borrowed(context))?;
            let fs_sid = mount_sids.fs_context.unwrap_or(fs_sid_from_policy);
            FileSystemLabel {
                sid: fs_sid,
                scheme: FileSystemLabelingScheme::FsUse {
                    fs_use_type: use_type,
                    default_sid: mount_sids.def_context.unwrap_or_else(|| InitialSid::File.into()),
                },
                mount_sids,
            }
        } else if let Some(context) =
            active_policy.parsed.genfscon_label_for_fs_and_path(fs_type, ROOT_PATH.into(), None)
        {
            // There is a `genfscon` statement for this file-system type in the policy.
            let genfscon_sid = resolve_sid(Cow::Borrowed(context))?;
            let fs_sid = mount_sids.fs_context.unwrap_or(genfscon_sid);

            // For relabeling to make sense with `genfscon` labeling they must ensure to persist the
            // `FsNode` security state. That is implicitly the case for filesystems which persist all
            // `FsNode`s in-memory (independent of the `DirEntry` cache), e.g. those whose contents are
            // managed as a `SimpleDirectory` structure.
            let fs_type = fs_type.as_bytes();
            let mut supports_seclabel = matches!(fs_type, b"sysfs" | b"tracefs" | b"pstore");
            supports_seclabel |= matches!(fs_type, b"cgroup" | b"cgroup2")
                && active_policy.parsed.has_policycap(PolicyCap::CgroupSeclabel);
            supports_seclabel |= fs_type == b"functionfs"
                && active_policy.parsed.has_policycap(PolicyCap::FunctionfsSeclabel);

            FileSystemLabel {
                sid: fs_sid,
                scheme: FileSystemLabelingScheme::GenFsCon { supports_seclabel },
                mount_sids,
            }
        } else {
            // The name of the filesystem type was not recognized.
            FileSystemLabel {
                sid: mount_sids.fs_context.unwrap_or_else(|| InitialSid::Unlabeled.into()),
                scheme: FileSystemLabelingScheme::None,
                mount_sids,
            }
        };
        Ok(label)
    }

    /// Returns the [`SecurityId`] with which to label an [`FsNode`] in a filesystem of `fs_type`,
    /// at the specified filesystem-relative `node_path`.  Callers are responsible for ensuring that
    /// this API is never called prior to a policy first being loaded, or for a filesystem that is
    /// not configured to be `genfscon`-labeled.
    pub fn genfscon_label_for_fs_and_path(
        &self,
        fs_type: NullessByteStr<'_>,
        node_path: NullessByteStr<'_>,
        class_id: Option<KernelClass>,
    ) -> Result<SecurityId, anyhow::Error> {
        self.backend.compute_sid(|_scope, active_policy| {
            active_policy
                .parsed
                .genfscon_label_for_fs_and_path(fs_type, node_path.into(), class_id)
                .map(Cow::Borrowed)
                .ok_or_else(|| {
                    anyhow::anyhow!("Genfscon label requested for non-genfscon labeled filesystem")
                })
        })
    }

    /// Returns true if the `bounded_sid` is bounded by the `parent_sid`.
    /// Bounds relationships are mostly enforced by policy tooling, so this only requires validating
    /// that the policy entry for the `TypeId` of `bounded_sid` has the `TypeId` of `parent_sid`
    /// specified in its `bounds`.
    pub fn is_bounded_by(&self, bounded_sid: SecurityId, parent_sid: SecurityId) -> bool {
        let scope = RcuReadScope::new();
        let active_policy = self.backend.expect_active_policy(&scope);
        let (bounded_context, parent_context) =
            active_policy.sid_table.sids_to_security_contexts(&scope, bounded_sid, parent_sid);
        active_policy.parsed.is_bounded_by(bounded_context.type_(), parent_context.type_())
    }

    /// Assign a [`SeLinuxStatusPublisher`] to be used for pushing updates to the security server's
    /// policy status. This should be invoked exactly once when `selinuxfs` is initialized.
    ///
    /// # Panics
    ///
    /// This will panic on debug builds if it is invoked multiple times.
    pub fn set_status_publisher(&self, status_holder: Box<dyn SeLinuxStatusPublisher>) {
        self.with_mut_state_and_update_status(|state| {
            assert!(state.status_publisher.is_none());
            state.status_publisher = Some(status_holder);
        });
    }

    /// Locks the security server state for modification and calls the supplied function to update
    /// it. Once the update is complete, the configured `SeLinuxStatusPublisher` (if any) is called
    /// to update the userspace-facing "status" file to reflect the new state.
    fn with_mut_state_and_update_status(&self, f: impl FnOnce(&mut SecurityServerState)) {
        let mut locked_state = self.state.lock();
        f(locked_state.deref_mut());
        self.update_status(&mut locked_state);
    }

    /// Locks the security server state to apply a policy change via `f`, then resets the shared
    /// access vector cache before incrementing `policy_change_count` and updating the
    /// userspace-facing "status" file.
    fn with_mut_state_and_reset_cache(&self, f: impl FnOnce(&mut SecurityServerState)) {
        let mut locked_state = self.state.lock();
        f(locked_state.deref_mut());

        // Reset the shared AVC before incrementing `policy_change_count` or updating the
        // userspace-facing status file.
        self.access_vector_cache.reset();

        // Use `Ordering::Release` so that any reader observing the incremented sequence number
        // (and executing an `Ordering::Acquire` fence on `PerThreadCache` reset) is guaranteed
        // to observe both the updated `active_policy` and the reset `access_vector_cache`.
        self.policy_change_count.fetch_add(1, Ordering::Release);
        self.update_status(&mut locked_state);
    }

    fn update_status(&self, state: &mut SecurityServerState) {
        let new_value = SeLinuxStatus {
            is_enforcing: self.is_enforcing(),
            change_count: self.policy_change_count.load(Ordering::Relaxed),
            deny_unknown: self.deny_unknown(),
        };
        if let Some(status_publisher) = &mut state.status_publisher {
            status_publisher.set_status(new_value);
        }
    }

    /// Returns the security identifier ([`SecurityId`]) with which to label a new object of `target_class`,
    /// based on the specified source & target security SIDs and optional `name`.
    /// Callers pass an empty slice (`&[]`) for `name` to express nameless transitions.
    // TODO: Move this API to sit alongside the other `compute_*()` APIs.
    // TODO: https://fxbug.dev/335397745 - APIs should not mix SecurityId and (raw) ClassId.
    pub fn compute_create_sid_raw(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: ClassId,
        name: &[u8],
    ) -> Result<SecurityId, anyhow::Error> {
        self.backend.compute_create_sid_raw(source_sid, target_sid, target_class.into(), name)
    }

    /// Returns the raw `AccessDecision` for a specified source, target and class.
    // TODO: APIs should not mix SecurityId and (raw) ClassId.
    pub fn compute_access_decision_raw(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: ClassId,
    ) -> AccessDecision {
        self.backend.compute_access_decision_raw(source_sid, target_sid, target_class.into())
    }
}

impl SecurityServerBackend {
    fn expect_active_policy<'a>(&self, scope: &'a RcuReadScope) -> &'a ActivePolicy {
        self.active_policy.as_ref(scope).expect("policy should be loaded")
    }

    fn compute_create_sid_raw(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: ObjectClass,
        name: &[u8],
    ) -> Result<SecurityId, anyhow::Error> {
        self.compute_sid(|scope, active_policy| {
            let (source_context, target_context) =
                active_policy.sid_table.sids_to_security_contexts(scope, source_sid, target_sid);

            Ok(Cow::Owned(active_policy.parsed.compute_create_context(
                source_context,
                target_context,
                target_class,
                name,
            )))
        })
        .context("computing new security context from policy")
    }

    /// Helper for call-sites that need to compute a `SecurityContext` and assign a SID to it.
    fn compute_sid(
        &self,
        compute_context: impl for<'a> Fn(
            &'a RcuReadScope,
            &'a ActivePolicy,
        ) -> Result<Cow<'a, SecurityContext>, anyhow::Error>,
    ) -> Result<SecurityId, anyhow::Error> {
        // Initially assume that the computed context will most likely already have a SID assigned,
        // so that the operation can be completed via RCU without taking any lock.
        let scope = RcuReadScope::new();
        let active_policy =
            self.active_policy.as_ref(&scope).ok_or_else(|| anyhow::anyhow!("no policy loaded"))?;
        let context = compute_context(&scope, active_policy)?;
        if let Some(sid) =
            active_policy.sid_table.security_context_to_existing_sid(&scope, &context)
        {
            return Ok(sid);
        }

        // Since the computed context was not found in the table, acquire the writer lock to
        // update the SID table without blocking concurrent RCU readers. In the rare case of a
        // new policy having been loaded before the writer lock was acquired, recompute `context`
        // against the new policy state.
        let mut portable_sids = self.portable_sids.lock();
        let current_policy =
            self.active_policy.as_ref(&scope).expect("policy cannot be unloaded once loaded");
        let context = if !std::ptr::eq(active_policy, current_policy) {
            compute_context(&scope, current_policy)?
        } else {
            context
        };
        // SAFETY: `self.portable_sids` is locked, ensuring exclusive writer access to `sid_table`.
        unsafe {
            current_policy
                .sid_table
                .security_context_to_sid(
                    &scope,
                    &current_policy.parsed,
                    &mut portable_sids,
                    context,
                )
                .map_err(anyhow::Error::from)
        }
    }

    fn compute_access_decision_raw(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: ObjectClass,
    ) -> AccessDecision {
        let scope = RcuReadScope::new();
        let Some(active_policy) = self.active_policy.as_ref(&scope) else {
            // All permissions are allowed when no policy is loaded, regardless of enforcing state.
            return AccessDecision::allow(AccessVector::ALL);
        };
        active_policy.compute_access_decision_raw(&scope, source_sid, target_sid, target_class)
    }
}

impl Query for SecurityServerBackend {
    fn compute_access_decision(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: KernelClass,
    ) -> KernelAccessDecision {
        let scope = RcuReadScope::new();
        let Some(active_policy) = self.active_policy.as_ref(&scope) else {
            return KernelAccessDecision {
                allow: AccessVector::ALL,
                audit: AccessVector::NONE,
                flags: 0,
                todo_bug: None,
            };
        };
        let decision = active_policy.compute_access_decision_raw(
            &scope,
            source_sid,
            target_sid,
            target_class.into(),
        );
        active_policy.parsed.access_decision_to_kernel_access_decision(target_class, decision)
    }

    fn compute_create_sid(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: KernelClass,
        name: &[u8],
    ) -> Result<SecurityId, anyhow::Error> {
        self.compute_create_sid_raw(source_sid, target_sid, target_class.into(), name)
    }

    fn compute_xperms_access_decision(
        &self,
        xperms_kind: XpermsKind,
        source_sid: SecurityId,
        target_sid: SecurityId,
        permission: KernelPermission,
        xperms_prefix: u8,
    ) -> KernelXpermsAccessDecision {
        let scope = RcuReadScope::new();
        let Some(active_policy) = self.active_policy.as_ref(&scope) else {
            // All permissions are allowed when no policy is loaded, regardless of enforcing state.
            return KernelXpermsAccessDecision {
                allow: XpermsBitmap::ALL,
                audit: XpermsBitmap::NONE,
                permissive: false,
                has_todo: false,
            };
        };

        let (source_context, target_context) =
            active_policy.sid_table.sids_to_security_contexts(&scope, source_sid, target_sid);

        // Look up the decision for the base permission.
        let base_decision_raw = active_policy.compute_access_decision_for_contexts(
            source_context,
            target_context,
            permission.class().into(),
        );
        let base_decision = active_policy
            .parsed
            .access_decision_to_kernel_access_decision(permission.class(), base_decision_raw);
        let permission_access_vector = permission.as_access_vector();
        let base_permit =
            base_decision.allow & permission_access_vector == permission_access_vector;
        let base_audit = base_decision.audit & permission_access_vector == permission_access_vector;

        // Look up the extended permission decision.
        let xperms_decision = active_policy.parsed.compute_xperms_access_decision(
            xperms_kind,
            source_context,
            target_context,
            permission.class(),
            xperms_prefix,
        );

        // Combine the base and extended decisions.
        let allow = if !base_permit { XpermsBitmap::NONE } else { xperms_decision.allow };
        let audit = if base_audit { XpermsBitmap::ALL } else { xperms_decision.audit() };
        let permissive = (base_decision.flags & SELINUX_AVD_FLAGS_PERMISSIVE) != 0;
        let has_todo = base_decision.todo_bug.is_some();
        KernelXpermsAccessDecision { allow, audit, permissive, has_todo }
    }
}

fn check_policy_feature_is_empty<T>(name: &'static str, items: &[T]) -> Result<(), anyhow::Error> {
    if !items.is_empty() {
        anyhow::bail!("Policy contains unsupported feature: {name} ({})", items.len());
    }
    Ok(())
}

fn validate_no_unsupported_policy_features_used(policy: &Policy) -> Result<(), anyhow::Error> {
    let ocontexts = policy.object_contexts();
    check_policy_feature_is_empty("ports", ocontexts.ports())?;
    check_policy_feature_is_empty("network_interfaces", ocontexts.network_interfaces())?;
    check_policy_feature_is_empty("nodes", ocontexts.ipv4_nodes())?;
    check_policy_feature_is_empty("ipv6_nodes", ocontexts.ipv6_nodes())?;
    check_policy_feature_is_empty(
        "infiniband_partition_keys",
        ocontexts.infiniband_partition_keys(),
    )?;
    check_policy_feature_is_empty("infiniband_end_ports", ocontexts.infiniband_end_ports())?;
    check_policy_feature_is_empty("filesystems", ocontexts.filesystems())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_check::PermissionCheckResult;
    use crate::{
        CommonFsNodePermission, DirPermission, FileClass, FilePermission, ForClass, KernelClass,
        ProcessPermission,
    };
    use std::num::NonZeroU32;

    const TESTSUITE_BINARY_POLICY: &[u8] = include_bytes!("../testdata/policies/selinux_testsuite");
    const TESTS_BINARY_POLICY: &[u8] =
        include_bytes!("../testdata/micro_policies/security_server_tests_policy");
    const MINIMAL_BINARY_POLICY: &[u8] =
        include_bytes!("../testdata/composite_policies/compiled/minimal_policy");

    fn security_server_with_tests_policy() -> Arc<SecurityServer> {
        let policy_bytes = TESTS_BINARY_POLICY.to_vec();
        let security_server = SecurityServer::new_default();
        assert_eq!(
            Ok(()),
            security_server.load_policy(policy_bytes).map_err(|e| format!("{:?}", e))
        );
        security_server
    }

    #[test]
    fn test_unsupported_policy_features_check() {
        let default_server = SecurityServer::new_default();
        let testsuite_server = SecurityServer::new("allow_unimplemented".into(), vec![]);

        assert!(default_server.load_policy(MINIMAL_BINARY_POLICY.to_vec()).is_ok());
        assert!(testsuite_server.load_policy(MINIMAL_BINARY_POLICY.to_vec()).is_ok());
        assert!(testsuite_server.load_policy(TESTSUITE_BINARY_POLICY.to_vec()).is_ok());
    }

    #[test]
    fn compute_access_vector_allows_all() {
        let security_server = SecurityServer::new_default();
        let sid1 = InitialSid::Kernel.into();
        let sid2 = InitialSid::Unlabeled.into();
        assert_eq!(
            security_server
                .backend
                .compute_access_decision(sid1, sid2, KernelClass::Process.into())
                .allow,
            AccessVector::ALL
        );
    }

    #[test]
    fn loaded_policy_can_be_retrieved() {
        let security_server = security_server_with_tests_policy();
        assert_eq!(TESTS_BINARY_POLICY, security_server.get_binary_policy().unwrap().as_ref());
    }

    #[test]
    fn loaded_policy_is_validated() {
        let not_really_a_policy = "not a real policy".as_bytes().to_vec();
        let security_server = SecurityServer::new_default();
        assert!(security_server.load_policy(not_really_a_policy.clone()).is_err());
    }

    #[test]
    fn enforcing_mode_is_reported() {
        let security_server = SecurityServer::new_default();
        assert!(!security_server.is_enforcing());

        security_server.set_enforcing(true);
        assert!(security_server.is_enforcing());
    }

    #[test]
    fn without_policy_conditional_booleans_are_empty() {
        let security_server = SecurityServer::new_default();
        assert!(security_server.conditional_booleans().is_empty());
    }

    #[test]
    fn conditional_booleans_can_be_queried() {
        let policy_bytes = TESTSUITE_BINARY_POLICY.to_vec();
        let security_server = SecurityServer::new("allow_unimplemented".into(), vec![]);
        assert_eq!(
            Ok(()),
            security_server.load_policy(policy_bytes).map_err(|e| format!("{:?}", e))
        );

        let booleans = security_server.conditional_booleans();
        assert!(!booleans.is_empty());
        let boolean = booleans[0].as_str();

        assert!(security_server.get_boolean("this_is_not_a_valid_boolean_name").is_err());
        assert!(security_server.get_boolean(boolean).is_ok());
    }

    #[test]
    fn conditional_booleans_can_be_changed() {
        let policy_bytes = TESTSUITE_BINARY_POLICY.to_vec();
        let security_server = SecurityServer::new("allow_unimplemented".into(), vec![]);
        assert_eq!(
            Ok(()),
            security_server.load_policy(policy_bytes).map_err(|e| format!("{:?}", e))
        );

        let booleans = security_server.conditional_booleans();
        assert!(!booleans.is_empty());
        let boolean = booleans[0].as_str();

        let (active, pending) = security_server.get_boolean(boolean).unwrap();
        assert_eq!(active, pending, "Initially active and pending values should match");

        security_server.set_pending_boolean(boolean, !active).unwrap();
        let (active, pending) = security_server.get_boolean(boolean).unwrap();
        assert!(active != pending, "Before commit pending should differ from active");

        security_server.commit_pending_booleans();
        let (final_active, final_pending) = security_server.get_boolean(boolean).unwrap();
        assert_eq!(final_active, pending, "Pending value should be active after commit");
        assert_eq!(final_active, final_pending, "Active and pending are the same after commit");
    }

    #[test]
    fn parse_security_context_no_policy() {
        let security_server = SecurityServer::new_default();
        let error = security_server
            .security_context_to_sid(b"unconfined_u:unconfined_r:unconfined_t:s0".into())
            .expect_err("expected error");
        let error_string = format!("{:?}", error);
        assert!(error_string.contains("no policy"));
    }

    #[test]
    fn compute_sid_recomputes_when_policy_reloaded() {
        let security_server = security_server_with_tests_policy();
        let compute_calls = std::cell::Cell::new(0usize);

        let sid = security_server
            .backend
            .compute_sid(|_scope, active_policy| {
                let call_index = compute_calls.get();
                compute_calls.set(call_index + 1);
                let context = active_policy
                    .parsed
                    .parse_security_context(b"user0:object_r:type0:s0".into())
                    .map_err(anyhow::Error::from)?;
                if call_index == 0 {
                    // Simulate a concurrent policy reload completing before the writer lock is
                    // acquired on the slow path.
                    security_server
                        .load_policy(TESTS_BINARY_POLICY.to_vec())
                        .expect("policy reload succeeds");
                }
                Ok(Cow::Owned(context))
            })
            .expect("compute_sid succeeds across policy reload");

        assert_eq!(compute_calls.get(), 2);
        assert_eq!(
            security_server.sid_to_security_context(sid),
            Some(b"user0:object_r:type0:s0".to_vec())
        );
    }

    #[test]
    fn compute_create_sid_no_defaults() {
        let security_server = SecurityServer::new_default();
        let policy_bytes =
            include_bytes!("../testdata/micro_policies/file_no_defaults_policy").to_vec();
        security_server.load_policy(policy_bytes).expect("binary policy loads");

        let source_sid = security_server
            .security_context_to_sid(b"user_u:unconfined_r:unconfined_t:s0-s1".into())
            .expect("creating SID from security context should succeed");
        let target_sid = security_server
            .security_context_to_sid(b"file_u:object_r:file_t:s0".into())
            .expect("creating SID from security context should succeed");

        let computed_sid = security_server
            .as_permission_check(&Default::default())
            .compute_create_sid(source_sid, target_sid, FileClass::File.into(), &[])
            .expect("new sid computed");
        let computed_context = security_server
            .sid_to_security_context(computed_sid)
            .expect("computed sid associated with context");

        // User and low security level should be copied from the source,
        // and the role and type from the target.
        assert_eq!(computed_context, b"user_u:object_r:file_t:s0");
    }

    #[test]
    fn compute_create_sid_source_defaults() {
        let security_server = SecurityServer::new_default();
        let policy_bytes =
            include_bytes!("../testdata/micro_policies/file_source_defaults_policy").to_vec();
        security_server.load_policy(policy_bytes).expect("binary policy loads");

        let source_sid = security_server
            .security_context_to_sid(b"user_u:unconfined_r:unconfined_t:s0-s2:c0".into())
            .expect("creating SID from security context should succeed");
        let target_sid = security_server
            .security_context_to_sid(b"file_u:object_r:file_t:s1-s3:c0".into())
            .expect("creating SID from security context should succeed");

        let computed_sid = security_server
            .as_permission_check(&Default::default())
            .compute_create_sid(source_sid, target_sid, FileClass::File.into(), &[])
            .expect("new sid computed");
        let computed_context = security_server
            .sid_to_security_context(computed_sid)
            .expect("computed sid associated with context");

        // All fields should be copied from the source, but only the "low" part of the security
        // range.
        assert_eq!(computed_context, b"user_u:unconfined_r:unconfined_t:s0");
    }

    #[test]
    fn compute_create_sid_target_defaults() {
        let security_server = SecurityServer::new_default();
        let policy_bytes =
            include_bytes!("../testdata/micro_policies/file_target_defaults_policy").to_vec();
        security_server.load_policy(policy_bytes).expect("binary policy loads");

        let source_sid = security_server
            .security_context_to_sid(b"user_u:unconfined_r:unconfined_t:s0-s2:c0".into())
            .expect("creating SID from security context should succeed");
        let target_sid = security_server
            .security_context_to_sid(b"file_u:object_r:file_t:s1-s3:c0".into())
            .expect("creating SID from security context should succeed");

        let computed_sid = security_server
            .as_permission_check(&Default::default())
            .compute_create_sid(source_sid, target_sid, FileClass::File.into(), &[])
            .expect("new sid computed");
        let computed_context = security_server
            .sid_to_security_context(computed_sid)
            .expect("computed sid associated with context");

        // User, role and type copied from target, with source's low security level.
        assert_eq!(computed_context, b"file_u:object_r:file_t:s0");
    }

    #[test]
    fn compute_create_sid_range_source_low_default() {
        let security_server = SecurityServer::new_default();
        let policy_bytes =
            include_bytes!("../testdata/micro_policies/file_range_source_low_policy").to_vec();
        security_server.load_policy(policy_bytes).expect("binary policy loads");

        let source_sid = security_server
            .security_context_to_sid(b"user_u:unconfined_r:unconfined_t:s0-s1:c0".into())
            .expect("creating SID from security context should succeed");
        let target_sid = security_server
            .security_context_to_sid(b"file_u:object_r:file_t:s1".into())
            .expect("creating SID from security context should succeed");

        let computed_sid = security_server
            .as_permission_check(&Default::default())
            .compute_create_sid(source_sid, target_sid, FileClass::File.into(), &[])
            .expect("new sid computed");
        let computed_context = security_server
            .sid_to_security_context(computed_sid)
            .expect("computed sid associated with context");

        // User and low security level copied from source, role and type as default.
        assert_eq!(computed_context, b"user_u:object_r:file_t:s0");
    }

    #[test]
    fn compute_create_sid_range_source_low_high_default() {
        let security_server = SecurityServer::new_default();
        let policy_bytes =
            include_bytes!("../testdata/micro_policies/file_range_source_low_high_policy").to_vec();
        security_server.load_policy(policy_bytes).expect("binary policy loads");

        let source_sid = security_server
            .security_context_to_sid(b"user_u:unconfined_r:unconfined_t:s0-s1:c0".into())
            .expect("creating SID from security context should succeed");
        let target_sid = security_server
            .security_context_to_sid(b"file_u:object_r:file_t:s1".into())
            .expect("creating SID from security context should succeed");

        let computed_sid = security_server
            .as_permission_check(&Default::default())
            .compute_create_sid(source_sid, target_sid, FileClass::File.into(), &[])
            .expect("new sid computed");
        let computed_context = security_server
            .sid_to_security_context(computed_sid)
            .expect("computed sid associated with context");

        // User and full security range copied from source, role and type as default.
        assert_eq!(computed_context, b"user_u:object_r:file_t:s0-s1:c0");
    }

    #[test]
    fn compute_create_sid_range_source_high_default() {
        let security_server = SecurityServer::new_default();
        let policy_bytes =
            include_bytes!("../testdata/micro_policies/file_range_source_high_policy").to_vec();
        security_server.load_policy(policy_bytes).expect("binary policy loads");

        let source_sid = security_server
            .security_context_to_sid(b"user_u:unconfined_r:unconfined_t:s0-s1:c0".into())
            .expect("creating SID from security context should succeed");
        let target_sid = security_server
            .security_context_to_sid(b"file_u:object_r:file_t:s0".into())
            .expect("creating SID from security context should succeed");

        let computed_sid = security_server
            .as_permission_check(&Default::default())
            .compute_create_sid(source_sid, target_sid, FileClass::File.into(), &[])
            .expect("new sid computed");
        let computed_context = security_server
            .sid_to_security_context(computed_sid)
            .expect("computed sid associated with context");

        // User and high security level copied from source, role and type as default.
        assert_eq!(computed_context, b"user_u:object_r:file_t:s1:c0");
    }

    #[test]
    fn compute_create_sid_range_target_low_default() {
        let security_server = SecurityServer::new_default();
        let policy_bytes =
            include_bytes!("../testdata/micro_policies/file_range_target_low_policy").to_vec();
        security_server.load_policy(policy_bytes).expect("binary policy loads");

        let source_sid = security_server
            .security_context_to_sid(b"user_u:unconfined_r:unconfined_t:s1".into())
            .expect("creating SID from security context should succeed");
        let target_sid = security_server
            .security_context_to_sid(b"file_u:object_r:file_t:s0-s1:c0".into())
            .expect("creating SID from security context should succeed");

        let computed_sid = security_server
            .as_permission_check(&Default::default())
            .compute_create_sid(source_sid, target_sid, FileClass::File.into(), &[])
            .expect("new sid computed");
        let computed_context = security_server
            .sid_to_security_context(computed_sid)
            .expect("computed sid associated with context");

        // User copied from source, low security level from target, role and type as default.
        assert_eq!(computed_context, b"user_u:object_r:file_t:s0");
    }

    #[test]
    fn compute_create_sid_range_target_low_high_default() {
        let security_server = SecurityServer::new_default();
        let policy_bytes =
            include_bytes!("../testdata/micro_policies/file_range_target_low_high_policy").to_vec();
        security_server.load_policy(policy_bytes).expect("binary policy loads");

        let source_sid = security_server
            .security_context_to_sid(b"user_u:unconfined_r:unconfined_t:s1".into())
            .expect("creating SID from security context should succeed");
        let target_sid = security_server
            .security_context_to_sid(b"file_u:object_r:file_t:s0-s1:c0".into())
            .expect("creating SID from security context should succeed");

        let computed_sid = security_server
            .as_permission_check(&Default::default())
            .compute_create_sid(source_sid, target_sid, FileClass::File.into(), &[])
            .expect("new sid computed");
        let computed_context = security_server
            .sid_to_security_context(computed_sid)
            .expect("computed sid associated with context");

        // User copied from source, full security range from target, role and type as default.
        assert_eq!(computed_context, b"user_u:object_r:file_t:s0-s1:c0");
    }

    #[test]
    fn compute_create_sid_range_target_high_default() {
        let security_server = SecurityServer::new_default();
        let policy_bytes =
            include_bytes!("../testdata/micro_policies/file_range_target_high_policy").to_vec();
        security_server.load_policy(policy_bytes).expect("binary policy loads");

        let source_sid = security_server
            .security_context_to_sid(b"user_u:unconfined_r:unconfined_t:s0".into())
            .expect("creating SID from security context should succeed");
        let target_sid = security_server
            .security_context_to_sid(b"file_u:object_r:file_t:s0-s1:c0".into())
            .expect("creating SID from security context should succeed");

        let computed_sid = security_server
            .as_permission_check(&Default::default())
            .compute_create_sid(source_sid, target_sid, FileClass::File.into(), &[])
            .expect("new sid computed");
        let computed_context = security_server
            .sid_to_security_context(computed_sid)
            .expect("computed sid associated with context");

        // User copied from source, high security level from target, role and type as default.
        assert_eq!(computed_context, b"user_u:object_r:file_t:s1:c0");
    }

    #[test]
    fn compute_create_sid_with_name() {
        let security_server = SecurityServer::new_default();
        let policy_bytes =
            include_bytes!("../testdata/composite_policies/compiled/type_transition_policy")
                .to_vec();
        security_server.load_policy(policy_bytes).expect("binary policy loads");

        let source_sid = security_server
            .security_context_to_sid(b"source_u:source_r:source_t:s0".into())
            .expect("creating SID from security context should succeed");
        let target_sid = security_server
            .security_context_to_sid(b"target_u:object_r:target_t:s0".into())
            .expect("creating SID from security context should succeed");

        const SPECIAL_FILE_NAME: &[u8] = b"special_file";
        let computed_sid = security_server
            .as_permission_check(&Default::default())
            .compute_create_sid(source_sid, target_sid, FileClass::File.into(), SPECIAL_FILE_NAME)
            .expect("new sid computed");
        let computed_context = security_server
            .sid_to_security_context(computed_sid)
            .expect("computed sid associated with context");

        // New domain should be derived from the filename-specific rule.
        assert_eq!(computed_context, b"source_u:object_r:special_transition_t:s0");

        let computed_sid = security_server
            .as_permission_check(&Default::default())
            .compute_create_sid(
                source_sid,
                target_sid,
                FileClass::ChrFile.into(),
                SPECIAL_FILE_NAME,
            )
            .expect("new sid computed");
        let computed_context = security_server
            .sid_to_security_context(computed_sid)
            .expect("computed sid associated with context");

        // New domain should be copied from the target, because the class does not match either the
        // filename-specific nor generic type transition rules.
        assert_eq!(computed_context, b"source_u:object_r:target_t:s0");

        const OTHER_FILE_NAME: &[u8] = b"other_file";
        let computed_sid = security_server
            .as_permission_check(&Default::default())
            .compute_create_sid(source_sid, target_sid, FileClass::File.into(), OTHER_FILE_NAME)
            .expect("new sid computed");
        let computed_context = security_server
            .sid_to_security_context(computed_sid)
            .expect("computed sid associated with context");

        // New domain should be derived from the non-filename-specific rule, because the filename
        // does not match.
        assert_eq!(computed_context, b"source_u:object_r:transition_t:s0");
    }

    #[test]
    fn permissions_are_fresh_after_different_policy_load() {
        let minimal_bytes = MINIMAL_BINARY_POLICY.to_vec();
        let allow_fork_bytes =
            include_bytes!("../testdata/composite_policies/compiled/allow_fork_policy").to_vec();
        let context = b"source_u:object_r:source_t:s0:c0";

        let security_server = SecurityServer::new_default();
        security_server.set_enforcing(true);

        let local_cache = Default::default();
        let permission_check = security_server.as_permission_check(&local_cache);

        // Load the minimal policy and get a SID for the context.
        assert_eq!(
            Ok(()),
            security_server.load_policy(minimal_bytes).map_err(|e| format!("{:?}", e))
        );
        let sid = security_server.security_context_to_sid(context.into()).unwrap();

        // The minimal policy does not grant fork allowance.
        assert!(!permission_check.has_permission(sid, sid, ProcessPermission::Fork).granted);

        // Load a policy that does grant fork allowance.
        assert_eq!(
            Ok(()),
            security_server.load_policy(allow_fork_bytes).map_err(|e| format!("{:?}", e))
        );

        // Reuse the cache to check invalidation.
        let permission_check = security_server.as_permission_check(&local_cache);

        // The now-loaded "allow_fork" policy allows the context represented by `sid` to fork.
        assert!(permission_check.has_permission(sid, sid, ProcessPermission::Fork).granted);
    }

    #[test]
    fn unknown_sids_are_effectively_unlabeled() {
        let with_unlabeled_access_domain_policy_bytes = include_bytes!(
            "../testdata/composite_policies/compiled/with_unlabeled_access_domain_policy"
        )
        .to_vec();
        let with_additional_domain_policy_bytes =
            include_bytes!("../testdata/composite_policies/compiled/with_additional_domain_policy")
                .to_vec();
        let allowed_type_context = b"source_u:object_r:allowed_t:s0:c0";
        let additional_type_context = b"source_u:object_r:additional_t:s0:c0";

        let security_server = SecurityServer::new_default();
        security_server.set_enforcing(true);

        // Load a policy, get a SID for a context that is valid for that policy, and verify
        // that a context that is not valid for that policy is not issued a SID.
        assert_eq!(
            Ok(()),
            security_server
                .load_policy(with_unlabeled_access_domain_policy_bytes.clone())
                .map_err(|e| format!("{:?}", e))
        );
        let allowed_type_sid =
            security_server.security_context_to_sid(allowed_type_context.into()).unwrap();
        assert!(security_server.security_context_to_sid(additional_type_context.into()).is_err());

        // Load the policy that makes the second context valid, and verify that it is valid, and
        // verify that the first context remains valid (and unchanged).
        assert_eq!(
            Ok(()),
            security_server
                .load_policy(with_additional_domain_policy_bytes.clone())
                .map_err(|e| format!("{:?}", e))
        );
        let additional_type_sid =
            security_server.security_context_to_sid(additional_type_context.into()).unwrap();
        assert_eq!(
            allowed_type_sid,
            security_server.security_context_to_sid(allowed_type_context.into()).unwrap()
        );

        let local_cache = Default::default();
        let permission_check = security_server.as_permission_check(&local_cache);

        // "allowed_t" is allowed the process getsched capability to "unlabeled_t" - but since
        // the currently-loaded policy defines "additional_t", the SID for "additional_t" does
        // not get treated as effectively unlabeled, and these permission checks are denied.
        assert!(
            !permission_check
                .has_permission(additional_type_sid, allowed_type_sid, ProcessPermission::GetSched)
                .granted
        );
        assert!(
            !permission_check
                .has_permission(additional_type_sid, allowed_type_sid, ProcessPermission::SetSched)
                .granted
        );
        assert!(
            !permission_check
                .has_permission(allowed_type_sid, additional_type_sid, ProcessPermission::GetSched)
                .granted
        );
        assert!(
            !permission_check
                .has_permission(allowed_type_sid, additional_type_sid, ProcessPermission::SetSched)
                .granted
        );

        // We now flip back to the policy that does not recognize "additional_t"...
        assert_eq!(
            Ok(()),
            security_server
                .load_policy(with_unlabeled_access_domain_policy_bytes)
                .map_err(|e| format!("{:?}", e))
        );

        // Reuse the cache to check invalidation.
        let permission_check = security_server.as_permission_check(&local_cache);

        // The now-loaded policy allows "allowed_t" the process getsched capability
        // to "unlabeled_t" and since the now-loaded policy does not recognize "additional_t",
        // "allowed_t" is now allowed the process getsched capability to "additional_t".
        assert!(
            permission_check
                .has_permission(allowed_type_sid, additional_type_sid, ProcessPermission::GetSched)
                .granted
        );
        assert!(
            !permission_check
                .has_permission(allowed_type_sid, additional_type_sid, ProcessPermission::SetSched)
                .granted
        );

        // ... and the now-loaded policy also allows "unlabeled_t" the process
        // setsched capability to "allowed_t" and since the now-loaded policy does not recognize
        // "additional_t", "unlabeled_t" is now allowed the process setsched capability to
        // "allowed_t".
        assert!(
            !permission_check
                .has_permission(additional_type_sid, allowed_type_sid, ProcessPermission::GetSched)
                .granted
        );
        assert!(
            permission_check
                .has_permission(additional_type_sid, allowed_type_sid, ProcessPermission::SetSched)
                .granted
        );

        // When "additional_t" is unrecognized, looking up its context string returns
        // the effective (unlabeled) context.
        let unlabeled_context =
            security_server.sid_to_security_context(InitialSid::Unlabeled.into()).unwrap();
        assert_eq!(
            unlabeled_context,
            security_server.sid_to_security_context(additional_type_sid).unwrap()
        );

        // ... but if we flip forward to the policy that recognizes "additional_t", then we see
        // the serialization succeed and return the original context string.
        assert_eq!(
            Ok(()),
            security_server
                .load_policy(with_additional_domain_policy_bytes)
                .map_err(|e| format!("{:?}", e))
        );
        assert_eq!(
            additional_type_context.to_vec(),
            security_server.sid_to_security_context(additional_type_sid).unwrap()
        );
    }

    #[test]
    fn permission_check_permissive() {
        let security_server = security_server_with_tests_policy();
        security_server.set_enforcing(false);
        assert!(!security_server.is_enforcing());

        let sid =
            security_server.security_context_to_sid("user0:object_r:type0:s0".into()).unwrap();
        let local_cache = Default::default();
        let permission_check = security_server.as_permission_check(&local_cache);

        // Test policy grants "type0" the process-fork permission to itself.
        // Since the permission is granted by policy, the check will not be audit logged.
        assert_eq!(
            permission_check.has_permission(sid, sid, ProcessPermission::Fork),
            PermissionCheckResult {
                granted: true,
                audit: false,
                permissive: false,
                todo_bug: None
            }
        );

        // Test policy does not grant "type0" the process-getrlimit permission to itself, but
        // the security server is configured to be permissive. Because the permission was not
        // granted by the policy, the check will be audit logged.
        let result = permission_check.has_permission(sid, sid, ProcessPermission::GetRlimit);
        assert_eq!(
            result,
            PermissionCheckResult { granted: false, audit: true, permissive: true, todo_bug: None }
        );
        assert!(result.permit());

        // Test policy is built with "deny unknown" behaviour, and has no "blk_file" class defined.
        // This permission should be treated like a defined permission that is not allowed to the
        // source, and both allowed and audited here.
        let result = permission_check.has_permission(
            sid,
            sid,
            CommonFsNodePermission::GetAttr.for_class(FileClass::BlkFile),
        );
        assert_eq!(
            result,
            PermissionCheckResult { granted: false, audit: true, permissive: true, todo_bug: None }
        );
        assert!(result.permit());
    }

    #[test]
    fn permission_check_enforcing() {
        let security_server = security_server_with_tests_policy();
        security_server.set_enforcing(true);
        assert!(security_server.is_enforcing());

        let sid =
            security_server.security_context_to_sid("user0:object_r:type0:s0".into()).unwrap();
        let local_cache = Default::default();
        let permission_check = security_server.as_permission_check(&local_cache);

        // Test policy grants "type0" the process-fork permission to itself.
        let result = permission_check.has_permission(sid, sid, ProcessPermission::Fork);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: false,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(result.permit());

        // Test policy does not grant "type0" the process-getrlimit permission to itself.
        // Permission denials are audit logged in enforcing mode.
        let result = permission_check.has_permission(sid, sid, ProcessPermission::GetRlimit);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());

        // Test policy is built with "deny unknown" behaviour, and has no "blk_file" class defined.
        // This permission should therefore be denied, and the denial audited.
        let result = permission_check.has_permission(
            sid,
            sid,
            CommonFsNodePermission::GetAttr.for_class(FileClass::BlkFile),
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());
    }

    #[test]
    fn permissive_domain() {
        let security_server = security_server_with_tests_policy();
        security_server.set_enforcing(true);
        assert!(security_server.is_enforcing());

        let permissive_sid = security_server
            .security_context_to_sid("user0:object_r:permissive_t:s0".into())
            .unwrap();
        let non_permissive_sid = security_server
            .security_context_to_sid("user0:object_r:non_permissive_t:s0".into())
            .unwrap();

        let local_cache = Default::default();
        let permission_check = security_server.as_permission_check(&local_cache);

        // Test policy grants process-getsched permission to both of the test domains.
        let result = permission_check.has_permission(
            permissive_sid,
            permissive_sid,
            ProcessPermission::GetSched,
        );
        assert_eq!(
            result,
            PermissionCheckResult { granted: true, audit: false, permissive: true, todo_bug: None }
        );
        assert!(result.permit());
        let result = permission_check.has_permission(
            non_permissive_sid,
            non_permissive_sid,
            ProcessPermission::GetSched,
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: false,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(result.permit());

        // Test policy does not grant process-getsched permission to the test domains on one another.
        // The permissive domain will be granted the permission, since it is marked permissive.
        let result = permission_check.has_permission(
            permissive_sid,
            non_permissive_sid,
            ProcessPermission::GetSched,
        );
        assert_eq!(
            result,
            PermissionCheckResult { granted: false, audit: true, permissive: true, todo_bug: None }
        );
        assert!(result.permit());
        let result = permission_check.has_permission(
            non_permissive_sid,
            permissive_sid,
            ProcessPermission::GetSched,
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());

        // Test policy has "deny unknown" behaviour and does not define the "blk_file" class, so
        // access to a permission on it will depend on whether the source is permissive.
        // The target domain is irrelevant, since the class/permission do not exist, so the non-
        // permissive SID is used for both checks.
        let result = permission_check.has_permission(
            permissive_sid,
            non_permissive_sid,
            CommonFsNodePermission::GetAttr.for_class(FileClass::BlkFile),
        );
        assert_eq!(
            result,
            PermissionCheckResult { granted: false, audit: true, permissive: true, todo_bug: None }
        );
        assert!(result.permit());
        let result = permission_check.has_permission(
            non_permissive_sid,
            non_permissive_sid,
            CommonFsNodePermission::GetAttr.for_class(FileClass::BlkFile),
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());
    }

    #[test]
    fn auditallow_and_dontaudit() {
        let security_server = security_server_with_tests_policy();
        security_server.set_enforcing(true);
        assert!(security_server.is_enforcing());

        let audit_sid = security_server
            .security_context_to_sid("user0:object_r:test_audit_t:s0".into())
            .unwrap();

        let local_cache = Default::default();
        let permission_check = security_server.as_permission_check(&local_cache);

        // Test policy grants the domain self-fork permission, and marks it audit-allow.
        let result = permission_check.has_permission(audit_sid, audit_sid, ProcessPermission::Fork);
        assert_eq!(
            result,
            PermissionCheckResult { granted: true, audit: true, permissive: false, todo_bug: None }
        );
        assert!(result.permit());

        // Self-setsched permission is granted, and marked dont-audit, which takes no effect.
        let result =
            permission_check.has_permission(audit_sid, audit_sid, ProcessPermission::SetSched);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: false,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(result.permit());

        // Self-getsched permission is denied, but marked dont-audit.
        let result =
            permission_check.has_permission(audit_sid, audit_sid, ProcessPermission::GetSched);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: false,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());

        // Self-getpgid permission is denied, with neither audit-allow nor dont-audit.
        let result =
            permission_check.has_permission(audit_sid, audit_sid, ProcessPermission::GetPgid);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());
    }

    #[test]
    fn access_checks_with_exceptions_config() {
        const EXCEPTIONS_CONFIG: &[&str] = &[
            // These statement should all be resolved.
            "todo_deny b/001 test_exception_source_t test_exception_target_t file",
            "todo_deny b/002 test_exception_other_t test_exception_target_t chr_file",
            "todo_deny b/003 test_exception_source_t test_exception_other_t anon_inode",
            "todo_deny b/004 test_exception_permissive_t test_exception_target_t file",
            "todo_permissive b/005 test_exception_todo_permissive_t",
            // These statements should not be resolved.
            "todo_deny b/101 test_undefined_source_t test_exception_target_t file",
            "todo_deny b/102 test_exception_source_t test_undefined_target_t file",
            "todo_permissive b/103 test_undefined_source_t",
        ];
        let exceptions_config = EXCEPTIONS_CONFIG.iter().map(|x| String::from(*x)).collect();
        let security_server = SecurityServer::new(String::new(), exceptions_config);
        security_server.set_enforcing(true);

        const EXCEPTIONS_POLICY: &[u8] =
            include_bytes!("../testdata/composite_policies/compiled/exceptions_config_policy");
        assert!(security_server.load_policy(EXCEPTIONS_POLICY.into()).is_ok());

        let source_sid = security_server
            .security_context_to_sid("test_exception_u:object_r:test_exception_source_t:s0".into())
            .unwrap();
        let target_sid = security_server
            .security_context_to_sid("test_exception_u:object_r:test_exception_target_t:s0".into())
            .unwrap();
        let other_sid = security_server
            .security_context_to_sid("test_exception_u:object_r:test_exception_other_t:s0".into())
            .unwrap();
        let permissive_sid = security_server
            .security_context_to_sid(
                "test_exception_u:object_r:test_exception_permissive_t:s0".into(),
            )
            .unwrap();
        let unmatched_sid = security_server
            .security_context_to_sid(
                "test_exception_u:object_r:test_exception_unmatched_t:s0".into(),
            )
            .unwrap();
        let todo_permissive_sid = security_server
            .security_context_to_sid(
                "test_exception_u:object_r:test_exception_todo_permissive_t:s0".into(),
            )
            .unwrap();

        let local_cache = Default::default();
        let permission_check = security_server.as_permission_check(&local_cache);

        // Source SID has no "process" permissions to target SID, and no exceptions.
        let result =
            permission_check.has_permission(source_sid, target_sid, ProcessPermission::GetPgid);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());

        // Source SID has no "file:entrypoint" permission to target SID, but there is an exception defined.
        let result =
            permission_check.has_permission(source_sid, target_sid, FilePermission::Entrypoint);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: true,
                permissive: false,
                todo_bug: Some(NonZeroU32::new(1).unwrap())
            }
        );
        assert!(result.permit());

        // Source SID has "file:execute_no_trans" permission to target SID.
        let result =
            permission_check.has_permission(source_sid, target_sid, FilePermission::ExecuteNoTrans);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: false,
                permissive: false,
                todo_bug: None,
            }
        );
        assert!(result.permit());

        // Other SID has no "file:entrypoint" permissions to target SID, and the exception does not match "file" class.
        let result =
            permission_check.has_permission(other_sid, target_sid, FilePermission::Entrypoint);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());

        // Other SID has no "chr_file" permissions to target SID, but there is an exception defined.
        let result = permission_check.has_permission(
            other_sid,
            target_sid,
            CommonFsNodePermission::Read.for_class(FileClass::ChrFile),
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: true,
                permissive: false,
                todo_bug: Some(NonZeroU32::new(2).unwrap())
            }
        );
        assert!(result.permit());

        // Source SID has no "file:entrypoint" permissions to unmatched SID, and no exception is defined.
        let result =
            permission_check.has_permission(source_sid, unmatched_sid, FilePermission::Entrypoint);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());

        // Unmatched SID has no "file:entrypoint" permissions to target SID, and no exception is defined.
        let result =
            permission_check.has_permission(unmatched_sid, target_sid, FilePermission::Entrypoint);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());

        // Todo-deny exceptions are processed before the permissive bit is handled.
        let result =
            permission_check.has_permission(permissive_sid, target_sid, FilePermission::Entrypoint);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: true,
                permissive: true,
                todo_bug: Some(NonZeroU32::new(4).unwrap())
            }
        );
        assert!(result.permit());

        // Todo-permissive SID is not granted any permissions, so all permissions should be granted,
        // to all target domains and classes, and all grants should be associated with the bug.
        let result = permission_check.has_permission(
            todo_permissive_sid,
            target_sid,
            FilePermission::Entrypoint,
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: true,
                permissive: false,
                todo_bug: Some(NonZeroU32::new(5).unwrap())
            }
        );
        assert!(result.permit());
        let result = permission_check.has_permission(
            todo_permissive_sid,
            todo_permissive_sid,
            FilePermission::Entrypoint,
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: true,
                permissive: false,
                todo_bug: Some(NonZeroU32::new(5).unwrap())
            }
        );
        assert!(result.permit());
        let result = permission_check.has_permission(
            todo_permissive_sid,
            target_sid,
            FilePermission::Entrypoint,
        );
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: true,
                permissive: false,
                todo_bug: Some(NonZeroU32::new(5).unwrap())
            }
        );
        assert!(result.permit());
    }

    #[test]
    fn handle_unknown() {
        let security_server = security_server_with_tests_policy();

        let sid = security_server
            .security_context_to_sid("user0:object_r:type0:s0".into())
            .expect("Resolve Context to SID");

        // Load a policy that is missing some elements, and marked handle_unknown=reject.
        // The policy should be rejected, since not all classes/permissions are defined.
        // Rejecting policy is not controlled by permissive vs enforcing.
        const REJECT_POLICY: &[u8] =
            include_bytes!("../testdata/composite_policies/compiled/handle_unknown_policy-reject");
        assert!(security_server.load_policy(REJECT_POLICY.to_vec()).is_err());

        security_server.set_enforcing(true);

        // Load a policy that is missing some elements, and marked handle_unknown=deny.
        const DENY_POLICY: &[u8] =
            include_bytes!("../testdata/composite_policies/compiled/handle_unknown_policy-deny");
        assert!(security_server.load_policy(DENY_POLICY.to_vec()).is_ok());
        let local_cache = Default::default();
        let permission_check = security_server.as_permission_check(&local_cache);

        // Check against undefined classes or permissions should deny access and audit.
        let result = permission_check.has_permission(sid, sid, ProcessPermission::GetSched);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());
        let result = permission_check.has_permission(sid, sid, DirPermission::AddName);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());

        // Check that permissions that are defined are unaffected by handle-unknown.
        let result = permission_check.has_permission(sid, sid, DirPermission::Search);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: false,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(result.permit());
        let result = permission_check.has_permission(sid, sid, DirPermission::Reparent);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());

        // Load a policy that is missing some elements, and marked handle_unknown=allow.
        const ALLOW_POLICY: &[u8] =
            include_bytes!("../testdata/composite_policies/compiled/handle_unknown_policy-allow");
        assert!(security_server.load_policy(ALLOW_POLICY.to_vec()).is_ok());
        let local_cache2 = Default::default();
        let permission_check = security_server.as_permission_check(&local_cache2);

        // Check against undefined classes or permissions should grant access without audit.
        let result = permission_check.has_permission(sid, sid, ProcessPermission::GetSched);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: false,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(result.permit());
        let result = permission_check.has_permission(sid, sid, DirPermission::AddName);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: false,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(result.permit());

        // Check that permissions that are defined are unaffected by handle-unknown.
        let result = permission_check.has_permission(sid, sid, DirPermission::Search);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: true,
                audit: false,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(result.permit());

        let result = permission_check.has_permission(sid, sid, DirPermission::Reparent);
        assert_eq!(
            result,
            PermissionCheckResult {
                granted: false,
                audit: true,
                permissive: false,
                todo_bug: None
            }
        );
        assert!(!result.permit());
    }
}
