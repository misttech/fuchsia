// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::policy::{Policy, SecurityContext, SecurityContextError};
use crate::{InitialSid, ReferenceInitialSid, SecurityId};

use fuchsia_rcu::{RcuDroppable, RcuReadScope};
use fuchsia_rcu_collections::rcu_array::RcuArray;
use fuchsia_rcu_collections::rcu_raw_hash_map::RcuRawHashMap;
use std::borrow::Cow;
use std::num::NonZeroU32;

// SAFETY: `SecurityContext` contains only plain data and heap-allocated slices that are safe to
// drop asynchronously on the RCU reclamation thread when [`SidTable`] is resized or replaced.
unsafe impl RcuDroppable for SecurityContext {}

/// Policy-independent table mapping dynamic [`SecurityId`]s to their canonical security context
/// byte strings across policy reloads.
///
/// Dynamic [`SecurityId`]s (those with values `>= ReferenceInitialSid::FirstUnused`) are minted
/// monotonically and never reused. Storing the canonical string representation separately from
/// the per-policy [`SidTable`] allows SIDs that become invalid after a policy reload to be
/// re-bound to their original security context if a subsequent policy reload makes that context
/// valid again, without keeping invalid string entries in the hot per-policy lookup table.
///
/// This table is used only for minting dynamic [`SecurityId`]s from security contexts and
/// re-populating a new per-policy [`SidTable`] on policy reload; `SID -> SecurityContext` string
/// queries serialize the effective [`SecurityContext`] from the active policy's [`SidTable`].
#[derive(Default)]
pub(super) struct PortableSidTable {
    /// Canonical security context strings for dynamic SIDs, indexed by
    /// `sid.0.get() as usize - ReferenceInitialSid::FirstUnused as usize`.
    entries: Vec<Box<[u8]>>,
}

impl PortableSidTable {
    /// Allocates the next sequential dynamic [`SecurityId`] for `context_string`.
    fn allocate_sid(&mut self, context_string: Vec<u8>) -> SecurityId {
        let sid_index = ReferenceInitialSid::FirstUnused as usize + self.entries.len();
        let sid = SecurityId(NonZeroU32::new(sid_index as u32).unwrap());
        self.entries.push(context_string.into_boxed_slice());
        sid
    }
}

/// Per-policy RCU-protected mapping between [`SecurityId`]s and effective [`SecurityContext`]s.
///
/// Readers access [`SidTable::sid_to_security_context`],
/// [`SidTable::sids_to_security_contexts`], and [`SidTable::security_context_to_existing_sid`]
/// lock-free within an [`RcuReadScope`]. Mutations via [`SidTable::security_context_to_sid`] are
/// serialized by the caller's writer lock without blocking concurrent RCU readers.
#[derive(RcuDroppable)]
pub(super) struct SidTable {
    /// Lock-free forward mapping from [`SecurityId`] (`sid.0.get() as usize`) to its effective
    /// [`SecurityContext`] under the owning policy.
    ///
    /// Slots for unused initial SIDs or dynamic SIDs whose context strings are invalid under
    /// the policy are populated with the [`InitialSid::Unlabeled`] context.
    entries: RcuArray<SecurityContext>,

    /// Lock-free reverse mapping from validated [`SecurityContext`] to dynamic [`SecurityId`].
    context_to_sid: RcuRawHashMap<SecurityContext, SecurityId>,
}

impl SidTable {
    /// Constructs a new [`SidTable`] for `policy`, populating initial SIDs and remapping any
    /// previously allocated dynamic SIDs from `portable_sids` that are valid under `policy`.
    pub(super) fn new(policy: &Policy, portable_sids: &PortableSidTable) -> Self {
        let num_entries = ReferenceInitialSid::FirstUnused as usize + portable_sids.entries.len();
        let unlabeled_context = policy.initial_context(InitialSid::Unlabeled).clone();
        let mut entries = vec![unlabeled_context; num_entries];

        for initial_sid in InitialSid::all_variants() {
            entries[*initial_sid as usize] = policy.initial_context(*initial_sid).clone();
        }

        let context_to_sid = RcuRawHashMap::with_capacity(portable_sids.entries.len());
        let scope = RcuReadScope::new();
        for (offset, context_string) in portable_sids.entries.iter().enumerate() {
            if let Ok(context) = policy.parse_security_context(context_string.as_ref().into())
                && policy.validate_security_context(&context).is_ok()
            {
                let sid_index = ReferenceInitialSid::FirstUnused as usize + offset;
                let sid = SecurityId(NonZeroU32::new(sid_index as u32).unwrap());
                entries[sid_index] = context.clone();
                if context_to_sid.get(&scope, &context).is_none() {
                    // SAFETY: `context_to_sid` is exclusively owned during construction.
                    unsafe {
                        context_to_sid.insert(&scope, context, sid);
                    }
                }
            }
        }

        Self { entries: RcuArray::from(entries), context_to_sid }
    }

