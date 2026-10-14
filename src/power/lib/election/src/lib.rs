// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Elections that arbitrate a setting across multiple voters.
//!
//! Each voter casts or withdraws its vote in its own `K` slot of an [`Election`], which elects the
//! winning value (the result):
//! - If an [`override_voter`](Election::with_override_voter) has an active vote, its vote wins.
//! - Otherwise, active votes are combined using `cmp` ([`Election::min`], [`Election::max`], or a
//!   custom comparator passed to [`Election::new`]).
//!
//! [`Election`] holds its internal lock only for short map reads and writes, never across `.await`
//! points. Callers that need to update several [`Election`]s and apply the results as a single
//! transaction must serialize those updates themselves.

use fuchsia_sync::Mutex;
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::future::Future;

const INSPECTOR_PROP_RESULT: &str = "result";
const INSPECTOR_PROP_OVERRIDDEN: &str = "overridden";
const INSPECTOR_NODE_VOTES: &str = "votes";

/// Records a vote or result onto an Inspect [`fuchsia_inspect::Node`] under `key`.
///
/// Value types that cannot implement this trait (for example, types from another crate) can be
/// recorded with [`Election::record_inspect_with`] instead.
pub trait InspectValue {
    fn record_inspect(&self, node: &fuchsia_inspect::Node, key: &str);
}

impl InspectValue for u32 {
    fn record_inspect(&self, node: &fuchsia_inspect::Node, key: &str) {
        node.record_uint(key, u64::from(*self));
    }
}

impl InspectValue for u64 {
    fn record_inspect(&self, node: &fuchsia_inspect::Node, key: &str) {
        node.record_uint(key, *self);
    }
}

impl InspectValue for i64 {
    fn record_inspect(&self, node: &fuchsia_inspect::Node, key: &str) {
        node.record_int(key, *self);
    }
}

impl InspectValue for bool {
    fn record_inspect(&self, node: &fuchsia_inspect::Node, key: &str) {
        node.record_bool(key, *self);
    }
}

/// An election for one setting: tracks per-voter (`K`) votes for a value of type `V` and elects
/// the winning value (the result).
///
/// This is what Linux charger drivers call a votable (Qualcomm `pmic-voter`) or an election
/// (Google `gvotable`): a setting that several clients vote on, together with the rule that elects
/// its value.
#[derive(Debug)]
pub struct Election<K, V> {
    /// Comparator used to rank non-override votes (`Ordering::Greater` wins).
    cmp: fn(&V, &V) -> Ordering,
    /// Active votes keyed by voter.
    votes: Mutex<BTreeMap<K, V>>,
    /// Optional voter whose active vote overrides all other voters.
    override_voter: Option<K>,
}

impl<K: Ord + Clone, V: Copy> Election<K, V> {
    /// Creates an [`Election`] with no votes that elects the winning vote using `cmp`
    /// (`Ordering::Greater` wins).
    pub fn new(cmp: fn(&V, &V) -> Ordering) -> Self {
        Self { cmp, votes: Mutex::new(BTreeMap::new()), override_voter: None }
    }

    /// Designates `override_voter` as taking precedence over all other voters when present.
    pub fn with_override_voter(mut self, override_voter: K) -> Self {
        self.override_voter = Some(override_voter);
        self
    }

    fn is_overridden_locked(&self, votes: &BTreeMap<K, V>) -> bool {
        self.override_voter.as_ref().is_some_and(|voter| votes.contains_key(voter))
    }

    fn result_locked(&self, votes: &BTreeMap<K, V>) -> Option<V> {
        self.override_voter
            .as_ref()
            .and_then(|voter| votes.get(voter).copied())
            .or_else(|| votes.values().copied().max_by(|a, b| (self.cmp)(a, b)))
    }

    /// Casts (`Some`) or withdraws (`None`) `voter`'s vote and returns
    /// `(previous_vote, previous_result, new_result)`.
    fn swap(&self, voter: K, vote: Option<V>) -> (Option<V>, Option<V>, Option<V>) {
        let mut votes = self.votes.lock();
        let previous_result = self.result_locked(&votes);
        let previous_vote = match vote {
            Some(vote) => votes.insert(voter, vote),
            None => votes.remove(&voter),
        };
        (previous_vote, previous_result, self.result_locked(&votes))
    }

