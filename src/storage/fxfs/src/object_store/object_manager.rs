// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! # Metadata Reservations in Fxfs
//!
//! Fxfs maintains a metadata reservation (`M_total`) in the allocator so that space-freeing
//! operations and journal compactions can proceed even when the filesystem has zero free space:
//! - **Funded transactions**: Normal transactions reserve metadata space from the allocator up
//!   front (`4x` their journal byte size) and transfer that reservation into `M_total` on commit.
//! - **Borrowing transactions**: Operations that free space (such as unlinking files, purging
//!   graveyard tombstones, or trimming extents) must succeed even when the disk is full, so they
//!   borrow from `M_total` (`BorrowedMetadata` / `BorrowedMetadataAndData`) instead of reserving
//!   from the allocator.
//! - **Minor vs. Major Compaction**: Flushing an object store normally performs a *minor
//!   compaction*, writing recent journal mutations (including deletion tombstones) into a new small
//!   layer file stacked on top of existing immutable layers. Minor compactions *consume* blocks
//!   from `M_total` to write the new layer file and do not free older layers. Borrowed metadata
//!   space is only reclaimed when a store performs a *major compaction*, which merges all layers
//!   into a single base layer, drops all tombstones, and deletes the old layer files. To prevent
//!   repeated minor compactions from draining `M_total`, Fxfs forces a major compaction once
//!   borrowed space reaches `50%` of the available borrowing headroom.
//!
//! ## Why Per-Store Borrowing Tracking Is Required (and the Locked-Store Problem)
//!
//! Encrypted volumes can be locked (`ObjectStore::lock`), dropping their decryption keys from
//! memory. If a locked store has unflushed journal mutations when the journal compacts,
//! `ObjectStore::flush_locked` appends them to a sidecar `encrypted_mutations` file in
//! `root_store`; critically, a locked store cannot decrypt and merge its layers to perform a major
//! compaction until it is unlocked again.
//!
//! If borrowed space and major-compaction thresholds were tracked as a single global scalar across
//! the entire filesystem (`borrowed = R_total - M_total >= R_total / 2`, as Fxfs did prior to V61),
//! a locked store could tie up the filesystem's metadata space in two ways:
//! 1. **Consuming other stores' headroom before locking**: Because a global `R_total / 2` threshold
//!    includes `journal::RESERVED_SPACE` plus `1.5 * L` for `root_store`, the `allocator`, and
//!    *all* child volumes, a single volume `S_locked` could perform many borrowing deletions and
//!    minor compactions while unlocked without ever crossing global `R_total / 2`, borrowing far
//!    more than its own `0.5 * L` headroom and eating into the reservations of other volumes and
//!    `root_store`. If `S_locked` was then locked, all of that borrowed space remained frozen in
//!    its unmerged layers while locked.
//! 2. **Starving other stores and forcing endless major compactions**: With `S_locked` tying up
//!    most of global `R_total / 2`, any small borrow by an active unlocked store `S_active` or
//!    `root_store` pushed global `borrowed` past `R_total / 2`. Every subsequent journal flush then
//!    forced a major compaction on *every* unlocked store (`S_active` and `root_store`) even when
//!    they had no tombstones to reclaim, while `S_locked` (the store actually holding the debt) was
//!    locked and skipped (or worse, exhausted `M_total` altogether).
//!
//! Assigning each child store `S` its own borrowing budget `req_S` and borrowed counter `B_S`
//! prevents both failure modes:
//! - While `S_locked` is unlocked, it is forced to major-compact as soon as its own borrowing
//!   reaches `B_S >= req_S / 2` (resetting `B_S = 0` and freeing its tombstone layers), regardless
//!   of how large other stores are. When `S_locked` is locked, the uncompacted debt it can hold is
//!   strictly bounded by its own `req_S` share (which `S_locked` itself funded when its layers were
//!   created) and can never dip into `req_other` or `req_sys`.
//! - While `S_locked` is locked, each unlocked store `S_active` is evaluated solely against its own
//!   `B_{S_active} >= req_{S_active} / 2`, `root_store` subtracts `sum(B_S)` when computing system
//!   borrowed space, and `S_locked` is ignored by compactions until unlocked.
//!
//! ## Reservation Model
//!
//! Each LSM tree (the allocator, the root store, and each child volume) with total layer size `L`
//! has a target reservation budget of `1.5 * L`, which decomposes into:
//! - **Compaction scratch space (`1.0 * L`)**: Worst-case temporary space needed to write a new
//!   compacted layer file before tombstoning the old layer files. Because only one tree is
//!   compacted at a time, this scratch space is shared across all trees (`max(L)` across all
//!   trees).
//! - **Borrowing and system headroom (`0.5 * L`)**: Space that can be borrowed across compactions
//!   (e.g. unmerged minor layers of deletion tombstones, or `encrypted_mutations` in a locked
//!   volume) before a major compaction is required to reclaim it.
//!
//! To isolate borrowing between volumes while sharing compaction scratch space, `ObjectManager`
//! computes required reservations as:
//!
//! 1. **System Requirement (`req_sys`)**: Covers shared compaction scratch space across all trees
//!    (`max(L)`), the allocator and root store headroom (`0.5 * (L_alloc + L_root)`), a scaling
//!    share of child stores (`1/8 * sum(L_child)` for `root_parent_store`, `root_store`, and
//!    `allocator` records of the child stores' layer files themselves; if extreme layer file
//!    fragmentation ever exceeded this share, `R_total` is unaffected and `root_store` would simply
//!    major-compact slightly earlier), unflushed system journal usage (`4 * J_sys`), and
//!    `journal::RESERVED_SPACE`.
//! 2. **Per-Child-Store Requirement (`req_S`)**: For each child volume `S`, covers that volume's
//!    borrowing headroom (`3/8 * L_S`) and its unflushed journal mutations (`4 * J_S`).
//! 3. **Total Required Reservation (`R_total`)**: `req_sys + sum(req_S)`.
//!
//! Rather than partitioning the physical reservation `M_total` (`metadata_reservation.amount()`)
//! into per-store balances (which would require scaling per-store balances down whenever `M_total`
//! has a shortfall and could cause repeated major compactions on subsequent transactions),
//! `ObjectManager` tracks:
//! - **`M_total`**: The total physical bytes currently held in `metadata_reservation`.
//! - **`store_borrowed[S]`**: The metadata space borrowed by each child store `S`'s flushed
//!   non-base layers (or `encrypted_mutations`), while `journal_checkpoints[S]` tracks unflushed
//!   journal borrowing before (`Old`) and after (`Current`) `BeginFlush`. Store `S`'s total
//!   borrowed space is `B_S = store_borrowed[S] + 4 * journal_checkpoints[S].borrowed_usage()`.
//!
//! ## Accounting Lifecycle
//!
//! - **Transaction Commit**: Non-borrowing transactions (`New` / `Hold`) transfer `4 * txn_size`
//!   into `M_total` and record `usage` in `journal_checkpoints[S]`. Borrowing transactions
//!   (`BorrowedMetadata`) transfer nothing into `M_total` and record both `usage` and
//!   `borrowed_usage` in `journal_checkpoints[S]` for each mutated child store `S`.
//! - **Tree Compaction (`BeginFlush` & `EndFlush`)**: When `BeginFlush` is applied to child store
//!   `S`, `journal_checkpoints[S]` moves `Current` into `Old` so any concurrent transactions
//!   between `BeginFlush` and `EndFlush` accumulate into a fresh `Current` span. At
//!   `EndFlush(borrowed)`, `Old` is removed (leaving only any post-`BeginFlush` borrowing in
//!   `Current`) and `store_borrowed[S]` is set to `borrowed` (`0` for a full compaction, or the
//!   updated flushed layer borrowing computed by `store_borrowed_for_flush` for a minor or locked
//!   flush).
//! - **Major Compaction**: Each child store `S` independently forces a major compaction when its
//!   borrowed space exceeds half of its layer headroom (`B_S > h_S / 2`, where `h_S = 3/8 * L_S`)
//!   and the store can be compacted (`needs_flush(S)` or `layers.len() > 1`). `root_store` forces
//!   a major compaction when system borrowed space (`(R_total - M_total).saturating_sub(sum(B_S))`)
//!   exceeds half of the system's non-scratch layer and journal headroom
//!   (`0.5 * (L_alloc + L_root) + 1/8 * sum(L_child) + RESERVED_SPACE`). Because a full compaction
//!   of `S` unconditionally resets `store_borrowed[S] = 0` and drops `Checkpoints::Old` even when
//!   `M_total < R_total`, a single subsequent transaction on `S` will not trigger another major
//!   compaction.
//!
//! ## Journal Replay and Software Upgrades
//!
//! The target reservation formulas (such as `journal::RESERVED_SPACE` or the layer/journal
//! multipliers) can change between software versions, and during journal replay the full set of
//! child volumes and their layer sizes are not yet known. If the super-block only recorded global
//! borrowed space `B` and computed the mount-time reservation as `R_total - B`, any software update
//! that increased `R_total` would try to reserve *more* space from the allocator at mount time than
//! was actually set aside before the unmount or crash, which could fail with `NoSpace` on a nearly
//! full filesystem.
//!
//! Instead, `SuperBlockHeader::reserved_metadata_space` records `M_total` at key `0` and each child
//! store's flushed layer borrowing `store_borrowed[S]` at key `store_id` as of
//! `super_block_journal_file_offset`, and the journal distinguishes non-borrowing (`Commit`) from
//! borrowing (`CommitBorrowed(u64)`) transactions:
//!
//! 1. **Seeding from the Super-Block**: `init_replay_metadata_reservation` seeds `M_total` from key
//!    `0` and `store_borrowed[S]` from each `store_id != 0`.
//! 2. **Replaying Journal Transactions (`replay_mutations`)**:
//!    - All mutations in `transactions` (which `Journal::read_transactions` already filtered to
//!      those unflushed as of `super_block.journal_file_offsets`) run through
//!      `Inner::apply_mutation` and `record_journal_usage`, taking `journal_checkpoints` and
//!      `store_borrowed` through the same `BeginFlush` -> `Old` -> `EndFlush(borrowed)` state
//!      transitions as live mode.
//!    - For transactions after `super_block_journal_file_offset`:
//!      - `Commit` (non-borrowing): Adds `4 * txn_size` to `M_total`.
//!      - `CommitBorrowed(reserved)` (borrowing): Updates `M_total` to `reserved` (the
//!        post-`apply_transaction` `metadata_reservation.amount()` recorded in live mode).
//! 3. **Reconciling with Current Software Requirements (`init_metadata_reservation`)**: Once replay
//!    completes and all child volumes are opened, `min(M_total, R_total)` is reserved from the
//!    allocator (while `total_store_borrowed` clamps each `store_borrowed[S]` to `h_S` on read):
//!    - **If required reservations increased** (`M_total < R_total`): Only `M_total` is reserved
//!      from the allocator at mount time, never asking for extra space.
//!    - **If required reservations decreased** (`M_total > R_total`): The reservation is capped
//!      at `R_total`, leaving the excess unreserved in the allocator.
//!    - **Legacy (< V61) migration**: Pre-V61 super-blocks only recorded global borrowed space `B`
//!      computed against the legacy required reservation (`legacy_required = max(1.5 * L_i) + 4 *
//!      J_total + reserved_space`), so `M_total = min(legacy_required - B, R_total)` is reserved
//!      and any non-journal deficit is distributed proportionally across child stores' layer
//!      headroom to seed `store_borrowed[S]`.