    /// Returns the [`SecurityId`] representing the supplied `security_context`, if it is
    /// already present in the SID table. If a SID is returned then `security_context` must be valid
    /// by definition, since [`SidTable::new`] and [`SidTable::security_context_to_sid`] only insert
    /// validated contexts.
    pub(super) fn security_context_to_existing_sid(
        &self,
        scope: &RcuReadScope,
        security_context: &SecurityContext,
    ) -> Option<SecurityId> {
        self.context_to_sid.get(scope, security_context).copied()
    }

    /// Returns a [`SecurityId`] representing the supplied `security_context`, inserting it into
    /// both `portable_sids` and this [`SidTable`] if not already present.
    ///
    /// If an entry already exists that matches `security_context` then the existing SID is
    /// returned. Otherwise the `security_context`'s fields are validated against `policy`
    /// constraints, and an explanatory error returned if they are inconsistent.
    ///
    /// # Safety
    ///
    /// Caller must hold the exclusive writer lock to serialize mutations to this [`SidTable`].
    pub(super) unsafe fn security_context_to_sid(
        &self,
        scope: &RcuReadScope,
        policy: &Policy,
        portable_sids: &mut PortableSidTable,
        security_context: Cow<'_, SecurityContext>,
    ) -> Result<SecurityId, SecurityContextError> {
        if let Some(sid) = self.security_context_to_existing_sid(scope, &security_context) {
            return Ok(sid);
        }
        policy.validate_security_context(&security_context)?;

        let serialized = policy.serialize_security_context(&security_context);
        let sid = portable_sids.allocate_sid(serialized);
        let sid_index = sid.0.get() as usize;

        // Publish the updated forward lookup array containing `security_context` at `sid_index`
        // BEFORE publishing `sid` into `self.context_to_sid`. By Release-Acquire transitivity, any
        // concurrent RCU reader that observes `sid` in `self.context_to_sid` is guaranteed to
        // observe the updated `self.entries` slice containing `sid_index` without needing a
        // per-slot atomic load.
        let current_entries = self.entries.as_slice(scope);
        assert_eq!(sid_index, current_entries.len());
        let mut new_entries = Vec::with_capacity(current_entries.len() + 1);
        new_entries.extend_from_slice(current_entries);
        let security_context = security_context.into_owned();
        new_entries.push(security_context.clone());
        self.entries.update(new_entries);

        // SAFETY: Caller guarantees exclusive writer access to this `SidTable`.
        unsafe {
            self.context_to_sid.insert(scope, security_context, sid);
        }

        Ok(sid)
    }

    #[inline]
    fn resolve_from_slice(entries: &[SecurityContext], sid: SecurityId) -> &SecurityContext {
        entries.get(sid.0.get() as usize).unwrap_or(&entries[InitialSid::Unlabeled as usize])
    }

