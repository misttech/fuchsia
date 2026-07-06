// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use anyhow::Error;
use block_protocol::{BlockFifoRequest, BlockFifoResponse};
use fblock::{BlockIoFlag, BlockOpcode, MAX_TRANSFER_UNBOUNDED};
use fidl_fuchsia_storage_block as fblock;
use fuchsia_async as fasync;
use fuchsia_async::epoch::{Epoch, EpochGuard};
use fuchsia_sync::{MappedMutexGuard, Mutex, MutexGuard};
use futures::{Future, FutureExt as _, TryStreamExt as _};
use slab::Slab;
use std::borrow::{Borrow, Cow};
use std::collections::BTreeMap;
use std::num::NonZero;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use storage_device::buffer::Buffer;

pub mod async_interface;
pub mod c_interface;
pub mod callback_interface;

#[cfg(test)]
mod decompression_tests;

pub(crate) const FIFO_MAX_REQUESTS: usize = 64;

type TraceFlowId = Option<NonZero<u64>>;

#[derive(Clone)]
pub enum DeviceInfo {
    Block(BlockInfo),
    Partition(PartitionInfo),
}

impl DeviceInfo {
    pub fn label(&self) -> &str {
        match self {
            Self::Block(BlockInfo { .. }) => "",
            Self::Partition(PartitionInfo { name, .. }) => name,
        }
    }
    pub fn device_flags(&self) -> fblock::DeviceFlag {
        match self {
            Self::Block(BlockInfo { device_flags, .. }) => *device_flags,
            Self::Partition(PartitionInfo { device_flags, .. }) => *device_flags,
        }
    }

    pub fn block_count(&self) -> Option<u64> {
        match self {
            Self::Block(BlockInfo { block_count, .. }) => Some(*block_count),
            Self::Partition(PartitionInfo { block_range, .. }) => {
                block_range.as_ref().map(|range| range.end - range.start)
            }
        }
    }

    pub fn max_transfer_blocks(&self) -> Option<NonZero<u32>> {
        match self {
            Self::Block(BlockInfo { max_transfer_blocks, .. }) => max_transfer_blocks.clone(),
            Self::Partition(PartitionInfo { max_transfer_blocks, .. }) => {
                max_transfer_blocks.clone()
            }
        }
    }

    fn max_transfer_size(&self, block_size: u32) -> u32 {
        if let Some(max_blocks) = self.max_transfer_blocks() {
            max_blocks.get() * block_size
        } else {
            MAX_TRANSFER_UNBOUNDED
        }
    }
}

/// Information associated with non-partition block devices.
#[derive(Clone, Default)]
pub struct BlockInfo {
    pub device_flags: fblock::DeviceFlag,
    pub block_count: u64,
    pub max_transfer_blocks: Option<NonZero<u32>>,
}

/// Information associated with a block device that is also a partition.
#[derive(Clone, Default)]
pub struct PartitionInfo {
    /// The device flags reported by the underlying device.
    pub device_flags: fblock::DeviceFlag,
    pub max_transfer_blocks: Option<NonZero<u32>>,
    /// If `block_range` is None, the partition is a volume and may not be contiguous.
    /// In this case, the server will use the `get_volume_info` method to get the count of assigned
    /// slices and use that (along with the slice and block sizes) to determine the block count.
    pub block_range: Option<Range<u64>>,
    pub type_guid: [u8; 16],
    pub instance_guid: [u8; 16],
    pub name: String,
    pub flags: u64,
}

/// We internally keep track of active requests, so that when the server is torn down, we can
/// deallocate all of the resources for pending requests.
struct ActiveRequest<S> {
    session: S,
    group_or_request: GroupOrRequest,
    trace_flow_id: TraceFlowId,
    _epoch_guard: EpochGuard<'static>,
    status: zx::Status,
    count: u32,
    req_id: Option<u32>,
    decompression_info: Option<DecompressionInfo>,
}

struct DecompressionInfo {
    // This is the range of compressed bytes in receiving buffer.
    compressed_range: Range<usize>,

    // This is the range in the target VMO where we will write uncompressed bytes.
    uncompressed_range: Range<u64>,

    bytes_so_far: u64,
    mapping: Arc<VmoMapping>,
    buffer: Option<Buffer<'static>>,
}

impl DecompressionInfo {
    /// Returns the uncompressed slice.
    fn uncompressed_slice(&self) -> *mut [u8] {
        std::ptr::slice_from_raw_parts_mut(
            (self.mapping.base + self.uncompressed_range.start as usize) as *mut u8,
            (self.uncompressed_range.end - self.uncompressed_range.start) as usize,
        )
    }
}

pub struct ActiveRequests<S>(Mutex<ActiveRequestsInner<S>>);

impl<S> Default for ActiveRequests<S> {
    fn default() -> Self {
        Self(Mutex::new(ActiveRequestsInner { requests: Slab::default() }))
    }
}

impl<S> ActiveRequests<S> {
    fn complete_and_take_response(
        &self,
        request_id: RequestId,
        status: zx::Status,
    ) -> Option<(S, BlockFifoResponse)> {
        self.0.lock().complete_and_take_response(request_id, status)
    }

    fn request(&self, request_id: RequestId) -> MappedMutexGuard<'_, ActiveRequest<S>> {
        MutexGuard::map(self.0.lock(), |i| &mut i.requests[request_id.0])
    }
}

struct ActiveRequestsInner<S> {
    requests: Slab<ActiveRequest<S>>,
}

// Keeps track of all the requests that are currently being processed
impl<S> ActiveRequestsInner<S> {
    /// Completes a request.
    fn complete(&mut self, request_id: RequestId, status: zx::Status) {
        let group = &mut self.requests[request_id.0];

        group.count = group.count.checked_sub(1).unwrap();
        if status != zx::Status::OK && group.status == zx::Status::OK {
            group.status = status
        }

        fuchsia_trace::duration!(
            "storage",
            "block_server::finish_transaction",
            "request_id" => request_id.0,
            "group_completed" => group.count == 0,
            "status" => status.into_raw());
        if let Some(trace_flow_id) = group.trace_flow_id {
            fuchsia_trace::flow_step!(
                "storage",
                "block_server::finish_request",
                trace_flow_id.get().into()
            );
        }

        if group.count == 0
            && group.status == zx::Status::OK
            && let Some(info) = &mut group.decompression_info
        {
            thread_local! {
                static DECOMPRESSOR: std::cell::RefCell<zstd::bulk::Decompressor<'static>> =
                    std::cell::RefCell::new(zstd::bulk::Decompressor::new().unwrap());
            }
            DECOMPRESSOR.with(|decompressor| {
                // SAFETY: We verified `uncompressed_range` fits within our mapping.
                let target_slice = unsafe { info.uncompressed_slice().as_mut().unwrap() };
                let mut decompressor = decompressor.borrow_mut();
                if let Err(error) = decompressor.decompress_to_buffer(
                    &info.buffer.take().unwrap().as_slice()[info.compressed_range.clone()],
                    target_slice,
                ) {
                    log::warn!(error:?; "Decompression error");
                    group.status = zx::Status::IO_DATA_INTEGRITY;
                };
            });
        }
    }

    /// Takes the response if all requests are finished.
    fn take_response(&mut self, request_id: RequestId) -> Option<(S, BlockFifoResponse)> {
        let group = &self.requests[request_id.0];
        match group.req_id {
            Some(reqid) if group.count == 0 => {
                let group = self.requests.remove(request_id.0);
                Some((
                    group.session,
                    BlockFifoResponse {
                        status: group.status.into_raw(),
                        reqid,
                        group: group.group_or_request.group_id().unwrap_or(0),
                        ..Default::default()
                    },
                ))
            }
            _ => None,
        }
    }

    /// Competes the request and returns a response if the request group is finished.
    fn complete_and_take_response(
        &mut self,
        request_id: RequestId,
        status: zx::Status,
    ) -> Option<(S, BlockFifoResponse)> {
        self.complete(request_id, status);
        self.take_response(request_id)
    }
}

/// BlockServer is an implementation of fuchsia.hardware.block.partition.Partition.
/// cbindgen:no-export
pub struct BlockServer<SM: SessionManager> {
    block_size: u32,
    orchestrator: Arc<SM::Orchestrator>,
}

/// A single entry in `[OffsetMap]`.
#[derive(Debug)]
pub struct BlockOffsetMapping {
    source_block_offset: u64,
    target_block_offset: u64,
    length: u64,
}

impl BlockOffsetMapping {
    fn are_blocks_within_source_range(&self, blocks: (u64, u32)) -> bool {
        blocks.0 >= self.source_block_offset
            && blocks.0 + blocks.1 as u64 - self.source_block_offset <= self.length
    }
}

impl std::convert::TryFrom<fblock::BlockOffsetMapping> for BlockOffsetMapping {
    type Error = zx::Status;

    fn try_from(wire: fblock::BlockOffsetMapping) -> Result<Self, Self::Error> {
        wire.source_block_offset.checked_add(wire.length).ok_or(zx::Status::INVALID_ARGS)?;
        wire.target_block_offset.checked_add(wire.length).ok_or(zx::Status::INVALID_ARGS)?;
        Ok(Self {
            source_block_offset: wire.source_block_offset,
            target_block_offset: wire.target_block_offset,
            length: wire.length,
        })
    }
}

/// Remaps the offset of block requests based on an internal map, and truncates long requests.
pub struct OffsetMap {
    mapping: Option<BlockOffsetMapping>,
    max_transfer_blocks: Option<NonZero<u32>>,
}

impl OffsetMap {
    /// An OffsetMap that remaps requests.
    pub fn new(mapping: BlockOffsetMapping, max_transfer_blocks: Option<NonZero<u32>>) -> Self {
        Self { mapping: Some(mapping), max_transfer_blocks }
    }

    /// An OffsetMap that just enforces maximum request sizes.
    pub fn empty(max_transfer_blocks: Option<NonZero<u32>>) -> Self {
        Self { mapping: None, max_transfer_blocks }
    }

    pub fn is_empty(&self) -> bool {
        self.mapping.is_none()
    }

    fn mapping(&self) -> Option<&BlockOffsetMapping> {
        self.mapping.as_ref()
    }

    fn max_transfer_blocks(&self) -> Option<NonZero<u32>> {
        self.max_transfer_blocks
    }
}

// Methods take Arc<Self> rather than &self because of
// https://github.com/rust-lang/rust/issues/42940.
pub trait SessionManager: 'static {
    /// The Orchestrator is an object that holds the `SessionManager` and any other state that needs
    /// to be shared between sessions.  It is responsible for keeping the `SessionManager` alive.
    /// We use this type instead of directly holding an Arc<SessionManager> in BlockServer, to avoid
    /// nested Arcs in concrete implementations which need to keep additional state.
    type Orchestrator: Borrow<Self> + Send + Sync;

    const SUPPORTS_DECOMPRESSION: bool;

    type Session;

    /// Returns true iff `a` and `b` identify the same session.  Used to scope
    /// group-ID lookups in the shared `active_requests` slab to the originating
    /// session.
    fn session_eq(a: &Self::Session, b: &Self::Session) -> bool;

    fn on_attach_vmo(
        orchestrator: Arc<Self::Orchestrator>,
        vmo: &Arc<zx::Vmo>,
    ) -> impl Future<Output = Result<(), zx::Status>> + Send;

    /// Creates a new session to handle `stream`.
    /// The returned future should run until the session completes, for example when the client end
    /// closes.
    /// `offset_map`, will be used to adjust the block offset/length of FIFO requests.
    fn open_session(
        orchestrator: Arc<Self::Orchestrator>,
        stream: fblock::SessionRequestStream,
        offset_map: OffsetMap,
        block_size: u32,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Called to get block/partition information for Block::GetInfo, Partition::GetTypeGuid, etc.
    fn get_info(&self) -> Cow<'_, DeviceInfo>;

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

    /// Called to handle the Shrink FIDL call.
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

    /// Returns the active requests.
    fn active_requests(&self) -> &ActiveRequests<Self::Session>;
}