use crate::errors::FxfsError;
use crate::filesystem::{ApplyContext, ApplyMode, FlushReason, ForceMajor, JournalingObject};
use crate::log::*;
use crate::metrics;
use crate::object_handle::{INVALID_OBJECT_ID, ReadObjectHandle};
use crate::object_store::allocator::{Allocator, Reservation};
use crate::object_store::directory::Directory;
use crate::object_store::journal::{self, BorrowedMetadata, JournalCheckpoint};
use crate::object_store::transaction::{
    AssocObj, AssociatedObject, MetadataReservation, Mutation, ObjectMutationIterator, Transaction,
    TxnMutation,
};
use crate::object_store::tree_cache::TreeCache;
use crate::object_store::volume::{VOLUMES_DIRECTORY, list_volumes};
use crate::object_store::{ObjectDescriptor, ObjectStore};
use crate::round::round_div;
use crate::serialized_types::{LATEST_VERSION, Version};
use anyhow::{Context, Error, anyhow, bail, ensure};
use fuchsia_inspect::{Property as _, UintProperty};
use fuchsia_sync::RwLock;
use futures::FutureExt as _;
use rustc_hash::FxHashMap as HashMap;
use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::num::Saturating;
use std::sync::{Arc, OnceLock};

// Data written to the journal eventually needs to be flushed somewhere (typically into layer
// files).  Here we conservatively assume that could take up to four times as much space as it does
// in the journal.  In the layer file, it'll take up at least as much, but we must reserve the same
// again that so that there's enough space for compactions, and then we need some spare for
// overheads.
//
// TODO(https://fxbug.dev/42178158): We should come up with a better way of determining what the
// multiplier should be here.  2x was too low, as it didn't cover any space for metadata.  4x might
// be too much.
pub const fn reserved_space_from_journal_usage(journal_usage: u64) -> u64 {
    journal_usage * 4
}

/// Tracks the metadata reservation state during journal replay before `init_metadata_reservation`
/// initializes the live `Reservation`.
#[derive(Clone, Copy, Debug)]
pub enum ReplayMetadataReservation {
    /// Legacy super-blocks (< V61) track borrowed space.
    Borrowed(u64),

    /// V61+ super-blocks (and newly formatted filesystems, initialized to `u64::MAX`) track the
    /// actual reserved metadata space.
    Reserved(u64),
}

/// ObjectManager is a global loading cache for object stores and other special objects.
pub struct ObjectManager {
    inner: RwLock<Inner>,
    metadata_reservation: OnceLock<Reservation>,
    volume_directory: OnceLock<Directory<ObjectStore>>,
    on_new_store: Option<Box<dyn Fn(&ObjectStore) + Send + Sync>>,
}

// Tracks the earliest unflushed journal checkpoint for an object alongside the journal bytes
// written for that object since the checkpoint.
#[derive(Clone, Debug)]
struct CheckpointUsage {
    checkpoint: JournalCheckpoint,

    // Total journal bytes (excluding `BeginFlush`/`EndFlush`/`DeleteVolume`) written for this
    // object in this checkpoint span.
    usage: u64,

    // Subset of `usage` written by transactions that borrowed metadata space (`CommitBorrowed` /
    // `LegacyUpdateBorrowed`).
    borrowed_usage: u64,
}

impl CheckpointUsage {
    fn new(checkpoint: JournalCheckpoint) -> Self {
        Self { checkpoint, usage: 0, borrowed_usage: 0 }
    }
}

// Whilst we are flushing we need to keep track of the old checkpoint that we are hoping to flush,
// and a new one that should apply if we successfully finish the flush.
#[derive(Debug)]
enum Checkpoints {
    Current(CheckpointUsage),
    Old(CheckpointUsage),
    Both(/* old: */ CheckpointUsage, /* current: */ CheckpointUsage),
}

impl Checkpoints {
    // Returns the earliest checkpoint (which will always be the old one if present).
    fn earliest(&self) -> &JournalCheckpoint {
        match self {
            Checkpoints::Old(x) | Checkpoints::Both(x, _) | Checkpoints::Current(x) => {
                &x.checkpoint
            }
        }
    }

    // Returns the total unflushed journal bytes for this object across both `Old` and `Current`
    // checkpoint spans.
    #[cfg(test)]
    fn usage(&self) -> u64 {
        match self {
            Checkpoints::Current(x) | Checkpoints::Old(x) => {
                debug_assert!(x.borrowed_usage <= x.usage);
                x.usage
            }
            Checkpoints::Both(x, y) => {
                debug_assert!(x.borrowed_usage <= x.usage);
                debug_assert!(y.borrowed_usage <= y.usage);
                x.usage.saturating_add(y.usage)
            }
        }
    }

