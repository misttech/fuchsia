// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use sapphire_collections::deque::Deque;
use sapphire_collections::map::HashMap;
use sapphire_sync::mutex::Mutex;

use crate::global_index::GlobalIndex;
use crate::notification::Notification;

use super::{BroadcastCfg, MissedMessages, SubId};

/// The outcome of evaluating a subscriber's interest in a broadcast payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    /// The subscriber is interested in receiving the payload.
    Interested,
    /// The subscriber is not interested and will skip the payload without being woken.
    Uninterested,
}

impl Interest {
    /// Returns `true` if the filter variant is [`Interest::Interested`].
    pub fn is_interested(self) -> bool {
        matches!(self, Interest::Interested)
    }
}

/// An asynchronous multi-subscriber Broadcast Channel with per-subscriber synchronous message filtering.
///
/// `FilteredBroadcastChannel` allows a publisher to broadcast messages to subscribers while invoking a
/// synchronous filter `fn(&T) -> Interest` for each subscriber. If a subscriber filter returns `Interest::Uninterested`,
/// the subscriber's global index is incremented to skip the message without waking the subscriber task.
///
/// # Examples
///
/// Basic concurrent publishing with per-subscriber filtering:
///
/// ```
/// use sapphire_async::broadcast::{FilteredBroadcastChannel, BroadcastCfg, Interest};
/// use sapphire_async::testing::TestExecutor;
/// use sapphire_async::executor::BoundedExecutor;
/// use sapphire_collections::storage::ArrayStorage;
/// use sapphire_sync::mutex::raw::SingleThreadMutex;
///
/// struct MyBroadcastCfg;
/// impl BroadcastCfg for MyBroadcastCfg {
///     type Buffer = ArrayStorage<3>;
///     type SubscriptionStore = ArrayStorage<2>;
///     type Mtx = SingleThreadMutex;
/// }
///
/// # ;
/// let channel = FilteredBroadcastChannel::<i32, MyBroadcastCfg>::new();
/// let sub_evens = channel
///     .subscribe(|&x| if x % 2 == 0 { Interest::Interested } else { Interest::Uninterested })
///     .unwrap();
/// let sub_all = channel.subscribe(|_| Interest::Interested).unwrap();
///
/// # BoundedExecutor::new(TestExecutor::new(), |s| {
/// #     s.block_on(async {
/// channel.publish(41).await;
/// channel.publish(42).await;
/// assert_eq!(sub_evens.next().await, Ok(42));
/// assert_eq!(sub_all.next().await, Ok(41));
/// assert_eq!(sub_all.next().await, Ok(42));
/// #     });
/// # });
/// ```
///
/// A `FilteredBroadcastChannel` configured with `SingleThreadMutex` cannot be shared across threads:
///
/// ```compile_fail
/// use sapphire_async::broadcast::{FilteredBroadcastChannel, BroadcastCfg};
/// use sapphire_collections::storage::ArrayStorage;
/// use sapphire_sync::mutex::raw::SingleThreadMutex;
///
/// struct SingleThreadedCfg;
///
/// impl BroadcastCfg for SingleThreadedCfg {
///     type Buffer = ArrayStorage<3>;
///     type SubscriptionStore = ArrayStorage<2>;
///     type Mtx = SingleThreadMutex;
/// }
///
/// type NonSyncBroadcast = FilteredBroadcastChannel<i32, SingleThreadedCfg>;
///
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<NonSyncBroadcast>(); // Correctly fails to compile because SingleThreadMutex is !Sync
/// ```
pub struct FilteredBroadcastChannel<T, Cfg: BroadcastCfg> {
    state: Mutex<Cfg::Mtx, FilteredBroadcastChannelState<T, Cfg>>,
    not_full: Notification<Cfg::Mtx>,
}

/// The synchronized internal state of a [`FilteredBroadcastChannel`].
struct FilteredBroadcastChannelState<T, Cfg: BroadcastCfg> {
    queue: Deque<T, Cfg::Buffer>,
    head_global_idx: GlobalIndex,
    next_global_idx: GlobalIndex,
    subscribers: HashMap<SubId, FilteredSubscriberState<T>, Cfg::SubscriptionStore>,
    next_sub_id: usize,
}

