// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Queue for datagram-like sockets.

use std::collections::VecDeque;
use std::num::NonZeroUsize;

use log::trace;
use netstack3_core::types::BufferSizeSettings;
use thiserror::Error;

use crate::bindings::util::DataNotifier;

#[derive(Copy, Clone, Debug, Error, Eq, PartialEq)]
#[error("application buffers are full")]
pub(crate) struct NoSpace;

/// A trait abstracting types that are notified of the queue being readable.
///
/// Upon creation, the listener must assume to be *not* readable.
pub(crate) trait QueueReadableListener {
    /// Notifies the listener of a readable change.
    fn on_readable_changed(&mut self, readable: bool);
}

/// A trait abstracting types that are notified about the queue having an error.
///
/// Upon creation, the listener must assume there's *no* error.
pub(crate) trait QueueErrorListener {
    /// Notifies the listener of an error on the socket.
    fn on_error_changed(&mut self, error: bool);
}

#[derive(Debug)]
pub(crate) struct MessageQueue<M, L> {
    listener: L,
    notifier: Option<DataNotifier>,
    queue: AvailableMessageQueue<M>,
    pub(crate) pending_error: Option<netstack3_core::PendingDatagramSocketError>,
}

impl<M, L> MessageQueue<M, L> {
    pub(crate) fn new(
        listener: L,
        notifier: Option<DataNotifier>,
        max_available_messages_size: NonZeroUsize,
    ) -> Self {
        Self {
            listener,
            notifier,
            queue: AvailableMessageQueue::new(max_available_messages_size),
            pending_error: None,
        }
    }

    pub(crate) fn peek(&self) -> Option<&M> {
        let Self { queue, listener: _, notifier: _, pending_error: _ } = self;
        queue.peek()
    }

    pub(crate) fn listener_mut(&mut self) -> &mut L {
        &mut self.listener
    }

    pub(crate) fn max_available_messages_size(&self) -> NonZeroUsize {
        let Self { listener: _, notifier: _, queue, pending_error: _ } = self;
        queue.max_available_messages_size
    }

    pub(crate) fn set_max_available_messages_size(
        &mut self,
        new_size: usize,
        settings: &BufferSizeSettings<NonZeroUsize>,
    ) {
        let Self { listener: _, notifier: _, queue, pending_error: _ } = self;
        let new_size = NonZeroUsize::new(new_size).unwrap_or_else(|| settings.min());
        queue.max_available_messages_size = settings.clamp(new_size);
    }

    #[cfg(test)]
    pub(crate) fn available_messages(&self) -> impl ExactSizeIterator<Item = &M> {
        let Self {
            listener: _,
            notifier: _,
            queue:
                AvailableMessageQueue {
                    available_messages,
                    available_messages_size: _,
                    max_available_messages_size: _,
                },
            pending_error: _,
        } = self;
        available_messages.iter()
    }
}

impl<M, L: QueueReadableListener> MessageQueue<M, L> {
    pub(crate) fn pop(&mut self) -> Option<M>
    where
        M: BodyLen,
    {
        let Self { queue, listener, notifier: _, pending_error: _ } = self;
        let message = queue.pop();
        // NB: Only notify the listener when the queue was not empty before to
        // avoid hitting the listener twice with the same signal.
        if queue.is_empty() && message.is_some() {
            listener.on_readable_changed(false);
        }
        message
    }

    pub(crate) fn receive(&mut self, message: M) -> Result<(), NoSpace>
    where
        M: BodyLen,
    {
        let Self { queue, listener, notifier, pending_error: _ } = self;
        let body_len = message.body_len();
        let queue_was_empty = queue.is_empty();
        match queue.push(message) {
            Err(NoSpace) => {
                trace!("dropping {}-byte packet because the receive queue is full", body_len);
                Err(NoSpace)
            }
            Ok(()) => {
                // NB: If the queue is non-empty, it would be redundant to
                // signal the event. Avoid the unnecessary syscall.
                // This is a safe optimization because signals are only set
                // on the event while holding an `&mut MessageQueue`.
                if queue_was_empty {
                    listener.on_readable_changed(true);
                }
                if let Some(notifier) = notifier {
                    notifier.notify();
                }
                Ok(())
            }
        }
    }
}

impl<M, L: QueueErrorListener> MessageQueue<M, L> {
    pub(crate) fn take_pending_error(
        &mut self,
    ) -> Option<netstack3_core::PendingDatagramSocketError> {
        let err = self.pending_error.take();
        if err.is_some() {
            self.listener.on_error_changed(false);
        }
        err
    }

    pub(crate) fn set_pending_error(&mut self, err: netstack3_core::PendingDatagramSocketError) {
        // Unconditionally overwrite any existing pending error. This matches
        // Linux's behavior.
        self.pending_error = Some(err);
        self.listener.on_error_changed(true);
    }
}

#[derive(Debug)]
struct AvailableMessageQueue<M> {
    available_messages: VecDeque<M>,
    /// The total size of the contents of `available_messages`.
    available_messages_size: usize,
    /// The maximum allowed value for `available_messages_size`.
    max_available_messages_size: NonZeroUsize,
}

pub(crate) trait BodyLen {
    fn body_len(&self) -> usize;

    /// The number of bytes charged against the queue's capacity for this
    /// message.
    ///
    /// In addition to the body, this accounts for the in-memory size of the
    /// message itself so that messages with empty (or tiny) bodies cannot be
    /// queued without bound. This is akin to Linux accounting for an skb's
    /// `truesize` rather than just its payload length.
    fn queued_size(&self) -> usize {
        core::mem::size_of_val(self).saturating_add(self.body_len())
    }
}