    // Returns the unflushed journal bytes from borrowing transactions for this object.
    fn borrowed_usage(&self) -> u64 {
        match self {
            Checkpoints::Current(x) | Checkpoints::Old(x) => {
                debug_assert!(x.borrowed_usage <= x.usage);
                x.borrowed_usage
            }
            Checkpoints::Both(x, y) => {
                debug_assert!(x.borrowed_usage <= x.usage);
                debug_assert!(y.borrowed_usage <= y.usage);
                x.borrowed_usage.saturating_add(y.borrowed_usage)
            }
        }
    }

    // Adds `amount` of journal bytes to the active (`Current`) checkpoint span.
    fn add_usage(&mut self, amount: u64, is_borrowed: bool) {
        match self {
            Checkpoints::Current(x) | Checkpoints::Both(_, x) => {
                x.usage = x.usage.saturating_add(amount);
                if is_borrowed {
                    x.borrowed_usage = x.borrowed_usage.saturating_add(amount);
                }
                debug_assert!(x.borrowed_usage <= x.usage);
            }

            // Any non-flush mutation applied while in `Checkpoints::Old` transitions the state to
            // `Checkpoints::Both` in `ObjectManager::apply_mutation` before `add_usage` is called,
            // and flush mutations (`BeginFlush`/`EndFlush`) are excluded by
            // `Mutation::counts_towards_object_journal_usage`.
            Checkpoints::Old(_) => unreachable!(),
        }
    }
}

// We currently maintain strong references to all stores that have been opened, but there's no
// currently no mechanism for releasing stores that aren't being used.
struct Inner {
    stores: HashMap<u64, Arc<ObjectStore>>,
    root_parent_store_object_id: u64,
    root_store_object_id: u64,
    allocator_object_id: u64,
    allocator: Option<Arc<Allocator>>,

    // Records dependencies on the journal for objects i.e. an entry for object ID 1, would mean it
    // has a dependency on journal records from that offset.
    journal_checkpoints: HashMap<u64, Checkpoints>,

    // Mappings from object-id to total tree layer size (including any encrypted mutations
    // estimate).
    layer_sizes: HashMap<u64, u64>,

    // Mappings from child store object-id to the metadata space borrowed by that store's flushed
    // non-base layers (or `encrypted_mutations`).
    store_borrowed: HashMap<u64, u64>,

    // The last journal end offset for a transaction that has been applied.  This is not necessarily
    // the same as the start offset for the next transaction because of padding.
    last_end_offset: u64,

    // Tracks metadata reservation state during journal replay (and for read-only mounts).
    replay_metadata_reservation: ReplayMetadataReservation,

    // The maximum transaction size that has been encountered so far.
    max_transaction_size: (u64, UintProperty),

    // Extra temporary space that might be tied up in the journal that hasn't yet been deallocated.
    reserved_space: u64,
}

impl Inner {
    // Returns the overhead headroom above `layer_size` for a system tree (`root_store`,
    // `root_parent_store`, or `allocator`), reserved to cover metadata growth between major
    // compactions.
    fn system_tree_headroom(layer_size: u64) -> u64 {
        layer_size / 2
    }

    // Returns the temporary scratch space needed to write out a new compacted layer file before
    // the old layers can be freed.
    fn tree_scratch(layer_size: u64) -> u64 {
        layer_size
    }

    // Returns the headroom attributed to the system bucket for a child store to cover allocator
    // and `root_store` metadata mutations incurred when compacting the child store.
    fn child_headroom_system_share(layer_size: u64) -> u64 {
        layer_size / 8
    }

    // Returns the headroom attributed to a child store's own borrowing budget to cover growth of
    // its own layer files between major compactions.
    fn child_headroom_store_share(layer_size: u64) -> u64 {
        layer_size * 3 / 8
    }

    // Returns the total headroom for `object_id` given its `layer_size`.
    fn tree_headroom(&self, object_id: u64, layer_size: u64) -> u64 {
        if self.is_child_store(object_id) {
            Self::child_headroom_system_share(layer_size)
                + Self::child_headroom_store_share(layer_size)
        } else {
            Self::system_tree_headroom(layer_size)
        }
    }

    // Returns the total reservation for `object_id` given its `layer_size`.
    fn tree_reservation(&self, object_id: u64, layer_size: u64) -> u64 {
        Self::tree_scratch(layer_size) + self.tree_headroom(object_id, layer_size)
    }

    fn is_system_store(&self, store_id: u64) -> bool {
        store_id == self.root_store_object_id || store_id == self.root_parent_store_object_id
    }

    fn is_child_store(&self, object_id: u64) -> bool {
        !self.is_system_store(object_id) && object_id != self.allocator_object_id
    }

    fn earliest_journal_offset(&self) -> Option<u64> {
        self.journal_checkpoints.values().map(|c| c.earliest().file_offset).min()
    }

    // Because only one tree is compacted at a time, a single scratch allocation equal to the
    // largest tree suffices for any compaction across the filesystem.
    fn max_tree_scratch(&self) -> u64 {
        self.layer_sizes.values().copied().map(Self::tree_scratch).max().unwrap_or(0)
    }

    // Returns the system bucket's non-scratch borrowing headroom.
    fn system_headroom(&self) -> u64 {
        let mut headroom = self.reserved_space;
        for (&object_id, &layer_size) in &self.layer_sizes {
            if self.is_child_store(object_id) {
                headroom += Self::child_headroom_system_share(layer_size);
            } else {
                headroom += Self::system_tree_headroom(layer_size);
            }
        }
        headroom
    }

    // Returns the byte span of the journal that still contains unflushed mutations for any object.
    fn total_journal_usage(&self) -> u64 {
        self.earliest_journal_offset().map_or(0, |o| self.last_end_offset - o)
    }

    // Returns the required reservation for child store `store_id`.
    #[cfg(test)]
    fn required_store_reservation(&self, store_id: u64) -> u64 {
        let layer_size = self.layer_sizes.get(&store_id).copied().unwrap_or(0);
        let journal_usage = self.journal_checkpoints.get(&store_id).map_or(0, |c| c.usage());
        Self::child_headroom_store_share(layer_size)
            + reserved_space_from_journal_usage(journal_usage)
    }

    // Returns the total metadata space (`B_S`) currently borrowed by child store `store_id` across
    // its flushed non-base layers (`store_borrowed`, clamped to the store's layer headroom) and its
    // unflushed journal checkpoints.
    fn total_store_borrowed(&self, store_id: u64) -> u64 {
        let max_layer_borrowed =
            Self::child_headroom_store_share(self.layer_sizes.get(&store_id).copied().unwrap_or(0));
        let layer_borrowed = std::cmp::min(
            self.store_borrowed.get(&store_id).copied().unwrap_or(0),
            max_layer_borrowed,
        );
        let journal_borrowed = reserved_space_from_journal_usage(
            self.journal_checkpoints.get(&store_id).map_or(0, |c| c.borrowed_usage()),
        );
        layer_borrowed.saturating_add(journal_borrowed)
    }

    // Removes `store_borrowed` entries for stores that are no longer child stores in `self.stores`.
    fn prune_stores_borrowed(&mut self) {
        self.store_borrowed.retain(|&id, _| {
            self.stores.contains_key(&id)
                && id != self.root_store_object_id
                && id != self.root_parent_store_object_id
                && id != self.allocator_object_id
        });
    }

    // Returns the total required size of the metadata reservation assuming that no space has been
    // borrowed.
    fn required_reservation(&self) -> u64 {
        let mut child_headroom = 0;
        for (&object_id, &layer_size) in &self.layer_sizes {
            if self.is_child_store(object_id) {
                child_headroom += Self::child_headroom_store_share(layer_size);
            }
        }
        self.max_tree_scratch()
            + self.system_headroom()
            + child_headroom
            + reserved_space_from_journal_usage(self.total_journal_usage())
    }

