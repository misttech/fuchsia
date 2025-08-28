// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod wire;

use std::marker::PhantomData;
use std::num::NonZero;
use std::pin::Pin;
use std::ptr::NonNull;
use std::task::{Context, Poll};

use fidl_next::Chunk;
use zx::Status;

use fdf_channel::arena::{Arena, ArenaBox};
use fdf_channel::channel::Channel;
use fdf_channel::futures::ReadMessageState;
use fdf_channel::message::Message;
use fdf_core::dispatcher::{CurrentDispatcher, OnDispatcher};
use fdf_core::handle::{DriverHandle, MixedHandle, MixedHandleType};

pub use self::wire::*;

/// A fidl-compatible driver channel that also holds a reference to the
/// dispatcher. Defaults to using [`CurrentDispatcher`].
#[derive(Debug)]
pub struct DriverChannel<D = CurrentDispatcher> {
    dispatcher: D,
    channel: Channel<[Chunk]>,
}

impl<D> DriverChannel<D> {
    /// Create a new driver fidl channel that will perform its operations on the given
    /// dispatcher handle.
    pub fn new_with_dispatcher(dispatcher: D, channel: Channel<[Chunk]>) -> Self {
        Self { dispatcher, channel }
    }

    /// Create a new driver fidl channel pair that will perform its operations on the given
    /// dispatcher handles.
    pub fn create_with_dispatchers(dispatcher1: D, dispatcher2: D) -> (Self, Self) {
        let (channel1, channel2) = Channel::create();
        (
            Self { dispatcher: dispatcher1, channel: channel1 },
            Self { dispatcher: dispatcher2, channel: channel2 },
        )
    }

    /// Create a new driver fidl channel pair that will perform its operations on the given
    /// dispatcher handle, if the dispatcher implements [`Clone`]
    pub fn create_with_dispatcher(dispatcher: D) -> (Self, Self)
    where
        D: Clone,
    {
        Self::create_with_dispatchers(dispatcher.clone(), dispatcher)
    }

    /// Returns the underlying data channel
    pub fn into_channel(self) -> Channel<[Chunk]> {
        self.channel
    }

    /// Returns the underlying `fdf_handle_t` for this channel
    pub fn into_driver_handle(self) -> DriverHandle {
        self.channel.into_driver_handle()
    }
}

impl DriverChannel<CurrentDispatcher> {
    /// Create a new driver fidl channel that will perform its operations on the
    /// [`CurrentDispatcher`].
    pub fn new(channel: Channel<[Chunk]>) -> Self {
        Self::new_with_dispatcher(CurrentDispatcher, channel)
    }

    /// Create a new driver fidl channel pair that will perform its operations on the
    /// [`CurrentDispatcher`].
    pub fn create() -> (Self, Self) {
        Self::create_with_dispatcher(CurrentDispatcher)
    }
}

/// Creates a pair of [`fidl_next::ClientEnd`] and [`fidl_next::ServerEnd`] backed by a new
/// pair of [`DriverChannel`]s using dispatchers of type `D`.
pub fn create_channel_with_dispatchers<P, D>(
    client_dispatcher: D,
    server_dispatcher: D,
) -> (fidl_next::ClientEnd<P, DriverChannel<D>>, fidl_next::ServerEnd<P, DriverChannel<D>>) {
    let (client_channel, server_channel) =
        DriverChannel::create_with_dispatchers(client_dispatcher, server_dispatcher);
    (
        fidl_next::ClientEnd::from_untyped(client_channel),
        fidl_next::ServerEnd::from_untyped(server_channel),
    )
}

/// Creates a pair of [`fidl_next::ClientEnd`] and [`fidl_next::ServerEnd`] backed by a new
/// pair of [`DriverChannel`]s using dispatchers of type `D`, where `D` implements [`Clone`]
pub fn create_channel_with_dispatcher<P, D: Clone>(
    dispatcher: D,
) -> (fidl_next::ClientEnd<P, DriverChannel<D>>, fidl_next::ServerEnd<P, DriverChannel<D>>) {
    create_channel_with_dispatchers(dispatcher.clone(), dispatcher)
}

/// Creates a pair of [`fidl_next::ClientEnd`] and [`fidl_next::ServerEnd`] backed by a new
/// pair of [`DriverChannel`]s using the default [`CurrentDispatcher`]
pub fn create_channel<P>()
-> (fidl_next::ClientEnd<P, DriverChannel>, fidl_next::ServerEnd<P, DriverChannel>) {
    create_channel_with_dispatcher(CurrentDispatcher)
}

/// A channel buffer.
pub struct SendBuffer {
    handles: Vec<Option<MixedHandle>>,
    data: Vec<Chunk>,
}