/// A helper trait for converting various types into an `Orchestrator`.
///
/// This exists to simplify [`BlockServer::new`].
pub trait IntoOrchestrator {
    type SM: SessionManager;

    fn into_orchestrator(self) -> Arc<<Self::SM as SessionManager>::Orchestrator>;
}

impl<SM: SessionManager> BlockServer<SM> {
    pub fn new(block_size: u32, orchestrator: impl IntoOrchestrator<SM = SM>) -> Self {
        Self { block_size, orchestrator: orchestrator.into_orchestrator() }
    }

    pub fn session_manager(&self) -> &SM {
        self.orchestrator.as_ref().borrow()
    }

    /// Called to process requests for fuchsia.storage.block.Block.
    pub async fn handle_requests(
        &self,
        mut requests: fblock::BlockRequestStream,
    ) -> Result<(), Error> {
        let scope = fasync::Scope::new();
        loop {
            match requests.try_next().await {
                Ok(Some(request)) => {
                    if let Some(session) = self.handle_request(request).await? {
                        scope.spawn(session.map(|_| ()));
                    }
                }
                Ok(None) => break,
                Err(err) => log::warn!(err:?; "Invalid request"),
            }
        }
        scope.await;
        Ok(())
    }

    /// Processes a Block request.  If a new session task is created in response to the request,
    /// it is returned.
    async fn handle_request(
        &self,
        request: fblock::BlockRequest,
    ) -> Result<Option<impl Future<Output = Result<(), Error>> + Send + use<SM>>, Error> {
        match request {
            fblock::BlockRequest::GetInfo { responder } => {
                let info = self.device_info();
                let max_transfer_size = info.max_transfer_size(self.block_size);
                let (block_count, mut flags) = match info.as_ref() {
                    DeviceInfo::Block(BlockInfo { block_count, device_flags, .. }) => {
                        (*block_count, *device_flags)
                    }
                    DeviceInfo::Partition(partition_info) => {
                        let block_count = if let Some(range) = partition_info.block_range.as_ref() {
                            range.end - range.start
                        } else {
                            let volume_info = self.session_manager().get_volume_info().await?;
                            volume_info.0.slice_size * volume_info.1.partition_slice_count
                                / self.block_size as u64
                        };
                        (block_count, partition_info.device_flags)
                    }
                };
                if SM::SUPPORTS_DECOMPRESSION {
                    flags |= fblock::DeviceFlag::ZSTD_DECOMPRESSION_SUPPORT;
                }
                responder.send(Ok(&fblock::BlockInfo {
                    block_count,
                    block_size: self.block_size,
                    max_transfer_size,
                    flags,
                }))?;
            }
            fblock::BlockRequest::OpenSession { session, control_handle: _ } => {
                let info = self.device_info();
                return Ok(Some(SM::open_session(
                    self.orchestrator.clone(),
                    session.into_stream(),
                    OffsetMap::empty(info.max_transfer_blocks()),
                    self.block_size,
                )));
            }
            fblock::BlockRequest::OpenSessionWithOffsetMap {
                session,
                mapping,
                control_handle: _,
            } => {
                let info = self.device_info();
                let initial_mapping: BlockOffsetMapping = match mapping.try_into() {
                    Ok(m) => m,
                    Err(status) => {
                        session.close_with_epitaph(status)?;
                        return Ok(None);
                    }
                };
                if let Some(max) = info.block_count() {
                    if initial_mapping.target_block_offset + initial_mapping.length > max {
                        log::warn!("Invalid mapping for session: {initial_mapping:?} (max {max})");
                        session.close_with_epitaph(zx::Status::INVALID_ARGS)?;
                        return Ok(None);
                    }
                }
                return Ok(Some(SM::open_session(
                    self.orchestrator.clone(),
                    session.into_stream(),
                    OffsetMap::new(initial_mapping, info.max_transfer_blocks()),
                    self.block_size,
                )));
            }
            fblock::BlockRequest::GetTypeGuid { responder } => {
                let info = self.device_info();
                if let DeviceInfo::Partition(partition_info) = info.as_ref() {
                    let mut guid = fblock::Guid { value: [0u8; fblock::GUID_LENGTH as usize] };
                    guid.value.copy_from_slice(&partition_info.type_guid);
                    responder.send(zx::sys::ZX_OK, Some(&guid))?;
                } else {
                    responder.send(zx::sys::ZX_ERR_NOT_SUPPORTED, None)?;
                }
            }
            fblock::BlockRequest::GetInstanceGuid { responder } => {
                let info = self.device_info();
                if let DeviceInfo::Partition(partition_info) = info.as_ref() {
                    let mut guid = fblock::Guid { value: [0u8; fblock::GUID_LENGTH as usize] };
                    guid.value.copy_from_slice(&partition_info.instance_guid);
                    responder.send(zx::sys::ZX_OK, Some(&guid))?;
                } else {
                    responder.send(zx::sys::ZX_ERR_NOT_SUPPORTED, None)?;
                }
            }
            fblock::BlockRequest::GetName { responder } => {
                let info = self.device_info();
                if let DeviceInfo::Partition(partition_info) = info.as_ref() {
                    responder.send(zx::sys::ZX_OK, Some(&partition_info.name))?;
                } else {
                    responder.send(zx::sys::ZX_ERR_NOT_SUPPORTED, None)?;
                }
            }
            fblock::BlockRequest::GetMetadata { responder } => {
                let info = self.device_info();
                if let DeviceInfo::Partition(info) = info.as_ref() {
                    let mut type_guid = fblock::Guid { value: [0u8; fblock::GUID_LENGTH as usize] };
                    type_guid.value.copy_from_slice(&info.type_guid);
                    let mut instance_guid =
                        fblock::Guid { value: [0u8; fblock::GUID_LENGTH as usize] };
                    instance_guid.value.copy_from_slice(&info.instance_guid);
                    responder.send(Ok(&fblock::BlockGetMetadataResponse {
                        name: Some(info.name.clone()),
                        type_guid: Some(type_guid),
                        instance_guid: Some(instance_guid),
                        start_block_offset: info.block_range.as_ref().map(|range| range.start),
                        num_blocks: info.block_range.as_ref().map(|range| range.end - range.start),
                        flags: Some(info.flags),
                        ..Default::default()
                    }))?;
                } else {
                    responder.send(Err(zx::sys::ZX_ERR_NOT_SUPPORTED))?;
                }
            }
            fblock::BlockRequest::QuerySlices { responder, start_slices } => {
                match self.session_manager().query_slices(&start_slices).await {
                    Ok(mut results) => {
                        let results_len = results.len();
                        assert!(results_len <= 16);
                        results.resize(16, fblock::VsliceRange { allocated: false, count: 0 });
                        responder.send(
                            zx::sys::ZX_OK,
                            &results.try_into().unwrap(),
                            results_len as u64,
                        )?;
                    }
                    Err(s) => {
                        responder.send(
                            s.into_raw(),
                            &[fblock::VsliceRange { allocated: false, count: 0 }; 16],
                            0,
                        )?;
                    }
                }
            }
            fblock::BlockRequest::GetVolumeInfo { responder, .. } => {
                match self.session_manager().get_volume_info().await {
                    Ok((manager_info, volume_info)) => {
                        responder.send(zx::sys::ZX_OK, Some(&manager_info), Some(&volume_info))?
                    }
                    Err(s) => responder.send(s.into_raw(), None, None)?,
                }
            }
            fblock::BlockRequest::Extend { responder, start_slice, slice_count } => {
                responder.send(
                    zx::Status::from(self.session_manager().extend(start_slice, slice_count).await)
                        .into_raw(),
                )?;
            }
            fblock::BlockRequest::Shrink { responder, start_slice, slice_count } => {
                responder.send(
                    zx::Status::from(self.session_manager().shrink(start_slice, slice_count).await)
                        .into_raw(),
                )?;
            }
            fblock::BlockRequest::Destroy { responder, .. } => {
                responder.send(zx::sys::ZX_ERR_NOT_SUPPORTED)?;
            }
        }
        Ok(None)
    }

    fn device_info(&self) -> Cow<'_, DeviceInfo> {
        self.session_manager().get_info()
    }
}

struct SessionHelper<SM: SessionManager> {
    orchestrator: Arc<SM::Orchestrator>,
    offset_map: OffsetMap,
    block_size: u32,
    peer_fifo: zx::Fifo<BlockFifoResponse, BlockFifoRequest>,
    vmos: Mutex<BTreeMap<u16, (Arc<zx::Vmo>, Option<Arc<VmoMapping>>)>>,
}

struct VmoMapping {
    base: usize,
    size: usize,
}

impl VmoMapping {
    fn new(vmo: &zx::Vmo) -> Result<Arc<Self>, zx::Status> {
        let size = vmo.get_size().unwrap() as usize;
        Ok(Arc::new(Self {
            base: fuchsia_runtime::vmar_root_self()
                .map(0, vmo, 0, size, zx::VmarFlags::PERM_WRITE | zx::VmarFlags::PERM_READ)
                .inspect_err(|error| {
                    log::warn!(error:?, size; "VmoMapping: unable to map VMO");
                })?,
            size,
        }))
    }
}

impl Drop for VmoMapping {
    fn drop(&mut self) {
        // SAFETY: We mapped this in `VmoMapping::new`.
        unsafe {
            let _ = fuchsia_runtime::vmar_root_self().unmap(self.base, self.size);
        }
    }
}

enum HandleRequestResult {
    /// The request was handled successfully.
    Ok,
    /// The request closed the stream.  The caller must shut down the session, and must call the
    /// provided callback after the session is completely shut down.  The caller should assume that
    /// no further requests need to be handled once this is received.
    Closed(Box<dyn FnOnce() + Send + 'static>),
}

impl<SM: SessionManager> SessionHelper<SM> {
    fn new(
        orchestrator: Arc<SM::Orchestrator>,
        offset_map: OffsetMap,
        block_size: u32,
    ) -> Result<(Self, zx::Fifo<BlockFifoRequest, BlockFifoResponse>), zx::Status> {
        let (peer_fifo, fifo) = zx::Fifo::create(16)?;
        Ok((Self { orchestrator, offset_map, block_size, peer_fifo, vmos: Mutex::default() }, fifo))
    }

    fn session_manager(&self) -> &SM {
        self.orchestrator.as_ref().borrow()
    }

