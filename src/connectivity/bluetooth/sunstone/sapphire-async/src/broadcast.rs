// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::global_index::GlobalIndex;
use crate::notification::Notification;
use core::task::Poll;

use sapphire_collections::deque::Deque;

use sapphire_collections::storage::StorageFamily;
use sapphire_collections::vec::Vec;
use sapphire_sync::mutex::Mutex;
use sapphire_sync::mutex::raw::RawMutex;

/// Configuration trait for configuring the types inside a [`BroadcastChannel`].
pub trait BroadcastCfg {
    /// The storage container family used for the internal message queue buffer.
    type Buffer: StorageFamily;
    /// The storage container family used for the active subscriber list.
    type SubscriptionList: StorageFamily;
    /// The raw mutex fundamental used to synchronize internal channel state.
    type Mtx: RawMutex;
}

/// An asynchronous multi-subscriber Broadcast Channel.
///
/// `BroadcastChannel` allows a publisher to broadcast messages to multiple subscribers
/// simultaneously. It supports both growable heap-allocated message buffers and zero-heap
/// stack-allocated arrays.
///
/// The buffer reclaims space based on the slowest reader (reclaiming only elements that
/// all active subscribers have read). To prevent blocking the publisher when one subscriber
/// is slow, `force_publish` can be used to evict the oldest elements and catch up slow readers.
///
/// # Examples
///
/// Basic concurrent publishing and subscribing:
///
/// ```
/// use sapphire_async::broadcast::{BroadcastChannel, BroadcastCfg};
/// use sapphire_async::testing::TestExecutor;
/// use sapphire_async::executor::BoundedExecutor;
/// use sapphire_collections::storage::ArrayStorage;
/// use sapphire_sync::mutex::raw::SingleThreadMutex;
///
/// struct MyBroadcastCfg;
/// impl BroadcastCfg for MyBroadcastCfg {
///     type Buffer = ArrayStorage<3>;
///     type SubscriptionList = ArrayStorage<2>;
///     type Mtx = SingleThreadMutex;
/// }
///
/// # ;
/// let channel = BroadcastChannel::<i32, MyBroadcastCfg>::new();
/// let sub1 = channel.subscribe().unwrap();
/// let sub2 = channel.subscribe().unwrap();
///
/// # BoundedExecutor::new(TestExecutor::new(), |s| {
/// #     s.block_on(async {
/// channel.publish(42).await;
/// assert_eq!(sub1.next().await, Ok(42));
/// assert_eq!(sub2.next().await, Ok(42));
/// #     });
/// # });
/// ```
///
/// A `BroadcastChannel` configured with `SingleThreadMutex` cannot be shared across threads:
///
/// ```compile_fail
/// use sapphire_async::broadcast::{BroadcastChannel, BroadcastCfg};
/// use sapphire_collections::storage::ArrayStorage;
/// use sapphire_sync::mutex::raw::SingleThreadMutex;
///
/// struct SingleThreadedCfg;
///
/// impl BroadcastCfg for SingleThreadedCfg {
///     type Buffer = ArrayStorage<3>;
///     type SubscriptionList = ArrayStorage<2>;
///     type Mtx = SingleThreadMutex;
/// }
///
/// type NonSyncBroadcast = BroadcastChannel<i32, SingleThreadedCfg>;
///
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<NonSyncBroadcast>(); // Correctly fails to compile because SingleThreadMutex is !Sync
/// ```
pub struct BroadcastChannel<T, Cfg: BroadcastCfg> {
    state: Mutex<Cfg::Mtx, BroadcastChannelState<T, Cfg>>,
    not_full: Notification<Cfg::Mtx>,
    not_empty: Notification<Cfg::Mtx>,
}

/// The synchronized internal state of a [`BroadcastChannel`].
struct BroadcastChannelState<T, Cfg: BroadcastCfg> {
    queue: Deque<T, Cfg::Buffer>,
    head_global_idx: GlobalIndex,
    next_global_idx: GlobalIndex, // Where the next message will be written

    // TODO(529758875): This is not the best datastructure for this. We want something with
    // fast insertion and deletion anywhere and a `Cursor`-like API
    subscribers: Vec<SubscriberState, Cfg::SubscriptionList>,
    next_sub_id: usize,
}

/// The trackable state of an active subscriber enqueued in the channel state.
struct SubscriberState {
    id: usize,
    next_global_idx: GlobalIndex,
}

