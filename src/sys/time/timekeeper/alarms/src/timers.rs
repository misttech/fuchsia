// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use log::error;
use std::cell::RefCell;
use std::cmp;
use std::collections::{BTreeMap, BinaryHeap, HashSet};
use std::rc::Rc;
use time_pretty::{format_duration, format_timer};
use {fidl, fidl_fuchsia_time_alarms as fta, fuchsia_async as fasync, fuchsia_trace as trace, zx};

pub(crate) trait Responder: std::fmt::Debug {
    fn send(
        &self,
        alarm_id: &str,
        result: Result<fidl::EventPair, fta::WakeAlarmsError>,
    ) -> Option<Result<(), fidl::Error>>;
}

impl Responder for RefCell<Option<fta::WakeAlarmsSetAndWaitResponder>> {
    fn send(
        &self,
        _alarm_id: &str,
        result: Result<fidl::EventPair, fta::WakeAlarmsError>,
    ) -> Option<Result<(), fidl::Error>> {
        self.borrow_mut().take().map(|responder| responder.send(result))
    }
}

impl Responder for RefCell<Option<fidl::endpoints::ClientEnd<fta::NotifierMarker>>> {
    fn send(
        &self,
        alarm_id: &str,
        result: Result<fidl::EventPair, fta::WakeAlarmsError>,
    ) -> Option<Result<(), fidl::Error>> {
        self.borrow_mut().take().map(|notifier| {
            let proxy = notifier.into_proxy();
            match result {
                Ok(keep_alive) => proxy.notify(alarm_id, keep_alive),
                Err(e) => proxy.notify_error(alarm_id, e),
            }
        })
    }
}

/// Items common for all nodes.
#[derive(Debug)]
struct CommonNode {
    // The Node's unique identifier.
    node_id: Id,
    /// The responder that is blocked until the timer expires.  Used to notify
    /// the alarms subsystem client when this alarm expires.
    responder: Rc<dyn Responder>,
}

impl Drop for CommonNode {
    // If the Node was evicted without having expired, notify the other
    // end that the timer has been canceled.
    fn drop(&mut self) {
        // If the Node is dropped, notify the client that may have
        // been waiting. We can not drop a responder, because that kills
        // the FIDL connection.
        if let Some(Err(e)) =
            self.responder.send(self.node_id.alarm(), Err(fta::WakeAlarmsError::Dropped))
        {
            error!("could not drop responder: {:?}", e);
        }
    }
}

/// A representation of the state of a single Timer that has an absolute deadline on the boot
/// timeline.
#[derive(Debug)]
pub(crate) struct BootNode {
    /// The common elements of all Node-like structs.
    common: CommonNode,
    /// The deadline at which the timer expires.
    deadline: fasync::BootInstant,
}

impl BootNode {
    pub fn new(
        deadline: fasync::BootInstant,
        alarm_id: String,
        conn_id: zx::Koid,
        responder: Rc<dyn Responder>,
    ) -> Self {
        Self { common: CommonNode { node_id: Id { alarm_id, conn: conn_id }, responder }, deadline }
    }

    pub fn get_deadline(&self) -> &fasync::BootInstant {
        &self.deadline
    }

    pub fn id(&self) -> &Id {
        &self.common.node_id
    }

    pub fn get_responder(&self) -> Rc<dyn Responder> {
        self.common.responder.clone()
    }
}

/// This and other comparison trait implementation are needed to establish
/// a total ordering of Nodes.
impl std::cmp::Eq for BootNode {}

impl std::cmp::PartialEq for BootNode {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.id() == other.id()
    }
}

impl std::cmp::PartialOrd for BootNode {
    /// Order by deadline first, but timers with same deadline are ordered
    /// by respective IDs to avoid ordering nondeterminism.
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BootNode {
    /// Compares two [BootNode]s, by "which is sooner".
    ///
    /// Ties are broken by alarm ID, then by connection ID.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let ordering = other.deadline.cmp(&self.deadline);
        if ordering == std::cmp::Ordering::Equal {
            let ordering = self.id().alarm().cmp(other.id().alarm());
            if ordering == std::cmp::Ordering::Equal {
                self.id().conn.cmp(&other.id().conn)
            } else {
                ordering
            }
        } else {
            ordering
        }
    }
}