    async fn handle_request(
        &self,
        request: fblock::SessionRequest,
    ) -> Result<HandleRequestResult, Error> {
        match request {
            fblock::SessionRequest::GetFifo { responder } => {
                let rights = zx::Rights::TRANSFER
                    | zx::Rights::READ
                    | zx::Rights::WRITE
                    | zx::Rights::SIGNAL
                    | zx::Rights::WAIT;
                match self.peer_fifo.duplicate_handle(rights) {
                    Ok(fifo) => responder.send(Ok(fifo.downcast()))?,
                    Err(s) => responder.send(Err(s.into_raw()))?,
                }
                Ok(HandleRequestResult::Ok)
            }
            fblock::SessionRequest::AttachVmo { vmo, responder } => {
                let vmo = Arc::new(vmo);
                let vmo_id = {
                    let mut vmos = self.vmos.lock();
                    if vmos.len() == u16::MAX as usize {
                        responder.send(Err(zx::Status::NO_RESOURCES.into_raw()))?;
                        return Ok(HandleRequestResult::Ok);
                    } else {
                        let vmo_id = match vmos.last_entry() {
                            None => 1,
                            Some(o) => {
                                o.key().checked_add(1).unwrap_or_else(|| {
                                    let mut vmo_id = 1;
                                    // Find the first gap...
                                    for (&id, _) in &*vmos {
                                        if id > vmo_id {
                                            break;
                                        }
                                        vmo_id = id + 1;
                                    }
                                    vmo_id
                                })
                            }
                        };
                        vmos.insert(vmo_id, (vmo.clone(), None));
                        vmo_id
                    }
                };
                SM::on_attach_vmo(self.orchestrator.clone(), &vmo).await?;
                responder.send(Ok(&fblock::VmoId { id: vmo_id }))?;
                Ok(HandleRequestResult::Ok)
            }
            fblock::SessionRequest::Close { responder } => {
                Ok(HandleRequestResult::Closed(Box::new(move || {
                    if let Err(err) = responder.send(Ok(())) {
                        log::warn!(err:?; "Error sending close response");
                    }
                })))
            }
        }
    }

    /// Decodes `request`.
    fn decode_fifo_request(
        &self,
        session: SM::Session,
        request: &BlockFifoRequest,
    ) -> Result<DecodedRequest, Option<BlockFifoResponse>> {
        let flags = BlockIoFlag::from_bits_truncate(request.command.flags);

        let request_bytes = request.length as u64 * self.block_size as u64;

        let mut operation = BlockOpcode::from_primitive(request.command.opcode)
            .ok_or(zx::Status::INVALID_ARGS)
            .and_then(|code| {
                if flags.contains(BlockIoFlag::DECOMPRESS_WITH_ZSTD) {
                    if code != BlockOpcode::Read {
                        return Err(zx::Status::INVALID_ARGS);
                    }
                    if !SM::SUPPORTS_DECOMPRESSION {
                        return Err(zx::Status::NOT_SUPPORTED);
                    }
                }
                if matches!(code, BlockOpcode::Read | BlockOpcode::Write | BlockOpcode::Trim) {
                    if request.length == 0 {
                        return Err(zx::Status::INVALID_ARGS);
                    }
                    // Make sure the end offsets won't wrap.
                    if request.dev_offset.checked_add(request.length as u64).is_none()
                        || (code != BlockOpcode::Trim
                            && request_bytes.checked_add(request.vmo_offset).is_none())
                    {
                        return Err(zx::Status::OUT_OF_RANGE);
                    }
                }
                Ok(match code {
                    BlockOpcode::Read => Operation::Read {
                        device_block_offset: request.dev_offset,
                        block_count: request.length,
                        _unused: 0,
                        vmo_offset: request
                            .vmo_offset
                            .checked_mul(self.block_size as u64)
                            .ok_or(zx::Status::OUT_OF_RANGE)?,
                        options: ReadOptions {
                            inline_crypto: InlineCryptoOptions {
                                is_enabled: flags.contains(BlockIoFlag::INLINE_ENCRYPTION_ENABLED),
                                dun: request.dun,
                                slot: request.slot,
                            },
                        },
                    },
                    BlockOpcode::Write => {
                        let mut options = WriteOptions {
                            inline_crypto: InlineCryptoOptions {
                                is_enabled: flags.contains(BlockIoFlag::INLINE_ENCRYPTION_ENABLED),
                                dun: request.dun,
                                slot: request.slot,
                            },
                            ..WriteOptions::default()
                        };
                        if flags.contains(BlockIoFlag::FORCE_ACCESS) {
                            options.flags |= WriteFlags::FORCE_ACCESS;
                        }
                        if flags.contains(BlockIoFlag::PRE_BARRIER) {
                            options.flags |= WriteFlags::PRE_BARRIER;
                        }
                        Operation::Write {
                            device_block_offset: request.dev_offset,
                            block_count: request.length,
                            _unused: 0,
                            options,
                            vmo_offset: request
                                .vmo_offset
                                .checked_mul(self.block_size as u64)
                                .ok_or(zx::Status::OUT_OF_RANGE)?,
                        }
                    }
                    BlockOpcode::Flush => Operation::Flush,
                    BlockOpcode::Trim => Operation::Trim {
                        device_block_offset: request.dev_offset,
                        block_count: request.length,
                    },
                    BlockOpcode::CloseVmo => Operation::CloseVmo,
                })
            });

        let group_or_request = if flags.contains(BlockIoFlag::GROUP_ITEM) {
            GroupOrRequest::Group(request.group)
        } else {
            GroupOrRequest::Request(request.reqid)
        };

        let mut active_requests = self.session_manager().active_requests().0.lock();
        let mut request_id = None;

        // Multiple Block I/O request may be sent as a group.
        // Notes:
        // - the group is identified by the group id in the request
        // - if using groups, a response will not be sent unless `BlockIoFlag::GROUP_LAST`
        //   flag is set.
        // - when processing a request of a group fails, subsequent requests of that
        //   group will not be processed.
        // - decompression is a special case, see block-fifo.h for semantics.
        //
        // Refer to sdk/fidl/fuchsia.hardware.block.driver/block.fidl for details.
        if group_or_request.is_group() {
            // Search for an existing entry that matches this group.  NOTE: This is a potentially
            // expensive way to find a group (it's iterating over all slots in the active-requests
            // slab).  This can be optimised easily should we need to.
            for (key, group) in &mut active_requests.requests {
                if group.group_or_request == group_or_request
                    && SM::session_eq(&group.session, &session)
                {
                    if group.req_id.is_some() {
                        // We have already received a request tagged as last.
                        if group.status == zx::Status::OK {
                            group.status = zx::Status::INVALID_ARGS;
                        }
                        // Ignore this request.
                        return Err(None);
                    }
                    // See if this is a continuation of a decompressed read.
                    if group.status == zx::Status::OK
                        && let Some(info) = &mut group.decompression_info
                    {
                        if let Ok(Operation::Read {
                            device_block_offset,
                            mut block_count,
                            options,
                            vmo_offset: 0,
                            ..
                        }) = operation
                        {
                            let remaining_bytes = info
                                .compressed_range
                                .end
                                .next_multiple_of(self.block_size as usize)
                                as u64
                                - info.bytes_so_far;
                            if !flags.contains(BlockIoFlag::DECOMPRESS_WITH_ZSTD)
                                || request.total_compressed_bytes != 0
                                || request.uncompressed_bytes != 0
                                || request.compressed_prefix_bytes != 0
                                || (flags.contains(BlockIoFlag::GROUP_LAST)
                                    && info.bytes_so_far + request_bytes
                                        < info.compressed_range.end as u64)
                                || (!flags.contains(BlockIoFlag::GROUP_LAST)
                                    && request_bytes >= remaining_bytes)
                            {
                                group.status = zx::Status::INVALID_ARGS;
                            } else {
                                // We are tolerant of `block_count` being more than we actually
                                // need.  This can happen if the client is working with a larger
                                // block size than the device block size.  For example, if Blobfs
                                // has a 8192 byte block size, but the device might has a 512 byte
                                // block size, it can ask for a multiple of 16 blocks, when fewer
                                // than that might actually be required to hold the compressed data.
                                // It is easier for us to tolerate this here than to get Blobfs to
                                // change to pass only the blocks that are required.
                                if request_bytes > remaining_bytes {
                                    block_count = (remaining_bytes / self.block_size as u64) as u32;
                                }

                                operation = Ok(Operation::ContinueDecompressedRead {
                                    offset: info.bytes_so_far,
                                    device_block_offset,
                                    block_count,
                                    options,
                                });

                                info.bytes_so_far += block_count as u64 * self.block_size as u64;
                            }
                        } else {
                            group.status = zx::Status::INVALID_ARGS;
                        }
                    }
                    if flags.contains(BlockIoFlag::GROUP_LAST) {
                        group.req_id = Some(request.reqid);
                        // If the group has had an error, there is no point trying to issue this
                        // request.
                        if group.status != zx::Status::OK {
                            operation = Err(group.status);
                        }
                    } else if group.status != zx::Status::OK {
                        // The group has already encountered an error, so there is no point trying
                        // to issue this request.
                        return Err(None);
                    }
                    request_id = Some(RequestId(key));
                    group.count += 1;
                    break;
                }
            }
        }

        let is_single_request =
            !flags.contains(BlockIoFlag::GROUP_ITEM) || flags.contains(BlockIoFlag::GROUP_LAST);

        let mut decompression_info = None;
        let vmo = match operation {
            Ok(Operation::Read {
                device_block_offset,
                mut block_count,
                options,
                vmo_offset,
                ..
            }) => match self.vmos.lock().get_mut(&request.vmoid) {
                Some((vmo, mapping)) => {
                    if flags.contains(BlockIoFlag::DECOMPRESS_WITH_ZSTD) {
                        let compressed_range = request.compressed_prefix_bytes as usize
                            ..request.compressed_prefix_bytes as usize
                                + request.total_compressed_bytes as usize;
                        let required_buffer_size =
                            compressed_range.end.next_multiple_of(self.block_size as usize);

                        // Validate the initial decompression request.
                        if compressed_range.start >= compressed_range.end
                            || vmo_offset.checked_add(request.uncompressed_bytes as u64).is_none()
                            || (is_single_request && request_bytes < compressed_range.end as u64)
                            || (!is_single_request && request_bytes >= required_buffer_size as u64)
                        {
                            Err(zx::Status::INVALID_ARGS)
                        } else {
                            // We are tolerant of `block_count` being more than we actually need.
                            // This can happen if the client is working in a larger block size than
                            // the device block size.  For example, Blobfs has a 8192 byte block
                            // size, but the device might have a 512 byte block size.  It is easier
                            // for us to tolerate this here than to get Blobfs to change to pass
                            // only the blocks that are required.
                            let bytes_so_far = if request_bytes > required_buffer_size as u64 {
                                block_count =
                                    (required_buffer_size / self.block_size as usize) as u32;
                                required_buffer_size as u64
                            } else {
                                request_bytes
                            };

                            // To decompress, we need to have the target VMO mapped (cached).
                            match mapping {
                                Some(mapping) => Ok(mapping.clone()),
                                None => {
                                    VmoMapping::new(&vmo).inspect(|m| *mapping = Some(m.clone()))
                                }
                            }
                            .and_then(|mapping| {
                                // Make sure the `vmo_offset` and `uncompressed_bytes` are within
                                // range.
                                if vmo_offset
                                    .checked_add(request.uncompressed_bytes as u64)
                                    .is_some_and(|end| end <= mapping.size as u64)
                                {
                                    Ok(mapping)
                                } else {
                                    Err(zx::Status::OUT_OF_RANGE)
                                }
                            })
                            .map(|mapping| {
                                // Convert the operation into a `StartDecompressedRead`
                                // operation. For non-fragmented requests, this will be the only
                                // operation, but if it's a fragmented read,
                                // `ContinueDecompressedRead` operations will follow.
                                operation = Ok(Operation::StartDecompressedRead {
                                    required_buffer_size,
                                    device_block_offset,
                                    block_count,
                                    options,
                                });
                                // Record sufficient information so that we can decompress when all
                                // the requests complete.
                                decompression_info = Some(DecompressionInfo {
                                    compressed_range,
                                    bytes_so_far,
                                    mapping,
                                    uncompressed_range: vmo_offset
                                        ..vmo_offset + request.uncompressed_bytes as u64,
                                    buffer: None,
                                });
                                None
                            })
                        }
                    } else {
                        Ok(Some(vmo.clone()))
                    }
                }
                None => Err(zx::Status::IO),
            },
            Ok(Operation::Write { .. }) => self
                .vmos
                .lock()
                .get(&request.vmoid)
                .cloned()
                .map_or(Err(zx::Status::IO), |(vmo, _)| Ok(Some(vmo))),
            Ok(Operation::CloseVmo) => {
                self.vmos.lock().remove(&request.vmoid).map_or(Err(zx::Status::IO), |(vmo, _)| {
                    let vmo_clone = vmo.clone();
                    // Make sure the VMO is dropped after all current Epoch guards have been
                    // dropped.
                    Epoch::global().defer(move || drop(vmo_clone));
                    Ok(Some(vmo))
                })
            }
            _ => Ok(None),
        }
        .unwrap_or_else(|e| {
            operation = Err(e);
            None
        });

        let trace_flow_id = NonZero::new(request.trace_flow_id);
        let request_id = request_id.unwrap_or_else(|| {
            RequestId(active_requests.requests.insert(ActiveRequest {
                session,
                group_or_request,
                trace_flow_id,
                _epoch_guard: Epoch::global().guard(),
                status: zx::Status::OK,
                count: 1,
                req_id: is_single_request.then_some(request.reqid),
                decompression_info,
            }))
        });

        Ok(DecodedRequest {
            request_id,
            trace_flow_id,
            operation: operation.map_err(|status| {
                active_requests.complete_and_take_response(request_id, status).map(|(_, r)| r)
            })?,
            vmo,
        })
    }

