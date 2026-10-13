// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::policy::{Policy, SecurityContext, SecurityContextError};
use crate::{InitialSid, ReferenceInitialSid, SecurityId};

use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::num::NonZeroU32;

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

    /// Reverse lookup table of allocated dynamic [`SecurityId`]s, keyed by the hash of their
    /// corresponding byte string in `entries`.
    string_to_sid: HashTable<SecurityId>,

    /// Hasher used to compute hashes of security context byte strings in `string_to_sid`.
    hasher: rapidhash::RapidBuildHasher,
}

impl PortableSidTable {
    /// Returns the existing [`SecurityId`] for `context_string`, or allocates a new dynamic
    /// [`SecurityId`] if `context_string` has not been seen before.
    pub(super) fn get_or_allocate_sid(&mut self, context_string: Vec<u8>) -> SecurityId {
        let hash = self.hasher.hash_one(context_string.as_slice());
        match self.string_to_sid.entry(
            hash,
            |&sid| Self::entry_for_sid(&self.entries, sid) == context_string.as_slice(),
            |&sid| self.hasher.hash_one(Self::entry_for_sid(&self.entries, sid)),
        ) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let sid_index = ReferenceInitialSid::FirstUnused as usize + self.entries.len();
                let sid = SecurityId(NonZeroU32::new(sid_index as u32).unwrap());
                self.entries.push(context_string.into_boxed_slice());
                *entry.insert(sid).get()
            }
        }
    }

    fn entry_for_sid(entries: &[Box<[u8]>], sid: SecurityId) -> &[u8] {
        &entries[sid.0.get() as usize - ReferenceInitialSid::FirstUnused as usize]
    }
}

/// Per-policy mapping between [`SecurityId`]s and effective [`SecurityContext`]s.
pub(super) struct SidTable {
    /// Forward mapping from [`SecurityId`] (`sid.0.get() as usize`) to its effective
    /// [`SecurityContext`] under the owning policy.
    ///
    /// Slots for unused initial SIDs or dynamic SIDs whose context strings are invalid under
    /// the policy are populated with the [`InitialSid::Unlabeled`] context.
    entries: Vec<SecurityContext>,

    /// Reverse mapping from validated [`SecurityContext`] to dynamic [`SecurityId`].
    context_to_sid: HashMap<SecurityContext, SecurityId>,
}

impl SidTable {
    /// Constructs a new [`SidTable`] for `policy`, populating initial SIDs and remapping any
    /// previously allocated dynamic SIDs from `portable_sids` that are valid under `policy`.
    pub(super) fn new(policy: &Policy, portable_sids: &PortableSidTable) -> Self {
        let num_entries = ReferenceInitialSid::FirstUnused as usize + portable_sids.entries.len();
        let unlabeled_context = policy.initial_context(InitialSid::Unlabeled);
        let mut entries = vec![unlabeled_context; num_entries];

        for initial_sid in InitialSid::all_variants() {
            entries[*initial_sid as usize] = policy.initial_context(*initial_sid);
        }

        let mut context_to_sid = HashMap::with_capacity(portable_sids.entries.len());
        for (offset, context_string) in portable_sids.entries.iter().enumerate() {
            if let Ok(context) = policy.parse_security_context(context_string.as_ref().into())
                && policy.validate_security_context(&context).is_ok()
            {
                let sid_index = ReferenceInitialSid::FirstUnused as usize + offset;
                let sid = SecurityId(NonZeroU32::new(sid_index as u32).unwrap());
                entries[sid_index] = context.clone();
                context_to_sid.entry(context).or_insert(sid);
            }
        }

        Self { entries, context_to_sid }
    }

    /// Returns the [`SecurityId`] representing the supplied `security_context`, if it is
    /// already present in the SID table. If a SID is returned then `security_context` must be valid
    /// by definition, since [`SidTable::new`] and [`SidTable::security_context_to_sid`] only insert
    /// validated contexts.
    pub(super) fn security_context_to_existing_sid(
        &self,
        security_context: &SecurityContext,
    ) -> Option<SecurityId> {
        self.context_to_sid.get(security_context).copied()
    }

    /// Returns a [`SecurityId`] representing the supplied `security_context`, inserting it into
    /// both `portable_sids` and this [`SidTable`] if not already present.
    ///
    /// If an entry already exists that matches `security_context` then the existing SID is
    /// returned. Otherwise the `security_context`'s fields are validated against `policy`
    /// constraints, and an explanatory error returned if they are inconsistent.
    pub(super) fn security_context_to_sid(
        &mut self,
        policy: &Policy,
        portable_sids: &mut PortableSidTable,
        security_context: &SecurityContext,
    ) -> Result<SecurityId, SecurityContextError> {
        if let Some(sid) = self.security_context_to_existing_sid(security_context) {
            return Ok(sid);
        }
        policy.validate_security_context(security_context)?;

        let serialized = policy.serialize_security_context(security_context);
        let sid = portable_sids.get_or_allocate_sid(serialized);
        let sid_index = sid.0.get() as usize;

        if self.entries.len() <= sid_index {
            let unlabeled_context = self.entries[InitialSid::Unlabeled as usize].clone();
            self.entries.resize(sid_index + 1, unlabeled_context);
        }
        self.entries[sid_index] = security_context.clone();
        self.context_to_sid.insert(security_context.clone(), sid);

        Ok(sid)
    }

