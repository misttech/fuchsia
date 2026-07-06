// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{
    ActiveRequests, DecodedRequest, DeviceInfo, FIFO_MAX_REQUESTS, HandleRequestResult,
    IntoOrchestrator, OffsetMap, Operation, SessionHelper, TraceFlowId,
};
use anyhow::Error;
use block_protocol::{BlockFifoRequest, BlockFifoResponse, ReadOptions, WriteFlags, WriteOptions};
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_block::DeviceFlag;
use fuchsia_async as fasync;
use fuchsia_sync::Mutex;
use futures::future::{Fuse, FusedFuture, join};
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt, select_biased};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::mem::MaybeUninit;
use std::pin::pin;
use std::sync::{Arc, OnceLock};
use std::task::{Poll, ready};
use storage_device::buffer::Buffer;
use storage_device::buffer_allocator::{BufferAllocator, BufferSource};

pub trait Interface: Send + Sync + Unpin + 'static {
    /// Runs `stream` to completion.
    ///
    /// Implementors can override this method if they want to create a passthrough session instead
    /// (and can use `[PassthroughSession]` below to do so).  See
    /// fuchsia.hardware.block.Block/OpenSessionWithOffsetMap.
    ///
    /// If the implementor uses a `[PassthroughSession]`, the following Interface methods
    /// will not be called, and can be stubbed out:
    ///   - on_attach_vmo
    ///   - on_detach_vmo
    ///   - read
    ///   - write
    ///   - flush
    ///   - trim
    fn open_session(
        &self,
        session_manager: Arc<SessionManager<Self>>,
        stream: fblock::SessionRequestStream,
        offset_map: OffsetMap,
        block_size: u32,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        // By default, serve the session rather than forwarding/proxying it.
        session_manager.serve_session(stream, offset_map, block_size)
    }

    /// Called whenever a VMO is attached, prior to the VMO's usage in any other methods.  Whilst
    /// the VMO is attached, `vmo` will keep the same address so it is safe to use the pointer
    /// value (as, say, a key into a HashMap).
    fn on_attach_vmo(&self, _vmo: &zx::Vmo) -> impl Future<Output = Result<(), zx::Status>> + Send {
        async { Ok(()) }
    }

    /// Called whenever a VMO is detached.
    fn on_detach_vmo(&self, _vmo: &zx::Vmo) {}

    /// Called to get block/partition information.
    fn get_info(&self) -> Cow<'_, DeviceInfo>;

    /// Called for a request to read bytes.
    fn read(
        &self,
        device_block_offset: u64,
        block_count: u32,
        vmo: &Arc<zx::Vmo>,
        vmo_offset: u64, // *bytes* not blocks
        opts: ReadOptions,
        trace_flow_id: TraceFlowId,
    ) -> impl Future<Output = Result<(), zx::Status>> + Send;

    /// Called for a request to write bytes.
    fn write(
        &self,
        device_block_offset: u64,
        block_count: u32,
        vmo: &Arc<zx::Vmo>,
        vmo_offset: u64, // *bytes* not blocks
        opts: WriteOptions,
        trace_flow_id: TraceFlowId,
    ) -> impl Future<Output = Result<(), zx::Status>> + Send;

    /// Called to flush the device.
    fn flush(
        &self,
        trace_flow_id: TraceFlowId,
    ) -> impl Future<Output = Result<(), zx::Status>> + Send;

    /// Called to trim a region.
    fn trim(
        &self,
        device_block_offset: u64,
        block_count: u32,
        trace_flow_id: TraceFlowId,
    ) -> impl Future<Output = Result<(), zx::Status>> + Send;

    /// Called to handle the GetVolumeInfo FIDL call.
    fn get_volume_info(
        &self,
    ) -> impl Future<Output = Result<(fblock::VolumeManagerInfo, fblock::VolumeInfo), zx::Status>> + Send
    {
        async { Err(zx::Status::NOT_SUPPORTED) }
    }

    /// Called to handle the QuerySlices FIDL call.
    fn query_slices(
        &self,
        _start_slices: &[u64],
    ) -> impl Future<Output = Result<Vec<fblock::VsliceRange>, zx::Status>> + Send {
        async { Err(zx::Status::NOT_SUPPORTED) }
    }

    /// Called to handle the Extend FIDL call.
    fn extend(
        &self,
        _start_slice: u64,
        _slice_count: u64,
    ) -> impl Future<Output = Result<(), zx::Status>> + Send {
        async { Err(zx::Status::NOT_SUPPORTED) }
    }

    /// Called to handle the Shrink FIDL call.
    fn shrink(
        &self,
        _start_slice: u64,
        _slice_count: u64,
    ) -> impl Future<Output = Result<(), zx::Status>> + Send {
        async { Err(zx::Status::NOT_SUPPORTED) }
    }
}