    fn take_vmos(&self) -> BTreeMap<u16, (Arc<zx::Vmo>, Option<Arc<VmoMapping>>)> {
        std::mem::take(&mut *self.vmos.lock())
    }

    /// Maps the request and returns the mapped request with an optional remainder.
    fn map_request(
        &self,
        mut request: DecodedRequest,
        active_request: &mut ActiveRequest<SM::Session>,
    ) -> Result<(DecodedRequest, Option<DecodedRequest>), zx::Status> {
        if active_request.status != zx::Status::OK {
            return Err(zx::Status::BAD_STATE);
        }
        let mapping = self.offset_map.mapping();
        match (mapping, request.operation.blocks()) {
            (Some(mapping), Some(blocks)) if !mapping.are_blocks_within_source_range(blocks) => {
                return Err(zx::Status::OUT_OF_RANGE);
            }
            _ => {}
        }
        let remainder = request.operation.map(
            self.offset_map.mapping(),
            self.offset_map.max_transfer_blocks(),
            self.block_size,
        );
        if remainder.is_some() {
            active_request.count += 1;
        }
        static CACHE: AtomicU64 = AtomicU64::new(0);
        if let Some(context) =
            fuchsia_trace::TraceCategoryContext::acquire_cached("storage", &CACHE)
        {
            use fuchsia_trace::ArgValue;
            let trace_args = [
                ArgValue::of("request_id", request.request_id.0),
                ArgValue::of("opcode", request.operation.trace_label()),
            ];
            let _scope =
                fuchsia_trace::duration("storage", "block_server::start_transaction", &trace_args);
            if let Some(trace_flow_id) = active_request.trace_flow_id {
                fuchsia_trace::flow_step(
                    &context,
                    "block_server::start_transaction",
                    trace_flow_id.get().into(),
                    &[],
                );
            }
        }
        let remainder = remainder.map(|operation| DecodedRequest { operation, ..request.clone() });
        Ok((request, remainder))
    }

    /// Drops all requests for which `pred` is true.
    ///
    /// NOTE: This should only be called once we are certain that the requests will not be
    /// completed asynchronously  Otherwise, requests might be completed twice.
    fn drop_active_requests(&self, pred: impl Fn(&SM::Session) -> bool) {
        self.session_manager().active_requests().0.lock().requests.retain(|_, r| !pred(&r.session));
    }

    /// Closes all grouped requests for which `pred` is true and which are held open pending the
    /// completion of their group.
    ///
    /// Normally, a request is dropped from ActiveRequests when it is completed.  However, if a
    /// request is part of a group, it will not be dropped until a request with GROUP_LAST arrives.
    /// If we're shutting down a session, the client may not ever send the GROUP_LAST, so we need to
    /// be sure to close these grouped requests.
    ///
    /// This is called during session shutdown in situations where [`Self::drop_active_requests`]
    /// cannot be used (e.g. for the callback interface, which hands off the responsibility of
    /// completing requests to its concrete implementation and cannot control when requests are
    /// completed relative to session shutdown).
    fn close_active_groups(&self, pred: impl Fn(&SM::Session) -> bool) {
        self.session_manager().active_requests().0.lock().requests.retain(|_, request| {
            if !pred(&request.session) || request.req_id.is_some() {
                return true;
            }
            // Mark the group as completed, and immediately drop any which have no outstanding
            // requests (since they will otherwise never be dropped).
            request.req_id = Some(u32::MAX);
            request.count > 0
        });
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct RequestId(usize);

#[derive(Clone, Debug)]
struct DecodedRequest {
    request_id: RequestId,
    trace_flow_id: TraceFlowId,
    operation: Operation,
    vmo: Option<Arc<zx::Vmo>>,
}

/// cbindgen:no-export
pub type WriteFlags = block_protocol::WriteFlags;
pub type WriteOptions = block_protocol::WriteOptions;
pub type ReadOptions = block_protocol::ReadOptions;
pub type InlineCryptoOptions = block_protocol::InlineCryptoOptions;

#[repr(C)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    // NOTE: On the C++ side, this ends up as a union and, for efficiency reasons, there is code
    // that assumes that some fields for reads and writes (and possibly trim) line-up (e.g. common
    // code can read `device_block_offset` from the read variant and then assume it's valid for the
    // write variant).
    Read {
        device_block_offset: u64,
        block_count: u32,
        _unused: u32,
        vmo_offset: u64,
        options: ReadOptions,
    },
    Write {
        device_block_offset: u64,
        block_count: u32,
        _unused: u32,
        vmo_offset: u64,
        options: WriteOptions,
    },
    Flush,
    Trim {
        device_block_offset: u64,
        block_count: u32,
    },
    /// This will never be seen by the C interface.
    CloseVmo,
    /// This will never be seen by the C interface.
    StartDecompressedRead {
        required_buffer_size: usize,
        device_block_offset: u64,
        block_count: u32,
        options: ReadOptions,
    },
    /// This will never be seen by the C interface.
    ContinueDecompressedRead {
        offset: u64,
        device_block_offset: u64,
        block_count: u32,
        options: ReadOptions,
    },
}

impl Operation {
    fn trace_label(&self) -> &'static str {
        match self {
            Operation::Read { .. } => "read",
            Operation::Write { .. } => "write",
            Operation::Flush { .. } => "flush",
            Operation::Trim { .. } => "trim",
            Operation::CloseVmo { .. } => "close_vmo",
            Operation::StartDecompressedRead { .. } => "start_decompressed_read",
            Operation::ContinueDecompressedRead { .. } => "continue_decompressed_read",
        }
    }

    /// Returns (offset, length).
    fn blocks(&self) -> Option<(u64, u32)> {
        match self {
            Operation::Read { device_block_offset, block_count, .. }
            | Operation::Write { device_block_offset, block_count, .. }
            | Operation::Trim { device_block_offset, block_count, .. } => {
                Some((*device_block_offset, *block_count))
            }
            _ => None,
        }
    }

    /// Returns mutable references to (offset, length).
    fn blocks_mut(&mut self) -> Option<(&mut u64, &mut u32)> {
        match self {
            Operation::Read { device_block_offset, block_count, .. }
            | Operation::Write { device_block_offset, block_count, .. }
            | Operation::Trim { device_block_offset, block_count, .. } => {
                Some((device_block_offset, block_count))
            }
            _ => None,
        }
    }

    /// Maps the operation using `mapping` and returns the remainder.  `mapping` *must* overlap the
    /// start of the operation.
    fn map(
        &mut self,
        mapping: Option<&BlockOffsetMapping>,
        max_blocks: Option<NonZero<u32>>,
        block_size: u32,
    ) -> Option<Self> {
        let mut max = match self {
            Operation::Read { .. } | Operation::Write { .. } => max_blocks.map(|m| m.get() as u64),
            _ => None,
        };
        let (offset, length) = self.blocks_mut()?;
        let orig_offset = *offset;
        if let Some(mapping) = mapping {
            let delta = *offset - mapping.source_block_offset;
            debug_assert!(*offset - mapping.source_block_offset < mapping.length);
            *offset = mapping.target_block_offset + delta;
            let mapping_max = mapping.target_block_offset + mapping.length - *offset;
            max = match max {
                None => Some(mapping_max),
                Some(m) => Some(std::cmp::min(m, mapping_max)),
            };
        };
        if let Some(max) = max {
            if *length as u64 > max {
                let rem = (*length as u64 - max) as u32;
                *length = max as u32;
                return Some(match self {
                    Operation::Read {
                        device_block_offset: _,
                        block_count: _,
                        vmo_offset,
                        _unused,
                        options,
                    } => {
                        let mut options = *options;
                        options.inline_crypto.dun += max as u32;
                        Operation::Read {
                            device_block_offset: orig_offset + max,
                            block_count: rem,
                            vmo_offset: *vmo_offset + max * block_size as u64,
                            _unused: *_unused,
                            options: options,
                        }
                    }
                    Operation::Write {
                        device_block_offset: _,
                        block_count: _,
                        _unused,
                        vmo_offset,
                        options,
                    } => {
                        let mut options = *options;
                        options.inline_crypto.dun += max as u32;
                        Operation::Write {
                            device_block_offset: orig_offset + max,
                            block_count: rem,
                            _unused: *_unused,
                            vmo_offset: *vmo_offset + max * block_size as u64,
                            options: options,
                        }
                    }
                    Operation::Trim { device_block_offset: _, block_count: _ } => {
                        Operation::Trim { device_block_offset: orig_offset + max, block_count: rem }
                    }
                    _ => unreachable!(),
                });
            }
        }
        None
    }

    /// Returns true if the specified write flags are set.
    pub fn has_write_flag(&self, value: WriteFlags) -> bool {
        if let Operation::Write { options, .. } = self {
            options.flags.contains(value)
        } else {
            false
        }
    }