/// The trackable state of an active subscriber in a [`FilteredBroadcastChannel`].
struct FilteredSubscriberState<T> {
    next_global_idx: GlobalIndex,
    filter: fn(&T) -> Interest,
    waker: Option<Waker>,
}

/// An active subscriber endpoint to a [`FilteredBroadcastChannel`].
pub struct FilteredSubscriber<'a, T, Cfg: BroadcastCfg> {
    channel: &'a FilteredBroadcastChannel<T, Cfg>,
    id: SubId,
}

/// A custom future returned by [`FilteredSubscriber::next`].
pub struct NextFuture<'a, 's, T, Cfg: BroadcastCfg> {
    subscriber: &'s FilteredSubscriber<'a, T, Cfg>,
}

impl<T, Cfg: BroadcastCfg> FilteredBroadcastChannelState<T, Cfg> {
    fn slowest_reader(&self) -> GlobalIndex {
        self.subscribers.values().map(|s| s.next_global_idx).min().unwrap_or(self.next_global_idx)
    }

    fn force_push_back(&mut self, payload: T) -> Option<T> {
        let prev = self.queue.force_push_back(payload);
        if prev.is_some() {
            self.head_global_idx += 1;
        }
        self.next_global_idx += 1;
        prev
    }

    fn push_back(&mut self, payload: T) -> Result<(), T> {
        self.queue.try_push_back(payload)?;
        self.next_global_idx += 1;
        Ok(())
    }

    fn pop_front(&mut self) -> Option<T> {
        let item = self.queue.pop_front()?;
        self.head_global_idx += 1;
        Some(item)
    }

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

impl<T, Cfg: BroadcastCfg> Default for FilteredBroadcastChannel<T, Cfg>
where
    HashMap<SubId, FilteredSubscriberState<T>, Cfg::SubscriptionStore>: Default,
    Deque<T, Cfg::Buffer>: Default,
{
    fn default() -> Self {
        Self {
            state: Mutex::new(FilteredBroadcastChannelState {
                queue: Deque::default(),
                head_global_idx: GlobalIndex::new(0),
                next_global_idx: GlobalIndex::new(0),
                subscribers: HashMap::default(),
                next_sub_id: 0,
            }),
            not_full: Notification::new(),
        }
    }
}

impl<T: Clone, Cfg: BroadcastCfg> FilteredBroadcastChannel<T, Cfg> {
    /// Creates a new, empty `FilteredBroadcastChannel`.
    pub fn new() -> Self
    where
        Self: Default,
    {
        Self::default()
    }

    /// Subscribes to the channel with a synchronous filter function.
    ///
    /// ## Why `fn()` and not `Fn()`
    ///
    /// We use `fn(&T) -> Interest` in order to support heterogenous filters accorss subscribers
    /// while still enabling a fixed-allocation scheme.
    pub fn subscribe(&self, filter: fn(&T) -> Interest) -> Option<FilteredSubscriber<'_, T, Cfg>> {
        let mut state = self.state.lock();
        let id = SubId::new(state.next_sub_id);
        state.next_sub_id += 1;

        let next_global_idx = state.next_global_idx;
        state
            .subscribers
            .try_insert(id, FilteredSubscriberState { next_global_idx, filter, waker: None })
            .ok()?;

        Some(FilteredSubscriber { channel: self, id })
    }

    /// Publishes a payload to the filtered channel.
    pub async fn publish(&self, payload: T) {
        let mut payload = Some(payload);
        let guard = self.state.lock();

        self.not_full
            .when(guard, |state| {
                state.reclaim_space(&self.not_full);

                let item = payload.take().expect("Payload not refreshed");
                let msg_idx = state.next_global_idx;
                match state.push_back(item.clone()) {
                    Ok(()) => {
                        for sub in state.subscribers.values_mut() {
                            if sub.next_global_idx == msg_idx {
                                if (sub.filter)(&item).is_interested() {
                                    if let Some(waker) = sub.waker.take() {
                                        waker.wake();
                                    }
                                } else {
                                    sub.next_global_idx += 1;
                                }
                            } else {
                                if let Some(waker) = sub.waker.take() {
                                    waker.wake();
                                }
                            }
                        }
                        Poll::Ready(())
                    }
                    Err(item) => {
                        payload.replace(item);
                        Poll::Pending
                    }
                }
            })
            .await;
    }

