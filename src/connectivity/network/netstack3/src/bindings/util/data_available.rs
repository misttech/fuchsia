// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A multiple-producer, single-consumer notification mechanism.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

use futures::Future;
use futures::task::AtomicWaker;

#[derive(Debug, Default)]
struct DataAvailable {
    available: AtomicBool,
    waker: AtomicWaker,
}

/// The notifier side of the underlying data availability signal.
///
/// Notifiers can be cloned to allow for multiple current producers.
#[derive(Debug, Clone)]
pub(crate) struct DataNotifier {
    inner: Arc<DataAvailable>,
}

impl DataNotifier {
    /// Notifies the watcher that data is available.
    ///
    /// A notification is latched until cleared by [`DataWatcher::reset`];
    /// waiting does not clear it. Multiple notifications are coalesced.
    pub(crate) fn notify(&self) {
        let DataAvailable { available, waker } = &*self.inner;

        let prev = available.swap(true, Ordering::Relaxed);
        if !prev {
            waker.wake();
        }
    }
}

/// The receiver side of the underlying data availability signal.
///
/// The watcher is used to wait for notifications from one or more
/// [`DataNotifier`]s.
#[derive(Debug)]
pub(crate) struct DataWatcher {
    inner: Arc<DataAvailable>,
}

impl DataWatcher {
    /// Creates a new watcher and notifier pair.
    pub(crate) fn new() -> (Self, DataNotifier) {
        let watcher = DataWatcher { inner: Arc::new(DataAvailable::default()) };
        let notifier = DataNotifier { inner: Arc::clone(&watcher.inner) };
        (watcher, notifier)
    }

    /// Clears any pending data availability notification.
    pub(crate) fn reset(&self) {
        let DataAvailable { available, waker: _ } = &*self.inner;
        available.store(false, Ordering::Relaxed);
    }

    /// Returns a future that completes when data availability is observed.
    pub(crate) fn wait(&self) -> impl Future<Output = ()> + use<'_> {
        let DataAvailable { available, waker } = &*self.inner;

        futures::future::poll_fn(|cx| {
            if available.load(Ordering::Relaxed) {
                return Poll::Ready(());
            }

            waker.register(cx.waker());

            // Check again after registering the waker to avoid a race where a notifier
            // flipped the flag after we checked it but before we registered to be woken up,
            // which would result in a lost notification.
            if available.load(Ordering::Relaxed) {
                return Poll::Ready(());
            }
            Poll::Pending
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications() {
        let mut exec = fuchsia_async::TestExecutor::new();

        let (watcher, tcp) = DataWatcher::new();
        let udp = tcp.clone();

        // A notification that arrives before the watcher waits is observed by
        // the wait, and waiting does not consume it.
        tcp.notify();
        assert_eq!(exec.run_until_stalled(&mut watcher.wait()), Poll::Ready(()));
        assert_eq!(exec.run_until_stalled(&mut watcher.wait()), Poll::Ready(()));

        // Unless it is cleared before the watcher waits.
        watcher.reset();
        let mut fut = watcher.wait();
        assert_eq!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // If we notify after the watcher wait has been initialized, it should wake the
        // watcher future.
        tcp.notify();
        assert_eq!(exec.run_until_stalled(&mut fut), Poll::Ready(()));
        drop(fut);

        // We can notify arbitrarily many times on the same notifier or on arbitrarily
        // many notifiers attached to the same watcher, and regardless it should result
        // in the future waking.
        watcher.reset();
        let mut fut = watcher.wait();
        assert_eq!(exec.run_until_stalled(&mut fut), Poll::Pending);
        tcp.notify();
        udp.notify();
        tcp.notify();
        assert_eq!(exec.run_until_stalled(&mut fut), Poll::Ready(()));
        drop(fut);

        // But the notifications are coalesced, so a subsequent wait after
        // clearing should not complete.
        watcher.reset();
        let mut fut = watcher.wait();
        assert_eq!(exec.run_until_stalled(&mut fut), Poll::Pending);
    }
}