/// A helper object to run a passthrough (proxy) session.
pub struct PassthroughSession(fblock::SessionProxy);

impl PassthroughSession {
    pub fn new(proxy: fblock::SessionProxy) -> Self {
        Self(proxy)
    }

    async fn handle_request(&self, request: fblock::SessionRequest) -> Result<(), Error> {
        match request {
            fblock::SessionRequest::GetFifo { responder } => {
                responder.send(self.0.get_fifo().await?)?;
            }
            fblock::SessionRequest::AttachVmo { vmo, responder } => {
                responder.send(self.0.attach_vmo(vmo).await?.as_ref().map_err(|s| *s))?;
            }
            fblock::SessionRequest::Close { responder } => {
                responder.send(self.0.close().await?)?;
            }
        }
        Ok(())
    }

    /// Runs `stream` until completion.
    pub async fn serve(&self, mut stream: fblock::SessionRequestStream) -> Result<(), Error> {
        while let Some(Ok(request)) = stream.next().await {
            if let Err(error) = self.handle_request(request).await {
                log::warn!(error:?; "FIDL error");
            }
        }
        Ok(())
    }
}

pub struct SessionManager<I: Interface + ?Sized> {
    interface: Arc<I>,
    active_requests: ActiveRequests<usize>,

    // NOTE: This must be dropped *after* `active_requests` because we store `Buffer<'_>` with an
    // erased ('static) lifetime in `ActiveRequest`.
    buffer_allocator: OnceLock<BufferAllocator>,
}

impl<I: Interface + ?Sized> Drop for SessionManager<I> {
    fn drop(&mut self) {
        if let Some(allocator) = self.buffer_allocator.get() {
            self.interface.on_detach_vmo(allocator.buffer_source().vmo());
        }
    }
}

impl<I: Interface + ?Sized> SessionManager<I> {
    pub fn new(interface: Arc<I>) -> Self {
        Self {
            interface,
            active_requests: ActiveRequests::default(),
            buffer_allocator: OnceLock::new(),
        }
    }

    pub fn interface(&self) -> &I {
        self.interface.as_ref()
    }

    /// Runs `stream` until completion.
    pub async fn serve_session(
        self: Arc<Self>,
        stream: fblock::SessionRequestStream,
        offset_map: OffsetMap,
        block_size: u32,
    ) -> Result<(), Error> {
        let (helper, fifo) = SessionHelper::new(self.clone(), offset_map, block_size)?;
        let session = Arc::new(Session {
            helper: Arc::new(helper),
            interface: self.interface.clone(),
            close_callback: Mutex::new(None),
        });

        let (stop_sender, stop_receiver) = futures::channel::oneshot::channel();

        let mut stream = stream.fuse();
        let scope = fasync::Scope::new();
        let session_clone = session.clone();
        let mut fifo_task = scope
            .spawn(async move {
                if let Err(status) = session_clone.run_fifo(fifo, stop_receiver).await {
                    if status != zx::Status::PEER_CLOSED {
                        log::error!(status:?; "FIFO error");
                    }
                }
            })
            .fuse();

        // Make sure we detach VMOs when we go out of scope.
        scopeguard::defer! {
            for (_, (vmo, _)) in session.helper.take_vmos() {
                self.interface.on_detach_vmo(&vmo);
            }
        }

        let mut closing = false;
        let mut stop_sender = Some(stop_sender);

        loop {
            futures::select! {
                maybe_req = if closing {
                    futures::future::pending().left_future()
                } else {
                    stream.next().right_future()
                } => {
                    if let Some(req) = maybe_req {
                        match session.helper.handle_request(req?).await? {
                            HandleRequestResult::Ok => {},
                            HandleRequestResult::Closed(callback) => {
                                *session.close_callback.lock() = Some(callback);
                                // Client explicitly closed stream, stop processing.
                                if let Some(sender) = stop_sender.take() {
                                    let _ = sender.send(());
                                }
                                closing = true;
                            }
                        }
                    } else {
                        // Client end of stream dropped, stop processing.
                        if let Some(sender) = stop_sender.take() {
                            let _ = sender.send(());
                        }
                        closing = true;
                    }
                }
                _ = fifo_task => break,
            }
        }
        Ok(())
    }
}

