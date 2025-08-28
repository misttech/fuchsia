// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::cell::UnsafeCell;
use core::hint::unreachable_unchecked;
use core::mem::{MaybeUninit, replace, take};
use core::sync::atomic::{AtomicU8, Ordering};
use core::task::{Context, Poll, Waker};
use std::sync::Mutex;

use core::future::Future;
use core::pin::Pin;

use futures::task::AtomicWaker;

use crate::{NonBlockingTransport, ProtocolError, Transport, encode_epitaph, encode_header};

pub const ORDINAL_EPITAPH: u64 = 0xffff_ffff_ffff_ffff;

// The connection is running normally.
const STATE_RUNNING: u8 = 0;
// The connection is stopping.
const STATE_STOPPING: u8 = 1;
// The connection has been terminated.
const STATE_TERMINATED: u8 = 2;

/// A wrapper around a transport which connectivity semantics.
///
/// The [`Transport`] trait only provides the bare minimum API surface required
/// to send and receive data. On top of that, FIDL requires that clients and
/// servers respect additional messaging semantics. Those semantics are provided
/// by [`Connection`]:
///
/// - `Transport`s are difficult to close because they may be accessed from
///   several threads simultaneously. `Connection`s provide a mechanism for
///   gracefully closing transports by causing all sends to pend until the
///   connection is terminated, and all receives to fail instead of pend.
/// - FIDL connections may send and receive an epitaph as the final message
///   before the underlying transport is closed. This epitaph should be provided
///   to all sends when they fail, which requires additional coordination.
pub struct Connection<T: Transport> {
    state: AtomicU8,
    shared: T::Shared,
    stop_waker: AtomicWaker,
    // TODO: switch this to intrusive linked list in send futures
    termination_wakers: Mutex<Vec<Waker>>,
    // Initialized as part of the transition from CLOSING to CLOSED
    termination_reason: UnsafeCell<MaybeUninit<ProtocolError<T::Error>>>,
}

unsafe impl<T: Transport> Send for Connection<T> {}
unsafe impl<T: Transport> Sync for Connection<T> {}

impl<T: Transport> Drop for Connection<T> {
    fn drop(&mut self) {
        if *self.state.get_mut() == STATE_TERMINATED {
            // SAFETY: `termination_reason` is initialized if the state is
            // `STATE_TERMINATED`.
            unsafe {
                self.termination_reason.get_mut().assume_init_drop();
            }
        }
    }
}