    // Returns the total borrowed metadata space, using the live reservation if initialized or
    // `replay_metadata_reservation` during journal replay / read-only mounts.
    fn borrowed_metadata_space(&self, metadata_reservation: Option<&Reservation>) -> u64 {
        let required = self.required_reservation();
        if let Some(reservation) = metadata_reservation {
            required.saturating_sub(reservation.amount())
        } else {
            match self.replay_metadata_reservation {
                ReplayMetadataReservation::Borrowed(borrowed) => borrowed,
                ReplayMetadataReservation::Reserved(reserved) => required.saturating_sub(reserved),
            }
        }
    }

    // Updates per-store journal checkpoint usage.
    fn record_journal_usage(&mut self, object_usages: &[(u64, u64)], is_borrowed: bool) {
        for &(object_id, bytes) in object_usages {
            if self.is_child_store(object_id)
                && let Some(checkpoint) = self.journal_checkpoints.get_mut(&object_id)
            {
                checkpoint.add_usage(bytes, is_borrowed);
            }
        }
    }

    // Returns true if `object_id` has unflushed journal mutations or multiple immutable layers that
    // can be merged by a major compaction.
    fn can_compact(&self, object_id: u64) -> bool {
        self.journal_checkpoints.contains_key(&object_id)
            || self
                .stores
                .get(&object_id)
                .is_some_and(|s| !s.is_locked() && s.tree().immutable_layer_count() > 1)
    }

    // Determines whether `object_id` should be forced to perform a major compaction during a
    // journal flush.
    fn should_force_major_compaction(&self, object_id: u64, total_reserved: u64) -> ForceMajor {
        let over_threshold = if self.is_child_store(object_id) {
            // Force a major compaction of child store `S` when its borrowed space `B_S` exceeds
            // half of its layer headroom. Excluding journal usage ensures that unflushed journal
            // writes do not inflate the threshold and allow `B_S` to exceed the store's layer
            // headroom.
            let b_s = self.total_store_borrowed(object_id);
            let store_headroom = Self::child_headroom_store_share(
                self.layer_sizes.get(&object_id).copied().unwrap_or(0),
            );
            b_s > store_headroom / 2
        } else if object_id == self.root_store_object_id {
            // Derive the system bucket's borrowed space and compare it against half of the system
            // bucket's non-scratch borrowing headroom.
            let total_borrowed = self.required_reservation().saturating_sub(total_reserved);
            let store_borrowed_sum = self
                .store_borrowed
                .keys()
                .fold(0u64, |acc, &id| acc.saturating_add(self.total_store_borrowed(id)));
            let b_sys = total_borrowed.saturating_sub(store_borrowed_sum);
            b_sys > self.system_headroom() / 2
        } else {
            false
        };
        if over_threshold && self.can_compact(object_id) {
            ForceMajor::True
        } else {
            ForceMajor::False
        }
    }

    fn journaling_object(&self, object_id: u64) -> Option<Arc<dyn JournalingObject>> {
        if object_id == self.allocator_object_id {
            Some(self.allocator.clone().unwrap() as Arc<dyn JournalingObject>)
        } else {
            self.stores.get(&object_id).map(|x| x.clone() as Arc<dyn JournalingObject>)
        }
    }

    // Removes the pre-`BeginFlush` (`Old`) checkpoint span for `object_id`.
    fn end_flush_checkpoints(&mut self, object_id: u64) {
        if let Entry::Occupied(mut o) = self.journal_checkpoints.entry(object_id) {
            match o.get_mut() {
                Checkpoints::Old(_) => {
                    o.remove();
                }
                Checkpoints::Both(_, current) => {
                    *o.get_mut() = Checkpoints::Current(current.clone());
                }
                Checkpoints::Current(_) => {}
            }
        }
    }

    // Updates `ObjectManager`'s checkpoint and reservation state for `mutation`. Returns `false` if
    // the mutation is `DeleteVolume` (which is handled entirely by `ObjectManager`).
    fn apply_mutation(
        &mut self,
        object_id: u64,
        mutation: &Mutation,
        context: &ApplyContext<'_, '_>,
    ) -> bool {
        match mutation {
            Mutation::BeginFlush => {
                if let Some(entry) = self.journal_checkpoints.get_mut(&object_id) {
                    match entry {
                        Checkpoints::Current(x) => {
                            *entry = Checkpoints::Old(x.clone());
                        }

                        // If a previous flush failed or was aborted before `EndFlush`, merge the
                        // two checkpoint spans into `Old`.
                        Checkpoints::Both(x, y) => {
                            *entry = Checkpoints::Old(CheckpointUsage {
                                checkpoint: x.checkpoint.clone(),
                                usage: x.usage.saturating_add(y.usage),
                                borrowed_usage: x.borrowed_usage.saturating_add(y.borrowed_usage),
                            });
                        }
                        Checkpoints::Old(_) => {}
                    }
                }
            }
            Mutation::EndFlush(borrowed) => {
                self.end_flush_checkpoints(object_id);
                // During journal replay, `store_borrowed` is seeded from the super-block at
                // `super_block_journal_file_offset` (stored in `self.last_end_offset`), so any
                // `EndFlush` prior to `self.last_end_offset` is already reflected in
                // `store_borrowed`.
                if self.is_child_store(object_id)
                    && context.checkpoint.file_offset >= self.last_end_offset
                {
                    self.store_borrowed.insert(object_id, *borrowed);
                }
            }
            Mutation::DeleteVolume => {
                if let Some(store) = self.stores.remove(&object_id) {
                    store.mark_deleted();
                }
                self.layer_sizes.remove(&object_id);
                self.store_borrowed.remove(&object_id);
                self.journal_checkpoints.remove(&object_id);
                return false;
            }
            _ => {
                if object_id != self.root_parent_store_object_id {
                    self.journal_checkpoints
                        .entry(object_id)
                        .and_modify(|entry| {
                            if let Checkpoints::Old(x) = entry {
                                *entry = Checkpoints::Both(
                                    x.clone(),
                                    CheckpointUsage::new(context.checkpoint.clone()),
                                );
                            }
                        })
                        .or_insert_with(|| {
                            Checkpoints::Current(CheckpointUsage::new(context.checkpoint.clone()))
                        });
                }
            }
        }
        true
    }
}

impl ObjectManager {
    pub fn new(on_new_store: Option<Box<dyn Fn(&ObjectStore) + Send + Sync>>) -> ObjectManager {
        ObjectManager {
            inner: RwLock::new(Inner {
                stores: HashMap::default(),
                root_parent_store_object_id: INVALID_OBJECT_ID,
                root_store_object_id: INVALID_OBJECT_ID,
                allocator_object_id: INVALID_OBJECT_ID,
                allocator: None,
                journal_checkpoints: HashMap::default(),
                layer_sizes: HashMap::default(),
                store_borrowed: HashMap::default(),
                last_end_offset: 0,
                replay_metadata_reservation: ReplayMetadataReservation::Reserved(u64::MAX),
                max_transaction_size: (0, metrics::detail().create_uint("max_transaction_size", 0)),
                reserved_space: journal::RESERVED_SPACE,
            }),
            metadata_reservation: OnceLock::new(),
            volume_directory: OnceLock::new(),
            on_new_store,
        }
    }

    pub fn required_reservation(&self) -> u64 {
        self.inner.read().required_reservation()
    }

    /// Returns the total reservation for `object_id` given its `layer_size`.
    pub fn tree_reservation(&self, object_id: u64, layer_size: u64) -> u64 {
        self.inner.read().tree_reservation(object_id, layer_size)
    }

    pub fn root_parent_store_object_id(&self) -> u64 {
        self.inner.read().root_parent_store_object_id
    }

    pub fn root_parent_store(&self) -> Arc<ObjectStore> {
        let inner = self.inner.read();
        inner.stores.get(&inner.root_parent_store_object_id).unwrap().clone()
    }