pub struct Session<I: Interface + ?Sized> {
    interface: Arc<I>,
    helper: Arc<SessionHelper<SessionManager<I>>>,
    close_callback: Mutex<Option<Box<dyn FnOnce() + Send + 'static>>>,
}

impl<I: Interface + ?Sized> Session<I> {
    // A task loop for receiving and responding to FIFO requests.
    async fn run_fifo(
        &self,
        fifo: zx::Fifo<BlockFifoRequest, BlockFifoResponse>,
        stop_signal: futures::channel::oneshot::Receiver<()>,
    ) -> Result<(), zx::Status> {
        scopeguard::defer! {
            // Ensure that we always clean up active requests for this session upon FIFO
            // termination.
            self.helper.drop_active_requests(|session| *session == self as *const _ as usize);
        }

        // The FIFO has to be processed by a single task due to implementation constraints on
        // fuchsia_async::Fifo.  Thus, we use an event loop to drive the FIFO.  FIFO reads and
        // writes can happen in batch, and request processing is parallel.
        //
        // The general flow is:
        //  - Read messages from the FIFO, write into `requests`.
        //  - Read `requests`, decode them, and spawn a task to process them in `active_requests`,
        //    which will eventually write them into `responses`.
        //  - Read `responses` and write out to the FIFO.
        let mut fifo = fasync::Fifo::from_fifo(fifo);
        let (mut reader, mut writer) = fifo.async_io();
        let mut requests = [MaybeUninit::<BlockFifoRequest>::uninit(); FIFO_MAX_REQUESTS];
        let active_requests = &self.helper.session_manager().active_requests;
        let mut active_request_futures = FuturesUnordered::new();
        let mut responses = Vec::new();

        // We map requests using a single future `map_future`.  `pending_mappings` is used to queue
        // up requests that need to be mapped.  This will serialise how mappings occur which might
        // make updating mapping caches simpler.  If this proves to be a performance issue, we can
        // optimise it.
        let mut map_future = pin!(Fuse::terminated());
        let mut pending_mappings: VecDeque<DecodedRequest> = VecDeque::new();

        // When `stop_signal` is received, we stop reading from the FIFO and wait for in-flight
        // tasks to complete.
        let mut stop_signal = pin!(stop_signal.fuse());
        let mut is_closed = false;

        loop {
            let new_requests = {
                // We provide some flow control by limiting how many in-flight requests we will
                // allow.
                let pending_requests = active_request_futures.len() + responses.len();

                if is_closed
                    && pending_requests == 0
                    && map_future.is_terminated()
                    && pending_mappings.is_empty()
                {
                    return Ok(());
                }

                let count = requests.len().saturating_sub(pending_requests);
                let mut receive_requests = pin!(if count == 0 || is_closed {
                    Fuse::terminated()
                } else {
                    reader.read_entries(&mut requests[..count]).fuse()
                });
                let mut send_responses = pin!(if responses.is_empty() {
                    Fuse::terminated()
                } else {
                    poll_fn(|cx| -> Poll<Result<(), zx::Status>> {
                        match ready!(writer.try_write(cx, &responses[..])) {
                            Ok(written) => {
                                responses.drain(..written);
                                Poll::Ready(Ok(()))
                            }
                            Err(status) => Poll::Ready(Err(status)),
                        }
                    })
                    .fuse()
                });

                // Order is important here.  We want to prioritize sending results on the FIFO and
                // processing FIFO messages over receiving new ones, to provide flow control.
                select_biased!(
                    res = send_responses => {
                        res?;
                        0
                    },
                    response = active_request_futures.select_next_some() => {
                        responses.extend(response);
                        0
                    }
                    result = map_future => {
                        match result {
                            Ok((request, remainder, commit_decompression_buffers)) => {
                                active_request_futures.push(self.process_fifo_request(
                                    request,
                                    commit_decompression_buffers,
                                ));
                                if let Some(remainder) = remainder {
                                    map_future.set(
                                        self.map_request_or_get_response(remainder).fuse()
                                    );
                                }
                            }
                            Err(response) => responses.extend(response),
                        }
                        if map_future.is_terminated() {
                            if let Some(request) = pending_mappings.pop_front() {
                                map_future.set(self.map_request_or_get_response(request).fuse());
                            }
                        }
                        0
                    }
                    _ = stop_signal => {
                        is_closed = true;
                        0
                    }
                    count = receive_requests => {
                        count?
                    }
                )
            };

            // NB: It is very important that there are no `await`s for the rest of the loop body, as
            // otherwise active requests might become stalled.
            for request in &mut requests[..new_requests] {
                match self.helper.decode_fifo_request(self as *const _ as usize, unsafe {
                    request.assume_init_mut()
                }) {
                    Ok(DecodedRequest {
                        operation: Operation::CloseVmo, vmo, request_id, ..
                    }) => {
                        if let Some(vmo) = vmo {
                            self.interface.on_detach_vmo(vmo.as_ref());
                        }
                        responses.extend(
                            active_requests
                                .complete_and_take_response(request_id, zx::Status::OK)
                                .map(|(_, response)| response),
                        );
                    }
                    Ok(request) => {
                        if map_future.is_terminated() {
                            map_future.set(self.map_request_or_get_response(request).fuse());
                        } else {
                            pending_mappings.push_back(request);
                        }
                    }
                    Err(None) => {}
                    Err(Some(response)) => responses.push(response),
                }
            }
        }
    }