/// An active subscriber endpoint to a [`BroadcastChannel`].
///
/// Receives cloned broadcasted messages. Can be polled asynchronously via [`Subscriber::next`].
pub struct Subscriber<'a, T, Cfg: BroadcastCfg> {
    channel: &'a BroadcastChannel<T, Cfg>,
    id: usize,
}

/// Error indicating that a slow subscriber missed broadcasted messages.
///
/// Occurs when the buffer's capacity was exceeded and oldest elements were evicted
/// via `force_publish` before this subscriber could read them.
#[derive(Debug, PartialEq, Eq)]
pub struct MissedMessages {
    /// The exact number of missed messages.
    pub count: usize,
}

impl<T, Cfg: BroadcastCfg> BroadcastChannelState<T, Cfg> {
    /// Returns the lowest monotonic global index read by any active subscriber.
    ///
    /// If there are no active subscribers, returns `next_global_idx`.
    fn slowest_reader(&self) -> GlobalIndex {
        self.subscribers.iter().map(|s| s.next_global_idx).min().unwrap_or(self.next_global_idx)
    }

    /// Attempts to push a payload to the back of the queue, incrementing `next_global_idx` on success.
    ///
    /// Returns `Err(payload)` if the queue is full and cannot grow.
    fn push_back(&mut self, payload: T) -> Result<(), T> {
        self.queue.try_push_back(payload)?;
        self.next_global_idx += 1;
        Ok(())
    }

    /// Pops and returns the oldest payload from the front of the queue, incrementing
    /// `head_global_idx` on success.
    fn pop_front(&mut self) -> Option<T> {
        let item = self.queue.pop_front()?;
        self.head_global_idx += 1;
        Some(item)
    }

    /// Pops and discards all elements in the queue that have been read by all active subscribers.
    fn reclaim_space(&mut self, waker: &Notification<Cfg::Mtx>) {
        let slowest = self.slowest_reader();
        let mut reclaimed = 0;
        while self.head_global_idx < slowest {
            self.pop_front();
            reclaimed += 1;
        }
        if reclaimed > 0 {
            waker.notify_many(reclaimed);
        }
    }
}

impl<T, Cfg: BroadcastCfg> Default for BroadcastChannel<T, Cfg>
where
    Vec<SubscriberState, Cfg::SubscriptionList>: Default,
    Deque<T, Cfg::Buffer>: Default,
{
    fn default() -> Self {
        Self {
            state: Mutex::new(BroadcastChannelState {
                queue: Deque::default(),
                head_global_idx: GlobalIndex::new(0),
                next_global_idx: GlobalIndex::new(0),
                subscribers: Vec::default(),
                next_sub_id: 0,
            }),
            not_full: Notification::new(),
            not_empty: Notification::new(),
        }
    }
}

impl<T: Clone, Cfg: BroadcastCfg> BroadcastChannel<T, Cfg> {
    /// Creates a new, empty `BroadcastChannel` with the configured mutex and notification fundamentals.
    pub fn new() -> Self
    where
        Self: Default,
    {
        Self::default()
    }

    /// Subscribes to the channel, returning a [`Subscriber`] endpoint if there is slot capacity.
    ///
    /// Returns `None` if the maximum number of subscribers (defined by `SubscriptionList` capacity)
    /// has been reached.
    pub fn subscribe(&self) -> Option<Subscriber<'_, T, Cfg>> {
        let mut state = self.state.lock();
        let id = state.next_sub_id;
        state.next_sub_id += 1;

        let next_global_idx = state.next_global_idx;
        state.subscribers.try_push(SubscriberState { id, next_global_idx }).ok()?;