impl SendBuffer {
    fn new() -> Self {
        Self { handles: Vec::new(), data: Vec::new() }
    }
}

impl fidl_next::Encoder for SendBuffer {
    #[inline]
    fn bytes_written(&self) -> usize {
        fidl_next::Encoder::bytes_written(&self.data)
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        fidl_next::Encoder::write(&mut self.data, bytes)
    }

    #[inline]
    fn rewrite(&mut self, pos: usize, bytes: &[u8]) {
        fidl_next::Encoder::rewrite(&mut self.data, pos, bytes)
    }

    fn write_zeroes(&mut self, len: usize) {
        fidl_next::Encoder::write_zeroes(&mut self.data, len);
    }
}

impl fidl_next::encoder::InternalHandleEncoder for SendBuffer {
    #[inline]
    fn __internal_handle_count(&self) -> usize {
        self.handles.len()
    }
}

impl fidl_next::fuchsia::HandleEncoder for SendBuffer {
    fn push_handle(&mut self, handle: zx::Handle) -> Result<(), fidl_next::EncodeError> {
        if let Some(handle) = MixedHandle::from_zircon_handle(handle) {
            if handle.is_driver() {
                return Err(fidl_next::EncodeError::ExpectedZirconHandle);
            }
            self.handles.push(Some(handle));
        } else {
            self.handles.push(None);
        }
        Ok(())
    }

    unsafe fn push_raw_driver_handle(&mut self, handle: u32) -> Result<(), fidl_next::EncodeError> {
        if let Some(handle) = NonZero::new(handle) {
            // SAFETY: the fidl framework is responsible for providing us with a valid, otherwise
            // unowned handle.
            let handle = unsafe { MixedHandle::from_raw(handle) };
            if !handle.is_driver() {
                return Err(fidl_next::EncodeError::ExpectedDriverHandle);
            }
            self.handles.push(Some(handle));
        } else {
            self.handles.push(None);
        }
        Ok(())
    }

    fn handles_pushed(&self) -> usize {
        self.handles.len()
    }
}

pub struct RecvBuffer {
    buffer: Option<Message<[Chunk]>>,
    data_offset: usize,
    handle_offset: usize,
}

impl RecvBuffer {
    fn next_handle(&self) -> Result<&MixedHandle, fidl_next::DecodeError> {
        let Some(buffer) = &self.buffer else {
            return Err(fidl_next::DecodeError::InsufficientHandles);
        };

        let Some(handles) = buffer.handles() else {
            return Err(fidl_next::DecodeError::InsufficientHandles);
        };
        if handles.len() < self.handle_offset + 1 {
            return Err(fidl_next::DecodeError::InsufficientHandles);
        }
        handles[self.handle_offset].as_ref().ok_or(fidl_next::DecodeError::RequiredHandleAbsent)
    }
}

// SAFETY: The decoder implementation stores the data buffer in a [`Message`] tied to an [`Arena`],
// and the memory in an [`Arena`] is guaranteed not to move while the arena is valid.
// Also, since we own the [`Message`] and nothing else can, it is ok to treat its contents
// as mutable through an `&mut self` reference to the struct.
unsafe impl fidl_next::Decoder for RecvBuffer {
    // SAFETY: if the caller requests a number of [`Chunk`]s that we can't supply, we return
    // `InsufficientData`.
    fn take_chunks_raw(&mut self, count: usize) -> Result<NonNull<Chunk>, fidl_next::DecodeError> {
        let Some(buffer) = &mut self.buffer else {
            return Err(fidl_next::DecodeError::InsufficientData);
        };

        let Some(data) = buffer.data_mut() else {
            return Err(fidl_next::DecodeError::InsufficientData);
        };
        if data.len() < self.data_offset + count {
            return Err(fidl_next::DecodeError::InsufficientData);
        }
        let pos = self.data_offset;
        self.data_offset += count;
        Ok(unsafe { NonNull::new_unchecked((&mut data[pos..(pos + count)]).as_mut_ptr()) })
    }

    fn commit(&mut self) {
        if let Some(handles) = self.buffer.as_mut().and_then(Message::handles_mut) {
            for i in 0..self.handle_offset {
                core::mem::forget(handles[i].take());
            }
        }
    }

    fn finish(&self) -> Result<(), fidl_next::DecodeError> {
        if let Some(buffer) = &self.buffer {
            let data_len = buffer.data().unwrap_or(&[]).len();
            if self.data_offset != data_len {
                return Err(fidl_next::DecodeError::ExtraBytes {
                    num_extra: data_len - self.data_offset,
                });
            }
            let handle_len = buffer.handles().unwrap_or(&[]).len();
            if self.handle_offset != handle_len {
                return Err(fidl_next::DecodeError::ExtraHandles {
                    num_extra: handle_len - self.handle_offset,
                });
            }
        }

        Ok(())
    }
}