    pub fn set_root_parent_store(&self, store: Arc<ObjectStore>) {
        if let Some(on_new_store) = &self.on_new_store {
            on_new_store(&store);
        }
        let mut inner = self.inner.write();
        let store_id = store.store_object_id();
        inner.stores.insert(store_id, store);
        inner.root_parent_store_object_id = store_id;
    }

    pub fn root_store_object_id(&self) -> u64 {
        self.inner.read().root_store_object_id
    }

    pub fn root_store(&self) -> Arc<ObjectStore> {
        let inner = self.inner.read();
        inner.stores.get(&inner.root_store_object_id).unwrap().clone()
    }

    pub fn set_root_store(&self, store: Arc<ObjectStore>) {
        if let Some(on_new_store) = &self.on_new_store {
            on_new_store(&store);
        }
        let mut inner = self.inner.write();
        let store_id = store.store_object_id();
        inner.stores.insert(store_id, store);
        inner.root_store_object_id = store_id;
    }

    /// Returns true if `store_id` is a system store (`root_store` or `root_parent_store`).
    pub fn is_system_store(&self, store_id: u64) -> bool {
        self.inner.read().is_system_store(store_id)
    }

    /// Returns the store which might or might not be locked.
    pub fn store(&self, store_object_id: u64) -> Option<Arc<ObjectStore>> {
        self.inner.read().stores.get(&store_object_id).cloned()
    }

    /// Returns the total bytes written to the LSM trees in the allocator and all object stores
    /// during compaction operations.
    pub fn compaction_bytes_written(&self) -> u64 {
        let inner = self.inner.read();
        let mut total = 0;
        if let Some(allocator) = &inner.allocator {
            total += allocator.tree().compaction_bytes_written();
        }
        for store in inner.stores.values() {
            total += store.tree().compaction_bytes_written();
        }
        total
    }

    /// This is not thread-safe: it assumes that a store won't be forgotten whilst the loop is
    /// running.  This is to be used after replaying the journal.
    pub async fn on_replay_complete(&self) -> Result<(), Error> {
        let root_store = self.root_store();

        let root_directory = Directory::open(&root_store, root_store.root_directory_object_id())
            .await
            .context("Unable to open root volume directory")?;

        match root_directory.lookup(VOLUMES_DIRECTORY).await? {
            None => bail!("Root directory not found"),
            Some((object_id, ObjectDescriptor::Directory, _)) => {
                let volume_directory = Directory::open(&root_store, object_id)
                    .await
                    .context("Unable to open volumes directory")?;
                self.volume_directory.set(volume_directory).unwrap();
            }
            _ => {
                bail!(
                    anyhow!(FxfsError::Inconsistent)
                        .context("Unexpected type for volumes directory")
                )
            }
        }

        let object_ids = list_volumes(self.volume_directory.get().unwrap())
            .await
            .context("Failed to list volumes")?;

        for store_id in object_ids {
            self.open_store(&root_store, store_id).await?;
        }

        // This can fail if a filesystem is created and truncated to a size
        // that doesn't leave enough free space for metadata reservations.
        self.init_metadata_reservation()
            .await
            .context("Insufficient free space for metadata reservation.")?;

        Ok(())
    }

    pub fn volume_directory(&self) -> &Directory<ObjectStore> {
        self.volume_directory.get().unwrap()
    }

    pub fn set_volume_directory(&self, volume_directory: Directory<ObjectStore>) {
        self.volume_directory.set(volume_directory).unwrap();
    }

    pub fn add_store(&self, store: Arc<ObjectStore>) {
        if let Some(on_new_store) = &self.on_new_store {
            on_new_store(&store);
        }
        let mut inner = self.inner.write();
        let store_object_id = store.store_object_id();
        assert_ne!(store_object_id, inner.root_parent_store_object_id);
        assert_ne!(store_object_id, inner.root_store_object_id);
        assert_ne!(store_object_id, inner.allocator_object_id);
        inner.stores.insert(store_object_id, store);
        inner.store_borrowed.entry(store_object_id).or_default();
    }

    pub fn forget_store(&self, store_object_id: u64) {
        let mut inner = self.inner.write();
        assert_ne!(store_object_id, inner.allocator_object_id);
        inner.stores.remove(&store_object_id);
        inner.layer_sizes.remove(&store_object_id);
        inner.store_borrowed.remove(&store_object_id);
        inner.journal_checkpoints.remove(&store_object_id);
    }

    pub fn set_allocator(&self, allocator: Arc<Allocator>) {
        let mut inner = self.inner.write();
        assert!(!inner.stores.contains_key(&allocator.object_id()));
        inner.allocator_object_id = allocator.object_id();
        inner.allocator = Some(allocator);
    }

    pub fn allocator(&self) -> Arc<Allocator> {
        self.inner.read().allocator.clone().unwrap()
    }

    /// Applies `mutation` to `object` with `context`.
    pub fn apply_mutation(
        &self,
        object_id: u64,
        mutation: Mutation,
        context: &ApplyContext<'_, '_>,
        associated_object: AssocObj<'_>,
    ) -> Result<(), Error> {
        debug!(oid = object_id, mutation:?; "applying mutation");
        associated_object.map(|o| o.will_apply_mutation(&mutation, object_id, self));
        let object = {
            let mut inner = self.inner.write();
            if !inner.apply_mutation(object_id, &mutation, context) {
                return Ok(());
            }
            inner.journaling_object(object_id).unwrap()
        };
        object.apply_mutation(mutation, context, associated_object)
    }