impl<T: Transport> Connection<T> {
    /// Creates a new connection from the shared part of a transport.
    pub fn new(shared: T::Shared) -> Self {
        Self {
            state: AtomicU8::new(STATE_RUNNING),
            shared,
            stop_waker: AtomicWaker::new(),
            termination_wakers: Mutex::new(Vec::new()),
            termination_reason: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// # Safety
    ///
    /// `state` must have been loaded with `Ordering::Acquire` and observed to
    /// be `STATE_TERMINATED`.
    unsafe fn get_termination_reason_unchecked(&self) -> ProtocolError<T::Error> {
        unsafe { (&*self.termination_reason.get()).assume_init_ref().clone() }
    }

    /// Returns the termination reason if the connection is terminated.
    pub fn get_termination_reason(&self) -> Option<ProtocolError<T::Error>> {
        let state = self.state.load(Ordering::Acquire);
        if state == STATE_TERMINATED {
            unsafe { Some(self.get_termination_reason_unchecked()) }
        } else {
            None
        }
    }

    /// Acquires an empty send buffer for the transport.
    pub fn acquire(&self) -> T::SendBuffer {
        T::acquire(&self.shared)
    }

    /// Returns a new [`SendFuture`] which sends the given buffer.
    pub fn send(&self, buffer: T::SendBuffer) -> SendFuture<'_, T> {
        SendFuture {
            connection: self,
            waker_index: None,
            future_state: T::begin_send(&self.shared, buffer),
        }
    }

    /// Sends an epitaph to the underlying transport.
    ///
    /// This send ignores the current state of the connection, and does not
    /// report back any errors encountered while sending.
    pub async fn send_epitaph(&self, error: i32) {
        struct SendEpitaphFuture<'a, T: Transport> {
            shared: &'a T::Shared,
            future_state: T::SendFutureState,
        }

        impl<T: Transport> Future for SendEpitaphFuture<'_, T> {
            type Output = Result<(), Option<T::Error>>;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let this = unsafe { Pin::into_inner_unchecked(self) };
                let future_state = unsafe { Pin::new_unchecked(&mut this.future_state) };
                T::poll_send(future_state, cx, this.shared)
            }
        }

        let mut buffer = self.acquire();
        encode_header::<T>(&mut buffer, 0, ORDINAL_EPITAPH).unwrap();
        encode_epitaph::<T>(&mut buffer, error).unwrap();
        let future_state = T::begin_send(&self.shared, buffer);

        // Don't care whether sending the epitaph succeeds or fails
        let _ = SendEpitaphFuture::<'_, T> { shared: &self.shared, future_state }.await;
    }

    /// Returns a new [`RecvFuture`] which receives the next message.
    pub fn recv<'a>(&'a self, exclusive: &'a mut T::Exclusive) -> RecvFuture<'a, T> {
        let future_state = T::begin_recv(&self.shared, exclusive);
        RecvFuture { connection: self, exclusive, future_state }
    }

    /// Stops the connection to wait for termination.
    ///
    /// This modifies the behavior of this connection's futures:
    ///
    /// - Polled [`SendFuture`]s will return `Poll::Pending` without calling
    ///   [`poll_send`].
    /// - Polled [`RecvFuture`]s will call [`poll_recv`], but will return
    ///   `Poll::Ready` with an error when they would normally return
    ///   `Poll::Pending`.
    ///
    /// [`poll_send`]: Transport::poll_send
    /// [`poll_recv`]: Transport::poll_recv
    pub fn stop(&self) {
        let prev_state = self.state.fetch_max(STATE_STOPPING, Ordering::Relaxed);
        if prev_state < STATE_STOPPING {
            self.stop_waker.wake();
        }
    }

    /// Terminates the connection.
    ///
    /// This causes this connection's futures to return `Poll::Ready` with an
    /// error of the given termination reason.
    ///
    /// Does nothing if the connection has already been terminated.
    pub fn terminate(&self, termination_reason: ProtocolError<T::Error>) {
        let mut wakers_guard = self.termination_wakers.lock().unwrap();
        let previous_state = self.state.fetch_max(STATE_TERMINATED, Ordering::Acquire);

        if previous_state != STATE_TERMINATED {
            // SAFETY: We successfully increased the state to
            // `STATE_TERMINATING` which gives us permission to write the
            // termination reason.
            unsafe {
                self.termination_reason.get().write(MaybeUninit::new(termination_reason));
            }

            // Wake all of the futures waiting for a termination reason
            let wakers = take(&mut *wakers_guard);
            drop(wakers_guard);

            for waker in wakers {
                waker.wake();
            }
        }
    }
}

/// A future which sends an encoded message to a connection.
#[must_use = "futures do nothing unless polled"]
pub struct SendFuture<'a, T: Transport> {
    connection: &'a Connection<T>,
    waker_index: Option<usize>,
    future_state: T::SendFutureState,
}

impl<T: Transport> SendFuture<'_, T> {
    fn register_termination_waker(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Result<(), ProtocolError<T::Error>> {
        let mut wakers = self.connection.termination_wakers.lock().unwrap();

        // Re-check the state now that we're holding the lock again. This
        // prevents us from adding wakers after termination (which would "leak"
        // them).
        if let Some(termination_reason) = self.connection.get_termination_reason() {
            Err(termination_reason)
        } else {
            let waker = cx.waker().clone();
            if let Some(waker_index) = self.waker_index {
                // Overwrite an existing waker
                let old_waker = replace(&mut wakers[waker_index], waker);

                // Drop the old waker outside of the mutex lock
                drop(wakers);
                drop(old_waker);
            } else {
                // Insert a new waker
                self.waker_index = Some(wakers.len());
                wakers.push(waker);
            }
            Ok(())
        }
    }
}

