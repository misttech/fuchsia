// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// While the task's callback is being processed, the dispatcher thread running
// the callback sets the status to `Polling`. While the status is `Polling`, the
// callback thread assumes responsibility for reposting, canceling, and dropping
// the task.

use core::sync::atomic::{AtomicUsize, Ordering};

pub enum Payload {
    Future = 0,
    Polling = 1,
    Output = 2,
}

const PAYLOAD_BITS: u32 = 2;
const PAYLOAD_MASK: usize = (1 << PAYLOAD_BITS) - 1;

// Set when the task is ready to be polled again.
const IS_READY_BIT: usize = 1 << PAYLOAD_BITS;
// Set when the task has been aborted. This means that we gave up on polling the
// future or reading the output of the task.
const IS_ABORTED_BIT: usize = 1 << (PAYLOAD_BITS + 1);
// Set when the dispatcher has begun shutting down. This means that the pointer
// to the dispatcher may be dangling.
const IS_SHUTTING_DOWN_BIT: usize = 1 << (PAYLOAD_BITS + 2);
// Each dispatcher refcount contributes to blocking the dispatcher during
// shutdown.
const DISPATCHER_REFCOUNT: usize = 1 << (PAYLOAD_BITS + 3);

#[repr(transparent)]
pub struct State {
    inner: AtomicUsize,
}

impl State {
    pub fn new_ready() -> Self {
        Self { inner: AtomicUsize::new(Payload::Future as usize | IS_READY_BIT) }
    }

    pub fn new_aborted() -> Self {
        Self { inner: AtomicUsize::new(Payload::Future as usize | IS_ABORTED_BIT) }
    }

    #[inline]
    pub fn set_is_ready(&self, ordering: Ordering) -> ObservedState {
        ObservedState(self.inner.fetch_or(IS_READY_BIT, ordering))
    }

    #[inline]
    pub fn set_is_aborted(&self, ordering: Ordering) -> ObservedState {
        ObservedState(self.inner.fetch_or(IS_ABORTED_BIT, ordering))
    }

    #[inline]
    pub fn set_is_shutting_down(&self, ordering: Ordering) -> ObservedState {
        ObservedState(self.inner.fetch_or(IS_SHUTTING_DOWN_BIT, ordering))
    }

    #[inline]
    pub fn inc_dispatcher_refcount(&self, ordering: Ordering) -> ObservedState {
        ObservedState(self.inner.fetch_add(DISPATCHER_REFCOUNT, ordering))
    }

    #[inline]
    pub fn dec_dispatcher_refcount(&self, ordering: Ordering) -> ObservedState {
        ObservedState(self.inner.fetch_sub(DISPATCHER_REFCOUNT, ordering))
    }

    #[inline]
    pub fn unset_is_ready_and_transition_future_to_polling(
        &self,
        ordering: Ordering,
    ) -> ObservedState {
        ObservedState(self.inner.fetch_sub(
            IS_READY_BIT + Payload::Future as usize - Payload::Polling as usize,
            ordering,
        ))
    }

    #[inline]
    pub fn transition_polling_to_future(&self, ordering: Ordering) -> ObservedState {
        ObservedState(self.inner.fetch_sub(1, ordering))
    }

    #[inline]
    pub fn transition_polling_to_output(&self, ordering: Ordering) -> ObservedState {
        ObservedState(self.inner.fetch_add(1, ordering))
    }

    #[inline]
    pub fn load(&self, ordering: Ordering) -> ObservedState {
        ObservedState(self.inner.load(ordering))
    }

    #[inline]
    pub fn load_mut(&mut self) -> ObservedState {
        ObservedState(*self.inner.get_mut())
    }
}

#[derive(Clone, Copy)]
pub struct ObservedState(usize);

impl ObservedState {
    #[inline]
    pub fn payload(self) -> Payload {
        match self.0 & PAYLOAD_MASK {
            0 => Payload::Future,
            1 => Payload::Polling,
            2 => Payload::Output,
            payload => unreachable!("invalid payload: {payload}"),
        }
    }

    #[inline]
    pub fn is_ready(self) -> bool {
        self.0 & IS_READY_BIT != 0
    }

    #[inline]
    pub fn is_aborted(self) -> bool {
        self.0 & IS_ABORTED_BIT != 0
    }

    #[inline]
    pub fn is_shutting_down(self) -> bool {
        self.0 & IS_SHUTTING_DOWN_BIT != 0
    }

    #[inline]
    pub fn dispatcher_refcount(self) -> usize {
        self.0 / DISPATCHER_REFCOUNT
    }
}