/// The unique alarm ID associated with a timer.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub(crate) struct Id {
    alarm_id: String,
    /// Connection identifier, unique per each FIDL connection.
    pub conn: zx::Koid,
}

impl Id {
    pub fn new(alarm: String, conn: zx::Koid) -> Self {
        Self { alarm_id: alarm, conn }
    }

    /// Connection-unique alarm ID.
    pub fn alarm(&self) -> &str {
        &self.alarm_id[..]
    }
}

// Compute a trace ID for a given alarm ID. This identifier is used across
// processes for tracking the alarm's lifetime.
pub fn get_trace_id(alarm_id: &str) -> trace::Id {
    if let Some(rest) = alarm_id.strip_prefix("starnix:Koid(") {
        if let Some((koid_str, _)) = rest.split_once(')') {
            if let Ok(trace_id) = koid_str.parse::<u64>() {
                return trace_id.into();
            }
        }
    }

    // For now, other components don't have a specific way to get the trace id.
    0.into()
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TimerId[alarm_id:{},conn_id:{:?}]", self.alarm(), self.conn)
    }
}

/// Contains all the timers known by the alarms subsystem.
///
/// [Heap] can efficiently find a timer with the earliest deadline,
/// and given a cutoff can expire one timer for which the deadline has
/// passed.
pub(crate) struct Heap {
    timers: BinaryHeap<BootNode>,
    active_timers: HashSet<Id>,
}

impl std::fmt::Display for Heap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let now = fasync::BootInstant::now();
        let sorted = self
            .timers
            .iter()
            .map(|n| (n.get_deadline().clone(), n.id().alarm_id.clone()))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .map(|(k, v)| {
                let remaining = k - now;
                format!(
                    "Timeout: {} => timer_id: {}, remaining: {}",
                    format_timer(k.into()),
                    v,
                    format_duration(remaining.into())
                )
            })
            .collect::<Vec<_>>();
        let joined = sorted.join("\n\t");
        write!(f, "\n\t{}", joined)
    }
}

impl Heap {
    /// Creates an empty [Heap].
    pub fn new() -> Self {
        Self { timers: BinaryHeap::new(), active_timers: HashSet::new() }
    }

    /// Adds a [Node] to [Heap].
    ///
    /// If the inserted node is identical to an already existing node, then
    /// nothing is changed.  If the deadline is different, then the timer node
    /// is replaced.
    pub fn push(&mut self, new_node: BootNode) {
        let new_id = new_node.id();
        if let Some(_) = self.active_timers.get(&new_id) {
            // Else replace. The deadline may be pushed out or pulled in.
            self.active_timers.insert(new_id.clone());
            self.timers.retain(|t| t.id() != new_node.id());
            self.timers.push(new_node);
        } else {
            // New timer node.
            self.active_timers.insert(new_id.clone());
            self.timers.push(new_node);
        }
    }

    /// Returns a reference to the timer with the earliest deadline.
    ///
    /// If no such timer exists, `None` is returned.
    pub fn peek_node(&self) -> Option<&BootNode> {
        self.timers.peek()
    }

    /// Returns the deadline of the proximate timer in [Timers].
    ///
    /// A shorthand for extracting deadline from the node.
    pub fn peek_deadline(&self) -> Option<fasync::BootInstant> {
        self.peek_node().map(|t| t.deadline)
    }

    /// Args:
    /// - `now` is the current time.
    /// - `deadline` is the timer deadline to check for expiry.
    pub fn expired(now: fasync::BootInstant, deadline: fasync::BootInstant) -> bool {
        deadline <= now
    }

    /// Returns true if there are no known timers.
    pub fn is_empty(&self) -> bool {
        let empty1 = self.timers.is_empty();
        let empty2 = self.active_timers.is_empty();
        assert!(empty1 == empty2, "broken invariant: empty1: {} empty2:{}", empty1, empty2);
        empty1
    }