    async fn map_request_or_get_response(
        &self,
        request: DecodedRequest,
    ) -> Result<(DecodedRequest, Option<DecodedRequest>, bool), Option<BlockFifoResponse>> {
        let request_id = request.request_id;
        self.map_request(request).await.map_err(|status| {
            self.helper
                .orchestrator
                .active_requests
                .complete_and_take_response(request_id, status)
                .map(|(_, r)| r)
        })
    }

    // NOTE: The implementation of this currently assumes that we are only processing a single map
    // request at a time.
    async fn map_request(
        &self,
        mut request: DecodedRequest,
    ) -> Result<(DecodedRequest, Option<DecodedRequest>, bool), zx::Status> {
        let mut active_requests;
        let active_request;
        let mut commit_decompression_buffers = false;
        let flags = self.interface.get_info().as_ref().device_flags();
        // Strip the PRE_BARRIER flag if we don't support it, and simulate the barrier with a
        // pre-flush.
        if !flags.contains(DeviceFlag::BARRIER_SUPPORT)
            && request.operation.take_write_flag(WriteFlags::PRE_BARRIER)
        {
            if let Some(id) = request.trace_flow_id {
                fuchsia_trace::async_instant!(
                    fuchsia_trace::Id::from(id.get()),
                    "storage",
                    "block_server::SimulatedBarrier",
                    "request_id" => request.request_id.0
                );
            }
            self.interface.flush(request.trace_flow_id).await?;
        }

        // Handle decompressed read operations by turning them into regular read operations.
        match request.operation {
            Operation::StartDecompressedRead {
                required_buffer_size,
                device_block_offset,
                block_count,
                options,
            } => {
                let allocator = match self.helper.session_manager().buffer_allocator.get() {
                    Some(a) => a,
                    None => {
                        // This isn't racy because there should only be one `map_request` future
                        // running at any one time.
                        let source = BufferSource::new(fblock::MAX_DECOMPRESSED_BYTES as usize);
                        self.interface.on_attach_vmo(&source.vmo()).await?;
                        let allocator = BufferAllocator::new(
                            std::cmp::max(
                                self.helper.block_size as usize,
                                zx::system_get_page_size() as usize,
                            ),
                            source,
                        );
                        self.helper.session_manager().buffer_allocator.set(allocator).unwrap();
                        self.helper.session_manager().buffer_allocator.get().unwrap()
                    }
                };

                if required_buffer_size > fblock::MAX_DECOMPRESSED_BYTES as usize {
                    return Err(zx::Status::OUT_OF_RANGE);
                }

                let buffer = allocator.allocate_buffer(required_buffer_size).await;
                let vmo_offset = buffer.range().start as u64;

                // # Safety
                //
                // See below.
                unsafe fn remove_lifetime(buffer: Buffer<'_>) -> Buffer<'static> {
                    unsafe { std::mem::transmute(buffer) }
                }

                active_requests = self.helper.session_manager().active_requests.0.lock();
                active_request = &mut active_requests.requests[request.request_id.0];

                // SAFETY: We guarantee that `buffer_allocator` is dropped after `active_requests`,
                // so this should be safe.
                active_request.decompression_info.as_mut().unwrap().buffer =
                    Some(unsafe { remove_lifetime(buffer) });

                request.operation = Operation::Read {
                    device_block_offset,
                    block_count,
                    _unused: 0,
                    vmo_offset,
                    options,
                };
                request.vmo = Some(allocator.buffer_source().vmo().clone());

                commit_decompression_buffers = true;
            }
            Operation::ContinueDecompressedRead {
                offset,
                device_block_offset,
                block_count,
                options,
            } => {
                active_requests = self.helper.session_manager().active_requests.0.lock();
                active_request = &mut active_requests.requests[request.request_id.0];

                let buffer =
                    active_request.decompression_info.as_ref().unwrap().buffer.as_ref().unwrap();

                // Make sure this read won't overflow our buffer.
                if offset >= buffer.len() as u64
                    || buffer.len() as u64 - offset
                        < block_count as u64 * self.helper.block_size as u64
                {
                    return Err(zx::Status::OUT_OF_RANGE);
                }

                request.operation = Operation::Read {
                    device_block_offset,
                    block_count,
                    _unused: 0,
                    vmo_offset: buffer.range().start as u64 + offset,
                    options,
                };

                let allocator = self.helper.session_manager().buffer_allocator.get().unwrap();
                request.vmo = Some(allocator.buffer_source().vmo().clone());
            }
            _ => {
                active_requests = self.helper.session_manager().active_requests.0.lock();
                active_request = &mut active_requests.requests[request.request_id.0];
            }
        }

        // NB: We propagate the FORCE_ACCESS flag to *both* request and remainder, even if we're
        // using simulated FUA.  However, in `process_fifo_request`, we'll only do the post-flush
        // once the last request completes.
        self.helper
            .map_request(request, active_request)
            .map(|(request, remainder)| (request, remainder, commit_decompression_buffers))
    }

    /// Processes a fifo request.
    async fn process_fifo_request(
        &self,
        DecodedRequest { request_id, operation, vmo, trace_flow_id }: DecodedRequest,
        commit_decompression_buffers: bool,
    ) -> Option<BlockFifoResponse> {
        let mut needs_postflush = false;
        let result = match operation {
            Operation::Read { device_block_offset, block_count, _unused, vmo_offset, options } => {
                join(
                    self.interface.read(
                        device_block_offset,
                        block_count,
                        vmo.as_ref().unwrap(),
                        vmo_offset,
                        options,
                        trace_flow_id,
                    ),
                    async {
                        if commit_decompression_buffers {
                            let (target_slice, buffer_slice, buffer_range) = {
                                let active_request = self
                                    .helper
                                    .session_manager()
                                    .active_requests
                                    .request(request_id);
                                let info = active_request.decompression_info.as_ref().unwrap();
                                (
                                    info.uncompressed_slice(),
                                    self.helper
                                        .orchestrator
                                        .buffer_allocator
                                        .get()
                                        .unwrap()
                                        .buffer_source()
                                        .slice(),
                                    info.buffer.as_ref().unwrap().range(),
                                )
                            };
                            let vmar = fuchsia_runtime::vmar_root_self();
                            // The target slice might not be page aligned.
                            let addr = target_slice.addr();
                            let unaligned = addr % zx::system_get_page_size() as usize;
                            if let Err(error) = vmar.op_range(
                                zx::VmarOp::COMMIT,
                                addr - unaligned,
                                target_slice.len() + unaligned,
                            ) {
                                log::warn!(error:?; "Unable to commit target range");
                            }
                            // But the buffer range should be.
                            if let Err(error) = vmar.op_range(
                                zx::VmarOp::PREFETCH,
                                buffer_slice.addr() + buffer_range.start,
                                buffer_range.len(),
                            ) {
                                log::warn!(
                                    error:?,
                                    buffer_range:?;
                                    "Unable to prefetch source range",
                                );
                            }
                        }
                    },
                )
                .await
                .0
            }
            Operation::Write {
                device_block_offset,
                block_count,
                _unused,
                vmo_offset,
                mut options,
            } => {
                // Strip the FORCE_ACCESS flag if we don't support it, and simulate the FUA with a
                // post-flush.
                if options.flags.contains(WriteFlags::FORCE_ACCESS) {
                    let flags = self.interface.get_info().as_ref().device_flags();
                    if !flags.contains(DeviceFlag::FUA_SUPPORT) {
                        options.flags.remove(WriteFlags::FORCE_ACCESS);
                        needs_postflush = true;
                    }
                }
                self.interface
                    .write(
                        device_block_offset,
                        block_count,
                        vmo.as_ref().unwrap(),
                        vmo_offset,
                        options,
                        trace_flow_id,
                    )
                    .await
            }
            Operation::Flush => self.interface.flush(trace_flow_id).await,
            Operation::Trim { device_block_offset, block_count } => {
                self.interface.trim(device_block_offset, block_count, trace_flow_id).await
            }
            Operation::CloseVmo
            | Operation::StartDecompressedRead { .. }
            | Operation::ContinueDecompressedRead { .. } => {
                // Handled in main request loop
                unreachable!()
            }
        };
        let response = self
            .helper
            .orchestrator
            .active_requests
            .complete_and_take_response(request_id, result.into())
            .map(|(_, r)| r);
        if let Some(mut response) = response {
            // Only do the post-flush on the very last request, and only if successful.
            if zx::Status::from_raw(response.status) == zx::Status::OK && needs_postflush {
                if let Some(id) = trace_flow_id {
                    fuchsia_trace::async_instant!(
                        fuchsia_trace::Id::from(id.get()),
                        "storage",
                        "block_server::SimulatedFUA",
                        "request_id" => request_id.0
                    );
                }
                response.status =
                    zx::Status::from(self.interface.flush(trace_flow_id).await).into_raw();
            }
            Some(response)
        } else {
            response
        }
    }
}

impl<I: Interface + ?Sized> super::SessionManager for SessionManager<I> {
    type Orchestrator = Self;