    /// Returns the result, whether the override voter has an active vote, and a copy of the votes,
    /// read under one lock acquisition.
    fn snapshot(&self) -> (Option<V>, bool, Vec<(K, V)>) {
        let votes = self.votes.lock();
        let copy = votes.iter().map(|(voter, vote)| (voter.clone(), *vote)).collect();
        (self.result_locked(&votes), self.is_overridden_locked(&votes), copy)
    }

    /// Returns whether `override_voter` currently has an active vote.
    pub fn is_overridden(&self) -> bool {
        self.is_overridden_locked(&self.votes.lock())
    }

    /// Returns the winning vote across all active voters, or `None` if no votes are active.
    pub fn result(&self) -> Option<V> {
        self.result_locked(&self.votes.lock())
    }

    /// Returns `voter`'s current vote, if any.
    pub fn vote<Q>(&self, voter: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.votes.lock().get(voter).copied()
    }

    /// Casts `voter`'s `vote`, replacing any previous vote, and returns the previous vote.
    pub fn cast(&self, voter: K, vote: V) -> Option<V> {
        self.votes.lock().insert(voter, vote)
    }

    /// Withdraws `voter`'s vote and returns it, or `None` if `voter` had no vote.
    pub fn withdraw<Q>(&self, voter: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.votes.lock().remove(voter)
    }

    /// Casts (`Some`) or withdraws (`None`) `voter`'s vote and returns the previous vote, so that
    /// callers can restore it if applying the new result fails.
    pub fn set_vote(&self, voter: K, vote: Option<V>) -> Option<V> {
        self.swap(voter, vote).0
    }

    /// Casts `voter`'s `vote` and, if the result changes, runs `apply(result)`. See
    /// [`Election::try_set_vote`].
    pub async fn try_cast<E, F, Fut>(&self, voter: K, vote: V, apply: F) -> Result<bool, E>
    where
        V: PartialEq,
        F: FnOnce(Option<V>) -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        self.try_set_vote(voter, Some(vote), apply).await
    }

    /// Withdraws `voter`'s vote and, if the result changes, runs `apply(result)`. See
    /// [`Election::try_set_vote`].
    pub async fn try_withdraw<E, F, Fut>(&self, voter: K, apply: F) -> Result<bool, E>
    where
        V: PartialEq,
        F: FnOnce(Option<V>) -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        self.try_set_vote(voter, None, apply).await
    }

    /// Casts (`Some`) or withdraws (`None`) `voter`'s vote and, if the result changes, runs
    /// `apply(result)`.
    ///
    /// Restores `voter`'s previous vote if `apply` fails or if the returned future is dropped
    /// before `apply` completes, so that the election matches what was last applied. A failed or
    /// cancelled `apply` may still have partially taken effect.
    ///
    /// Returns `Ok(true)` if the result changed and `apply` succeeded, or `Ok(false)` if the result
    /// was unchanged (and `apply` did not run).
    pub async fn try_set_vote<E, F, Fut>(
        &self,
        voter: K,
        vote: Option<V>,
        apply: F,
    ) -> Result<bool, E>
    where
        V: PartialEq,
        F: FnOnce(Option<V>) -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        let (previous_vote, previous_result, result) = self.swap(voter.clone(), vote);
        if result == previous_result {
            return Ok(false);
        }
        let rollback = Rollback { election: self, voter: Some(voter), previous_vote };
        apply(result).await?;
        rollback.defuse();
        Ok(true)
    }
}

/// Restores a voter's previous vote when dropped, unless defused.
struct Rollback<'a, K: Ord + Clone, V: Copy> {
    election: &'a Election<K, V>,
    /// Voter to restore, or `None` once defused.
    voter: Option<K>,
    previous_vote: Option<V>,
}

impl<K: Ord + Clone, V: Copy> Rollback<'_, K, V> {
    /// Keeps the new vote.
    fn defuse(mut self) {
        self.voter = None;
    }
}

impl<K: Ord + Clone, V: Copy> Drop for Rollback<'_, K, V> {
    fn drop(&mut self) {
        if let Some(voter) = self.voter.take() {
            self.election.swap(voter, self.previous_vote);
        }
    }
}