    /// Returns the [`SecurityContext`] associated with `sid`.
    ///
    /// If `sid` was invalidated by a policy reload then the "unlabeled"
    /// context is returned instead.
    #[inline]
    pub(super) fn sid_to_security_context<'a>(
        &self,
        scope: &'a RcuReadScope,
        sid: SecurityId,
    ) -> &'a SecurityContext {
        let entries = self.entries.as_slice(scope);
        Self::resolve_from_slice(entries, sid)
    }

    /// Returns the [`SecurityContext`]s associated with `source_sid` and `target_sid` using a
    /// single RCU array load.
    ///
    /// If either SID was invalidated by a policy reload then the "unlabeled" context is returned
    /// for that SID instead.
    #[inline]
    pub(super) fn sids_to_security_contexts<'a>(
        &self,
        scope: &'a RcuReadScope,
        source_sid: SecurityId,
        target_sid: SecurityId,
    ) -> (&'a SecurityContext, &'a SecurityContext) {
        let entries = self.entries.as_slice(scope);
        (
            Self::resolve_from_slice(entries, source_sid),
            Self::resolve_from_slice(entries, target_sid),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TESTS_BINARY_POLICY: &[u8] =
        include_bytes!("../testdata/micro_policies/security_server_tests_policy");

    fn test_policy() -> Policy {
        let policy = Policy::parse(TESTS_BINARY_POLICY).unwrap();
        policy.validate().unwrap();
        policy
    }

    #[test]
    fn sid_to_security_context() {
        let policy = test_policy();
        let security_context = policy
            .parse_security_context(b"unconfined_u:unconfined_r:unconfined_t:s0".into())
            .unwrap();
        let mut portable_sids = PortableSidTable::default();
        let sid_table = SidTable::new(&policy, &portable_sids);
        let scope = RcuReadScope::new();
        // SAFETY: Single-threaded test with exclusive access to `sid_table`.
        let sid = unsafe {
            sid_table.security_context_to_sid(
                &scope,
                &policy,
                &mut portable_sids,
                Cow::Borrowed(&security_context),
            )
        }
        .unwrap();
        assert_eq!(*sid_table.sid_to_security_context(&scope, sid), security_context);
    }

    #[test]
    fn sids_for_different_security_contexts_differ() {
        let policy = test_policy();
        let mut portable_sids = PortableSidTable::default();
        let sid_table = SidTable::new(&policy, &portable_sids);
        let scope = RcuReadScope::new();
        // SAFETY: Single-threaded test with exclusive access to `sid_table`.
        let sid1 = unsafe {
            sid_table.security_context_to_sid(
                &scope,
                &policy,
                &mut portable_sids,
                Cow::Owned(
                    policy.parse_security_context(b"user0:object_r:type0:s0".into()).unwrap(),
                ),
            )
        };
        // SAFETY: Single-threaded test with exclusive access to `sid_table`.
        let sid2 = unsafe {
            sid_table.security_context_to_sid(
                &scope,
                &policy,
                &mut portable_sids,
                Cow::Owned(
                    policy
                        .parse_security_context(b"unconfined_u:unconfined_r:unconfined_t:s0".into())
                        .unwrap(),
                ),
            )
        };
        assert_ne!(sid1, sid2);
    }

    #[test]
    fn sids_for_same_security_context_are_equal() {
        let policy = test_policy();
        let security_context = policy
            .parse_security_context(b"unconfined_u:unconfined_r:unconfined_t:s0".into())
            .unwrap();
        let redundant_range_context = policy
            .parse_security_context(b"unconfined_u:unconfined_r:unconfined_t:s0-s0".into())
            .unwrap();
        let mut portable_sids = PortableSidTable::default();
        let sid_table = SidTable::new(&policy, &portable_sids);
        let scope = RcuReadScope::new();
        assert_eq!(portable_sids.entries.len(), 0);
        // SAFETY: Single-threaded test with exclusive access to `sid_table`.
        let sid1 = unsafe {
            sid_table.security_context_to_sid(
                &scope,
                &policy,
                &mut portable_sids,
                Cow::Borrowed(&security_context),
            )
        };
        // SAFETY: Single-threaded test with exclusive access to `sid_table`.
        let sid2 = unsafe {
            sid_table.security_context_to_sid(
                &scope,
                &policy,
                &mut portable_sids,
                Cow::Borrowed(&redundant_range_context),
            )
        };
        assert_eq!(sid1, sid2);
        assert_eq!(portable_sids.entries.len(), 1);

        // Verify that differently formatted but equivalent category spans also hash and compare
        // equal in `SidTable`.
        let canonical_categories_context =
            policy.parse_security_context(b"user1:object_r:type0:s1:c0,c2,c3".into()).unwrap();
        let unnormalized_categories_context =
            policy.parse_security_context(b"user1:object_r:type0:s1:c2.c3,c0".into()).unwrap();
        // SAFETY: Single-threaded test with exclusive access to `sid_table`.
        let categories_sid1 = unsafe {
            sid_table.security_context_to_sid(
                &scope,
                &policy,
                &mut portable_sids,
                Cow::Borrowed(&canonical_categories_context),
            )
        }
        .unwrap();
        // SAFETY: Single-threaded test with exclusive access to `sid_table`.
        let categories_sid2 = unsafe {
            sid_table.security_context_to_sid(
                &scope,
                &policy,
                &mut portable_sids,
                Cow::Borrowed(&unnormalized_categories_context),
            )
        }
        .unwrap();
        assert_eq!(categories_sid1, categories_sid2);
        assert_eq!(portable_sids.entries.len(), 2);
    }

    #[test]
    fn sid_table_grows_and_rehashes_beyond_initial_capacity() {
        let policy = test_policy();
        let mut portable_sids = PortableSidTable::default();
        let sid_table = SidTable::new(&policy, &portable_sids);
        let scope = RcuReadScope::new();
        let mut expected = Vec::new();

        // Insert more than 32 distinct security contexts to exercise `RcuRawHashMap` capacity
        // growth/rehashing and `RcuArray` reallocation.
        let types = ["type0", "security_t", "unlabeled_t", "file_fallback_t"];
        let categories =
            ["c0", "c1", "c2", "c3", "c4", "c0,c1", "c0,c2", "c0,c3", "c0,c4", "c1,c2"];
        for type_name in types {
            for category in categories {
                let context_str = format!("user1:object_r:{type_name}:s0-s1:{category}");
                let context = policy.parse_security_context(context_str.as_bytes().into()).unwrap();
                // SAFETY: Single-threaded test with exclusive access to `sid_table`.
                let sid = unsafe {
                    sid_table.security_context_to_sid(
                        &scope,
                        &policy,
                        &mut portable_sids,
                        Cow::Borrowed(&context),
                    )
                }
                .unwrap();
                expected.push((context, sid));
            }
        }
        assert_eq!(expected.len(), 40);
        assert_eq!(portable_sids.entries.len(), 40);

        for (context, sid) in &expected {
            assert_eq!(sid_table.sid_to_security_context(&scope, *sid), context);
            assert_eq!(sid_table.security_context_to_existing_sid(&scope, context), Some(*sid));
        }
    }

    #[test]
    fn sids_allocated_outside_initial_range() {
        let policy = test_policy();
        let security_context = policy
            .parse_security_context(b"unconfined_u:unconfined_r:unconfined_t:s0".into())
            .unwrap();
        let mut portable_sids = PortableSidTable::default();
        let sid_table = SidTable::new(&policy, &portable_sids);
        let scope = RcuReadScope::new();
        assert_eq!(portable_sids.entries.len(), 0);
        // SAFETY: Single-threaded test with exclusive access to `sid_table`.
        let sid = unsafe {
            sid_table.security_context_to_sid(
                &scope,
                &policy,
                &mut portable_sids,
                Cow::Borrowed(&security_context),
            )
        }
        .unwrap();
        assert_eq!(portable_sids.entries.len(), 1);
        assert!(sid.0.get() >= ReferenceInitialSid::FirstUnused as u32);
    }

    #[test]
    fn initial_sids_remapped_to_dynamic_sids() {
        let file_initial_sid = InitialSid::File.into();
        let policy = test_policy();
        let mut portable_sids = PortableSidTable::default();
        let sid_table = SidTable::new(&policy, &portable_sids);
        let scope = RcuReadScope::new();
        let file_initial_security_context =
            sid_table.sid_to_security_context(&scope, file_initial_sid);
        // SAFETY: Single-threaded test with exclusive access to `sid_table`.
        let file_dynamic_sid = unsafe {
            sid_table.security_context_to_sid(
                &scope,
                &policy,
                &mut portable_sids,
                Cow::Borrowed(file_initial_security_context),
            )
        }
        .unwrap();
        assert_ne!(file_initial_sid.0.get(), file_dynamic_sid.0.get());
        assert!(file_dynamic_sid.0.get() >= ReferenceInitialSid::FirstUnused as u32);
    }

    #[test]
    fn sids_to_security_contexts() {
        let policy = test_policy();
        let source_context =
            policy.parse_security_context(b"user0:object_r:type0:s0".into()).unwrap();
        let target_context = policy
            .parse_security_context(b"unconfined_u:unconfined_r:unconfined_t:s0".into())
            .unwrap();
        let unlabeled_context = policy.initial_context(InitialSid::Unlabeled);

        let mut portable_sids = PortableSidTable::default();
        let sid_table = SidTable::new(&policy, &portable_sids);
        let scope = RcuReadScope::new();

        // SAFETY: Single-threaded test with exclusive access to `sid_table`.
        let source_sid = unsafe {
            sid_table.security_context_to_sid(
                &scope,
                &policy,
                &mut portable_sids,
                Cow::Borrowed(&source_context),
            )
        }
        .unwrap();
        // SAFETY: Single-threaded test with exclusive access to `sid_table`.
        let target_sid = unsafe {
            sid_table.security_context_to_sid(
                &scope,
                &policy,
                &mut portable_sids,
                Cow::Borrowed(&target_context),
            )
        }
        .unwrap();

        let (resolved_source, resolved_target) =
            sid_table.sids_to_security_contexts(&scope, source_sid, target_sid);
        assert_eq!(resolved_source, &source_context);
        assert_eq!(resolved_target, &target_context);

        // Allocate a SID in `portable_sids` that is not mapped in `sid_table` to simulate an
        // invalidated SID.
        let unmapped_sid = portable_sids.allocate_sid(b"unknown_u:object_r:type0:s0".into());
        let (resolved_unmapped, resolved_target) =
            sid_table.sids_to_security_contexts(&scope, unmapped_sid, target_sid);
        assert_eq!(resolved_unmapped, unlabeled_context);
        assert_eq!(resolved_target, &target_context);

        let (resolved_source, resolved_unmapped) =
            sid_table.sids_to_security_contexts(&scope, source_sid, unmapped_sid);
        assert_eq!(resolved_source, &source_context);
        assert_eq!(resolved_unmapped, unlabeled_context);
    }

    #[test]
    fn new_remaps_portable_sids_and_validates_constraints() {
        let policy = test_policy();
        let valid_context = policy
            .parse_security_context(b"unconfined_u:unconfined_r:unconfined_t:s0".into())
            .unwrap();
        // All individual symbol names exist in `policy`, so `parse_security_context` succeeds,
        // but `user0` is not granted role `unconfined_r`, so `validate_security_context` fails.
        let invalid_constraints_context =
            policy.parse_security_context(b"user0:unconfined_r:unconfined_t:s0".into()).unwrap();
        assert!(policy.validate_security_context(&invalid_constraints_context).is_err());

        let mut portable_sids = PortableSidTable::default();
        let valid_sid =
            portable_sids.allocate_sid(b"unconfined_u:unconfined_r:unconfined_t:s0".into());
        let duplicate_valid_sid =
            portable_sids.allocate_sid(b"unconfined_u:unconfined_r:unconfined_t:s0-s0".into());
        let invalid_parse_sid = portable_sids.allocate_sid(b"unknown_u:object_r:type0:s0".into());
        let invalid_constraints_sid =
            portable_sids.allocate_sid(b"user0:unconfined_r:unconfined_t:s0".into());

        let sid_table = SidTable::new(&policy, &portable_sids);
        let scope = RcuReadScope::new();
        let unlabeled_context = policy.initial_context(InitialSid::Unlabeled);

        assert_eq!(sid_table.sid_to_security_context(&scope, valid_sid), &valid_context);
        assert_eq!(sid_table.sid_to_security_context(&scope, duplicate_valid_sid), &valid_context);
        assert_eq!(
            sid_table.security_context_to_existing_sid(&scope, &valid_context),
            Some(valid_sid)
        );

        assert_eq!(sid_table.sid_to_security_context(&scope, invalid_parse_sid), unlabeled_context);
        assert_eq!(
            sid_table.sid_to_security_context(&scope, invalid_constraints_sid),
            unlabeled_context
        );
        assert_eq!(
            sid_table.security_context_to_existing_sid(&scope, &invalid_constraints_context),
            None
        );
    }
}