    /// Removes `value` from the request's write flags and returns true if the flag was set.
    pub fn take_write_flag(&mut self, value: WriteFlags) -> bool {
        if let Operation::Write { options, .. } = self {
            let result = options.flags.contains(value);
            options.flags.remove(value);
            result
        } else {
            false
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum GroupOrRequest {
    Group(u16),
    Request(u32),
}

impl GroupOrRequest {
    fn is_group(&self) -> bool {
        matches!(self, Self::Group(_))
    }

    fn group_id(&self) -> Option<u16> {
        match self {
            Self::Group(id) => Some(*id),
            Self::Request(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BlockOffsetMapping, BlockServer, DeviceInfo, FIFO_MAX_REQUESTS, Operation, PartitionInfo,
        TraceFlowId,
    };
    use assert_matches::assert_matches;
    use block_protocol::{
        BlockFifoCommand, BlockFifoRequest, BlockFifoResponse, InlineCryptoOptions, ReadOptions,
        WriteFlags, WriteOptions,
    };
    use fidl_fuchsia_storage_block as fblock;
    use fidl_fuchsia_storage_block::{BlockIoFlag, BlockOpcode};
    use fuchsia_async as fasync;
    use fuchsia_sync::Mutex;
    use futures::FutureExt as _;
    use futures::channel::oneshot;
    use futures::future::BoxFuture;
    use std::borrow::Cow;
    use std::future::poll_fn;
    use std::num::NonZero;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::task::{Context, Poll};

    #[derive(Default)]
    struct MockInterface {
        read_hook: Option<
            Box<
                dyn Fn(u64, u32, &Arc<zx::Vmo>, u64) -> BoxFuture<'static, Result<(), zx::Status>>
                    + Send
                    + Sync,
            >,
        >,
        write_hook:
            Option<Box<dyn Fn(u64) -> BoxFuture<'static, Result<(), zx::Status>> + Send + Sync>>,
        barrier_hook: Option<Box<dyn Fn() -> Result<(), zx::Status> + Send + Sync>>,
    }

    impl super::async_interface::Interface for MockInterface {
        async fn on_attach_vmo(&self, _vmo: &zx::Vmo) -> Result<(), zx::Status> {
            Ok(())
        }

        fn get_info(&self) -> Cow<'_, DeviceInfo> {
            Cow::Owned(test_device_info())
        }

        async fn read(
            &self,
            device_block_offset: u64,
            block_count: u32,
            vmo: &Arc<zx::Vmo>,
            vmo_offset: u64,
            _opts: ReadOptions,
            _trace_flow_id: TraceFlowId,
        ) -> Result<(), zx::Status> {
            if let Some(read_hook) = &self.read_hook {
                read_hook(device_block_offset, block_count, vmo, vmo_offset).await
            } else {
                unimplemented!();
            }
        }

        async fn write(
            &self,
            device_block_offset: u64,
            _block_count: u32,
            _vmo: &Arc<zx::Vmo>,
            _vmo_offset: u64,
            opts: WriteOptions,
            _trace_flow_id: TraceFlowId,
        ) -> Result<(), zx::Status> {
            if opts.flags.contains(WriteFlags::PRE_BARRIER)
                && let Some(barrier_hook) = &self.barrier_hook
            {
                barrier_hook()?;
            }
            if let Some(write_hook) = &self.write_hook {
                write_hook(device_block_offset).await
            } else {
                unimplemented!();
            }
        }

        async fn flush(&self, _trace_flow_id: TraceFlowId) -> Result<(), zx::Status> {
            Ok(())
        }

        async fn trim(
            &self,
            _device_block_offset: u64,
            _block_count: u32,
            _trace_flow_id: TraceFlowId,
        ) -> Result<(), zx::Status> {
            unreachable!();
        }

        async fn get_volume_info(
            &self,
        ) -> Result<(fblock::VolumeManagerInfo, fblock::VolumeInfo), zx::Status> {
            // Hang forever for the test_requests_dont_block_sessions test.
            let () = std::future::pending().await;
            unreachable!();
        }
    }

    const BLOCK_SIZE: u32 = 512;
    const MAX_TRANSFER_BLOCKS: u32 = 10;

    fn test_device_info() -> DeviceInfo {
        DeviceInfo::Partition(PartitionInfo {
            device_flags: fblock::DeviceFlag::READONLY
                | fblock::DeviceFlag::BARRIER_SUPPORT
                | fblock::DeviceFlag::FUA_SUPPORT,
            max_transfer_blocks: NonZero::new(MAX_TRANSFER_BLOCKS),
            block_range: Some(0..100),
            type_guid: [1; 16],
            instance_guid: [2; 16],
            name: "foo".to_string(),
            flags: 0xabcd,
        })
    }

    #[fuchsia::test]
    async fn test_barriers_ordering() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();
        let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
        let barrier_called = Arc::new(AtomicBool::new(false));

        futures::join!(
            async move {
                let barrier_called_clone = barrier_called.clone();
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        barrier_hook: Some(Box::new(move || {
                            barrier_called.store(true, Ordering::Relaxed);
                            Ok(())
                        })),
                        write_hook: Some(Box::new(move |device_block_offset| {
                            let barrier_called = barrier_called_clone.clone();
                            Box::pin(async move {
                                // The sleep allows the server to reorder the fifo requests.
                                if device_block_offset % 2 == 0 {
                                    fasync::Timer::new(fasync::MonotonicInstant::after(
                                        zx::MonotonicDuration::from_millis(200),
                                    ))
                                    .await;
                                }
                                assert!(barrier_called.load(Ordering::Relaxed));
                                Ok(())
                            })
                        })),
                        ..MockInterface::default()
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();
                assert_ne!(vmo_id.id, 0);

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Write.into_primitive(),
                            flags: BlockIoFlag::PRE_BARRIER.bits(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        dev_offset: 0,
                        length: 5,
                        vmo_offset: 6,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                for i in 0..10 {
                    writer
                        .write_entries(&BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::Write.into_primitive(),
                                ..Default::default()
                            },
                            vmoid: vmo_id.id,
                            dev_offset: i + 1,
                            length: 5,
                            vmo_offset: 6,
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                }
                for _ in 0..11 {
                    let mut response = BlockFifoResponse::default();
                    reader.read_entries(&mut response).await.unwrap();
                    assert_eq!(response.status, zx::sys::ZX_OK);
                }

                std::mem::drop(proxy);
            }
        );
    }

    #[fuchsia::test]
    async fn test_info() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        futures::join!(
            async {
                let block_server = BlockServer::new(BLOCK_SIZE, Arc::new(MockInterface::default()));
                block_server.handle_requests(stream).await.unwrap();
            },
            async {
                let expected_info = test_device_info();
                let partition_info = if let DeviceInfo::Partition(info) = &expected_info {
                    info
                } else {
                    unreachable!()
                };

                let block_info = proxy.get_info().await.unwrap().unwrap();
                assert_eq!(
                    block_info.block_count,
                    partition_info.block_range.as_ref().unwrap().end
                        - partition_info.block_range.as_ref().unwrap().start
                );
                assert_eq!(
                    block_info.flags,
                    fblock::DeviceFlag::READONLY
                        | fblock::DeviceFlag::ZSTD_DECOMPRESSION_SUPPORT
                        | fblock::DeviceFlag::BARRIER_SUPPORT
                        | fblock::DeviceFlag::FUA_SUPPORT
                );

                assert_eq!(block_info.max_transfer_size, MAX_TRANSFER_BLOCKS * BLOCK_SIZE);

                let (status, type_guid) = proxy.get_type_guid().await.unwrap();
                assert_eq!(status, zx::sys::ZX_OK);
                assert_eq!(&type_guid.as_ref().unwrap().value, &partition_info.type_guid);

                let (status, instance_guid) = proxy.get_instance_guid().await.unwrap();
                assert_eq!(status, zx::sys::ZX_OK);
                assert_eq!(&instance_guid.as_ref().unwrap().value, &partition_info.instance_guid);

                let (status, name) = proxy.get_name().await.unwrap();
                assert_eq!(status, zx::sys::ZX_OK);
                assert_eq!(name.as_ref(), Some(&partition_info.name));

                let metadata = proxy.get_metadata().await.unwrap().expect("get_flags failed");
                assert_eq!(metadata.name, name);
                assert_eq!(metadata.type_guid.as_ref(), type_guid.as_deref());
                assert_eq!(metadata.instance_guid.as_ref(), instance_guid.as_deref());
                assert_eq!(
                    metadata.start_block_offset,
                    Some(partition_info.block_range.as_ref().unwrap().start)
                );
                assert_eq!(metadata.num_blocks, Some(block_info.block_count));
                assert_eq!(metadata.flags, Some(partition_info.flags));

                std::mem::drop(proxy);
            }
        );
    }

    #[fuchsia::test]
    async fn test_attach_vmo() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
        let koid = vmo.koid().unwrap();

        futures::join!(
            async {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |_, _, vmo, _| {
                            assert_eq!(vmo.koid().unwrap(), koid);
                            Box::pin(async { Ok(()) })
                        })),
                        ..MockInterface::default()
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();
                assert_ne!(vmo_id.id, 0);

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                // Keep attaching VMOs until we eventually hit the maximum.
                let mut count = 1;
                loop {
                    match session_proxy
                        .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                        .await
                        .unwrap()
                    {
                        Ok(vmo_id) => assert_ne!(vmo_id.id, 0),
                        Err(e) => {
                            assert_eq!(e, zx::sys::ZX_ERR_NO_RESOURCES);
                            break;
                        }
                    }

                    // Only test every 10 to keep test time down.
                    if count % 10 == 0 {
                        writer
                            .write_entries(&BlockFifoRequest {
                                command: BlockFifoCommand {
                                    opcode: BlockOpcode::Read.into_primitive(),
                                    ..Default::default()
                                },
                                vmoid: vmo_id.id,
                                length: 1,
                                ..Default::default()
                            })
                            .await
                            .unwrap();

                        let mut response = BlockFifoResponse::default();
                        reader.read_entries(&mut response).await.unwrap();
                        assert_eq!(response.status, zx::sys::ZX_OK);
                    }

                    count += 1;
                }

                assert_eq!(count, u16::MAX as u64);

                // Detach the original VMO, and make sure we can then attach another one.
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::CloseVmo.into_primitive(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);

                let new_vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();
                // It should reuse the same ID.
                assert_eq!(new_vmo_id.id, vmo_id.id);

                std::mem::drop(proxy);
            }
        );
    }

    #[fuchsia::test]
    async fn test_close() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        let mut server = std::pin::pin!(
            async {
                let block_server = BlockServer::new(BLOCK_SIZE, Arc::new(MockInterface::default()));
                block_server.handle_requests(stream).await.unwrap();
            }
            .fuse()
        );

        let mut client = std::pin::pin!(
            async {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                // Dropping the proxy should not cause the session to terminate because the session is
                // still live.
                std::mem::drop(proxy);

                session_proxy.close().await.unwrap().unwrap();

                // Keep the session alive.  Calling `close` should cause the server to terminate.
                let _: () = std::future::pending().await;
            }
            .fuse()
        );

        futures::select!(
            _ = server => {}
            _ = client => unreachable!(),
        );
    }

    #[derive(Default)]
    struct IoMockInterface {
        do_checks: bool,
        expected_op: Arc<Mutex<Option<ExpectedOp>>>,
        return_errors: bool,
    }

    #[derive(Debug)]
    enum ExpectedOp {
        Read(u64, u32, u64),
        Write(u64, u32, u64),
        Trim(u64, u32),
        Flush,
    }

    impl super::async_interface::Interface for IoMockInterface {
        async fn on_attach_vmo(&self, _vmo: &zx::Vmo) -> Result<(), zx::Status> {
            Ok(())
        }

        fn get_info(&self) -> Cow<'_, DeviceInfo> {
            Cow::Owned(test_device_info())
        }

