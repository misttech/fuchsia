// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A basic [`Transport`] implementation based on MPSC channels.

use core::fmt;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, take};
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll};
use std::sync::{Arc, mpsc};

use fidl_next_codec::decoder::InternalHandleDecoder;
use fidl_next_codec::{CHUNK_SIZE, Chunk, DecodeError, Decoder};
use futures::task::AtomicWaker;

use crate::Transport;

/// A paired mpsc transport.
pub struct Mpsc {
    shared: Shared,
    exclusive: Exclusive,
}

impl Mpsc {
    /// Creates two mpscs which can communicate with each other.
    pub fn new() -> (Self, Self) {
        let send_wakers = Arc::new([AtomicWaker::new(), AtomicWaker::new()]);
        let (a_send, a_recv) = mpsc::channel();
        let (b_send, b_recv) = mpsc::channel();
        (
            Mpsc {
                shared: Shared {
                    send_wakers: send_wakers.clone(),
                    end: 0,
                    sender: ManuallyDrop::new(a_send),
                },
                exclusive: Exclusive { receiver: b_recv },
            },
            Mpsc {
                shared: Shared { send_wakers, end: 1, sender: ManuallyDrop::new(b_send) },
                exclusive: Exclusive { receiver: a_recv },
            },
        )
    }
}

/// The error type for paired mpsc transports.
#[derive(Clone, Debug)]
pub enum Error {}

impl fmt::Display for Error {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {}
    }
}

impl core::error::Error for Error {}

/// The shared part of a paired mpsc transport.
pub struct Shared {
    send_wakers: Arc<[AtomicWaker; 2]>,
    end: usize,
    sender: ManuallyDrop<mpsc::Sender<Vec<Chunk>>>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Make sure that the mpsc is closed before waking the other end
        unsafe {
            ManuallyDrop::drop(&mut self.sender);
        }
        self.send_wakers[self.end].wake();
    }
}

/// The send future for a paired mpsc transport.
pub struct SendFutureState {
    buffer: Vec<Chunk>,
}

/// The exclusive part of a paired mpsc transport.
pub struct Exclusive {
    receiver: mpsc::Receiver<Vec<Chunk>>,
}

/// The receive future for a paired mpsc transport.
pub struct RecvFutureState {
    _phantom: PhantomData<()>,
}

/// A received message buffer.
pub struct RecvBuffer {
    chunks: Vec<Chunk>,
    chunks_taken: usize,
}

impl InternalHandleDecoder for RecvBuffer {
    fn __internal_take_handles(&mut self, _: usize) -> Result<(), DecodeError> {
        Err(DecodeError::InsufficientHandles)
    }

    fn __internal_handles_remaining(&self) -> usize {
        0
    }
}

unsafe impl Decoder for RecvBuffer {
    fn take_chunks_raw(&mut self, count: usize) -> Result<NonNull<Chunk>, DecodeError> {
        if count > self.chunks.len() - self.chunks_taken {
            return Err(DecodeError::InsufficientData);
        }

        let chunks = unsafe { self.chunks.as_mut_ptr().add(self.chunks_taken) };
        self.chunks_taken += count;

        unsafe { Ok(NonNull::new_unchecked(chunks)) }
    }

    fn commit(&mut self) {
        // No resources to take, so commit is a no-op
    }

    fn finish(&self) -> Result<(), DecodeError> {
        if self.chunks_taken != self.chunks.len() {
            return Err(DecodeError::ExtraBytes {
                num_extra: (self.chunks.len() - self.chunks_taken) * CHUNK_SIZE,
            });
        }

        Ok(())
    }
}

impl Transport for Mpsc {
    type Error = Error;

    fn split(self) -> (Self::Shared, Self::Exclusive) {
        (self.shared, self.exclusive)
    }

    type Shared = Shared;
    type SendBuffer = Vec<Chunk>;
    type SendFutureState = SendFutureState;

    fn acquire(_: &Self::Shared) -> Self::SendBuffer {
        Vec::new()
    }

    fn begin_send(_: &Self::Shared, buffer: Self::SendBuffer) -> Self::SendFutureState {
        SendFutureState { buffer }
    }

    fn poll_send(
        mut future_state: Pin<&mut SendFutureState>,
        _: &mut Context<'_>,
        sender: &Self::Shared,
    ) -> Poll<Result<(), Option<Error>>> {
        let chunks = take(&mut future_state.buffer);
        match sender.sender.send(chunks) {
            Ok(()) => {
                sender.send_wakers[sender.end].wake();
                Poll::Ready(Ok(()))
            }
            Err(_) => Poll::Ready(Err(None)),
        }
    }

    type Exclusive = Exclusive;
    type RecvFutureState = RecvFutureState;
    type RecvBuffer = RecvBuffer;

    fn begin_recv(_: &Self::Shared, _: &mut Self::Exclusive) -> Self::RecvFutureState {
        RecvFutureState { _phantom: PhantomData }
    }

    fn poll_recv(
        _: Pin<&mut Self::RecvFutureState>,
        cx: &mut Context<'_>,
        shared: &Self::Shared,
        exclusive: &mut Self::Exclusive,
    ) -> Poll<Result<Self::RecvBuffer, Option<Self::Error>>> {
        shared.send_wakers[1 - shared.end].register(cx.waker());
        match exclusive.receiver.try_recv() {
            Ok(chunks) => Poll::Ready(Ok(RecvBuffer { chunks, chunks_taken: 0 })),
            Err(mpsc::TryRecvError::Empty) => Poll::Pending,
            Err(mpsc::TryRecvError::Disconnected) => Poll::Ready(Err(None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use fuchsia_async as fasync;

    use super::Mpsc;
    use crate::testing::*;

    #[fasync::run_singlethreaded(test)]
    async fn close_on_drop() {
        let (client_end, server_end) = Mpsc::new();
        test_close_on_drop(client_end, server_end).await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn one_way() {
        let (client_end, server_end) = Mpsc::new();
        test_one_way(client_end, server_end).await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn two_way() {
        let (client_end, server_end) = Mpsc::new();
        test_two_way(client_end, server_end).await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn multiple_two_way() {
        let (client_end, server_end) = Mpsc::new();
        test_multiple_two_way(client_end, server_end).await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn event() {
        let (client_end, server_end) = Mpsc::new();
        test_event(client_end, server_end).await;
    }
}