    const SUPPORTS_DECOMPRESSION: bool = true;

    // We don't need the session, we just need something unique to identify the session.
    type Session = usize;

    fn session_eq(a: &usize, b: &usize) -> bool {
        a == b
    }

    async fn on_attach_vmo(orchestrator: Arc<Self>, vmo: &Arc<zx::Vmo>) -> Result<(), zx::Status> {
        I::on_attach_vmo(&orchestrator.interface, vmo).await
    }

    async fn open_session(
        orchestrator: Arc<Self>,
        stream: fblock::SessionRequestStream,
        offset_map: OffsetMap,
        block_size: u32,
    ) -> Result<(), Error> {
        I::open_session(
            &orchestrator.interface,
            orchestrator.clone(),
            stream,
            offset_map,
            block_size,
        )
        .await
    }

    fn get_info(&self) -> Cow<'_, DeviceInfo> {
        self.interface.get_info()
    }

    async fn get_volume_info(
        &self,
    ) -> Result<(fblock::VolumeManagerInfo, fblock::VolumeInfo), zx::Status> {
        self.interface.get_volume_info().await
    }

    async fn query_slices(
        &self,
        start_slices: &[u64],
    ) -> Result<Vec<fblock::VsliceRange>, zx::Status> {
        self.interface.query_slices(start_slices).await
    }

    async fn extend(&self, start_slice: u64, slice_count: u64) -> Result<(), zx::Status> {
        self.interface.extend(start_slice, slice_count).await
    }

    async fn shrink(&self, start_slice: u64, slice_count: u64) -> Result<(), zx::Status> {
        self.interface.shrink(start_slice, slice_count).await
    }

    fn active_requests(&self) -> &ActiveRequests<Self::Session> {
        return &self.active_requests;
    }
}

impl<I: Interface + ?Sized> Drop for Session<I> {
    fn drop(&mut self) {
        let callback = std::mem::take(&mut *self.close_callback.lock());
        if let Some(callback) = callback {
            callback();
        }
    }
}

impl<I: Interface> IntoOrchestrator for Arc<I> {
    type SM = SessionManager<I>;

    fn into_orchestrator(self) -> Arc<Self::SM> {
        Arc::new(SessionManager {
            interface: self,
            active_requests: ActiveRequests::default(),
            buffer_allocator: OnceLock::new(),
        })
    }
}