        async fn read(
            &self,
            device_block_offset: u64,
            block_count: u32,
            _vmo: &Arc<zx::Vmo>,
            vmo_offset: u64,
            _opts: ReadOptions,
            _trace_flow_id: TraceFlowId,
        ) -> Result<(), zx::Status> {
            if self.return_errors {
                Err(zx::Status::INTERNAL)
            } else {
                if self.do_checks {
                    assert_matches!(
                        self.expected_op.lock().take(),
                        Some(ExpectedOp::Read(a, b, c)) if device_block_offset == a &&
                            block_count == b && vmo_offset / BLOCK_SIZE as u64 == c,
                        "Read {device_block_offset} {block_count} {vmo_offset}"
                    );
                }
                Ok(())
            }
        }

        async fn write(
            &self,
            device_block_offset: u64,
            block_count: u32,
            _vmo: &Arc<zx::Vmo>,
            vmo_offset: u64,
            _write_opts: WriteOptions,
            _trace_flow_id: TraceFlowId,
        ) -> Result<(), zx::Status> {
            if self.return_errors {
                Err(zx::Status::NOT_SUPPORTED)
            } else {
                if self.do_checks {
                    assert_matches!(
                        self.expected_op.lock().take(),
                        Some(ExpectedOp::Write(a, b, c)) if device_block_offset == a &&
                            block_count == b && vmo_offset / BLOCK_SIZE as u64 == c,
                        "Write {device_block_offset} {block_count} {vmo_offset}"
                    );
                }
                Ok(())
            }
        }

        async fn flush(&self, _trace_flow_id: TraceFlowId) -> Result<(), zx::Status> {
            if self.return_errors {
                Err(zx::Status::NO_RESOURCES)
            } else {
                if self.do_checks {
                    assert_matches!(self.expected_op.lock().take(), Some(ExpectedOp::Flush));
                }
                Ok(())
            }
        }