        Some(Subscriber { channel: self, id })
    }

    /// Publishes a message to the channel asynchronously.
    ///
    /// If the channel's buffer is at capacity, this method blocks until the slowest reader
    /// reads enough elements to reclaim space.
    pub async fn publish(&self, payload: T) {
        let mut payload = Some(payload);
        let guard = self.state.lock();

        self.not_full
            .when(guard, |state| {
                state.reclaim_space(&self.not_full);

                match state.push_back(payload.take().expect("Payload not refreshed")) {
                    Ok(()) => Poll::Ready(()),
                    Err(item) => {
                        payload.replace(item);
                        Poll::Pending
                    }
                }
            })
            .await;
        // Notify all consumers that a message is enqueued
        self.not_empty.notify_all();
    }

    /// Publishes a message to the channel, evicting the oldest message if at capacity.
    ///
    /// This method never blocks. If the channel is at capacity, the oldest message is evicted
    /// and slow readers will miss it, returning `Err(MissedMessages)` on their next poll.
    pub fn force_publish(&self, payload: T) {
        let mut state = self.state.lock();
        state.reclaim_space(&self.not_full);

        match state.push_back(payload) {
            Ok(()) => {}
            Err(val) => {
                if state.pop_front().is_some() {
                    state.push_back(val).unwrap_or_else(|_| panic!("Should succeed after popping"));
                }
            }
        }

        // Notify all consumers that a message is enqueued
        self.not_empty.notify_all();
    }
}

impl<'a, T: Clone, Cfg: BroadcastCfg> Subscriber<'a, T, Cfg> {
    /// Asynchronously polls and retrieves the next broadcasted message.
    pub async fn next(&self) -> Result<T, MissedMessages> {
        let guard = self.channel.state.lock();

        let res = self
            .channel
            .not_empty
            .when(guard, |state| {
                let sub = state
                    .subscribers
                    .iter_mut()
                    .find(|s| s.id == self.id)
                    .expect("Subscriber not found");

                if sub.next_global_idx < state.head_global_idx {
                    let missed = (state.head_global_idx - sub.next_global_idx) as usize;
                    sub.next_global_idx = state.head_global_idx;
                    return Poll::Ready(Err(MissedMessages { count: missed }));
                }

                if sub.next_global_idx < state.next_global_idx {
                    let logical_idx = (sub.next_global_idx - state.head_global_idx) as usize;
                    let item = state.queue.get(logical_idx).expect("Valid logical index");
                    let payload = item.clone();
                    sub.next_global_idx += 1;
                    state.reclaim_space(&self.channel.not_full);
                    Poll::Ready(Ok(payload))
                } else {
                    Poll::Pending
                }
            })
            .await;

        res
    }
}