impl<K: Ord + Clone + AsRef<str>, V: Copy> Election<K, V> {
    /// Records `result`, `overridden`, and per-voter `votes` under `parent/<name>`, using `record`
    /// to write each value.
    pub fn record_inspect_with(
        &self,
        parent: &fuchsia_inspect::Node,
        name: &str,
        record: impl Fn(&V, &fuchsia_inspect::Node, &str),
    ) {
        // Record from a snapshot so that the lock is not held while `record` runs: `record` may
        // call back into the election, and voters are not blocked while Inspect allocates.
        let (result, overridden, votes) = self.snapshot();
        parent.record_child(name, |node| {
            if let Some(result) = result {
                record(&result, node, INSPECTOR_PROP_RESULT);
            }
            node.record_bool(INSPECTOR_PROP_OVERRIDDEN, overridden);
            node.record_child(INSPECTOR_NODE_VOTES, |votes_node| {
                for (voter, vote) in &votes {
                    record(vote, votes_node, voter.as_ref());
                }
            });
        });
    }
}

impl<K: Ord + Clone + AsRef<str>, V: Copy + InspectValue> Election<K, V> {
    /// Records `result`, `overridden`, and per-voter `votes` under `parent/<name>`.
    pub fn record_inspect(&self, parent: &fuchsia_inspect::Node, name: &str) {
        self.record_inspect_with(parent, name, InspectValue::record_inspect);
    }
}

impl<K: Ord + Clone, V: Copy + Ord> Election<K, V> {
    /// Creates an [`Election`] where the lowest vote wins (`V::Ord`).
    pub fn min() -> Self {
        Self::new(|a, b| b.cmp(a))
    }