impl<T: NonBlockingTransport> SendFuture<'_, T> {
    /// Completes the send operation synchronously and without blocking.
    ///
    /// Using this method prevents transports from applying backpressure. Prefer
    /// awaiting when possible.
    ///
    /// Because failed sends return immediately without waiting for an epitaph
    /// to be read, `send_immediately` may observe transport closure
    /// prematurely.
    pub fn send_immediately(mut self) -> Result<(), Option<T::Error>> {
        T::send_immediately(&mut self.future_state, &self.connection.shared)
    }
}

impl<T: Transport> Future for SendFuture<'_, T> {
    type Output = Result<(), ProtocolError<T::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { Pin::into_inner_unchecked(self) };

        let mut state = this.connection.state.load(Ordering::Acquire);
        loop {
            match state {
                STATE_RUNNING => {
                    // Connection is running, poll the future.

                    let future_state = unsafe { Pin::new_unchecked(&mut this.future_state) };
                    match T::poll_send(future_state, cx, &this.connection.shared) {
                        // Send didn't complete, we'll get polled again later.
                        Poll::Pending => (),

                        // Send succeeded.
                        Poll::Ready(Ok(())) => return Poll::Ready(Ok(())),

                        // Transport failed
                        Poll::Ready(Err(error)) => {
                            if let Some(e) = error {
                                // Abnormal failure: return the transport error.
                                return Poll::Ready(Err(ProtocolError::TransportError(e)));
                            } else {
                                // Normal failure: wait for termination reason.
                                if let Err(error) = this.register_termination_waker(cx) {
                                    return Poll::Ready(Err(error));
                                }
                            }
                        }
                    }
                }

                STATE_STOPPING => {
                    // Connection is stopping, but not terminated yet. Wait for
                    // a termination reason.
                    if let Err(error) = this.register_termination_waker(cx) {
                        return Poll::Ready(Err(error));
                    }
                }

                STATE_TERMINATED => {
                    // Connection has terminated, return the termination reason.
                    let error = unsafe { this.connection.get_termination_reason_unchecked() };
                    return Poll::Ready(Err(error));
                }

                _ => unsafe { unreachable_unchecked() },
            }

            // We're ready to pend, but need to make sure the state hasn't been
            // updated since we last checked.

            let state_after = this.connection.state.load(Ordering::Acquire);
            if state == state_after {
                // The state hasn't changed and we're ready to pend.
                return Poll::Pending;
            }

            // The state changed, poll again.
            state = state_after;
        }
    }
}

/// A future which receives an encoded message over the transport.
#[must_use = "futures do nothing unless polled"]
pub struct RecvFuture<'a, T: Transport> {
    connection: &'a Connection<T>,
    exclusive: &'a mut T::Exclusive,
    future_state: T::RecvFutureState,
}

impl<T: Transport> Future for RecvFuture<'_, T> {
    type Output = Result<T::RecvBuffer, ProtocolError<T::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { Pin::into_inner_unchecked(self) };
        let state = this.connection.state.load(Ordering::Acquire);

        if state == STATE_TERMINATED {
            // Connection is terminated, return the termination reason.
            let error = unsafe { this.connection.get_termination_reason_unchecked() };
            return Poll::Ready(Err(error));
        }

        let future_state = unsafe { Pin::new_unchecked(&mut this.future_state) };
        let termination_reason =
            match T::poll_recv(future_state, cx, &this.connection.shared, this.exclusive) {
                Poll::Pending => {
                    // Receive didn't complete, register waker before re-checking state
                    this.connection.stop_waker.register(cx.waker());

                    if this.connection.state.load(Ordering::Relaxed) == STATE_STOPPING {
                        // The connection is stopping. Return an error that the
                        // connection has been closed locally.
                        ProtocolError::Stopped
                    } else {
                        // Still running, we'll get polled again later.
                        return Poll::Pending;
                    }
                }

                // Receive succeeded.
                Poll::Ready(Ok(buffer)) => return Poll::Ready(Ok(buffer)),

                // Normal failure: return peer closed error.
                Poll::Ready(Err(None)) => ProtocolError::PeerClosed,

                // Abnormal failure: return transport error.
                Poll::Ready(Err(Some(error))) => ProtocolError::TransportError(error),
            };

        Poll::Ready(Err(termination_reason))
    }
}