impl<'a, T, Cfg: BroadcastCfg> Drop for Subscriber<'a, T, Cfg> {
    fn drop(&mut self) {
        let mut state = self.channel.state.lock();
        if let Some(idx) = state.subscribers.iter().position(|s| s.id == self.id) {
            state.subscribers.remove(idx);
        }
        state.reclaim_space(&self.channel.not_full);
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::executor::BoundedExecutor;
    use crate::testing::TestExecutor;
    use sapphire_sync::mutex::raw::SingleThreadMutex;

    use sapphire_collections::storage::ArrayStorage;

    struct StackCfg<const B: usize, const S: usize>;
    impl<const B: usize, const S: usize> BroadcastCfg for StackCfg<B, S> {
        type Buffer = ArrayStorage<B>;
        type SubscriptionList = ArrayStorage<S>;
        type Mtx = SingleThreadMutex;
    }

    type StackBroadcast<T, const B: usize, const S: usize> = BroadcastChannel<T, StackCfg<B, S>>;

    #[test]
    fn test_broadcast_basic() {
        let channel = StackBroadcast::<i32, 10, 2>::new();

        let sub1 = channel.subscribe().unwrap();
        let sub2 = channel.subscribe().unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                channel.publish(42).await;

                let v1 = sub1.next().await.unwrap();
                let v2 = sub2.next().await.unwrap();

                assert_eq!(v1, 42);
                assert_eq!(v2, 42);
            });
        });
    }

    #[test]
    fn test_broadcast_blocking_publisher() {
        // Capacity 1, max 2 subscribers
        let channel = StackBroadcast::<i32, 1, 2>::new();
        let sub = channel.subscribe().unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                channel.publish(1).await; // Should succeed immediately
            });

            let handle = s.spawn(async {
                channel.publish(2).await; // Should block because capacity is 1 and `sub` hasn't read `1`
            });

            s.run_until_stalled();
            assert!(!handle.is_finished(), "Publisher should be blocked");

            s.block_on(async {
                let val = sub.next().await.unwrap();
                assert_eq!(val, 1);
            });

            s.run_until_stalled();
            assert!(handle.is_finished(), "Publisher should be unblocked after read");
        });
    }

    #[test]
    fn test_broadcast_force_publish() {
        // Capacity 1, max 2 subscribers
        let channel = StackBroadcast::<i32, 1, 2>::new();
        let sub1 = channel.subscribe().unwrap();
        let sub2 = channel.subscribe().unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                channel.publish(10).await; // Bounded queue is now full
                let r1 = sub1.next().await;
                assert_eq!(r1, Ok(10));
            });

            // Force publish 20, which should discard 10
            channel.force_publish(20);

            s.block_on(async {
                // Both subscribers should report missed messages
                let r1 = sub1.next().await;
                let r2 = sub2.next().await;

                assert_eq!(r1, Ok(20));
                assert_eq!(r2, Err(MissedMessages { count: 1 }));

                // Next read should get the new message
                let r2 = sub2.next().await;

                assert_eq!(r2, Ok(20));
            });
        });
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_broadcast_growable() {
        use sapphire_collections::storage::Global;

        struct StdCfg;
        impl BroadcastCfg for StdCfg {
            type Buffer = Global;
            type SubscriptionList = Global;
            type Mtx = SingleThreadMutex;
        }
        type StdBroadcast<T> = BroadcastChannel<T, StdCfg>;

        let channel = StdBroadcast::<i32>::new();
        let sub = channel.subscribe().unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                // Publish multiple messages. The queue should grow automatically
                // even if the subscriber hasn't read them yet.
                channel.publish(1).await;
                channel.publish(2).await;
                channel.publish(3).await;

                assert_eq!(sub.next().await.unwrap(), 1);
                assert_eq!(sub.next().await.unwrap(), 2);
                assert_eq!(sub.next().await.unwrap(), 3);
            });
        });
    }

    mod proptests {
        use super::*;
        use crate::executor::BoundedExecutor;
        use crate::testing::TestExecutor;
        use proptest::prelude::*;

        use std::collections::VecDeque;

        #[derive(Debug, Clone)]
        enum BroadcastOp {
            Publish(i32),
            ForcePublish(i32),
            RecvSub1,
            RecvSub2,
        }

        type TestBroadcast = StackBroadcast<i32, 2, 2>;

        proptest! {
            #[test]
            fn test_broadcast_proptest(
                ops in prop::collection::vec(
                    prop_oneof![
                        any::<i32>().prop_map(BroadcastOp::Publish),
                        any::<i32>().prop_map(BroadcastOp::ForcePublish),
                        Just(BroadcastOp::RecvSub1),
                        Just(BroadcastOp::RecvSub2),
                    ],
                    0..50
                )
            ) {
                let channel = TestBroadcast::new();
                let sub1 = channel.subscribe().unwrap();
                let sub2 = channel.subscribe().unwrap();

                let mut expected_vals = VecDeque::new();
                let mut next_global_idx = 0;
                let mut head_global_idx = 0;
                let mut sub1_next = 0;
                let mut sub2_next = 0;

                BoundedExecutor::new(TestExecutor::new(), |s| {
                    for op in ops {
                        let cur_len = next_global_idx - head_global_idx;

                        match op {
                            BroadcastOp::Publish(val) => {
                                if cur_len < 2 {
                                    s.block_on(channel.publish(val));
                                    expected_vals.push_back((next_global_idx, val));
                                    next_global_idx += 1;
                                }
                            }
                            BroadcastOp::ForcePublish(val) => {
                                channel.force_publish(val);
                                if cur_len == 2 {
                                    expected_vals.pop_front();
                                    head_global_idx += 1;
                                }
                                expected_vals.push_back((next_global_idx, val));
                                next_global_idx += 1;
                            }
                            BroadcastOp::RecvSub1 => {
                                if sub1_next < next_global_idx {
                                    let res = s.block_on(sub1.next());
                                    if sub1_next < head_global_idx {
                                        let missed = head_global_idx - sub1_next;
                                        assert_eq!(res, Err(MissedMessages { count: missed }));
                                        sub1_next = head_global_idx;
                                    } else {
                                        let expected_val = expected_vals.iter()
                                            .find(|(idx, _)| *idx == sub1_next)
                                            .map(|(_, val)| *val)
                                            .unwrap();
                                        assert_eq!(res, Ok(expected_val));
                                        sub1_next += 1;
                                    }
                                    head_global_idx = std::cmp::max(head_global_idx, std::cmp::min(sub1_next, sub2_next));
                                    while expected_vals.front().map(|(idx, _)| *idx < head_global_idx).unwrap_or(false) {
                                        expected_vals.pop_front();
                                    }
                                }
                            }
                            BroadcastOp::RecvSub2 => {
                                if sub2_next < next_global_idx {
                                    let res = s.block_on(sub2.next());
                                    if sub2_next < head_global_idx {
                                        let missed = head_global_idx - sub2_next;
                                        assert_eq!(res, Err(MissedMessages { count: missed }));
                                        sub2_next = head_global_idx;
                                    } else {
                                        let expected_val = expected_vals.iter()
                                            .find(|(idx, _)| *idx == sub2_next)
                                            .map(|(_, val)| *val)
                                            .unwrap();
                                        assert_eq!(res, Ok(expected_val));
                                        sub2_next += 1;
                                    }
                                    head_global_idx = std::cmp::max(head_global_idx, std::cmp::min(sub1_next, sub2_next));
                                    while expected_vals.front().map(|(idx, _)| *idx < head_global_idx).unwrap_or(false) {
                                        expected_vals.pop_front();
                                    }
                                }
                            }
                        }
                    }
                });
            }
        }

        #[cfg(feature = "std")]
        use sapphire_collections::storage::Global;

        #[cfg(feature = "std")]
        struct GrowableCfg;
        #[cfg(feature = "std")]
        impl BroadcastCfg for GrowableCfg {
            type Buffer = Global;
            type SubscriptionList = Global;
            type Mtx = SingleThreadMutex;
        }

        #[cfg(feature = "std")]
        type GrowableBroadcast = BroadcastChannel<i32, GrowableCfg>;

        #[cfg(feature = "std")]
        proptest! {
            #[test]
            fn test_broadcast_growable_proptest(
                ops in prop::collection::vec(
                    prop_oneof![
                        any::<i32>().prop_map(BroadcastOp::Publish),
                        any::<i32>().prop_map(BroadcastOp::ForcePublish),
                        Just(BroadcastOp::RecvSub1),
                        Just(BroadcastOp::RecvSub2),
                    ],
                    0..50
                )
            ) {
                let channel = GrowableBroadcast::new();
                let sub1 = channel.subscribe().unwrap();
                let sub2 = channel.subscribe().unwrap();

                let mut expected_vals = VecDeque::new();
                let mut next_global_idx = 0;
                let mut head_global_idx = 0;
                let mut sub1_next = 0;
                let mut sub2_next = 0;

                BoundedExecutor::new(TestExecutor::new(), |s| {
                    for op in ops {
                        match op {
                            BroadcastOp::Publish(val) => {
                                s.block_on(channel.publish(val));
                                expected_vals.push_back((next_global_idx, val));
                                next_global_idx += 1;
                            }
                            BroadcastOp::ForcePublish(val) => {
                                channel.force_publish(val);
                                expected_vals.push_back((next_global_idx, val));
                                next_global_idx += 1;
                            }
                            BroadcastOp::RecvSub1 => {
                                if sub1_next < next_global_idx {
                                    let res = s.block_on(sub1.next());
                                    assert!(sub1_next >= head_global_idx);
                                    let expected_val = expected_vals.iter()
                                        .find(|(idx, _)| *idx == sub1_next)
                                        .map(|(_, val)| *val)
                                        .unwrap();
                                    assert_eq!(res, Ok(expected_val));
                                    sub1_next += 1;

                                    head_global_idx = std::cmp::max(head_global_idx, std::cmp::min(sub1_next, sub2_next));
                                    while expected_vals.front().map(|(idx, _)| *idx < head_global_idx).unwrap_or(false) {
                                        expected_vals.pop_front();
                                    }
                                }
                            }
                            BroadcastOp::RecvSub2 => {
                                if sub2_next < next_global_idx {
                                    let res = s.block_on(sub2.next());
                                    assert!(sub2_next >= head_global_idx);
                                    let expected_val = expected_vals.iter()
                                        .find(|(idx, _)| *idx == sub2_next)
                                        .map(|(_, val)| *val)
                                        .unwrap();
                                    assert_eq!(res, Ok(expected_val));
                                    sub2_next += 1;

                                    head_global_idx = std::cmp::max(head_global_idx, std::cmp::min(sub1_next, sub2_next));
                                    while expected_vals.front().map(|(idx, _)| *idx < head_global_idx).unwrap_or(false) {
                                        expected_vals.pop_front();
                                    }
                                }
                            }
                        }
                    }
                });
            }
        }
    }
}