    /// Returns the [`SecurityContext`] associated with `sid`.
    ///
    /// If `sid` was invalidated by a policy reload then the "unlabeled"
    /// context is returned instead.
    #[inline]
    pub(super) fn sid_to_security_context(&self, sid: SecurityId) -> &SecurityContext {
        self.entries
            .get(sid.0.get() as usize)
            .unwrap_or(&self.entries[InitialSid::Unlabeled as usize])
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
        let mut sid_table = SidTable::new(&policy, &portable_sids);
        let sid = sid_table
            .security_context_to_sid(&policy, &mut portable_sids, &security_context)
            .unwrap();
        assert_eq!(*sid_table.sid_to_security_context(sid), security_context);
    }

    #[test]
    fn sids_for_different_security_contexts_differ() {
        let policy = test_policy();
        let mut portable_sids = PortableSidTable::default();
        let mut sid_table = SidTable::new(&policy, &portable_sids);
        let sid1 = sid_table.security_context_to_sid(
            &policy,
            &mut portable_sids,
            &policy.parse_security_context(b"user0:object_r:type0:s0".into()).unwrap(),
        );
        let sid2 = sid_table.security_context_to_sid(
            &policy,
            &mut portable_sids,
            &policy
                .parse_security_context(b"unconfined_u:unconfined_r:unconfined_t:s0".into())
                .unwrap(),
        );
        assert_ne!(sid1, sid2);
    }

    #[test]
    fn sids_for_same_security_context_are_equal() {
        let policy = test_policy();
        let security_context = policy
            .parse_security_context(b"unconfined_u:unconfined_r:unconfined_t:s0".into())
            .unwrap();
        let mut portable_sids = PortableSidTable::default();
        let mut sid_table = SidTable::new(&policy, &portable_sids);
        assert_eq!(portable_sids.entries.len(), 0);
        let sid1 =
            sid_table.security_context_to_sid(&policy, &mut portable_sids, &security_context);
        let sid2 =
            sid_table.security_context_to_sid(&policy, &mut portable_sids, &security_context);
        assert_eq!(sid1, sid2);
        assert_eq!(portable_sids.entries.len(), 1);
    }

    #[test]
    fn sids_allocated_outside_initial_range() {
        let policy = test_policy();
        let security_context = policy
            .parse_security_context(b"unconfined_u:unconfined_r:unconfined_t:s0".into())
            .unwrap();
        let mut portable_sids = PortableSidTable::default();
        let mut sid_table = SidTable::new(&policy, &portable_sids);
        assert_eq!(portable_sids.entries.len(), 0);
        let sid = sid_table
            .security_context_to_sid(&policy, &mut portable_sids, &security_context)
            .unwrap();
        assert_eq!(portable_sids.entries.len(), 1);
        assert!(sid.0.get() >= ReferenceInitialSid::FirstUnused as u32);
    }

    #[test]
    fn initial_sids_remapped_to_dynamic_sids() {
        let file_initial_sid = InitialSid::File.into();
        let policy = test_policy();
        let mut portable_sids = PortableSidTable::default();
        let mut sid_table = SidTable::new(&policy, &portable_sids);
        let file_initial_security_context =
            sid_table.sid_to_security_context(file_initial_sid).clone();
        let file_dynamic_sid = sid_table
            .security_context_to_sid(&policy, &mut portable_sids, &file_initial_security_context)
            .unwrap();
        assert_ne!(file_initial_sid.0.get(), file_dynamic_sid.0.get());
        assert!(file_dynamic_sid.0.get() >= ReferenceInitialSid::FirstUnused as u32);
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
            portable_sids.get_or_allocate_sid(b"unconfined_u:unconfined_r:unconfined_t:s0".into());
        let invalid_parse_sid =
            portable_sids.get_or_allocate_sid(b"unknown_u:object_r:type0:s0".into());
        let invalid_constraints_sid =
            portable_sids.get_or_allocate_sid(b"user0:unconfined_r:unconfined_t:s0".into());

        let sid_table = SidTable::new(&policy, &portable_sids);
        let unlabeled_context = policy.initial_context(InitialSid::Unlabeled);

        assert_eq!(sid_table.sid_to_security_context(valid_sid), &valid_context);
        assert_eq!(sid_table.security_context_to_existing_sid(&valid_context), Some(valid_sid));

        assert_eq!(sid_table.sid_to_security_context(invalid_parse_sid), &unlabeled_context);
        assert_eq!(sid_table.sid_to_security_context(invalid_constraints_sid), &unlabeled_context);
        assert_eq!(sid_table.security_context_to_existing_sid(&invalid_constraints_context), None);
    }
}
