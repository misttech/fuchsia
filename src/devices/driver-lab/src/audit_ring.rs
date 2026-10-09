// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Bounded target-side audit ring.
//!
//! Sequence numbers increase monotonically for the life of the proxy
//! instance. Wrap overwrites the oldest entries but never reuses or resets
//! sequence numbers, so a host reading from a stale cursor can detect the
//! gap. The ring is in-memory: undrained entries are lost if the driver
//! host crashes, which is why hosts drain incrementally during mutating
//! plans.

use crate::access_policy::{Denial, ResourceId};
use std::collections::VecDeque;
use std::fmt;

/// The policy outcome recorded for an attempted operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The operation passed every policy layer.
    Allowed,
    /// The operation was rejected without touching hardware.
    Denied(Denial),
}

/// Execution outcome recorded alongside the policy decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpStatus {
    /// The operation completed.
    Ok,
    /// Policy rejected the operation; hardware was not touched.
    Rejected,
    /// The backend failed after policy allowed the operation.
    BackendFault,
}

/// One audit record. Callers supply bounded, normalized values; the ring
/// never stores pointers, physical addresses, handles, or unbounded caller
/// strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRecord {
    /// Session the operation belonged to; `None` for lifecycle events.
    pub session: Option<u64>,
    /// Resource the operation targeted, when applicable.
    pub resource: Option<ResourceId>,
    /// Normalized operation name from a fixed vocabulary, e.g. `"read32"`
    /// or `"driver_start"`.
    pub operation: &'static str,
    /// Byte offset for MMIO operations.
    pub offset: Option<u64>,
    /// Policy decision.
    pub decision: Decision,
    /// Execution outcome.
    pub status: OpStatus,
    /// Value read or written, when applicable.
    pub value: Option<u32>,
    /// Target monotonic timestamp in nanoseconds, supplied by the driver
    /// layer so this crate stays host-testable.
    pub timestamp_ns: i64,
    /// Host run identifier, recorded on session-open records (success and
    /// rejection) where the session-to-run association is established.
    pub run_id: Option<String>,
    /// Index of the item within a multi-item operation (for example a
    /// snapshot), when applicable. Plan-level operation indexes are host
    /// knowledge; the target records only what it can observe.
    pub item_index: Option<u32>,
}

impl AuditRecord {
    /// A session-independent lifecycle event such as driver start or stop.
    pub fn lifecycle(operation: &'static str, timestamp_ns: i64) -> Self {
        Self {
            session: None,
            resource: None,
            operation,
            offset: None,
            decision: Decision::Allowed,
            status: OpStatus::Ok,
            value: None,
            timestamp_ns,
            run_id: None,
            item_index: None,
        }
    }
}

/// An audit record together with its assigned sequence number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// Monotonically increasing sequence number.
    pub seq: u64,
    /// The record.
    pub record: AuditRecord,
}

/// One bounded page returned from [`AuditRing::read`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditPage {
    /// Entries with `seq >= cursor`, oldest first, at most `limit` of them.
    pub entries: Vec<AuditEntry>,
    /// Oldest sequence still retained, or `None` if the ring is empty. A
    /// cursor older than this indicates a gap: those entries were
    /// overwritten before they were drained.
    pub oldest_retained: Option<u64>,
    /// Cursor to pass to the next read.
    pub next_cursor: u64,
}

/// Callback invoked with each record and its assigned sequence number as
/// the record is appended.
///
/// Observers run synchronously on the appending thread, typically while
/// the caller holds the proxy state lock, so they must be cheap and must
/// not call back into the ring. The driver layer uses this to mirror audit
/// records into system tracing while keeping this crate free of Fuchsia
/// dependencies.
pub type AuditObserver = Box<dyn Fn(u64, &AuditRecord) + Send + Sync>;

/// Fixed-capacity audit ring.
pub struct AuditRing {
    capacity: usize,
    next_seq: u64,
    entries: VecDeque<AuditEntry>,
    observer: Option<AuditObserver>,
}

impl fmt::Debug for AuditRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuditRing")
            .field("capacity", &self.capacity)
            .field("next_seq", &self.next_seq)
            .field("entries", &self.entries)
            .field("has_observer", &self.observer.is_some())
            .finish()
    }
}