        async fn trim(
            &self,
            device_block_offset: u64,
            block_count: u32,
            _trace_flow_id: TraceFlowId,
        ) -> Result<(), zx::Status> {
            if self.return_errors {
                Err(zx::Status::NO_MEMORY)
            } else {
                if self.do_checks {
                    assert_matches!(
                        self.expected_op.lock().take(),
                        Some(ExpectedOp::Trim(a, b)) if device_block_offset == a &&
                            block_count == b,
                        "Trim {device_block_offset} {block_count}"
                    );
                }
                Ok(())
            }
        }
    }

    #[fuchsia::test]
    async fn test_io() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        let expected_op = Arc::new(Mutex::new(None));
        let expected_op_clone = expected_op.clone();

        let server = async {
            let block_server = BlockServer::new(
                BLOCK_SIZE,
                Arc::new(IoMockInterface {
                    return_errors: false,
                    do_checks: true,
                    expected_op: expected_op_clone,
                }),
            );
            block_server.handle_requests(stream).await.unwrap();
        };

        let client = async move {
            let (session_proxy, server) = fidl::endpoints::create_proxy();

            proxy.open_session(server).unwrap();

            let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
            let vmo_id = session_proxy
                .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                .await
                .unwrap()
                .unwrap();

            let mut fifo =
                fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
            let (mut reader, mut writer) = fifo.async_io();

            // READ
            *expected_op.lock() = Some(ExpectedOp::Read(1, 2, 3));
            writer
                .write_entries(&BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Read.into_primitive(),
                        ..Default::default()
                    },
                    vmoid: vmo_id.id,
                    dev_offset: 1,
                    length: 2,
                    vmo_offset: 3,
                    ..Default::default()
                })
                .await
                .unwrap();

            let mut response = BlockFifoResponse::default();
            reader.read_entries(&mut response).await.unwrap();
            assert_eq!(response.status, zx::sys::ZX_OK);

            // WRITE
            *expected_op.lock() = Some(ExpectedOp::Write(4, 5, 6));
            writer
                .write_entries(&BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Write.into_primitive(),
                        ..Default::default()
                    },
                    vmoid: vmo_id.id,
                    dev_offset: 4,
                    length: 5,
                    vmo_offset: 6,
                    ..Default::default()
                })
                .await
                .unwrap();

            let mut response = BlockFifoResponse::default();
            reader.read_entries(&mut response).await.unwrap();
            assert_eq!(response.status, zx::sys::ZX_OK);

            // FLUSH
            *expected_op.lock() = Some(ExpectedOp::Flush);
            writer
                .write_entries(&BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Flush.into_primitive(),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .await
                .unwrap();

            reader.read_entries(&mut response).await.unwrap();
            assert_eq!(response.status, zx::sys::ZX_OK);

            // TRIM
            *expected_op.lock() = Some(ExpectedOp::Trim(7, 8));
            writer
                .write_entries(&BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Trim.into_primitive(),
                        ..Default::default()
                    },
                    dev_offset: 7,
                    length: 8,
                    ..Default::default()
                })
                .await
                .unwrap();

            reader.read_entries(&mut response).await.unwrap();
            assert_eq!(response.status, zx::sys::ZX_OK);

            std::mem::drop(proxy);
        };

        futures::join!(server, client);
    }

    #[fuchsia::test]
    async fn test_io_errors() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        futures::join!(
            async {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(IoMockInterface {
                        return_errors: true,
                        do_checks: false,
                        expected_op: Arc::new(Mutex::new(None)),
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                // READ
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        length: 1,
                        reqid: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_INTERNAL);

                // WRITE
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Write.into_primitive(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        length: 1,
                        reqid: 2,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_NOT_SUPPORTED);

                // FLUSH
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Flush.into_primitive(),
                            ..Default::default()
                        },
                        reqid: 3,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_NO_RESOURCES);

                // TRIM
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Trim.into_primitive(),
                            ..Default::default()
                        },
                        reqid: 4,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_NO_MEMORY);

                std::mem::drop(proxy);
            }
        );
    }

    #[fuchsia::test]
    async fn test_invalid_args() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        futures::join!(
            async {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(IoMockInterface {
                        return_errors: false,
                        do_checks: false,
                        expected_op: Arc::new(Mutex::new(None)),
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());

                async fn test(
                    fifo: &mut fasync::Fifo<BlockFifoResponse, BlockFifoRequest>,
                    request: BlockFifoRequest,
                ) -> Result<(), zx::Status> {
                    let (mut reader, mut writer) = fifo.async_io();
                    writer.write_entries(&request).await.unwrap();
                    let mut response = BlockFifoResponse::default();
                    reader.read_entries(&mut response).await.unwrap();
                    zx::Status::ok(response.status)
                }

                // READ

                let good_read_request = || BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Read.into_primitive(),
                        ..Default::default()
                    },
                    length: 1,
                    vmoid: vmo_id.id,
                    ..Default::default()
                };

                assert_eq!(
                    test(
                        &mut fifo,
                        BlockFifoRequest { vmoid: vmo_id.id + 1, ..good_read_request() }
                    )
                    .await,
                    Err(zx::Status::IO)
                );

                assert_eq!(
                    test(
                        &mut fifo,
                        BlockFifoRequest {
                            vmo_offset: 0xffff_ffff_ffff_ffff,
                            ..good_read_request()
                        }
                    )
                    .await,
                    Err(zx::Status::OUT_OF_RANGE)
                );

                assert_eq!(
                    test(&mut fifo, BlockFifoRequest { length: 0, ..good_read_request() }).await,
                    Err(zx::Status::INVALID_ARGS)
                );

                // WRITE

                let good_write_request = || BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Write.into_primitive(),
                        ..Default::default()
                    },
                    length: 1,
                    vmoid: vmo_id.id,
                    ..Default::default()
                };

                assert_eq!(
                    test(
                        &mut fifo,
                        BlockFifoRequest { vmoid: vmo_id.id + 1, ..good_write_request() }
                    )
                    .await,
                    Err(zx::Status::IO)
                );

                assert_eq!(
                    test(
                        &mut fifo,
                        BlockFifoRequest {
                            vmo_offset: 0xffff_ffff_ffff_ffff,
                            ..good_write_request()
                        }
                    )
                    .await,
                    Err(zx::Status::OUT_OF_RANGE)
                );

                assert_eq!(
                    test(&mut fifo, BlockFifoRequest { length: 0, ..good_write_request() }).await,
                    Err(zx::Status::INVALID_ARGS)
                );

                // CLOSE VMO

                assert_eq!(
                    test(
                        &mut fifo,
                        BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::CloseVmo.into_primitive(),
                                ..Default::default()
                            },
                            vmoid: vmo_id.id + 1,
                            ..Default::default()
                        }
                    )
                    .await,
                    Err(zx::Status::IO)
                );

                std::mem::drop(proxy);
            }
        );
    }

    #[fuchsia::test]
    async fn test_concurrent_requests() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        let waiting_readers = Arc::new(Mutex::new(Vec::new()));
        let waiting_readers_clone = waiting_readers.clone();

        futures::join!(
            async move {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |dev_block_offset, _, _, _| {
                            let (tx, rx) = oneshot::channel();
                            waiting_readers_clone.lock().push((dev_block_offset as u32, tx));
                            Box::pin(async move {
                                let _ = rx.await;
                                Ok(())
                            })
                        })),
                        ..MockInterface::default()
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            ..Default::default()
                        },
                        reqid: 1,
                        dev_offset: 1, // Intentionally use the same as `reqid`.
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            ..Default::default()
                        },
                        reqid: 2,
                        dev_offset: 2,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                // Wait till both those entries are pending.
                poll_fn(|cx: &mut Context<'_>| {
                    if waiting_readers.lock().len() == 2 {
                        Poll::Ready(())
                    } else {
                        // Yield to the executor.
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;

                let mut response = BlockFifoResponse::default();
                assert!(futures::poll!(pin!(reader.read_entries(&mut response))).is_pending());

                let (id, tx) = waiting_readers.lock().pop().unwrap();
                tx.send(()).unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);
                assert_eq!(response.reqid, id);

                assert!(futures::poll!(pin!(reader.read_entries(&mut response))).is_pending());

                let (id, tx) = waiting_readers.lock().pop().unwrap();
                tx.send(()).unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);
                assert_eq!(response.reqid, id);
            }
        );
    }

    #[fuchsia::test]
    async fn test_session_close_is_synchronous() {
        use futures::{FutureExt as _, StreamExt as _};

        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        let (start_tx, mut start_rx) = futures::channel::mpsc::channel(1);
        let (finish_tx, finish_rx) = futures::channel::oneshot::channel();
        let finish_rx = Arc::new(Mutex::new(Some(finish_rx)));

        futures::join!(
            async move {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |_, _, _, _| {
                            let mut start_tx = start_tx.clone();
                            let finish_rx = finish_rx.lock().take().unwrap();
                            Box::pin(async move {
                                start_tx.try_send(()).unwrap();
                                let _ = finish_rx.await;
                                Ok(())
                            })
                        })),
                        ..MockInterface::default()
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();
                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo = fasync::Fifo::<BlockFifoResponse, BlockFifoRequest>::from_fifo(
                    session_proxy.get_fifo().await.unwrap().unwrap(),
                );
                let (_reader, mut writer) = fifo.async_io();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            ..Default::default()
                        },
                        reqid: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                // Wait for the read to actually start.
                start_rx.next().await.unwrap();

                // The close request shouldn't complete yet because the read is still hanging.
                let mut close_fut = std::pin::pin!(session_proxy.close().fuse());
                let mut timer_fut = std::pin::pin!(
                    fasync::Timer::new(std::time::Duration::from_millis(100)).fuse()
                );
                futures::select! {
                    res = close_fut => panic!("close completed too early: {:?}", res),
                    _ = timer_fut => {}
                }

                // Finish the pending request.
                finish_tx.send(()).unwrap();

                // Verify that close() now completes.
                close_fut.await.unwrap().unwrap();

                std::mem::drop(proxy);
            }
        );
    }

    #[fuchsia::test]
    async fn test_groups() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        futures::join!(
            async move {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |_, _, _, _| Box::pin(async { Ok(()) }))),
                        ..MockInterface::default()
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: BlockIoFlag::GROUP_ITEM.bits(),
                            ..Default::default()
                        },
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                            ..Default::default()
                        },
                        reqid: 2,
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);
                assert_eq!(response.reqid, 2);
                assert_eq!(response.group, 1);
            }
        );
    }

    #[fuchsia::test]
    async fn test_group_error() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        let counter = Arc::new(AtomicU64::new(0));
        let counter_clone = counter.clone();

        futures::join!(
            async move {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |_, _, _, _| {
                            counter_clone.fetch_add(1, Ordering::Relaxed);
                            Box::pin(async { Err(zx::Status::BAD_STATE) })
                        })),
                        ..MockInterface::default()
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: BlockIoFlag::GROUP_ITEM.bits(),
                            ..Default::default()
                        },
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                // Wait until processed.
                poll_fn(|cx: &mut Context<'_>| {
                    if counter.load(Ordering::Relaxed) == 1 {
                        Poll::Ready(())
                    } else {
                        // Yield to the executor.
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;

                let mut response = BlockFifoResponse::default();
                assert!(futures::poll!(pin!(reader.read_entries(&mut response))).is_pending());

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: BlockIoFlag::GROUP_ITEM.bits(),
                            ..Default::default()
                        },
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                            ..Default::default()
                        },
                        reqid: 2,
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_BAD_STATE);
                assert_eq!(response.reqid, 2);
                assert_eq!(response.group, 1);

                assert!(futures::poll!(pin!(reader.read_entries(&mut response))).is_pending());

                // Only the first request should have been processed.
                assert_eq!(counter.load(Ordering::Relaxed), 1);
            }
        );
    }

    #[fuchsia::test]
    async fn test_group_with_two_lasts() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        let (tx, rx) = oneshot::channel();

        futures::join!(
            async move {
                let rx = Mutex::new(Some(rx));
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |_, _, _, _| {
                            let rx = rx.lock().take().unwrap();
                            Box::pin(async {
                                let _ = rx.await;
                                Ok(())
                            })
                        })),
                        ..MockInterface::default()
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                            ..Default::default()
                        },
                        reqid: 1,
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                            ..Default::default()
                        },
                        reqid: 2,
                        group: 1,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                // Send an independent request to flush through the fifo.
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::CloseVmo.into_primitive(),
                            ..Default::default()
                        },
                        reqid: 3,
                        vmoid: vmo_id.id,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                // It should succeed.
                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);
                assert_eq!(response.reqid, 3);

                // Now release the original request.
                tx.send(()).unwrap();

                // The response should be for the first message tagged as last, and it should be
                // an error because we sent two messages with the LAST marker.
                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_INVALID_ARGS);
                assert_eq!(response.reqid, 1);
                assert_eq!(response.group, 1);
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_requests_dont_block_sessions() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        let (tx, rx) = oneshot::channel();

        fasync::Task::local(async move {
            let rx = Mutex::new(Some(rx));
            let block_server = BlockServer::new(
                BLOCK_SIZE,
                Arc::new(MockInterface {
                    read_hook: Some(Box::new(move |_, _, _, _| {
                        let rx = rx.lock().take().unwrap();
                        Box::pin(async {
                            let _ = rx.await;
                            Ok(())
                        })
                    })),
                    ..MockInterface::default()
                }),
            );
            block_server.handle_requests(stream).await.unwrap();
        })
        .detach();

        let mut fut = pin!(async {
            let (session_proxy, server) = fidl::endpoints::create_proxy();

            proxy.open_session(server).unwrap();

            let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
            let vmo_id = session_proxy
                .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                .await
                .unwrap()
                .unwrap();

            let mut fifo =
                fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
            let (mut reader, mut writer) = fifo.async_io();

            writer
                .write_entries(&BlockFifoRequest {
                    command: BlockFifoCommand {
                        opcode: BlockOpcode::Read.into_primitive(),
                        flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                        ..Default::default()
                    },
                    reqid: 1,
                    group: 1,
                    vmoid: vmo_id.id,
                    length: 1,
                    ..Default::default()
                })
                .await
                .unwrap();

            let mut response = BlockFifoResponse::default();
            reader.read_entries(&mut response).await.unwrap();
            assert_eq!(response.status, zx::sys::ZX_OK);
        });

        // The response won't come back until we send on `tx`.
        assert!(fasync::TestExecutor::poll_until_stalled(&mut fut).await.is_pending());

        let mut fut2 = pin!(proxy.get_volume_info());

        // get_volume_info is set up to stall forever.
        assert!(fasync::TestExecutor::poll_until_stalled(&mut fut2).await.is_pending());

        // If we now free up the first future, it should resolve; the stalled call to
        // get_volume_info should not block the fifo response.
        let _ = tx.send(());

        assert!(fasync::TestExecutor::poll_until_stalled(&mut fut).await.is_ready());
    }

    #[fuchsia::test]
    async fn test_request_flow_control() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        // The client will ensure that MAX_REQUESTS are queued up before firing `event`, and the
        // server will block until that happens.
        const MAX_REQUESTS: u64 = FIFO_MAX_REQUESTS as u64;
        let event = Arc::new((event_listener::Event::new(), AtomicBool::new(false)));
        let event_clone = event.clone();
        futures::join!(
            async move {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(MockInterface {
                        read_hook: Some(Box::new(move |_, _, _, _| {
                            let event_clone = event_clone.clone();
                            Box::pin(async move {
                                if !event_clone.1.load(Ordering::SeqCst) {
                                    event_clone.0.listen().await;
                                }
                                Ok(())
                            })
                        })),
                        ..MockInterface::default()
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                proxy.open_session(server).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                for i in 0..MAX_REQUESTS {
                    writer
                        .write_entries(&BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::Read.into_primitive(),
                                ..Default::default()
                            },
                            reqid: (i + 1) as u32,
                            dev_offset: i,
                            vmoid: vmo_id.id,
                            length: 1,
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                }
                assert!(
                    futures::poll!(pin!(writer.write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            ..Default::default()
                        },
                        reqid: u32::MAX,
                        dev_offset: MAX_REQUESTS,
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })))
                    .is_pending()
                );
                // OK, let the server start to process.
                event.1.store(true, Ordering::SeqCst);
                event.0.notify(usize::MAX);
                // For each entry we read, make sure we can write a new one in.
                let mut finished_reqids = vec![];
                for i in MAX_REQUESTS..2 * MAX_REQUESTS {
                    let mut response = BlockFifoResponse::default();
                    reader.read_entries(&mut response).await.unwrap();
                    assert_eq!(response.status, zx::sys::ZX_OK);
                    finished_reqids.push(response.reqid);
                    writer
                        .write_entries(&BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::Read.into_primitive(),
                                ..Default::default()
                            },
                            reqid: (i + 1) as u32,
                            dev_offset: i,
                            vmoid: vmo_id.id,
                            length: 1,
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                }
                let mut response = BlockFifoResponse::default();
                for _ in 0..MAX_REQUESTS {
                    reader.read_entries(&mut response).await.unwrap();
                    assert_eq!(response.status, zx::sys::ZX_OK);
                    finished_reqids.push(response.reqid);
                }
                // Verify that we got a response for each request.  Note that we can't assume FIFO
                // ordering.
                finished_reqids.sort();
                assert_eq!(finished_reqids.len(), 2 * MAX_REQUESTS as usize);
                let mut i = 1;
                for reqid in finished_reqids {
                    assert_eq!(reqid, i);
                    i += 1;
                }
            }
        );
    }

    #[fuchsia::test]
    async fn test_passthrough_io_with_fixed_map() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        let expected_op = Arc::new(Mutex::new(None));
        let expected_op_clone = expected_op.clone();
        futures::join!(
            async {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    Arc::new(IoMockInterface {
                        return_errors: false,
                        do_checks: true,
                        expected_op: expected_op_clone,
                    }),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();

                let mapping = fblock::BlockOffsetMapping {
                    source_block_offset: 0,
                    target_block_offset: 10,
                    length: 20,
                };
                proxy.open_session_with_offset_map(server, &mapping).unwrap();

                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                // READ
                *expected_op.lock() = Some(ExpectedOp::Read(11, 2, 3));
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        dev_offset: 1,
                        length: 2,
                        vmo_offset: 3,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);

                // WRITE
                *expected_op.lock() = Some(ExpectedOp::Write(14, 5, 6));
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Write.into_primitive(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        dev_offset: 4,
                        length: 5,
                        vmo_offset: 6,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);

                // FLUSH
                *expected_op.lock() = Some(ExpectedOp::Flush);
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Flush.into_primitive(),
                            ..Default::default()
                        },
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);

                // TRIM
                *expected_op.lock() = Some(ExpectedOp::Trim(17, 3));
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Trim.into_primitive(),
                            ..Default::default()
                        },
                        dev_offset: 7,
                        length: 3,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_OK);

                // READ past window
                *expected_op.lock() = None;
                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Read.into_primitive(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        dev_offset: 19,
                        length: 2,
                        vmo_offset: 3,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_OUT_OF_RANGE);

                std::mem::drop(proxy);
            }
        );
    }

    #[fuchsia::test]
    fn operation_map() {
        const BLOCK_SIZE: u32 = 512;

        #[track_caller]
        fn expect_map_result(
            mut operation: Operation,
            mapping: Option<BlockOffsetMapping>,
            max_blocks: Option<NonZero<u32>>,
            expected_operations: Vec<Operation>,
        ) {
            let mut ops = vec![];
            while let Some(remainder) =
                operation.map(mapping.as_ref(), max_blocks.clone(), BLOCK_SIZE)
            {
                ops.push(operation);
                operation = remainder;
            }
            ops.push(operation);
            assert_eq!(ops, expected_operations);
        }

        // No limits
        expect_map_result(
            Operation::Read {
                device_block_offset: 10,
                block_count: 200,
                _unused: 0,
                vmo_offset: 0,
                options: ReadOptions { inline_crypto: InlineCryptoOptions::enabled(1, 1000) },
            },
            None,
            None,
            vec![Operation::Read {
                device_block_offset: 10,
                block_count: 200,
                _unused: 0,
                vmo_offset: 0,
                options: ReadOptions { inline_crypto: InlineCryptoOptions::enabled(1, 1000) },
            }],
        );

        // Max block count
        expect_map_result(
            Operation::Read {
                device_block_offset: 10,
                block_count: 200,
                _unused: 0,
                vmo_offset: 0,
                options: ReadOptions { inline_crypto: InlineCryptoOptions::enabled(1, 1000) },
            },
            None,
            NonZero::new(120),
            vec![
                Operation::Read {
                    device_block_offset: 10,
                    block_count: 120,
                    _unused: 0,
                    vmo_offset: 0,
                    options: ReadOptions { inline_crypto: InlineCryptoOptions::enabled(1, 1000) },
                },
                Operation::Read {
                    device_block_offset: 130,
                    block_count: 80,
                    _unused: 0,
                    vmo_offset: 120 * BLOCK_SIZE as u64,
                    options: ReadOptions {
                        // The DUN should be offset by the number of blocks in the first request.
                        inline_crypto: InlineCryptoOptions::enabled(1, 1000 + 120),
                    },
                },
            ],
        );
        expect_map_result(
            Operation::Trim { device_block_offset: 10, block_count: 200 },
            None,
            NonZero::new(120),
            vec![Operation::Trim { device_block_offset: 10, block_count: 200 }],
        );

        // Remapping + Max block count
        expect_map_result(
            Operation::Read {
                device_block_offset: 10,
                block_count: 200,
                _unused: 0,
                vmo_offset: 0,
                options: ReadOptions { inline_crypto: InlineCryptoOptions::enabled(1, 1000) },
            },
            Some(BlockOffsetMapping {
                source_block_offset: 10,
                target_block_offset: 100,
                length: 200,
            }),
            NonZero::new(120),
            vec![
                Operation::Read {
                    device_block_offset: 100,
                    block_count: 120,
                    _unused: 0,
                    vmo_offset: 0,
                    options: ReadOptions { inline_crypto: InlineCryptoOptions::enabled(1, 1000) },
                },
                Operation::Read {
                    device_block_offset: 220,
                    block_count: 80,
                    _unused: 0,
                    vmo_offset: 120 * BLOCK_SIZE as u64,
                    options: ReadOptions {
                        inline_crypto: InlineCryptoOptions::enabled(1, 1000 + 120),
                    },
                },
            ],
        );
        expect_map_result(
            Operation::Trim { device_block_offset: 10, block_count: 200 },
            Some(BlockOffsetMapping {
                source_block_offset: 10,
                target_block_offset: 100,
                length: 200,
            }),
            NonZero::new(120),
            vec![Operation::Trim { device_block_offset: 100, block_count: 200 }],
        );
    }

    // Verifies that if the pre-flush (for a simulated barrier) fails, the write is not executed.
    #[fuchsia::test]
    async fn test_pre_barrier_flush_failure() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        struct NoBarrierInterface;
        impl super::async_interface::Interface for NoBarrierInterface {
            fn get_info(&self) -> Cow<'_, DeviceInfo> {
                Cow::Owned(DeviceInfo::Partition(PartitionInfo {
                    device_flags: fblock::DeviceFlag::empty(), // No BARRIER_SUPPORT
                    max_transfer_blocks: NonZero::new(100),
                    block_range: Some(0..100),
                    type_guid: [0; 16],
                    instance_guid: [0; 16],
                    name: "test".to_string(),
                    flags: 0,
                }))
            }
            async fn on_attach_vmo(&self, _vmo: &zx::Vmo) -> Result<(), zx::Status> {
                Ok(())
            }
            async fn read(
                &self,
                _: u64,
                _: u32,
                _: &Arc<zx::Vmo>,
                _: u64,
                _: ReadOptions,
                _: TraceFlowId,
            ) -> Result<(), zx::Status> {
                unreachable!()
            }
            async fn write(
                &self,
                _: u64,
                _: u32,
                _: &Arc<zx::Vmo>,
                _: u64,
                _: WriteOptions,
                _: TraceFlowId,
            ) -> Result<(), zx::Status> {
                panic!("Write should not be called");
            }
            async fn flush(&self, _: TraceFlowId) -> Result<(), zx::Status> {
                Err(zx::Status::IO)
            }
            async fn trim(&self, _: u64, _: u32, _: TraceFlowId) -> Result<(), zx::Status> {
                unreachable!()
            }
        }

        futures::join!(
            async move {
                let block_server = BlockServer::new(BLOCK_SIZE, Arc::new(NoBarrierInterface));
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();
                proxy.open_session(server).unwrap();
                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Write.into_primitive(),
                            flags: BlockIoFlag::PRE_BARRIER.bits(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_IO);
            }
        );
    }

    // Verifies that if the write fails when a post-flush is required (for a simulated FUA), the
    // post-flush is not executed.
    #[fuchsia::test]
    async fn test_post_barrier_write_failure() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        struct NoBarrierInterface;
        impl super::async_interface::Interface for NoBarrierInterface {
            fn get_info(&self) -> Cow<'_, DeviceInfo> {
                Cow::Owned(DeviceInfo::Partition(PartitionInfo {
                    device_flags: fblock::DeviceFlag::empty(), // No FUA_SUPPORT
                    max_transfer_blocks: NonZero::new(100),
                    block_range: Some(0..100),
                    type_guid: [0; 16],
                    instance_guid: [0; 16],
                    name: "test".to_string(),
                    flags: 0,
                }))
            }
            async fn on_attach_vmo(&self, _vmo: &zx::Vmo) -> Result<(), zx::Status> {
                Ok(())
            }
            async fn read(
                &self,
                _: u64,
                _: u32,
                _: &Arc<zx::Vmo>,
                _: u64,
                _: ReadOptions,
                _: TraceFlowId,
            ) -> Result<(), zx::Status> {
                unreachable!()
            }
            async fn write(
                &self,
                _: u64,
                _: u32,
                _: &Arc<zx::Vmo>,
                _: u64,
                _: WriteOptions,
                _: TraceFlowId,
            ) -> Result<(), zx::Status> {
                Err(zx::Status::IO)
            }
            async fn flush(&self, _: TraceFlowId) -> Result<(), zx::Status> {
                panic!("Flush should not be called")
            }
            async fn trim(&self, _: u64, _: u32, _: TraceFlowId) -> Result<(), zx::Status> {
                unreachable!()
            }
        }

        futures::join!(
            async move {
                let block_server = BlockServer::new(BLOCK_SIZE, Arc::new(NoBarrierInterface));
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                let (session_proxy, server) = fidl::endpoints::create_proxy();
                proxy.open_session(server).unwrap();
                let vmo = zx::Vmo::create(zx::system_get_page_size() as u64).unwrap();
                let vmo_id = session_proxy
                    .attach_vmo(vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
                    .await
                    .unwrap()
                    .unwrap();

                let mut fifo =
                    fasync::Fifo::from_fifo(session_proxy.get_fifo().await.unwrap().unwrap());
                let (mut reader, mut writer) = fifo.async_io();

                writer
                    .write_entries(&BlockFifoRequest {
                        command: BlockFifoCommand {
                            opcode: BlockOpcode::Write.into_primitive(),
                            flags: BlockIoFlag::FORCE_ACCESS.bits(),
                            ..Default::default()
                        },
                        vmoid: vmo_id.id,
                        length: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap();

                let mut response = BlockFifoResponse::default();
                reader.read_entries(&mut response).await.unwrap();
                assert_eq!(response.status, zx::sys::ZX_ERR_IO);
            }
        );
    }

    /// Verifies that group IDs are isolated per session.
    ///
    /// Even if two independent sessions on the same BlockServer use the same group ID,
    /// their in-flight transaction groups must remain isolated.
    #[fuchsia::test]
    async fn test_group_ids_isolated_per_session() {
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fblock::BlockMarker>();

        futures::join!(
            async {
                let block_server = BlockServer::new(
                    BLOCK_SIZE,
                    // MockInterface::flush() is a no-op that returns Ok(()).
                    Arc::new(MockInterface::default()),
                );
                block_server.handle_requests(stream).await.unwrap();
            },
            async move {
                async fn settle() {
                    // Let the single-threaded executor drain server work.
                    for _ in 0..32 {
                        fasync::yield_now().await;
                    }
                }

                // --- Open session A. ---
                let (session_a, server_a) = fidl::endpoints::create_proxy();
                proxy.open_session(server_a).unwrap();
                let mut fifo_a =
                    fasync::Fifo::from_fifo(session_a.get_fifo().await.unwrap().unwrap());

                // --- Open session B. ---
                let (session_b, server_b) = fidl::endpoints::create_proxy();
                proxy.open_session(server_b).unwrap();
                let mut fifo_b =
                    fasync::Fifo::from_fifo(session_b.get_fifo().await.unwrap().unwrap());

                // ----------------------------------------------------------------
                // Control: with no interference, A's two-part Flush group is OK.
                // ----------------------------------------------------------------
                {
                    let (mut reader_a, mut writer_a) = fifo_a.async_io();
                    writer_a
                        .write_entries(&BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::Flush.into_primitive(),
                                flags: BlockIoFlag::GROUP_ITEM.bits(),
                                ..Default::default()
                            },
                            group: 1,
                            reqid: 0xAAAA,
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                    settle().await;
                    writer_a
                        .write_entries(&BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::Flush.into_primitive(),
                                flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                                ..Default::default()
                            },
                            group: 1,
                            reqid: 0xAAAA,
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                    let mut response = BlockFifoResponse::default();
                    reader_a.read_entries(&mut response).await.unwrap();
                    assert_eq!(response.reqid, 0xAAAA);
                    assert_eq!(
                        zx::Status::from_raw(response.status),
                        zx::Status::OK,
                        "control: A's valid Flush group must succeed"
                    );
                }

                // ----------------------------------------------------------------
                // Run concurrent group requests with the same group ID (7) on
                // both sessions, and verify they both succeed independently.
                // ----------------------------------------------------------------

                // Step 1: Session A starts group 7.
                {
                    let (_reader_a, mut writer_a) = fifo_a.async_io();
                    writer_a
                        .write_entries(&BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::Flush.into_primitive(),
                                flags: BlockIoFlag::GROUP_ITEM.bits(),
                                ..Default::default()
                            },
                            group: 7,
                            reqid: 100,
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                }
                settle().await;

                // Step 2: Session B starts group 7.
                {
                    let (_reader_b, mut writer_b) = fifo_b.async_io();
                    writer_b
                        .write_entries(&BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::Flush.into_primitive(),
                                flags: BlockIoFlag::GROUP_ITEM.bits(),
                                ..Default::default()
                            },
                            group: 7,
                            reqid: 200,
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                }
                settle().await;

                // Step 3: Session A finishes group 7.
                {
                    let (_reader_a, mut writer_a) = fifo_a.async_io();
                    writer_a
                        .write_entries(&BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::Flush.into_primitive(),
                                flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                                ..Default::default()
                            },
                            group: 7,
                            reqid: 100,
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                }
                settle().await;

                // Step 4: Session B finishes group 7.
                {
                    let (_reader_b, mut writer_b) = fifo_b.async_io();
                    writer_b
                        .write_entries(&BlockFifoRequest {
                            command: BlockFifoCommand {
                                opcode: BlockOpcode::Flush.into_primitive(),
                                flags: (BlockIoFlag::GROUP_ITEM | BlockIoFlag::GROUP_LAST).bits(),
                                ..Default::default()
                            },
                            group: 7,
                            reqid: 200,
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                }
                settle().await;

                // Verify Session A's response.
                {
                    let (mut reader_a, _writer_a) = fifo_a.async_io();
                    let mut response_a = BlockFifoResponse::default();
                    reader_a.read_entries(&mut response_a).await.unwrap();
                    assert_eq!(response_a.reqid, 100);
                    assert_eq!(response_a.group, 7);
                    assert_eq!(zx::Status::from_raw(response_a.status), zx::Status::OK);
                }

                // Verify Session B's response.
                {
                    let (mut reader_b, _writer_b) = fifo_b.async_io();
                    let mut response_b = BlockFifoResponse::default();
                    reader_b.read_entries(&mut response_b).await.unwrap();
                    assert_eq!(response_b.reqid, 200);
                    assert_eq!(response_b.group, 7);
                    assert_eq!(zx::Status::from_raw(response_b.status), zx::Status::OK);
                }

                std::mem::drop(session_a);
                std::mem::drop(session_b);
                std::mem::drop(proxy);
            }
        );
    }
}