    /// Publishes a payload, evicting the oldest message if full.
    pub fn force_publish(&self, payload: T) {
        let mut state = self.state.lock();
        state.reclaim_space(&self.not_full);

        let msg_idx = state.next_global_idx;
        state.force_push_back(payload.clone());
        for sub in state.subscribers.values_mut() {
            // If the subscriber is caught up and ready to read this specific message,
            // apply the filter. If uninterested, skip it silently. If interested, notify it.
            // If the subscriber is behind (not caught up), we notify it unconditionally so
            // it can wake up and process its backlog in the queue.
            if sub.next_global_idx == msg_idx {
                if (sub.filter)(&payload).is_interested() {
                    if let Some(waker) = sub.waker.take() {
                        waker.wake();
                    }
                } else {
                    sub.next_global_idx += 1;
                }
            } else {
                if let Some(waker) = sub.waker.take() {
                    waker.wake();
                }
            }
        }
    }
}

impl<'a, T: Clone, Cfg: BroadcastCfg> FilteredSubscriber<'a, T, Cfg> {
    /// Asynchronously polls and retrieves the next broadcasted message that matched the filter.
    pub fn next<'s>(&'s self) -> NextFuture<'a, 's, T, Cfg> {
        NextFuture { subscriber: self }
    }
}

impl<'a, 's, T: Clone, Cfg: BroadcastCfg> Future for NextFuture<'a, 's, T, Cfg> {
    type Output = Result<T, MissedMessages>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.subscriber.channel.state.lock();

        let head = state.head_global_idx;
        let next = state.next_global_idx;

        let state = &mut *state;
        let sub = state.subscribers.get_mut(&self.subscriber.id).expect("Subscriber not found");

        if sub.next_global_idx < head {
            let missed = (head - sub.next_global_idx) as usize;
            sub.next_global_idx = head;
            return Poll::Ready(Err(MissedMessages { count: missed }));
        }

        let filter = sub.filter;
        let mut current_idx = sub.next_global_idx;
        let mut found_payload = None;

        while current_idx < next {
            let logical_idx = (current_idx - head) as usize;
            // If the subscriber has fallen so far behind that modular index arithmetic
            // wrapped around (or if logical_idx is out of bounds), queue.get will return None.
            // Instead of panicking, reset the subscriber to head_global_idx and report
            // usize::MAX missed messages since the exact count is unbounded.
            let Some(item) = state.queue.get(logical_idx) else {
                sub.next_global_idx = head;
                state.reclaim_space(&self.subscriber.channel.not_full);
                return Poll::Ready(Err(MissedMessages { count: usize::MAX }));
            };
            let is_interested = filter(item).is_interested();
            current_idx += 1;

            if is_interested {
                found_payload = Some(item.clone());
                break;
            }
        }

        if let Some(payload) = found_payload {
            sub.next_global_idx = current_idx;
            state.reclaim_space(&self.subscriber.channel.not_full);
            return Poll::Ready(Ok(payload));
        }

        sub.next_global_idx = current_idx;
        sub.waker = Some(cx.waker().clone());
        state.reclaim_space(&self.subscriber.channel.not_full);
        Poll::Pending
    }
}

impl<'a, 's, T, Cfg: BroadcastCfg> Drop for NextFuture<'a, 's, T, Cfg> {
    fn drop(&mut self) {
        let mut state = self.subscriber.channel.state.lock();
        if let Some(sub) = state.subscribers.get_mut(&self.subscriber.id) {
            sub.waker = None;
        }
    }
}