    /// Replays `mutations` for a single transaction.  `journal_offsets` contains the per-object
    /// starting offsets; if the current transaction offset precedes an offset, the mutations for
    /// that object are ignored.  `context` contains the location in the journal file for this
    /// transaction and `end_offset` is the ending journal offset for this transaction.
    pub async fn replay_mutations(
        &self,
        mutations: Vec<(u64, Mutation)>,
        object_journal_usage: &[(u64, u64)],
        journal_offsets: &HashMap<u64, u64>,
        context: &ApplyContext<'_, '_>,
        end_offset: u64,
        borrowed_metadata: BorrowedMetadata,
    ) -> Result<(), Error> {
        debug!(checkpoint = context.checkpoint.file_offset; "REPLAY");
        let allocator_object_id = self.inner.read().allocator_object_id;

        for (object_id, mutation) in mutations {
            // If this transaction precedes the object's last flush (or the volume was later
            // deleted), the object's on-disk layers already reflect it, so only update
            // `ObjectManager`'s checkpoint state without opening or mutating the object itself.
            if let Some(&offset) = journal_offsets.get(&object_id)
                && context.checkpoint.file_offset < offset
            {
                self.inner.write().apply_mutation(object_id, &mutation, context);
                continue;
            }

            // If this is the first time we've encountered this store, we'll need to open it.
            if object_id != allocator_object_id {
                self.open_store(&self.root_store(), object_id).await?;
            }

            self.apply_mutation(object_id, mutation, context, AssocObj::None)?;
        }

        let mut inner = self.inner.write();
        let is_borrowed = !matches!(borrowed_metadata, BorrowedMetadata::None);
        inner.record_journal_usage(object_journal_usage, is_borrowed);

        if end_offset > inner.last_end_offset {
            let txn_size = end_offset - std::mem::replace(&mut inner.last_end_offset, end_offset);
            let txn_space = reserved_space_from_journal_usage(txn_size);
            match borrowed_metadata {
                BorrowedMetadata::None => {
                    if let ReplayMetadataReservation::Reserved(ref mut reserved) =
                        inner.replay_metadata_reservation
                    {
                        *reserved = reserved
                            .checked_add(txn_space)
                            .ok_or(FxfsError::Inconsistent)
                            .context("reserved overflow on funded txn")?;
                    }
                }
                BorrowedMetadata::Borrowed(reserved) => {
                    inner.replay_metadata_reservation =
                        ReplayMetadataReservation::Reserved(reserved);
                }
                BorrowedMetadata::LegacyBorrowed(borrowed) => {
                    match inner.replay_metadata_reservation {
                        ReplayMetadataReservation::Borrowed(ref mut b) => {
                            *b = borrowed
                                .checked_add(txn_space)
                                .ok_or(FxfsError::Inconsistent)
                                .context("legacy borrowed overflow")?;
                        }
                        ReplayMetadataReservation::Reserved(_) => {
                            bail!(
                                anyhow!(FxfsError::Inconsistent)
                                    .context("unexpected LegacyBorrowed in Reserved mode")
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Opens the specified store if it isn't already.  This is *not* thread-safe.
    async fn open_store(&self, parent: &Arc<ObjectStore>, object_id: u64) -> Result<(), Error> {
        if self.inner.read().stores.contains_key(&object_id) {
            return Ok(());
        }
        let store = ObjectStore::open(parent, object_id, Some(Box::new(TreeCache::new())))
            .await
            .with_context(|| format!("Failed to open store {object_id}"))?;
        if let Some(on_new_store) = &self.on_new_store {
            on_new_store(&store);
        }
        let mut inner = self.inner.write();
        assert!(inner.stores.insert(object_id, store).is_none());
        inner.store_borrowed.entry(object_id).or_default();
        Ok(())
    }

    /// Called by the journaling system to apply a transaction.  `checkpoint` indicates the location
    /// in the journal file for this transaction.
    pub fn apply_transaction(
        &self,
        transaction: &mut Transaction<'_>,
        checkpoint: &JournalCheckpoint,
    ) -> Result<(), Error> {
        // Record old value so we can see if anything changes as a result of this transaction.
        let old_amount = self.metadata_reservation().amount();

        debug!(checkpoint = checkpoint.file_offset; "BEGIN TXN");
        let mutations = transaction.take_mutations();
        let context =
            ApplyContext { mode: ApplyMode::Live(transaction), checkpoint: checkpoint.clone() };
        for TxnMutation { object_id, mutation, associated_object, .. } in mutations {
            self.apply_mutation(object_id, mutation, &context, associated_object)?;
        }
        debug!("END TXN");

        // Non-borrowed transactions should have no impact on the metadata reservation.
        debug_assert!(
            transaction.metadata_reservation.is_borrowed()
                || self.metadata_reservation().amount() == old_amount
        );

        Ok(())
    }

    /// Called by the journaling system after a transaction has been written providing the end
    /// offset for the transaction so that we can adjust metadata reservations accordingly.
    pub fn did_commit_transaction(
        &self,
        transaction: &mut Transaction<'_>,
        end_offset: u64,
        object_usages: &[(u64, u64)],
    ) {
        let reservation = self.metadata_reservation();
        let mut inner = self.inner.write();
        let journal_usage = end_offset - std::mem::replace(&mut inner.last_end_offset, end_offset);

        if journal_usage > inner.max_transaction_size.0 {
            inner.max_transaction_size.0 = journal_usage;
            inner.max_transaction_size.1.set(journal_usage);
        }

        let txn_space = reserved_space_from_journal_usage(journal_usage);
        let is_borrowed = transaction.metadata_reservation.is_borrowed();
        match &mut transaction.metadata_reservation {
            MetadataReservation::BorrowedMetadata
            | MetadataReservation::BorrowedMetadataAndData => {}
            MetadataReservation::Hold(hold) => {
                // Transfer reserved space into the metadata reservation.
                let txn_reservation = *hold.owner();
                assert_ne!(
                    txn_reservation as *const _, reservation as *const _,
                    "MetadataReservation::BorrowedMetadataAndData should be used."
                );
                txn_reservation.commit(txn_space);
                if txn_reservation.owner_object_id() != reservation.owner_object_id() {
                    assert_eq!(
                        reservation.owner_object_id(),
                        None,
                        "Should not be mixing attributed owners."
                    );
                    inner
                        .allocator
                        .as_ref()
                        .unwrap()
                        .disown_reservation(txn_reservation.owner_object_id(), txn_space);
                }
                assert!(
                    hold.amount() >= txn_space,
                    "Transaction was larger than metadata reservation"
                );
                hold.forget_some(txn_space);
                reservation.add(txn_space);
            }
            MetadataReservation::Reservation(txn_reservation) => {
                // Transfer reserved space into the metadata reservation.
                txn_reservation.move_to(reservation, txn_space);
            }
        }
        inner.record_journal_usage(object_usages, is_borrowed);
    }

    /// Drops a transaction.  This is called automatically when a transaction is dropped.  If the
    /// transaction has been committed, it should contain no mutations and so nothing will get
    /// rolled back.  For each mutation, drop_mutation is called to allow for roll back (e.g. the
    /// allocator will unreserve allocations).
    pub fn drop_transaction(&self, transaction: &mut Transaction<'_>) {
        for TxnMutation { object_id, mutation, .. } in transaction.take_mutations() {
            self.journaling_object(object_id).map(|o| o.drop_mutation(mutation, transaction));
        }
    }

    /// Returns the journal file offsets that each object depends on and the checkpoint for the
    /// minimum offset.
    pub fn journal_file_offsets(&self) -> (HashMap<u64, u64>, Option<JournalCheckpoint>) {
        let inner = self.inner.read();
        let mut min_checkpoint = None;
        let mut offsets = HashMap::default();
        for (&object_id, checkpoint) in &inner.journal_checkpoints {
            let checkpoint = checkpoint.earliest();
            match &mut min_checkpoint {
                None => min_checkpoint = Some(checkpoint),
                Some(min_checkpoint) => {
                    if checkpoint.file_offset < min_checkpoint.file_offset {
                        *min_checkpoint = checkpoint;
                    }
                }
            }
            offsets.insert(object_id, checkpoint.file_offset);
        }
        (offsets, min_checkpoint.cloned())
    }

    /// Returns the checkpoint into the journal that the object depends on, or None if the object
    /// has no journaled updates.
    pub fn journal_checkpoint(&self, object_id: u64) -> Option<JournalCheckpoint> {
        self.inner
            .read()
            .journal_checkpoints
            .get(&object_id)
            .map(|checkpoints| checkpoints.earliest().clone())
    }

    /// Returns true if the object identified by `object_id` is known to have updates recorded in
    /// the journal that the object depends upon.
    pub fn needs_flush(&self, object_id: u64) -> bool {
        self.inner.read().journal_checkpoints.contains_key(&object_id)
    }

    /// Flushes changes to device with the given reason.
    /// Also returns the earliest known version of a struct on the filesystem.
    pub async fn flush(&self, reason: FlushReason) -> Result<Version, Error> {
        let objects = {
            let inner = self.inner.read();
            let total_reserved = self.metadata_reservation.get().map_or(0, |r| r.amount());
            let mut objects = Vec::new();
            let candidate_ids = inner
                .stores
                .keys()
                .copied()
                .filter(|&id| id != inner.root_parent_store_object_id)
                .chain(std::iter::once(inner.allocator_object_id));
            for object_id in candidate_ids {
                let has_checkpoint = inner.journal_checkpoints.contains_key(&object_id);
                let object_reason = match reason {
                    FlushReason::Journal(ForceMajor::False) => {
                        let force_major =
                            inner.should_force_major_compaction(object_id, total_reserved);
                        if !has_checkpoint && force_major == ForceMajor::False {
                            continue;
                        }
                        FlushReason::Journal(force_major)
                    }
                    FlushReason::Journal(ForceMajor::True) => {
                        if !inner.can_compact(object_id) {
                            continue;
                        }
                        FlushReason::Journal(ForceMajor::True)
                    }
                    other => {
                        if !has_checkpoint {
                            continue;
                        }
                        other
                    }
                };
                objects.push((
                    object_id,
                    inner.journaling_object(object_id).unwrap(),
                    object_reason,
                ));
            }

            // Process objects in reverse sorted order because that will mean we compact the root
            // object store last which will ensure we include the metadata from the compactions of
            // other objects.
            objects.sort_unstable_by_key(|(oid, _, _)| Reverse(*oid));
            objects
        };

        // As we iterate, keep track of the earliest version used by structs in these objects
        let mut earliest_version: Version = LATEST_VERSION;
        for (object_id, object, object_reason) in objects {
            let object_earliest_version = object
                .flush(object_reason)
                .await
                .with_context(|| format!("Failed to flush oid {object_id}"))?;
            if object_earliest_version < earliest_version {
                earliest_version = object_earliest_version;
            }
        }

        Ok(earliest_version)
    }

    pub fn journaling_object(&self, object_id: u64) -> Option<Arc<dyn JournalingObject>> {
        self.inner.read().journaling_object(object_id)
    }

    // Computes the `< V61` required metadata reservation from first principles so that `< V61`
    // migration remains fixed even if `layer_sizes`, `tree_headroom`,
    // `layer_size_from_encrypted_mutations_size`, `reserved_space_from_journal_usage`, or
    // `journal::RESERVED_SPACE` change in future versions.
    async fn legacy_required_reservation(&self) -> Result<u64, Error> {
        const LEGACY_RESERVED_SPACE: u64 = 1_048_576;

        let (stores, allocator, total_journal_usage) = {
            let inner = self.inner.read();
            (
                inner.stores.values().cloned().collect::<Vec<_>>(),
                inner.allocator.clone().unwrap(),
                inner.total_journal_usage(),
            )
        };

        let mut max_tree_reservation = allocator
            .tree()
            .immutable_layer_set()
            .layers
            .iter()
            .map(|l| l.handle().map(ReadObjectHandle::get_size).unwrap_or(0))
            .sum::<u64>()
            * 3
            / 2;

        for store in stores {
            let total_layer_size = if store.is_locked() {
                let info = store.load_store_info().await?;
                let parent_store = store.parent_store().unwrap();
                let mut size = 0;
                for &oid in &info.layers {
                    size += parent_store.get_file_size(oid).await?;
                }
                if info.encrypted_mutations_object_id != INVALID_OBJECT_ID {
                    size +=
                        parent_store.get_file_size(info.encrypted_mutations_object_id).await? * 3;
                }
                size
            } else {
                store
                    .tree()
                    .immutable_layer_set()
                    .layers
                    .iter()
                    .map(|l| l.handle().map(ReadObjectHandle::get_size).unwrap_or(0))
                    .sum()
            };
            max_tree_reservation = std::cmp::max(max_tree_reservation, total_layer_size * 3 / 2);
        }

        Ok(max_tree_reservation + total_journal_usage * 4 + LEGACY_RESERVED_SPACE)
    }

    /// Initializes the live `metadata_reservation` from the allocator after journal replay and
    /// opening all volumes.
    pub async fn init_metadata_reservation(&self) -> Result<(), Error> {
        let legacy_required = if matches!(
            self.inner.read().replay_metadata_reservation,
            ReplayMetadataReservation::Borrowed(_)
        ) {
            Some(self.legacy_required_reservation().await?)
        } else {
            None
        };
        let (amount, required, replay_state, allocator) = {
            let mut inner = self.inner.write();
            let required = inner.required_reservation();
            let replay_state = inner.replay_metadata_reservation;
            let amount = match replay_state {
                // When migrating from a `< V61` super-block, only a single global `borrowed`
                // scalar is known, which was computed against `legacy_required_reservation()`.
                // Recover `M_total = min(legacy_required - legacy_borrowed, required)`, subtract
                // any unflushed journal borrowing already tracked in `journal_checkpoints`, and
                // distribute the remaining non-journal deficit proportionally across child stores'
                // layer headroom using u128 arithmetic to avoid overflow.
                ReplayMetadataReservation::Borrowed(legacy_borrowed) => {
                    let legacy_required = legacy_required.unwrap();
                    ensure!(legacy_required >= legacy_borrowed, FxfsError::Inconsistent);
                    let amount = std::cmp::min(legacy_required - legacy_borrowed, required);
                    let borrowed = required - amount;
                    let max_scratch = inner.max_tree_scratch();
                    let total_journal_space =
                        reserved_space_from_journal_usage(inner.total_journal_usage());
                    let flushed_headroom =
                        required.saturating_sub(max_scratch + total_journal_space);
                    let total_unflushed_borrowed: u64 = inner
                        .journal_checkpoints
                        .values()
                        .map(|c| reserved_space_from_journal_usage(c.borrowed_usage()))
                        .sum();
                    let flushed_borrowed = borrowed.saturating_sub(total_unflushed_borrowed);
                    inner.store_borrowed.clear();
                    let store_ids: Vec<u64> = inner.stores.keys().copied().collect();
                    for id in store_ids {
                        if inner.is_child_store(id) {
                            let layer_size = inner.layer_sizes.get(&id).copied().unwrap_or(0);
                            let h_s = Inner::child_headroom_store_share(layer_size);
                            let layer_borrowed = if flushed_headroom > 0 {
                                std::cmp::min(
                                    h_s,
                                    ((h_s as u128 * flushed_borrowed as u128)
                                        / flushed_headroom as u128)
                                        as u64,
                                )
                            } else {
                                0
                            };
                            inner.store_borrowed.insert(id, layer_borrowed);
                        }
                    }
                    inner.prune_stores_borrowed();
                    inner.replay_metadata_reservation = ReplayMetadataReservation::Reserved(amount);
                    amount
                }

                // For V61+ super-blocks, prune any removed stores from `store_borrowed` and
                // reserve `min(reserved_total, required)` so that if a software update increased
                // `required_reservation()`, we never attempt to reserve more from the allocator at
                // mount time than was previously held on disk.
                ReplayMetadataReservation::Reserved(reserved_total) => {
                    inner.prune_stores_borrowed();
                    std::cmp::min(reserved_total, required)
                }
            };
            (amount, required, replay_state, inner.allocator.as_ref().cloned().unwrap())
        };
        if self.root_parent_store().filesystem().options().read_only {
            // We don't need metadata reservations in read-only mode.
            return Ok(());
        }
        self.metadata_reservation
            .set(allocator.clone().reserve(None, amount).with_context(|| {
                format!(
                    "Failed to reserve {} bytes (required: {}, replay: {:?}), free={}, \
                             owner_bytes={}",
                    amount,
                    required,
                    replay_state,
                    Saturating(allocator.get_disk_bytes()) - allocator.get_used_bytes(),
                    allocator.owner_bytes_debug(),
                )
            })?)
            .unwrap();
        Ok(())
    }

    pub fn metadata_reservation(&self) -> &Reservation {
        self.metadata_reservation.get().unwrap()
    }

    pub fn is_metadata_reservation(&self, reservation: &Reservation) -> bool {
        self.metadata_reservation.get().is_some_and(|r| std::ptr::eq(r, reservation))
    }

    /// Reconciles the metadata reservation at `checkpoint_offset` (returning any excess over
    /// `required_reservation()` to the allocator) and returns the map to persist in the
    /// super-block (key 0 for `M_total`, and child store IDs for each child store's flushed
    /// `store_borrowed`).
    pub fn reconcile_metadata_reservation(&self, checkpoint_offset: u64) -> HashMap<u64, u64> {
        let reservation = self.metadata_reservation();
        let mut inner = self.inner.write();
        inner.last_end_offset = checkpoint_offset;
        let required = inner.required_reservation();
        inner.prune_stores_borrowed();
        let mut result = inner.store_borrowed.clone();

        // Cap `M_total` at `required` (`R_total`) and return any excess to the allocator. Note
        // that we do not subtract `sum(B_S)` here because after a minor compaction or locked
        // flush, `B_S` can differ from the exact global deficit (`R_total - M_total`).
        let to_give_back = reservation.amount().saturating_sub(required);
        if to_give_back > 0 {
            reservation.give_back(to_give_back);
        }
        result.insert(0, reservation.amount());
        result
    }

    /// Sets the tree layer size (`layer_size`) for `object_id`.
    pub fn update_layer_size(&self, object_id: u64, layer_size: u64) {
        self.inner.write().layer_sizes.insert(object_id, layer_size);
    }

    /// Returns the new flushed layer borrowing (`store_borrowed`) for `object_id` when a minor or
    /// locked flush changes its layer size from `old_size` to `new_size` (with total layer size
    /// `total_layer_size`).
    pub fn store_borrowed_for_flush(
        &self,
        object_id: u64,
        old_size: u64,
        new_size: u64,
        total_layer_size: u64,
    ) -> u64 {
        let inner = self.inner.read();
        if !inner.is_child_store(object_id) {
            return 0;
        }
        let current = inner.store_borrowed.get(&object_id).copied().unwrap_or(0);
        let updated = if new_size >= old_size {
            let borrowed_usage = match inner.journal_checkpoints.get(&object_id) {
                Some(Checkpoints::Old(old) | Checkpoints::Both(old, _)) => old.borrowed_usage,
                _ => 0,
            };
            let added = std::cmp::min(
                reserved_space_from_journal_usage(borrowed_usage),
                inner.tree_reservation(object_id, new_size - old_size),
            );
            current.saturating_add(added)
        } else {
            let released = inner.tree_reservation(object_id, old_size - new_size);
            current.saturating_sub(released)
        };
        std::cmp::min(updated, Inner::child_headroom_store_share(total_layer_size))
    }

    pub fn reservation(&self, object_id: u64) -> Option<u64> {
        let inner = self.inner.read();
        inner
            .layer_sizes
            .get(&object_id)
            .copied()
            .map(|layer_size| inner.tree_reservation(object_id, layer_size))
    }

    /// Returns the maximum layer reservation held by any object.
    pub fn max_store_reservation(&self) -> u64 {
        let inner = self.inner.read();
        inner
            .layer_sizes
            .iter()
            .map(|(&id, &layer_size)| inner.tree_reservation(id, layer_size))
            .max()
            .unwrap_or(0)
    }

    pub fn set_reserved_space(&self, amount: u64) {
        self.inner.write().reserved_space = amount;
    }

    pub fn last_end_offset(&self) -> u64 {
        self.inner.read().last_end_offset
    }

    pub fn set_last_end_offset(&self, v: u64) {
        self.inner.write().last_end_offset = v;
    }

    /// Returns the total borrowed metadata space.
    pub fn borrowed_metadata_space(&self) -> u64 {
        self.inner.read().borrowed_metadata_space(self.metadata_reservation.get())
    }

    /// Returns the total borrowed metadata space for each child store.
    pub fn stores_borrowed(&self) -> HashMap<u64, u64> {
        let inner = self.inner.read();
        inner.store_borrowed.keys().map(|&id| (id, inner.total_store_borrowed(id))).collect()
    }

    /// Initializes the replay metadata reservation state from the super-block header before
    /// replaying the journal. For `< V61` super-blocks, key `0` holds the legacy `borrowed`
    /// scalar; for `>= V61` super-blocks, key `0` holds `M_total` and non-zero keys hold each
    /// child store's borrowed space `B_S`.
    pub fn init_replay_metadata_reservation(&self, version: Version, reserved: &HashMap<u64, u64>) {
        let mut inner = self.inner.write();
        inner.store_borrowed.clear();
        if version.major < 61 {
            inner.replay_metadata_reservation =
                ReplayMetadataReservation::Borrowed(reserved.get(&0).copied().unwrap_or(0));
        } else {
            let total = reserved.get(&0).copied().unwrap_or(0);
            for (&object_id, &amount) in reserved {
                if object_id != 0 {
                    inner.store_borrowed.insert(object_id, amount);
                }
            }
            inner.replay_metadata_reservation = ReplayMetadataReservation::Reserved(total);
        }
    }

    pub fn write_mutations(
        &self,
        object_id: u64,
        mutations: ObjectMutationIterator<'_, '_>,
        writer: journal::Writer<'_>,
    ) {
        self.journaling_object(object_id).unwrap().write_mutations(mutations, writer);
    }

    pub fn unlocked_stores(&self) -> Vec<Arc<ObjectStore>> {
        let inner = self.inner.read();
        let mut stores = Vec::new();
        for store in inner.stores.values() {
            if !store.is_locked() {
                stores.push(store.clone());
            }
        }
        stores
    }

    /// Creates a lazy inspect node named `str` under `parent` which will yield statistics for the
    /// object manager when queried.
    pub fn track_statistics(self: &Arc<Self>, parent: &fuchsia_inspect::Node, name: &str) {
        let this = Arc::downgrade(self);
        parent.record_lazy_child(name, move || {
            let this_clone = this.clone();
            async move {
                let inspector = fuchsia_inspect::Inspector::default();
                if let Some(this) = this_clone.upgrade() {
                    let (required, borrowed, earliest_checkpoint) = {
                        // TODO(https://fxbug.dev/42069513): Push-back or rate-limit to prevent DoS.
                        let inner = this.inner.read();
                        let required = inner.required_reservation();
                        let borrowed =
                            inner.borrowed_metadata_space(this.metadata_reservation.get());
                        (required, borrowed, inner.earliest_journal_offset())
                    };
                    let root = inspector.root();
                    if let Some(reservation) = this.metadata_reservation.get() {
                        root.record_uint("metadata_reservation", reservation.amount());
                    }
                    root.record_uint("required_reservation", required);
                    root.record_uint("borrowed_reservation", borrowed);
                    if let Some(earliest_checkpoint) = earliest_checkpoint {
                        root.record_uint("earliest_checkpoint", earliest_checkpoint);
                    }

                    // TODO(https://fxbug.dev/42068224): Post-compute rather than manually
                    // computing metrics.
                    if let Some(x) = round_div(100 * borrowed, required) {
                        root.record_uint("borrowed_to_required_reservation_percent", x);
                    }
                }
                Ok(inspector)
            }
            .boxed()
        });
    }

    /// Normally, we make new transactions pay for overheads incurred by the journal, such as
    /// checksums and padding, but if the journal has discarded a significant amount after a replay,
    /// we run the risk of there not being enough reserved.  To handle this, if the amount is
    /// significant, we force the journal to borrow the space (using a journal created transaction).
    pub fn needs_borrow_for_journal(&self, checkpoint: u64) -> bool {
        checkpoint.checked_sub(self.inner.read().last_end_offset).unwrap() > 256
    }

    #[cfg(test)]
    pub fn store_borrowed(&self, store_id: u64) -> Option<(u64, u64)> {
        let inner = self.inner.read();
        inner.store_borrowed.contains_key(&store_id).then(|| {
            (inner.total_store_borrowed(store_id), inner.required_store_reservation(store_id))
        })
    }

    #[cfg(test)]
    pub fn set_store_borrowed(&self, store_id: u64, amount: u64) {
        let mut inner = self.inner.write();
        let unflushed_borrowed = reserved_space_from_journal_usage(
            inner.journal_checkpoints.get(&store_id).map_or(0, |c| c.borrowed_usage()),
        );
        inner.store_borrowed.insert(store_id, amount.saturating_sub(unflushed_borrowed));
    }

    #[cfg(test)]
    pub fn reserved_metadata_space(&self) -> HashMap<u64, u64> {
        let inner = self.inner.read();
        let required = inner.required_reservation();
        let total = self.metadata_reservation.get().map_or(0, |r| r.amount());
        let mut result = HashMap::default();
        for &id in inner.store_borrowed.keys() {
            result.insert(id, inner.total_store_borrowed(id));
        }
        result.insert(0, std::cmp::min(total, required));
        result
    }
}

/// `ReservationUpdate` is an associated object attached to `Mutation::EndFlush` that updates the
/// tree layer size (`layer_size`) for an object.
pub struct ReservationUpdate(u64);

impl ReservationUpdate {
    pub fn new(layer_size: u64) -> Self {
        Self(layer_size)
    }
}

impl AssociatedObject for ReservationUpdate {
    fn will_apply_mutation(&self, _mutation: &Mutation, object_id: u64, manager: &ObjectManager) {
        manager.update_layer_size(object_id, self.0);
    }
}