    /// Attempts to expire the earliest timer.
    ///
    /// If a timer is expired, it is removed from [Timers] and returned to the caller. Note that
    /// there may be more timers that need expiring at the provided `reference instant`. To drain
    /// [Timers] of all expired timers, one must repeat the call to this method with the same
    /// value of `reference_instant` until it returns `None`.
    ///
    /// Args:
    /// - `now`: the time instant to compare the stored timers against.  Timers for
    ///   which the deadline has been reached or surpassed are eligible for expiry.
    pub fn maybe_expire_earliest(&mut self, now: fasync::BootInstant) -> Option<BootNode> {
        self.peek_deadline()
            .map(|d| {
                if Heap::expired(now, d) {
                    self.timers.pop().map(|e| {
                        self.active_timers.remove(&e.id());
                        e
                    })
                } else {
                    None
                }
            })
            .flatten()
    }

    /// Removes an alarm by ID.  If the earliest alarm is the alarm to be removed,
    /// it is returned.
    pub fn remove_by_id(&mut self, timer_id: &Id) -> Option<BootNode> {
        let ret = if let Some(t) = self.peek_node().map(|node| node.id()) {
            if *t == *timer_id {
                self.timers.pop()
            } else {
                None
            }
        } else {
            None
        };

        self.timers.retain(|t| *t.id() != *timer_id);
        self.active_timers.remove(timer_id);
        ret
    }

    /// Returns the number of currently pending timers.
    pub fn timer_count(&self) -> usize {
        let count1 = self.timers.len();
        let count2 = self.active_timers.len();
        assert!(count1 == count2, "broken invariant: count1: {}, count2: {}", count1, count2);
        count1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::AsHandleRef;
    use fuchsia_async as fasync;
    use std::rc::Rc;
    use test_case::test_case;
    use test_util::assert_gt;

    #[derive(Debug)]
    struct FakeResponder {}

    impl Responder for FakeResponder {
        fn send(
            &self,
            _alarm_id: &str,
            _result: Result<fidl::EventPair, fidl_fuchsia_time_alarms::WakeAlarmsError>,
        ) -> Option<Result<(), fidl::Error>> {
            None
        }
    }

    // Make sure the first alarm is always one with a greater koid value.
    #[test_case(
        "alarm1",
        fasync::BootInstant::from_nanos(42),
        "alarm2",
        fasync::BootInstant::from_nanos(42),
        std::cmp::Ordering::Less ; "same deadline tie broken by alarm id"
    )]
    #[test_case(
        "alarm1",
        fasync::BootInstant::from_nanos(42),
        "alarm2",
        fasync::BootInstant::from_nanos(43),
        std::cmp::Ordering::Greater ; "sooner deadline is greater"
    )]
    #[test_case(
        "alarm",
        fasync::BootInstant::from_nanos(42),
        "alarm",
        fasync::BootInstant::from_nanos(42),
        std::cmp::Ordering::Greater ; "same deadline and alarm id tie broken by connection ID"
    )]
    #[test_case(
        "alarm",
        fasync::BootInstant::from_nanos(42),
        "alarm",
        fasync::BootInstant::from_nanos(43),
        std::cmp::Ordering::Greater ; "different deadlines, sooner is 'greater'"
    )]
    fn test_timer_node_comparison_by_alarm_id(
        id1: &str,
        deadline1: fasync::BootInstant,
        id2: &str,
        deadline2: fasync::BootInstant,
        expected: std::cmp::Ordering,
    ) {
        let (koid1, koid2) = {
            let koid1 = zx::Event::create().as_handle_ref().get_koid().unwrap();
            let koid2 = zx::Event::create().as_handle_ref().get_koid().unwrap();
            (std::cmp::max(koid1, koid2), std::cmp::min(koid1, koid2))
        };

        assert_gt!(koid1, koid2);

        let one = BootNode::new(deadline1, id1.into(), koid1, Rc::new(FakeResponder {}));
        let other = BootNode::new(deadline2, id2.into(), koid2, Rc::new(FakeResponder {}));

        assert_eq!(expected, one.cmp(&other), "one={one:?}, other={other:?}");
    }
}