impl<'a, T, Cfg: BroadcastCfg> Drop for FilteredSubscriber<'a, T, Cfg> {
    fn drop(&mut self) {
        let mut state = self.channel.state.lock();
        state.subscribers.remove(&self.id);
        state.reclaim_space(&self.channel.not_full);
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::executor::BoundedExecutor;
    use crate::testing::TestExecutor;
    use sapphire_collections::storage::ArrayStorage;
    use sapphire_sync::mutex::raw::SingleThreadMutex;

    struct StackCfg<const B: usize, const S: usize>;
    impl<const B: usize, const S: usize> BroadcastCfg for StackCfg<B, S> {
        type Buffer = ArrayStorage<B>;
        type SubscriptionStore = ArrayStorage<S>;
        type Mtx = SingleThreadMutex;
    }

    #[test]
    fn test_filtered_broadcast_basic() {
        type TestFilteredChannel = FilteredBroadcastChannel<i32, StackCfg<10, 2>>;
        let channel = TestFilteredChannel::new();

        let sub1 = channel
            .subscribe(|x| if x % 2 == 0 { Interest::Interested } else { Interest::Uninterested })
            .unwrap();

        let sub2 = channel
            .subscribe(|x| if x % 2 != 0 { Interest::Interested } else { Interest::Uninterested })
            .unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                channel.publish(1).await;
                channel.publish(2).await;
                channel.publish(3).await;
                channel.publish(4).await;

                assert_eq!(sub1.next().await, Ok(2));
                assert_eq!(sub1.next().await, Ok(4));

                assert_eq!(sub2.next().await, Ok(1));
                assert_eq!(sub2.next().await, Ok(3));
            });
        });
    }

    #[test]
    fn test_filtered_broadcast_selective_wake() {
        type TestFilteredChannel = FilteredBroadcastChannel<i32, StackCfg<10, 2>>;
        let channel = TestFilteredChannel::new();

        let sub1 = channel
            .subscribe(|x| if *x == 42 { Interest::Interested } else { Interest::Uninterested })
            .unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            let h1 = s.spawn(async move { sub1.next().await });

            s.run_until_stalled();
            assert!(!h1.is_finished());

            s.block_on(async {
                channel.publish(10).await;
            });
            s.run_until_stalled();
            assert!(!h1.is_finished());

            s.block_on(async {
                channel.publish(42).await;
            });
            s.run_until_stalled();
            assert!(h1.is_finished());
        });
    }

    #[test]
    fn test_filtered_broadcast_force_publish() {
        type TestFilteredChannel = FilteredBroadcastChannel<i32, StackCfg<1, 2>>;
        let channel = TestFilteredChannel::new();

        let sub1 = channel
            .subscribe(|x| if x % 2 == 0 { Interest::Interested } else { Interest::Uninterested })
            .unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                channel.publish(2).await;
            });

            channel.force_publish(4);

            s.block_on(async {
                assert_eq!(sub1.next().await, Err(MissedMessages { count: 1 }));
                assert_eq!(sub1.next().await, Ok(4));
            });
        });
    }

    #[test]
    fn test_filtered_broadcast_blocking_publisher_unblock_on_read() {
        // Capacity 1, max 2 subscribers
        type TestFilteredChannel = FilteredBroadcastChannel<i32, StackCfg<1, 2>>;
        let channel = TestFilteredChannel::new();

        let sub1 = channel
            .subscribe(|x| if *x % 2 == 0 { Interest::Interested } else { Interest::Uninterested })
            .unwrap();
        let sub2 = channel.subscribe(|_| Interest::Interested).unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                channel.publish(1).await; // sub1 skips (uninterested), sub2 keeps (interested)
            });

            // Channel buffer has msg 1 (read by sub1, but unread by sub2). Buffer is full (capacity 1).
            let handle = s.spawn(async {
                channel.publish(2).await; // Should block because sub2 hasn't read 1
            });

            s.run_until_stalled();
            assert!(!handle.is_finished(), "Publisher should be blocked waiting for sub2");

            s.block_on(async {
                let val = sub2.next().await.unwrap();
                assert_eq!(val, 1);
            });

            s.run_until_stalled();
            assert!(handle.is_finished(), "Publisher should be unblocked after sub2 reads");

            s.block_on(async {
                assert_eq!(sub1.next().await, Ok(2));
                assert_eq!(sub2.next().await, Ok(2));
            });
        });
    }

    #[test]
    fn test_filtered_broadcast_blocking_publisher_unblock_on_skip() {
        // Capacity 2, max 2 subscribers
        type TestFilteredChannel = FilteredBroadcastChannel<i32, StackCfg<2, 2>>;
        let channel = TestFilteredChannel::new();

        let sub1 = channel
            .subscribe(|x| if *x % 2 == 0 { Interest::Interested } else { Interest::Uninterested })
            .unwrap();
        let sub2 = channel
            .subscribe(|x| if *x % 2 == 0 { Interest::Interested } else { Interest::Uninterested })
            .unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                channel.publish(2).await; // Both interested, sub1 and sub2 stay at idx 0
                channel.publish(3).await; // Both behind (at idx 0 != 1), so 3 is enqueued without advancing sub indices
            });

            // Queue now has [2, 3] and is full (capacity 2).
            let handle1 = s.spawn(async {
                channel.publish(4).await;
            });
            s.run_until_stalled();
            assert!(!handle1.is_finished(), "Publisher should block when queue is full");

            // sub1 and sub2 read 2
            s.block_on(async {
                assert_eq!(sub1.next().await, Ok(2));
                assert_eq!(sub2.next().await, Ok(2));
            });

            // Space for 2 is reclaimed; publisher unblocks and publishes 4.
            s.run_until_stalled();
            assert!(handle1.is_finished(), "Publisher should unblock after reading 2");

            // Queue now has [3, 4] and is full (capacity 2). Both subscribers are at idx 1 (pointing to 3).
            let handle2 = s.spawn(async {
                channel.publish(6).await;
            });
            s.run_until_stalled();
            assert!(!handle2.is_finished(), "Publisher should block when queue is full again");

            // sub1 and sub2 call next(), which skips 3 (uninterested) and reads 4 (interested)
            s.block_on(async {
                assert_eq!(sub1.next().await, Ok(4));
                assert_eq!(sub2.next().await, Ok(4));
            });

            // Space for 3 and 4 is reclaimed by subscribers skipping 3 and reading 4; publisher unblocks and publishes 6.
            s.run_until_stalled();
            assert!(
                handle2.is_finished(),
                "Publisher should unblock after subscribers skip 3 and read 4"
            );

            s.block_on(async {
                assert_eq!(sub1.next().await, Ok(6));
                assert_eq!(sub2.next().await, Ok(6));
            });
        });
    }

    #[test]
    fn test_filtered_broadcast_drop_subscriber_unblocks_publisher() {
        // Capacity 1, max 2 subscribers
        type TestFilteredChannel = FilteredBroadcastChannel<i32, StackCfg<1, 2>>;
        let channel = TestFilteredChannel::new();

        let sub1 = channel.subscribe(|_| Interest::Interested).unwrap();
        let sub2 = channel.subscribe(|_| Interest::Interested).unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                channel.publish(1).await;
                assert_eq!(sub1.next().await, Ok(1));
            });

            // sub1 read 1 (sub1 at idx 1), sub2 did not read 1 (sub2 at idx 0).
            // Buffer contains [1] and is full (capacity 1).
            let handle = s.spawn(async {
                channel.publish(2).await;
            });
            s.run_until_stalled();
            assert!(!handle.is_finished(), "Publisher should be blocked waiting for sub2");

            // Dropping sub2 reclaims space held by sub2
            drop(sub2);

            s.run_until_stalled();
            assert!(handle.is_finished(), "Publisher should be unblocked after sub2 is dropped");

            s.block_on(async {
                assert_eq!(sub1.next().await, Ok(2));
            });
        });
    }

    #[test]
    fn test_filtered_broadcast_drop_all_subscribers_unblocks_publisher() {
        // Capacity 1, max 1 subscriber
        type TestFilteredChannel = FilteredBroadcastChannel<i32, StackCfg<1, 1>>;
        let channel = TestFilteredChannel::new();

        let sub = channel.subscribe(|_| Interest::Interested).unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                channel.publish(10).await;
            });

            let handle = s.spawn(async {
                channel.publish(20).await;
            });
            s.run_until_stalled();
            assert!(!handle.is_finished(), "Publisher should be blocked");

            drop(sub);

            s.run_until_stalled();
            assert!(
                handle.is_finished(),
                "Publisher should be unblocked after all subscribers dropped"
            );
        });
    }

    #[test]
    fn test_filtered_broadcast_drop_next_future_pending_and_poll_again() {
        type TestFilteredChannel = FilteredBroadcastChannel<i32, StackCfg<5, 2>>;
        let channel = TestFilteredChannel::new();

        let sub = channel
            .subscribe(|x| if *x % 2 == 0 { Interest::Interested } else { Interest::Uninterested })
            .unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            // 1. Spawn a task that polls sub.next() while channel is empty.
            let handle = s.spawn(async { sub.next().await });
            s.run_until_stalled();
            assert!(!handle.is_finished(), "NextFuture should be pending");

            // 2. Cancel/drop the task while NextFuture is pending.
            handle.cancel();

            // 3. Verify sub.waker was cleared by checking internal state.
            {
                let state = channel.state.lock();
                let sub_state = state.subscribers.get(&sub.id).unwrap();
                assert!(sub_state.waker.is_none(), "Waker should be cleared on NextFuture drop");
            }

            // 4. Spawn a new task polling sub.next().
            let handle2 = s.spawn(async { sub.next().await });
            s.run_until_stalled();
            assert!(!handle2.is_finished(), "New NextFuture should be pending");

            // 5. Publish uninterested message (1).
            s.block_on(async {
                channel.publish(1).await;
            });
            s.run_until_stalled();
            assert!(!handle2.is_finished(), "Should not wake on uninterested message");

            // 6. Publish interested message (2).
            s.block_on(async {
                channel.publish(2).await;
            });
            s.run_until_stalled();
            assert!(handle2.is_finished(), "Should wake and finish on interested message");

            s.block_on(async {
                assert_eq!(handle2.join().await, Ok(2));
            });
        });
    }

    #[test]
    fn test_filtered_broadcast_drop_next_future_with_unfiltered_backlog() {
        type TestFilteredChannel = FilteredBroadcastChannel<i32, StackCfg<5, 2>>;
        let channel = TestFilteredChannel::new();

        let sub = channel
            .subscribe(|x| if *x % 2 == 0 { Interest::Interested } else { Interest::Uninterested })
            .unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                channel.publish(1).await;
                channel.publish(3).await;
            });

            // Start next() future: should be pending because no even numbers.
            let handle = s.spawn(async { sub.next().await });
            s.run_until_stalled();
            assert!(!handle.is_finished());

            // Cancel the future
            handle.cancel();

            // Publish another odd and an even
            s.block_on(async {
                channel.publish(5).await;
                channel.publish(6).await;
            });

            // Next read should find 6
            s.block_on(async {
                assert_eq!(sub.next().await, Ok(6));
            });
        });
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_filtered_broadcast_growable() {
        use sapphire_collections::storage::Global;

        struct StdCfg;
        impl BroadcastCfg for StdCfg {
            type Buffer = Global;
            type SubscriptionStore = Global;
            type Mtx = SingleThreadMutex;
        }
        type StdBroadcast<T> = FilteredBroadcastChannel<T, StdCfg>;

        let channel = StdBroadcast::<i32>::new();
        let sub1 = channel
            .subscribe(|x| if *x % 2 == 0 { Interest::Interested } else { Interest::Uninterested })
            .unwrap();
        let sub2 = channel
            .subscribe(|x| if *x % 2 != 0 { Interest::Interested } else { Interest::Uninterested })
            .unwrap();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                channel.publish(1).await;
                channel.publish(2).await;
                channel.publish(3).await;
                channel.publish(4).await;

                assert_eq!(sub1.next().await.unwrap(), 2);
                assert_eq!(sub1.next().await.unwrap(), 4);

                assert_eq!(sub2.next().await.unwrap(), 1);
                assert_eq!(sub2.next().await.unwrap(), 3);
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

        type TestBroadcast = FilteredBroadcastChannel<i32, StackCfg<2, 2>>;

        fn filter1(x: &i32) -> Interest {
            if *x % 2 == 0 { Interest::Interested } else { Interest::Uninterested }
        }

        fn filter2(x: &i32) -> Interest {
            if *x % 3 == 0 { Interest::Interested } else { Interest::Uninterested }
        }

        proptest! {
            #[test]
            fn test_filtered_broadcast_proptest(
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
                let sub1 = channel.subscribe(filter1).unwrap();
                let sub2 = channel.subscribe(filter2).unwrap();

                let mut expected_vals = VecDeque::new();
                let mut next_global_idx = 0;
                let mut head_global_idx = 0;
                let mut sub1_next = 0;
                let mut sub2_next = 0;

                BoundedExecutor::new(TestExecutor::new(), |s| {
                    for op in ops {
                        head_global_idx = std::cmp::max(head_global_idx, std::cmp::min(sub1_next, sub2_next));
                        while expected_vals.front().map(|(idx, _)| *idx < head_global_idx).unwrap_or(false) {
                            expected_vals.pop_front();
                        }
                        let cur_len = next_global_idx - head_global_idx;

                        match op {
                            BroadcastOp::Publish(val) => {
                                if cur_len < 2 {
                                    s.block_on(channel.publish(val));
                                    if sub1_next == next_global_idx && !filter1(&val).is_interested() {
                                        sub1_next += 1;
                                    }
                                    if sub2_next == next_global_idx && !filter2(&val).is_interested() {
                                        sub2_next += 1;
                                    }
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
                                if sub1_next == next_global_idx && !filter1(&val).is_interested() {
                                    sub1_next += 1;
                                }
                                if sub2_next == next_global_idx && !filter2(&val).is_interested() {
                                    sub2_next += 1;
                                }
                                expected_vals.push_back((next_global_idx, val));
                                next_global_idx += 1;
                            }
                            BroadcastOp::RecvSub1 => {
                                if sub1_next < head_global_idx {
                                    let res = s.block_on(sub1.next());
                                    let missed = head_global_idx - sub1_next;
                                    assert_eq!(res, Err(MissedMessages { count: missed }));
                                    sub1_next = head_global_idx;
                                    head_global_idx = std::cmp::max(head_global_idx, std::cmp::min(sub1_next, sub2_next));
                                    while expected_vals.front().map(|(idx, _)| *idx < head_global_idx).unwrap_or(false) {
                                        expected_vals.pop_front();
                                    }
                                } else if sub1_next < next_global_idx {
                                    let matching = expected_vals
                                        .iter()
                                        .find(|(idx, val)| *idx >= sub1_next && filter1(val).is_interested());
                                    if let Some(&(match_idx, match_val)) = matching {
                                        let res = s.block_on(sub1.next());
                                        assert_eq!(res, Ok(match_val));
                                        sub1_next = match_idx + 1;
                                        head_global_idx = std::cmp::max(head_global_idx, std::cmp::min(sub1_next, sub2_next));
                                        while expected_vals.front().map(|(idx, _)| *idx < head_global_idx).unwrap_or(false) {
                                            expected_vals.pop_front();
                                        }
                                    }
                                }
                            }
                            BroadcastOp::RecvSub2 => {
                                if sub2_next < head_global_idx {
                                    let res = s.block_on(sub2.next());
                                    let missed = head_global_idx - sub2_next;
                                    assert_eq!(res, Err(MissedMessages { count: missed }));
                                    sub2_next = head_global_idx;
                                    head_global_idx = std::cmp::max(head_global_idx, std::cmp::min(sub1_next, sub2_next));
                                    while expected_vals.front().map(|(idx, _)| *idx < head_global_idx).unwrap_or(false) {
                                        expected_vals.pop_front();
                                    }
                                } else if sub2_next < next_global_idx {
                                    let matching = expected_vals
                                        .iter()
                                        .find(|(idx, val)| *idx >= sub2_next && filter2(val).is_interested());
                                    if let Some(&(match_idx, match_val)) = matching {
                                        let res = s.block_on(sub2.next());
                                        assert_eq!(res, Ok(match_val));
                                        sub2_next = match_idx + 1;
                                        head_global_idx = std::cmp::max(head_global_idx, std::cmp::min(sub1_next, sub2_next));
                                        while expected_vals.front().map(|(idx, _)| *idx < head_global_idx).unwrap_or(false) {
                                            expected_vals.pop_front();
                                        }
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
            type SubscriptionStore = Global;
            type Mtx = SingleThreadMutex;
        }

        #[cfg(feature = "std")]
        type GrowableBroadcast = FilteredBroadcastChannel<i32, GrowableCfg>;

        #[cfg(feature = "std")]
        proptest! {
            #[test]
            fn test_filtered_broadcast_growable_proptest(
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
                let sub1 = channel.subscribe(filter1).unwrap();
                let sub2 = channel.subscribe(filter2).unwrap();

                let mut expected_vals = VecDeque::new();
                let mut next_global_idx = 0;
                let mut head_global_idx = 0;
                let mut sub1_next = 0;
                let mut sub2_next = 0;

                BoundedExecutor::new(TestExecutor::new(), |s| {
                    for op in ops {
                        head_global_idx = std::cmp::max(head_global_idx, std::cmp::min(sub1_next, sub2_next));
                        while expected_vals.front().map(|(idx, _)| *idx < head_global_idx).unwrap_or(false) {
                            expected_vals.pop_front();
                        }

                        match op {
                            BroadcastOp::Publish(val) => {
                                s.block_on(channel.publish(val));
                                if sub1_next == next_global_idx && !filter1(&val).is_interested() {
                                    sub1_next += 1;
                                }
                                if sub2_next == next_global_idx && !filter2(&val).is_interested() {
                                    sub2_next += 1;
                                }
                                expected_vals.push_back((next_global_idx, val));
                                next_global_idx += 1;
                            }
                            BroadcastOp::ForcePublish(val) => {
                                channel.force_publish(val);
                                if sub1_next == next_global_idx && !filter1(&val).is_interested() {
                                    sub1_next += 1;
                                }
                                if sub2_next == next_global_idx && !filter2(&val).is_interested() {
                                    sub2_next += 1;
                                }
                                expected_vals.push_back((next_global_idx, val));
                                next_global_idx += 1;
                            }
                            BroadcastOp::RecvSub1 => {
                                if sub1_next < next_global_idx {
                                    assert!(sub1_next >= head_global_idx);
                                    let matching = expected_vals
                                        .iter()
                                        .find(|(idx, val)| *idx >= sub1_next && filter1(val).is_interested());
                                    if let Some(&(match_idx, match_val)) = matching {
                                        let res = s.block_on(sub1.next());
                                        assert_eq!(res, Ok(match_val));
                                        sub1_next = match_idx + 1;

                                        head_global_idx = std::cmp::max(head_global_idx, std::cmp::min(sub1_next, sub2_next));
                                        while expected_vals.front().map(|(idx, _)| *idx < head_global_idx).unwrap_or(false) {
                                            expected_vals.pop_front();
                                        }
                                    }
                                }
                            }
                            BroadcastOp::RecvSub2 => {
                                if sub2_next < next_global_idx {
                                    assert!(sub2_next >= head_global_idx);
                                    let matching = expected_vals
                                        .iter()
                                        .find(|(idx, val)| *idx >= sub2_next && filter2(val).is_interested());
                                    if let Some(&(match_idx, match_val)) = matching {
                                        let res = s.block_on(sub2.next());
                                        assert_eq!(res, Ok(match_val));
                                        sub2_next = match_idx + 1;

                                        head_global_idx = std::cmp::max(head_global_idx, std::cmp::min(sub1_next, sub2_next));
                                        while expected_vals.front().map(|(idx, _)| *idx < head_global_idx).unwrap_or(false) {
                                            expected_vals.pop_front();
                                        }
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