impl<M> AvailableMessageQueue<M> {
    pub(crate) fn new(max_available_messages_size: NonZeroUsize) -> Self {
        Self {
            available_messages: Default::default(),
            available_messages_size: 0,
            max_available_messages_size,
        }
    }

    pub(crate) fn push(&mut self, message: M) -> Result<(), NoSpace>
    where
        M: BodyLen,
    {
        let Self { available_messages, available_messages_size, max_available_messages_size } =
            self;

        // Respect the configured limit except if this would be the only message
        // in the buffer. This is compatible with Linux behavior.
        let len = message.queued_size();
        if *available_messages_size + len > max_available_messages_size.get()
            && !available_messages.is_empty()
        {
            return Err(NoSpace);
        }

        available_messages.push_back(message);
        *available_messages_size += len;
        Ok(())
    }

    pub(crate) fn pop(&mut self) -> Option<M>
    where
        M: BodyLen,
    {
        let Self { available_messages, available_messages_size, max_available_messages_size: _ } =
            self;

        available_messages.pop_front().map(|msg| {
            *available_messages_size -= msg.queued_size();
            msg
        })
    }

    pub(crate) fn peek(&self) -> Option<&M> {
        let Self { available_messages, available_messages_size: _, max_available_messages_size: _ } =
            self;
        available_messages.front()
    }

    pub(crate) fn is_empty(&self) -> bool {
        let Self { available_messages, available_messages_size: _, max_available_messages_size: _ } =
            self;
        available_messages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use netstack3_core::PendingDatagramSocketError;

    struct MockListener {
        error_signaled: bool,
    }

    impl QueueErrorListener for MockListener {
        fn on_error_changed(&mut self, on: bool) {
            self.error_signaled = on;
        }
    }

    #[test]
    fn message_queue_error() {
        let listener = MockListener { error_signaled: false };
        let mut mq = MessageQueue::<(), _>::new(listener, None, NonZeroUsize::new(1024).unwrap());

        assert_eq!(mq.take_pending_error(), None);
        assert_eq!(mq.listener.error_signaled, false);

        mq.set_pending_error(PendingDatagramSocketError::NetworkUnreachable);
        assert_eq!(mq.listener.error_signaled, true);

        mq.set_pending_error(PendingDatagramSocketError::HostUnreachable);
        assert_eq!(mq.listener.error_signaled, true);
        assert_eq!(mq.pending_error, Some(PendingDatagramSocketError::HostUnreachable));

        assert_eq!(mq.take_pending_error(), Some(PendingDatagramSocketError::HostUnreachable));
        assert_eq!(mq.listener.error_signaled, false);
    }

    #[derive(Debug, PartialEq)]
    struct TestMessage(Vec<u8>);

    impl BodyLen for TestMessage {
        fn body_len(&self) -> usize {
            let Self(body) = self;
            body.len()
        }
    }

    const TEST_MESSAGE_OVERHEAD: usize = core::mem::size_of::<TestMessage>();

    #[test]
    fn queued_size_includes_overhead() {
        assert_eq!(TestMessage(Vec::new()).queued_size(), TEST_MESSAGE_OVERHEAD);
        assert_eq!(TestMessage(vec![0; 10]).queued_size(), TEST_MESSAGE_OVERHEAD + 10);
    }

    #[test]
    fn empty_messages_are_bounded() {
        const MAX_MESSAGES: usize = 16;
        let mut queue = AvailableMessageQueue::new(
            NonZeroUsize::new(MAX_MESSAGES * TEST_MESSAGE_OVERHEAD).unwrap(),
        );
        for _ in 0..MAX_MESSAGES {
            assert_eq!(queue.push(TestMessage(Vec::new())), Ok(()));
        }
        assert_eq!(queue.push(TestMessage(Vec::new())), Err(NoSpace));
        assert_eq!(queue.available_messages.len(), MAX_MESSAGES);
        assert_eq!(queue.available_messages_size, MAX_MESSAGES * TEST_MESSAGE_OVERHEAD);

        // Popping a message frees up space for exactly one more.
        assert_eq!(queue.pop(), Some(TestMessage(Vec::new())));
        assert_eq!(queue.push(TestMessage(Vec::new())), Ok(()));
        assert_eq!(queue.push(TestMessage(Vec::new())), Err(NoSpace));

        while let Some(_) = queue.pop() {}
        assert_eq!(queue.available_messages_size, 0);
    }

    #[test]
    fn body_and_overhead_count_toward_limit() {
        const BODY_LEN: usize = 100;
        let mut queue = AvailableMessageQueue::new(
            NonZeroUsize::new(2 * (BODY_LEN + TEST_MESSAGE_OVERHEAD)).unwrap(),
        );
        assert_eq!(queue.push(TestMessage(vec![0; BODY_LEN])), Ok(()));
        assert_eq!(queue.push(TestMessage(vec![0; BODY_LEN])), Ok(()));
        assert_eq!(queue.push(TestMessage(Vec::new())), Err(NoSpace));
    }

    #[test]
    fn oversized_message_accepted_when_empty() {
        let mut queue = AvailableMessageQueue::new(NonZeroUsize::new(1).unwrap());
        assert_eq!(queue.push(TestMessage(vec![0; 100])), Ok(()));
        assert_eq!(queue.push(TestMessage(Vec::new())), Err(NoSpace));
        assert_eq!(queue.pop(), Some(TestMessage(vec![0; 100])));
        assert_eq!(queue.available_messages_size, 0);
    }
}