impl fidl_next::decoder::InternalHandleDecoder for RecvBuffer {
    fn __internal_take_handles(&mut self, count: usize) -> Result<(), fidl_next::DecodeError> {
        let Some(handles) = self.buffer.as_mut().and_then(Message::handles_mut) else {
            return Err(fidl_next::DecodeError::InsufficientHandles);
        };
        if handles.len() < self.handle_offset + count {
            return Err(fidl_next::DecodeError::InsufficientHandles);
        }
        let pos = self.handle_offset;
        self.handle_offset = pos + count;
        Ok(())
    }

    fn __internal_handles_remaining(&self) -> usize {
        self.buffer
            .as_ref()
            .map(|buffer| buffer.handles().unwrap_or(&[]).len() - self.handle_offset)
            .unwrap_or(0)
    }
}

impl fidl_next::fuchsia::HandleDecoder for RecvBuffer {
    fn take_raw_handle(&mut self) -> Result<zx::sys::zx_handle_t, fidl_next::DecodeError> {
        let result = {
            let handle = self.next_handle()?.resolve_ref();
            let MixedHandleType::Zircon(handle) = handle else {
                return Err(fidl_next::DecodeError::ExpectedZirconHandle);
            };
            handle.raw_handle()
        };
        let pos = self.handle_offset;
        self.handle_offset = pos + 1;
        Ok(result)
    }

    fn take_raw_driver_handle(&mut self) -> Result<u32, fidl_next::DecodeError> {
        let result = {
            let handle = self.next_handle()?.resolve_ref();
            let MixedHandleType::Driver(handle) = handle else {
                return Err(fidl_next::DecodeError::ExpectedDriverHandle);
            };
            unsafe { handle.get_raw().get() }
        };
        let pos = self.handle_offset;
        self.handle_offset = pos + 1;
        Ok(result)
    }

    fn handles_remaining(&mut self) -> usize {
        fidl_next::decoder::InternalHandleDecoder::__internal_handles_remaining(self)
    }
}

/// The inner state of a receive future used by [`fidl_next::protocol::Transport`].
pub struct DriverRecvState(ReadMessageState);

/// The shared part of a driver channel.
pub struct Shared<D> {
    channel: DriverChannel<D>,
}

impl<D> Shared<D> {
    fn new(channel: DriverChannel<D>) -> Self {
        Self { channel }
    }
}

/// The exclusive part of a driver channel.
pub struct Exclusive {
    _phantom: PhantomData<()>,
}

impl<D: OnDispatcher> fidl_next::protocol::Transport for DriverChannel<D> {
    type Error = Status;

    fn split(self) -> (Self::Shared, Self::Exclusive) {
        (Shared::new(self), Exclusive { _phantom: PhantomData })
    }

    type Shared = Shared<D>;

    type SendBuffer = SendBuffer;

    type SendFutureState = SendBuffer;

    fn acquire(_shared: &Self::Shared) -> Self::SendBuffer {
        SendBuffer::new()
    }

    type Exclusive = Exclusive;

    type RecvFutureState = DriverRecvState;

    type RecvBuffer = RecvBuffer;

    fn begin_send(_shared: &Self::Shared, buffer: Self::SendBuffer) -> Self::SendFutureState {
        buffer
    }

    fn poll_send(
        mut buffer: Pin<&mut Self::SendFutureState>,
        _cx: &mut Context<'_>,
        shared: &Self::Shared,
    ) -> Poll<Result<(), Option<Self::Error>>> {
        let arena = Arena::new();
        let message = Message::new_with(arena, |arena| {
            let data = arena.insert_slice(&buffer.data);
            let handles = buffer.handles.split_off(0);
            let handles = arena.insert_from_iter(handles.into_iter());
            (Some(data), Some(handles))
        });
        let result = match shared.channel.channel.write(message) {
            Ok(()) => Ok(()),
            Err(Status::PEER_CLOSED) => Err(None),
            Err(e) => Err(Some(e)),
        };
        Poll::Ready(result)
    }

    fn begin_recv(
        shared: &Self::Shared,
        _exclusive: &mut Self::Exclusive,
    ) -> Self::RecvFutureState {
        // SAFETY: The `receiver` owns the channel we're using here and will be the same
        // receiver given to `poll_recv`, so must outlive the state object we're constructing.
        let state = unsafe { ReadMessageState::new(shared.channel.channel.driver_handle()) };
        DriverRecvState(state)
    }