impl AuditRing {
    /// Creates a ring holding at most `capacity` entries.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "audit ring capacity must be nonzero");
        Self { capacity, next_seq: 0, entries: VecDeque::with_capacity(capacity), observer: None }
    }

    /// Installs `observer`, replacing any previous one. It sees every
    /// record appended from this point on, including records that are
    /// later overwritten before a host drains them.
    pub fn set_observer(&mut self, observer: AuditObserver) {
        self.observer = Some(observer);
    }

    /// Whether an observer is installed.
    pub fn has_observer(&self) -> bool {
        self.observer.is_some()
    }

    /// Appends a record, overwriting the oldest entry when full, and
    /// returns the assigned sequence number.
    pub fn append(&mut self, record: AuditRecord) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        if let Some(observer) = &self.observer {
            observer(seq, &record);
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(AuditEntry { seq, record });
        seq
    }

    /// The oldest sequence number still retained.
    pub fn oldest_retained(&self) -> Option<u64> {
        self.entries.front().map(|entry| entry.seq)
    }

    /// The sequence number the next appended record will receive.
    pub fn next_sequence(&self) -> u64 {
        self.next_seq
    }

    /// The fixed entry capacity, for capability reporting.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Reads at most `limit` entries with `seq >= cursor`, oldest first.
    pub fn read(&self, cursor: u64, limit: usize) -> AuditPage {
        let entries: Vec<AuditEntry> =
            self.entries.iter().filter(|entry| entry.seq >= cursor).take(limit).cloned().collect();
        let next_cursor = entries.last().map_or(cursor, |entry| entry.seq + 1);
        AuditPage { entries, oldest_retained: self.oldest_retained(), next_cursor }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(op: &'static str) -> AuditRecord {
        AuditRecord::lifecycle(op, 0)
    }

    #[test]
    fn sequences_are_monotonic() {
        let mut ring = AuditRing::new(4);
        for expected in 0..10 {
            assert_eq!(ring.append(record("op")), expected);
        }
        assert_eq!(ring.next_sequence(), 10);
    }

    #[test]
    fn wrap_discards_oldest_but_keeps_sequence() {
        let mut ring = AuditRing::new(3);
        for _ in 0..5 {
            ring.append(record("op"));
        }
        // Entries 0 and 1 were overwritten; 2, 3, 4 remain.
        assert_eq!(ring.oldest_retained(), Some(2));
        let page = ring.read(0, 10);
        let seqs: Vec<u64> = page.entries.iter().map(|entry| entry.seq).collect();
        assert_eq!(seqs, vec![2, 3, 4]);
        // A cursor of 0 against oldest_retained of 2 is a detectable gap.
        assert_eq!(page.oldest_retained, Some(2));
    }

    #[test]
    fn pagination_via_next_cursor() {
        let mut ring = AuditRing::new(10);
        for _ in 0..6 {
            ring.append(record("op"));
        }
        let first = ring.read(0, 4);
        assert_eq!(first.entries.len(), 4);
        assert_eq!(first.next_cursor, 4);
        let second = ring.read(first.next_cursor, 4);
        let seqs: Vec<u64> = second.entries.iter().map(|entry| entry.seq).collect();
        assert_eq!(seqs, vec![4, 5]);
        assert_eq!(second.next_cursor, 6);
        // Reading past the end returns nothing and does not advance.
        let done = ring.read(second.next_cursor, 4);
        assert!(done.entries.is_empty());
        assert_eq!(done.next_cursor, 6);
    }

    #[test]
    fn empty_ring_reads_empty() {
        let ring = AuditRing::new(4);
        let page = ring.read(0, 4);
        assert!(page.entries.is_empty());
        assert_eq!(page.oldest_retained, None);
        assert_eq!(page.next_cursor, 0);
    }

    #[test]
    fn observer_sees_every_append_with_its_sequence() {
        use std::sync::{Arc, Mutex};

        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut ring = AuditRing::new(2);
        ring.append(record("before_observer"));
        let seen_by_observer = Arc::clone(&seen);
        ring.set_observer(Box::new(move |seq, record| {
            seen_by_observer.lock().unwrap().push((seq, record.operation));
        }));
        for op in ["a", "b", "c"] {
            ring.append(record(op));
        }
        // The observer sees records appended after installation, including
        // "a", which wrap overwrote before any host could drain it.
        assert_eq!(*seen.lock().unwrap(), vec![(1, "a"), (2, "b"), (3, "c")]);
        let page = ring.read(0, 10);
        let ops: Vec<&str> = page.entries.iter().map(|entry| entry.record.operation).collect();
        assert_eq!(ops, vec!["b", "c"]);
    }

    #[test]
    fn debug_reports_observer_presence() {
        let mut ring = AuditRing::new(1);
        assert!(!ring.has_observer());
        ring.set_observer(Box::new(|_, _| {}));
        assert!(ring.has_observer());
        assert!(format!("{ring:?}").contains("has_observer: true"));
    }

    #[test]
    #[should_panic(expected = "capacity must be nonzero")]
    fn zero_capacity_panics() {
        let _ = AuditRing::new(0);
    }
}