    /// Creates an [`Election`] where the highest vote wins (`V::Ord`).
    pub fn max() -> Self {
        Self::new(Ord::cmp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::assert_data_tree;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    /// Verifies `Election::min` elects the minimum vote across numeric voter IDs, falls back as
    /// votes are withdrawn, and honors `with_override_voter`.
    #[fuchsia::test]
    fn test_election_min_with_override_voter() {
        const VOTER_A: u64 = 1;
        const VOTER_B: u64 = 2;
        const OVERRIDE_VOTER: u64 = 99;
        const HIGH_LIMIT_UA: u32 = 1_500_000;
        const MID_LIMIT_UA: u32 = 500_000;
        const LOW_LIMIT_UA: u32 = 300_000;
        const OVERRIDE_LIMIT_UA: u32 = 2_000_000;

        let e = Election::min().with_override_voter(OVERRIDE_VOTER);
        assert_eq!(e.result(), None);

        assert_eq!(e.cast(VOTER_A, HIGH_LIMIT_UA), None);
        assert_eq!(e.result(), Some(HIGH_LIMIT_UA));
        assert_eq!(e.vote(&VOTER_A), Some(HIGH_LIMIT_UA));

        e.cast(VOTER_B, MID_LIMIT_UA);
        assert_eq!(e.result(), Some(MID_LIMIT_UA));

        assert_eq!(e.cast(VOTER_A, LOW_LIMIT_UA), Some(HIGH_LIMIT_UA));
        assert_eq!(e.result(), Some(LOW_LIMIT_UA));

        // The override voter forces a higher value even under `Election::min`, and withdrawing
        // restores the min across remaining voters.
        e.cast(OVERRIDE_VOTER, OVERRIDE_LIMIT_UA);
        assert!(e.is_overridden());
        assert_eq!(e.result(), Some(OVERRIDE_LIMIT_UA));

        assert_eq!(e.withdraw(&OVERRIDE_VOTER), Some(OVERRIDE_LIMIT_UA));
        assert!(!e.is_overridden());
        assert_eq!(e.vote(&OVERRIDE_VOTER), None);
        assert_eq!(e.result(), Some(LOW_LIMIT_UA));

        assert_eq!(e.set_vote(VOTER_A, None), Some(LOW_LIMIT_UA));
        assert_eq!(e.result(), Some(MID_LIMIT_UA));
        assert_eq!(e.set_vote(VOTER_A, Some(HIGH_LIMIT_UA)), None);
        assert_eq!(e.result(), Some(MID_LIMIT_UA));

        // Withdrawing a missing vote returns `None`.
        assert_eq!(e.withdraw(&VOTER_A), Some(HIGH_LIMIT_UA));
        assert_eq!(e.withdraw(&VOTER_A), None);

        assert_eq!(e.withdraw(&VOTER_B), Some(MID_LIMIT_UA));
        assert_eq!(e.result(), None);
    }

    /// Verifies `Election::max` elects the maximum vote across string voter keys, honors
    /// `with_override_voter`, and records `result`, `overridden`, and per-voter `votes` in Inspect.
    #[fuchsia::test]
    async fn test_election_max_with_override_voter_and_record_inspect() {
        let e = Election::max().with_override_voter("debug");
        e.cast("vreg", false);
        e.cast("controller", true);
        assert_eq!(e.result(), Some(true));

        // The override voter can force false even when another voter votes true.
        e.cast("debug", false);
        assert_eq!(e.result(), Some(false));

        let inspector = fuchsia_inspect::Inspector::default();
        e.record_inspect(inspector.root(), "otg_enabled");
        assert_data_tree!(inspector, root: {
            otg_enabled: {
                result: false,
                overridden: true,
                votes: {
                    vreg: false,
                    controller: true,
                    debug: false,
                },
            },
        });

        e.withdraw("debug");
        assert!(!e.is_overridden());
        assert_eq!(e.result(), Some(true));

        e.cast("controller", false);
        assert_eq!(e.result(), Some(false));
    }

    /// Verifies `Election::new` works on value types that do not implement `Ord` or
    /// [`InspectValue`] using a custom precedence comparator, owned `String` voter keys, and
    /// `record_inspect_with`.
    #[fuchsia::test]
    async fn test_election_custom_comparator_string_voters_and_record_inspect_with() {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Mode {
            Charging,
            Passthrough,
            Discharging,
        }

        fn rank(m: &Mode) -> u8 {
            match m {
                Mode::Charging => 0,
                Mode::Passthrough => 1,
                Mode::Discharging => 2,
            }
        }

        let debug = String::from("debug");
        let msc = String::from("msc");
        let thermal = String::from("thermal");
        let defender = String::from("defender");

        let e = Election::new(|a, b| rank(a).cmp(&rank(b))).with_override_voter(debug.clone());
        e.cast(msc.clone(), Mode::Charging);
        assert_eq!(e.vote("msc"), Some(Mode::Charging));
        e.cast(thermal.clone(), Mode::Passthrough);
        assert_eq!(e.result(), Some(Mode::Passthrough));

        e.cast(defender.clone(), Mode::Discharging);
        assert_eq!(e.result(), Some(Mode::Discharging));

        let inspector = fuchsia_inspect::Inspector::default();
        e.record_inspect_with(inspector.root(), "operating_mode", |mode, node, key| {
            node.record_string(key, format!("{mode:?}"));
        });
        assert_data_tree!(inspector, root: {
            operating_mode: {
                result: "Discharging",
                overridden: false,
                votes: {
                    defender: "Discharging",
                    msc: "Charging",
                    thermal: "Passthrough",
                },
            },
        });

        // The debug override voter can force Charging even when defender votes Discharging.
        e.cast(debug.clone(), Mode::Charging);
        assert_eq!(e.result(), Some(Mode::Charging));

        e.withdraw(&debug);
        assert_eq!(e.result(), Some(Mode::Discharging));

        e.withdraw(&defender);
        assert_eq!(e.result(), Some(Mode::Passthrough));
    }

    /// Verifies `try_cast` and `try_withdraw` commit on `Ok`, restore the previous vote on `Err`,
    /// and skip `apply` when the result is unchanged.
    #[fuchsia::test]
    async fn test_election_try_cast_and_try_withdraw_rollback() {
        const VOTER_A: u8 = 1;
        const VOTER_B: u8 = 2;
        const INITIAL_LIMIT_UA: u32 = 100;
        const LOWER_LIMIT_UA: u32 = 50;

        let e = Election::min();

        // A failed first vote restores no vote.
        let res = e
            .try_cast(VOTER_A, INITIAL_LIMIT_UA, |result| async move {
                assert_eq!(result, Some(INITIAL_LIMIT_UA));
                Err("io")
            })
            .await;
        assert_eq!(res, Err("io"));
        assert_eq!(e.vote(&VOTER_A), None);
        assert_eq!(e.result(), None);

        // A successful vote commits and returns Ok(true); repeating the identical vote returns
        // Ok(false) without running apply.
        let res: Result<bool, &'static str> = e
            .try_cast(VOTER_A, INITIAL_LIMIT_UA, |result| async move {
                assert_eq!(result, Some(INITIAL_LIMIT_UA));
                Ok(())
            })
            .await;
        assert_eq!(res, Ok(true));
        assert_eq!(e.vote(&VOTER_A), Some(INITIAL_LIMIT_UA));

        let res: Result<bool, &'static str> = e
            .try_cast(VOTER_A, INITIAL_LIMIT_UA, |_result| async {
                panic!("apply should not run when the vote is unchanged");
            })
            .await;
        assert_eq!(res, Ok(false));

        // A failed vote restores the previous vote.
        let res = e
            .try_cast(VOTER_A, LOWER_LIMIT_UA, |result| async move {
                assert_eq!(result, Some(LOWER_LIMIT_UA));
                Err("io")
            })
            .await;
        assert_eq!(res, Err("io"));
        assert_eq!(e.vote(&VOTER_A), Some(INITIAL_LIMIT_UA));

        // A failed withdrawal restores the previous vote.
        let res = e
            .try_withdraw(VOTER_A, |result| async move {
                assert_eq!(result, None);
                Err("io")
            })
            .await;
        assert_eq!(res, Err("io"));
        assert_eq!(e.vote(&VOTER_A), Some(INITIAL_LIMIT_UA));

        // A winning lower vote from VOTER_B runs apply.
        let res: Result<bool, &'static str> = e
            .try_cast(VOTER_B, LOWER_LIMIT_UA, |result| async move {
                assert_eq!(result, Some(LOWER_LIMIT_UA));
                Ok(())
            })
            .await;
        assert_eq!(res, Ok(true));

        // Withdrawing VOTER_A's losing vote leaves the result unchanged, so apply does not run.
        let res: Result<bool, &'static str> = e
            .try_withdraw(VOTER_A, |_result| async {
                panic!("apply should not run when the result is unchanged");
            })
            .await;
        assert_eq!(res, Ok(false));
        assert_eq!(e.vote(&VOTER_A), None);

        // Withdrawing the last vote applies `None`; a second withdrawal returns Ok(false).
        let res: Result<bool, &'static str> = e
            .try_withdraw(VOTER_B, |result| async move {
                assert_eq!(result, None);
                Ok(())
            })
            .await;
        assert_eq!(res, Ok(true));
        let res: Result<bool, &'static str> = e
            .try_withdraw(VOTER_B, |_result| async {
                panic!("apply should not run when the voter has no vote");
            })
            .await;
        assert_eq!(res, Ok(false));
    }

    /// Verifies that `record_inspect_with` does not hold the election's lock while running
    /// `record`, so that `record` can call back into the election.
    #[fuchsia::test]
    async fn test_election_record_inspect_with_allows_reentrant_record() {
        let e = Election::min();
        e.cast("a", 2u32);
        e.cast("b", 1u32);

        let inspector = fuchsia_inspect::Inspector::default();
        e.record_inspect_with(inspector.root(), "limit", |vote, node, key| {
            // This would deadlock if the lock were held while recording.
            assert_eq!(e.result(), Some(1));
            node.record_uint(key, u64::from(*vote));
        });
        assert_data_tree!(inspector, root: {
            limit: {
                result: 1u64,
                overridden: false,
                votes: {
                    a: 2u64,
                    b: 1u64,
                },
            },
        });
    }

    /// Verifies that dropping a `try_cast` future while `apply` is pending restores the previous
    /// vote and result.
    #[fuchsia::test]
    fn test_election_try_cast_restores_vote_when_cancelled() {
        const VOTER: u8 = 1;
        const INITIAL_LIMIT_UA: u32 = 100;
        const LOWER_LIMIT_UA: u32 = 50;

        let e = Election::min();
        e.cast(VOTER, INITIAL_LIMIT_UA);
        {
            let mut update = pin!(e.try_cast(VOTER, LOWER_LIMIT_UA, |_result| {
                std::future::pending::<Result<(), ()>>()
            }));
            let mut cx = Context::from_waker(Waker::noop());
            assert_eq!(update.as_mut().poll(&mut cx), Poll::Pending);
            assert_eq!(e.result(), Some(LOWER_LIMIT_UA));
        }
        assert_eq!(e.vote(&VOTER), Some(INITIAL_LIMIT_UA));
        assert_eq!(e.result(), Some(INITIAL_LIMIT_UA));
    }
}