    fn poll_recv(
        mut future: Pin<&mut Self::RecvFutureState>,
        cx: &mut Context<'_>,
        shared: &Self::Shared,
        _exclusive: &mut Self::Exclusive,
    ) -> Poll<Result<Self::RecvBuffer, Option<Self::Error>>> {
        use std::task::Poll::*;
        match future.as_mut().0.poll_with_dispatcher(cx, shared.channel.dispatcher.clone()) {
            Ready(Ok(maybe_buffer)) => {
                let buffer = maybe_buffer.map(|buffer| {
                    buffer.map_data(|_, data| {
                        let bytes = data.len();
                        assert_eq!(
                            0,
                            bytes % size_of::<Chunk>(),
                            "Received driver channel buffer was not a multiple of {} bytes",
                            size_of::<Chunk>()
                        );
                        // SAFETY: we verified that the size of the message we received was the correct
                        // multiple of chunks and we know that the data pointer is otherwise valid and
                        // from the correct arena by construction.
                        let new_box = unsafe {
                            let ptr = ArenaBox::into_ptr(data).cast();
                            ArenaBox::new(NonNull::slice_from_raw_parts(
                                ptr,
                                bytes / size_of::<Chunk>(),
                            ))
                        };
                        new_box
                    })
                });

                Ready(Ok(RecvBuffer { buffer, data_offset: 0, handle_offset: 0 }))
            }
            Ready(Err(err)) => {
                if err == Status::PEER_CLOSED {
                    Ready(Err(None))
                } else {
                    Ready(Err(Some(err)))
                }
            }
            Pending => Pending,
        }
    }
}

#[cfg(test)]
mod test {
    use fidl_next::{Client, ClientEnd, Responder, Server, ServerEnd, ServerSender};
    use fidl_next_fuchsia_examples_gizmo::device::{GetEvent, GetHardwareId};
    use fidl_next_fuchsia_examples_gizmo::{
        Device, DeviceClientHandler, DeviceGetEventResponse, DeviceGetHardwareIdResponse,
        DeviceServerHandler,
    };
    use fuchsia_async::OnSignals;
    use zx::{AsHandleRef, Event, Signals};

    use super::*;
    use fdf_core::dispatcher::{CurrentDispatcher, OnDispatcher};
    use fdf_env::test::spawn_in_driver;

    struct DeviceServer;
    impl DeviceServerHandler<DriverChannel> for DeviceServer {
        async fn get_hardware_id(
            &mut self,
            sender: &ServerSender<Device, DriverChannel>,
            responder: Responder<GetHardwareId>,
        ) {
            responder
                .respond(
                    &sender,
                    Result::<_, i32>::Ok(DeviceGetHardwareIdResponse { response: 4004 }),
                )
                .unwrap()
                .await
                .unwrap();
        }

        async fn get_event(
            &mut self,
            sender: &ServerSender<Device, DriverChannel>,
            responder: Responder<GetEvent>,
        ) {
            let event = Event::create();
            event.signal_handle(Signals::empty(), Signals::USER_0).unwrap();
            let response = DeviceGetEventResponse { event };
            responder.respond(&sender, response).unwrap().await.unwrap();
        }
    }

    struct DeviceClient;
    impl DeviceClientHandler<DriverChannel> for DeviceClient {}

    #[test]
    fn driver_fidl_server() {
        spawn_in_driver("driver fidl server", async {
            let (server_chan, client_chan) = Channel::<[Chunk]>::create();
            let client_end: ClientEnd<Device, _> =
                ClientEnd::<Device, _>::from_untyped(DriverChannel::new(client_chan));
            let server_end: ServerEnd<Device, _> =
                ServerEnd::from_untyped(DriverChannel::new(server_chan));
            let mut client = Client::new(client_end);
            let mut server = Server::new(server_end);
            let client_sender = client.sender().clone();

            CurrentDispatcher
                .spawn_task(async move {
                    server.run(DeviceServer).await.unwrap();
                    println!("server task finished");
                })
                .unwrap();
            CurrentDispatcher
                .spawn_task(async move {
                    client.run(DeviceClient).await.unwrap();
                    println!("client task finished");
                })
                .unwrap();

            {
                let res = client_sender.get_hardware_id().unwrap().await.unwrap();
                let hardware_id = res.unwrap();
                assert_eq!(hardware_id.response, 4004);
            }

            {
                let res = client_sender
                    .get_event()
                    .unwrap()
                    .await
                    .unwrap()
                    .take::<DeviceGetEventResponse>();

                // wait for the event on a fuchsia_async executor
                let mut executor = fuchsia_async::LocalExecutor::new();
                let signalled = executor
                    .run_singlethreaded(OnSignals::new(res.event, Signals::USER_0))
                    .unwrap();
                assert_eq!(Signals::USER_0, signalled);
            }
        });
    }
}
