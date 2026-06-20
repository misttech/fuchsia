// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A Netstack3 worker to serve fuchsia.net.debug.PacketCaptureProvider API
//! requests.

use std::pin::pin;

use fidl_fuchsia_io as fio;
use fidl_fuchsia_net_debug as fnet_debug;
use fidl_fuchsia_posix_socket_packet as fppacket;
use fuchsia_async as fasync;
use fuchsia_async::scope::ScopeActiveGuard;

use chunked_ringbuf::RingBuffer;
use futures::channel::oneshot;
use futures::{FutureExt as _, TryStreamExt as _};
use log::warn;
use pcap::LinkType;
use vfs::file::File as _;

use crate::bindings::bpf::{SocketFilterProgram, ValidVerifiedProgram};
use crate::bindings::devices::HasDeviceName as _;
use crate::bindings::socket::packet::SocketState;
use crate::bindings::util::{ErrorLogExt as _, RemoveResourceResultExt as _, ResultExt as _};
use crate::bindings::{BindingId, BindingsCtx, Ctx};
use netstack3_core::device::DeviceId;
use netstack3_core::device_socket::{Protocol, SocketId, TargetDevice};
use netstack3_core::sync::Mutex;

#[derive(Debug)]
struct CaptureData {
    id: SocketId<BindingsCtx>,
    pcap_headers: Vec<u8>,
}

/// The state of the single allowed rolling packet capture.
///
/// Netstack3 restricts packet captures to at most one active capture at a time
/// to conserve memory.
#[derive(Debug)]
enum RollingCaptureState {
    /// No packet capture is active.
    Empty,
    // TODO(https://fxbug.dev/485274945): When StopAndDownload can be called
    // multiple times the Closing state will no longer be entered on the first
    // call.
    /// A packet capture is in the process of being torn down or waiting for
    /// the client to download the capture so that resources can be freed.
    ///
    /// This state blocks other clients from starting a new packet capture.
    Closing,
    /// A packet capture is active and the client is currently connected.
    ///
    /// The lifetime of the packet capture is tied to the client's connection.
    Attached { task: fasync::Task<Option<CaptureData>>, cancel: oneshot::Sender<()> },
    /// A packet capture is active but has been detached from the client's
    /// connection.
    ///
    /// The client is currently connected, but closing the connection will not
    /// terminate the capture.
    DetachedConnected {
        name: String,
        task: fasync::Task<Option<CaptureData>>,
        cancel: oneshot::Sender<()>,
    },
    // TODO(https://fxbug.dev/485274945): Implement timeouts.
    /// A packet capture is detached and there is no client connected.
    ///
    /// The server holds the captured data and is waiting for a reconnect
    /// or a timeout.
    Disconnected { name: String, data: CaptureData },
}

impl RollingCaptureState {
    fn replace_with<O, F: FnOnce(Self) -> (Self, O)>(&mut self, f: F) -> O {
        let old_state = std::mem::replace(self, RollingCaptureState::Empty);
        let (new_state, retval) = f(old_state);
        let _empty = std::mem::replace(self, new_state);
        retval
    }

    #[track_caller]
    fn transition_to_closing(&mut self) {
        self.replace_with(|old| match old {
            RollingCaptureState::Attached { .. }
            | RollingCaptureState::DetachedConnected { .. } => (RollingCaptureState::Closing, ()),
            old => unreachable!("transition to closing for teardown in unexpected state: {old:?}"),
        })
    }
}

pub(crate) struct PacketCaptures {
    state: Mutex<RollingCaptureState>,
}

impl Default for PacketCaptures {
    fn default() -> Self {
        Self { state: Mutex::new(RollingCaptureState::Empty) }
    }
}

async fn remove_socket(ctx: &mut Ctx, id: SocketId<BindingsCtx>) -> SocketState {
    let weak = id.downgrade();
    ctx.api()
        .device_socket()
        .remove(id)
        .map_deferred(|d| d.into_future("packet socket", &weak, ctx))
        .into_future()
        .await
}

fn handle_detach(
    ctx: &mut Ctx,
    name: String,
) -> Result<(), fnet_debug::RollingPacketCaptureDetachError> {
    ctx.bindings_ctx().packet_captures.state.lock().replace_with(|old_state| match old_state {
        RollingCaptureState::Attached { task, cancel } => {
            (RollingCaptureState::DetachedConnected { name: name.clone(), task, cancel }, Ok(()))
        }
        s @ RollingCaptureState::DetachedConnected { .. } => {
            (s, Err(fnet_debug::RollingPacketCaptureDetachError::AlreadyDetached))
        }
        RollingCaptureState::Empty
        | RollingCaptureState::Closing
        | RollingCaptureState::Disconnected { .. } => {
            unreachable!("Detach called in unexpected state {old_state:?}");
        }
    })
}

/// Serves a rolling packet capture request stream.
///
/// Returns `Some(CaptureData)` if the packet capture is taken over
/// via signalling on `takeover_cancel` due to handling a call to
/// `ReconnectRolling`; `None` otherwise.
async fn serve_rolling_packet_capture<Fut>(
    mut ctx: Ctx,
    mut rs: fnet_debug::RollingPacketCaptureRequestStream,
    id: SocketId<BindingsCtx>,
    pcap_headers: Vec<u8>,
    scope_cancel_fut: Fut,
    mut takeover_cancel: oneshot::Receiver<()>,
) -> Option<CaptureData>
where
    Fut: futures::Future<Output = ()>,
{
    enum CloseType {
        StreamClosed,
        Takeover,
        Canceled,
        Terminate(fidl::endpoints::ServerEnd<fio::FileMarker>),
        Discard(fnet_debug::RollingPacketCaptureDiscardResponder),
    }
    let mut scope_cancel_fut = pin!(scope_cancel_fut.fuse());
    let close_type = loop {
        let req = futures::select_biased! {
            _ = scope_cancel_fut => {
                break CloseType::Canceled;
            }
            _ = takeover_cancel => {
                break CloseType::Takeover;
            }
            req = rs.try_next().fuse() => match req {
                Ok(Some(req)) => req,
                Ok(None) => break CloseType::StreamClosed,
                Err(e) => {
                    e.log("rolling packet capture stream error");
                    break CloseType::StreamClosed;
                }
            },
        };
        match req {
            fnet_debug::RollingPacketCaptureRequest::Detach { name, responder } => {
                let ret = handle_detach(&mut ctx, name);
                responder.send(ret).unwrap_or_log("failed to respond");
            }
            fnet_debug::RollingPacketCaptureRequest::Discard { responder } => {
                break CloseType::Discard(responder);
            }
            fnet_debug::RollingPacketCaptureRequest::StopAndDownload { channel, .. } => {
                // TODO(https://fxbug.dev/485274945): Support multiple calls
                // to StopAndDownload instead of it terminating the protocol.
                break CloseType::Terminate(channel);
            }
        }
    };

    match close_type {
        CloseType::Takeover => Some(CaptureData { id, pcap_headers }),
        CloseType::StreamClosed => {
            let socket_id = {
                let mut state_lock = ctx.bindings_ctx().packet_captures.state.lock();

                // If takeover has been signalled, the state protected by the mutex now
                // belongs to the new task, so we must check for this and hand over
                // capture data correctly instead of overwriting state with Disconnected!
                if let Some(()) = takeover_cancel
                    .try_recv()
                    .expect("takeover sender should not have been dropped")
                {
                    return Some(CaptureData { id, pcap_headers });
                }

                state_lock.replace_with(|old| match old {
                    RollingCaptureState::DetachedConnected { name, .. } => (
                        RollingCaptureState::Disconnected {
                            name,
                            data: CaptureData { id, pcap_headers },
                        },
                        None,
                    ),
                    RollingCaptureState::Attached { .. } => {
                        (RollingCaptureState::Closing, Some(id))
                    }
                    old @ (RollingCaptureState::Empty
                    | RollingCaptureState::Closing
                    | RollingCaptureState::Disconnected { .. }) => {
                        unreachable!("unexpected state at closure: {old:?}");
                    }
                })
            };

            if let Some(socket_id) = socket_id {
                let _: SocketState = remove_socket(&mut ctx, socket_id).await;
                let mut state_lock = ctx.bindings_ctx().packet_captures.state.lock();
                assert_matches::assert_matches!(*state_lock, RollingCaptureState::Closing);
                *state_lock = RollingCaptureState::Empty;
            }

            None
        }
        CloseType::Discard(responder) => {
            {
                let mut state_lock = ctx.bindings_ctx().packet_captures.state.lock();
                state_lock.transition_to_closing();
            }

            let _: SocketState = remove_socket(&mut ctx, id).await;

            {
                let mut state_lock = ctx.bindings_ctx().packet_captures.state.lock();
                assert_matches::assert_matches!(*state_lock, RollingCaptureState::Closing);
                *state_lock = RollingCaptureState::Empty;
            }

            responder.send().unwrap_or_log("failed to respond");
            None
        }
        CloseType::Canceled => {
            {
                let mut state_lock = ctx.bindings_ctx().packet_captures.state.lock();
                state_lock.transition_to_closing();
            }

            let _: SocketState = remove_socket(&mut ctx, id).await;

            {
                let mut state_lock = ctx.bindings_ctx().packet_captures.state.lock();
                assert_matches::assert_matches!(*state_lock, RollingCaptureState::Closing);
                *state_lock = RollingCaptureState::Empty;
            }
            None
        }
        CloseType::Terminate(channel) => {
            {
                let mut state_lock = ctx.bindings_ctx().packet_captures.state.lock();
                state_lock.transition_to_closing();
            }

            let socket_state = remove_socket(&mut ctx, id).await;

            let ring_buffer = socket_state.into_rolling_pcap_buffer();
            let file = std::sync::Arc::new(PcapFile::new(ring_buffer, pcap_headers));
            let scope = vfs::execution_scope::ExecutionScope::new();
            let mut object_request = vfs::object_request::ObjectRequest::new(
                fio::PERM_READABLE,
                &fio::Options::default(),
                channel.into(),
            );
            match vfs::file::serve(file, scope.clone(), &fio::PERM_READABLE, &mut object_request) {
                Ok(()) => {}
                Err(e) => warn!("failed to serve rolling packet capture file: {e}"),
            }

            let ctx_clone = ctx.clone();
            let _: fasync::JoinHandle<_> = fasync::Scope::current().spawn(async move {
                scope.wait().await;
                let mut state_lock = ctx_clone.bindings_ctx().packet_captures.state.lock();
                assert_matches::assert_matches!(*state_lock, RollingCaptureState::Closing);
                *state_lock = RollingCaptureState::Empty;
            });

            None
        }
    }
}

// Serve a stream of fuchsia.net.debug.PacketCaptureProvider API requests for a single
// channel (e.g. a single client connection).
fn handle_start_rolling(
    ctx: &mut Ctx,
    common_params: fnet_debug::CommonPacketCaptureParams,
    params: fnet_debug::RollingPacketCaptureParams,
    guard: ScopeActiveGuard,
) -> Result<
    fidl::endpoints::ClientEnd<fnet_debug::RollingPacketCaptureMarker>,
    fnet_debug::PacketCaptureStartError,
> {
    let fnet_debug::CommonPacketCaptureParams {
        interfaces,
        bpf_program,
        snap_len,
        __source_breaking: _,
    } = common_params;
    let fnet_debug::RollingPacketCaptureParams { capture_size, __source_breaking: _ } = params;

    let ctx_clone = ctx.clone();
    // Note that this lock is held across the entirety of this function and the
    // Attached state is written into the protected state at the end.
    let mut state_lock = ctx_clone.bindings_ctx().packet_captures.state.lock();
    match *state_lock {
        RollingCaptureState::Attached { .. }
        | RollingCaptureState::DetachedConnected { .. }
        | RollingCaptureState::Disconnected { .. }
        | RollingCaptureState::Closing => {
            return Err(fnet_debug::PacketCaptureStartError::QuotaExceeded);
        }
        RollingCaptureState::Empty => {}
    }

    let capture_size = match capture_size {
        None | Some(0) => fnet_debug::DEFAULT_BUFFER_SIZE,
        Some(capture_size) => {
            if capture_size < fnet_debug::MIN_BUFFER_SIZE
                || capture_size > fnet_debug::MAX_BUFFER_SIZE
            {
                return Err(fnet_debug::PacketCaptureStartError::InvalidBufferSize);
            }
            capture_size
        }
    };

    let interface_id = match interfaces {
        Some(fnet_debug::InterfaceSpecifier::Any(fnet_debug::Empty)) => {
            // TODO(https://fxbug.dev/485274945): Add support
            // for capturing on all interfaces.
            warn!("Capture on all interfaces requested but unimplemented");
            return Err(fnet_debug::PacketCaptureStartError::InvalidInterfaceIds);
        }
        Some(fnet_debug::InterfaceSpecifier::InterfaceIds(ids)) => {
            if ids.len() == 0 {
                return Err(fnet_debug::PacketCaptureStartError::InvalidInterfaceIds);
            } else if ids.len() > 1 {
                // TODO(https://fxbug.dev/485274945):
                // Add support for capturing on multiple
                // interfaces.
                warn!("Capture on multiple interfaces requested but unimplemented");
                return Err(fnet_debug::PacketCaptureStartError::InvalidInterfaceIds);
            } else {
                ids[0]
            }
        }
        Some(fnet_debug::InterfaceSpecifier::__SourceBreaking { .. }) => {
            warn!("Unknown InterfaceSpecifier variant received");
            return Err(fnet_debug::PacketCaptureStartError::InvalidInterfaceIds);
        }
        None => {
            return Err(fnet_debug::PacketCaptureStartError::InvalidInterfaceIds);
        }
    };
    let device_id = BindingId::new(interface_id)
        .and_then(|id| ctx.bindings_ctx().devices.get_core_id(id))
        .ok_or(fnet_debug::PacketCaptureStartError::InvalidInterfaceIds)?;

    let link_type = match device_id {
        DeviceId::Ethernet(_) | DeviceId::Loopback(_) | DeviceId::Blackhole(_) => {
            LinkType::Ethernet
        }
        DeviceId::PureIp(_) => LinkType::PureIp,
    };

    const MIN_CHUNK_COUNT: u32 = 8;
    let chunk_size = std::cmp::min(fnet_debug::DEFAULT_SNAP_LEN, capture_size / MIN_CHUNK_COUNT);

    let bpf_filter = match bpf_program {
        Some(program) => {
            let valid_program: ValidVerifiedProgram = program.try_into().map_err(|e| {
                warn!("invalid BPF program: {e:?}");
                fnet_debug::PacketCaptureStartError::InvalidBpfFilter
            })?;

            if valid_program.code.is_empty() {
                warn!("empty BPF code not allowed");
                return Err(fnet_debug::PacketCaptureStartError::InvalidBpfFilter);
            }

            if !valid_program.struct_access_instructions.is_empty()
                || !valid_program.maps.is_empty()
            {
                warn!("struct access or maps not allowed in socket filter");
                return Err(fnet_debug::PacketCaptureStartError::InvalidBpfFilter);
            }

            let maps_cache = ctx.bindings_ctx().ebpf_manager.maps_cache();
            let program = SocketFilterProgram::new(valid_program, maps_cache).map_err(|e| {
                warn!("failed to create BPF program: {e:?}");
                fnet_debug::PacketCaptureStartError::InvalidBpfFilter
            })?;

            Some(program)
        }
        None => None,
    };

    let snap_len = match snap_len {
        None | Some(0) => fnet_debug::DEFAULT_SNAP_LEN,
        Some(l) => l,
    }
    .try_into()
    .expect("default snap len fits in usize");
    let id = ctx.api().device_socket().create(SocketState::new_rolling_pcap(
        RingBuffer::new(
            capture_size.try_into().expect("capture_size fits in usize"),
            chunk_size.try_into().expect("chunk_size fits in usize"),
        ),
        snap_len,
        std::time::SystemTime::now(),
        zx::BootInstant::get(),
        fppacket::Kind::Link,
        bpf_filter,
    ));

    ctx.api().device_socket().set_device_and_protocol(
        &id,
        TargetDevice::SpecificDevice(&device_id),
        Protocol::All,
    );

    let (rolling_client, rolling_server) =
        fidl::endpoints::create_endpoints::<fnet_debug::RollingPacketCaptureMarker>();
    let request_stream = rolling_server.into_stream();
    let ctx_clone = ctx.clone();
    let device_name = device_id.device_name().clone();
    let mut pcap_headers = Vec::new();
    pcap::write_prelude(&mut pcap_headers, link_type, &device_name)
        .expect("failed to write pcap prelude");

    let scope = fasync::Scope::current();
    let (cancel_sender, cancel_receiver) = oneshot::channel::<()>();

    let new_task = scope.compute(async move {
        let scope_cancel = guard.on_cancel();
        serve_rolling_packet_capture(
            ctx_clone,
            request_stream,
            id,
            pcap_headers,
            scope_cancel,
            cancel_receiver,
        )
        .await
    });

    *state_lock = RollingCaptureState::Attached { task: new_task, cancel: cancel_sender };

    Ok(rolling_client)
}

fn handle_reconnect_rolling(
    ctx: &mut Ctx,
    name: String,
    guard: ScopeActiveGuard,
) -> Result<
    fidl::endpoints::ClientEnd<fnet_debug::RollingPacketCaptureMarker>,
    fnet_debug::PacketCaptureReconnectError,
> {
    let (rolling_client, rolling_server) =
        fidl::endpoints::create_endpoints::<fnet_debug::RollingPacketCaptureMarker>();
    let request_stream = rolling_server.into_stream();
    let (cancel_sender, cancel_receiver) = oneshot::channel();
    let ctx_clone = ctx.clone();
    let scope = fasync::Scope::current();

    let mut state_lock = ctx.bindings_ctx().packet_captures.state.lock();

    state_lock.replace_with(|old_state| {
        let capture_data_fut = match old_state {
            RollingCaptureState::Disconnected { name: n, data } if n == name => {
                futures::future::ready(Some(data)).left_future()
            }
            RollingCaptureState::DetachedConnected { name: n, task, cancel } if n == name => {
                cancel.send(()).expect(
                    "cancel recevier should not have been dropped if in DetachedConnected state",
                );
                task.right_future()
            }
            old_state => {
                return (old_state, Err(fnet_debug::PacketCaptureReconnectError::NotFound));
            }
        };

        let new_task = scope.compute(async move {
            let scope_cancel = guard.on_cancel();
            let CaptureData { id, pcap_headers } = capture_data_fut.await?;
            serve_rolling_packet_capture(
                ctx_clone,
                request_stream,
                id,
                pcap_headers,
                scope_cancel,
                cancel_receiver,
            )
            .await
        });

        (
            RollingCaptureState::DetachedConnected {
                name: name.clone(),
                task: new_task,
                cancel: cancel_sender,
            },
            Ok(rolling_client),
        )
    })
}

pub(crate) async fn serve_packet_captures(
    mut ctx: Ctx,
    mut rs: fnet_debug::PacketCaptureProviderRequestStream,
) -> Result<(), fidl::Error> {
    let scope = fasync::Scope::current();
    while let Some(req) = rs.try_next().await? {
        let Some(guard) = scope.active_guard() else {
            warn!("aborted serving packet captures because scope is finished");
            break;
        };
        match req {
            fnet_debug::PacketCaptureProviderRequest::ReconnectRolling { name, responder } => {
                let result = handle_reconnect_rolling(&mut ctx, name, guard);
                responder.send(result).unwrap_or_log("failed to respond");
            }
            fnet_debug::PacketCaptureProviderRequest::StartRolling {
                common_params,
                params,
                responder,
            } => {
                let result = handle_start_rolling(&mut ctx, common_params, params, guard);
                responder.send(result).unwrap_or_log("failed to respond");
            }
        }
    }
    Ok(())
}

struct PcapFile {
    headers: Vec<u8>,
    ring_buffer: RingBuffer,
}

impl PcapFile {
    fn new(ring_buffer: RingBuffer, headers: Vec<u8>) -> Self {
        Self { headers, ring_buffer }
    }
}

impl vfs::directory::entry::GetEntryInfo for PcapFile {
    fn entry_info(&self) -> vfs::directory::entry::EntryInfo {
        vfs::directory::entry::EntryInfo::new(
            fidl_fuchsia_io::INO_UNKNOWN,
            fidl_fuchsia_io::DirentType::File,
        )
    }
}

impl vfs::node::Node for PcapFile {
    async fn get_attributes(
        &self,
        requested_attributes: fidl_fuchsia_io::NodeAttributesQuery,
    ) -> Result<fidl_fuchsia_io::NodeAttributes2, zx::Status> {
        let content_size = self.get_size().await?;
        Ok(vfs::immutable_attributes!(
            requested_attributes,
            Immutable {
                protocols: fidl_fuchsia_io::NodeProtocolKinds::FILE,
                abilities: fidl_fuchsia_io::Operations::GET_ATTRIBUTES
                    | fidl_fuchsia_io::Operations::READ_BYTES,
                content_size: content_size,
                storage_size: content_size,
            }
        ))
    }
}

impl vfs::file::FileIo for PcapFile {
    async fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<u64, zx::Status> {
        let mut offset: usize = match offset.try_into() {
            Ok(o) => o,
            Err(_) => return Err(zx::Status::INVALID_ARGS),
        };
        let headers_len = self.headers.len();
        let (first, second) = self.ring_buffer.get_view();
        let first_len = first.len();
        let second_len = second.len();
        let total_len = headers_len + first_len + second_len;

        if offset >= total_len {
            return Ok(0);
        }

        let mut buffer_offset = 0;

        let mut read_from_region = |region: &[u8], region_start_offset: usize| {
            if buffer_offset < buffer.len()
                && offset >= region_start_offset
                && offset < region_start_offset + region.len()
            {
                let local_offset = offset - region_start_offset;
                let available = region.len() - local_offset;
                let to_read = std::cmp::min(available, buffer.len() - buffer_offset);
                buffer[buffer_offset..buffer_offset + to_read]
                    .copy_from_slice(&region[local_offset..local_offset + to_read]);
                buffer_offset += to_read;
                offset += to_read;
            }
        };

        read_from_region(&self.headers, 0);
        read_from_region(first, headers_len);
        read_from_region(second, headers_len + first_len);

        Ok(u64::try_from(buffer_offset).expect("buffer offset fits in u64"))
    }

    async fn write_at(&self, _offset: u64, _content: &[u8]) -> Result<u64, zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    async fn append(&self, _content: &[u8]) -> Result<(u64, u64), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }
}

impl vfs::file::File for PcapFile {
    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        false
    }

    fn executable(&self) -> bool {
        false
    }

    async fn open_file(&self, _options: &vfs::file::FileOptions) -> Result<(), zx::Status> {
        Ok(())
    }
    async fn truncate(&self, _length: u64) -> Result<(), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    async fn get_size(&self) -> Result<u64, zx::Status> {
        let (first, second) = self.ring_buffer.get_view();
        Ok(u64::try_from(self.headers.len() + first.len() + second.len())
            .expect("pcap file size fits in u64"))
    }

    async fn update_attributes(
        &self,
        _attributes: fidl_fuchsia_io::MutableNodeAttributes,
    ) -> Result<(), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    async fn sync(&self, _mode: vfs::file::SyncMode) -> Result<(), zx::Status> {
        Ok(())
    }
}

impl vfs::file::FileLike for PcapFile {
    fn open(
        self: std::sync::Arc<Self>,
        scope: vfs::execution_scope::ExecutionScope,
        options: vfs::file::FileOptions,
        object_request: vfs::ObjectRequestRef<'_>,
    ) -> Result<(), zx::Status> {
        vfs::file::connection::FidlIoConnection::create_sync(
            scope,
            self,
            options,
            object_request.take(),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::integration_tests::{StackSetupBuilder, TestSetupBuilder};
    use fuchsia_async as fasync;

    #[fixture::teardown(crate::bindings::integration_tests::TestSetup::shutdown)]
    #[fasync::run_singlethreaded(test)]
    async fn test_scope_cancellation_resets_state() {
        let t = TestSetupBuilder::new().add_stack(StackSetupBuilder::new()).build().await;

        let test_stack = t.get(0);
        let ns = test_stack.netstack();

        let loopback_id = test_stack.wait_for_loopback_id().await;

        let (provider_proxy, provider_stream) =
            fidl::endpoints::create_proxy_and_stream::<fnet_debug::PacketCaptureProviderMarker>();

        let provider_scope = fasync::Scope::new_with_name("provider_scope");
        let ns_clone = ns.clone();
        let _ = provider_scope.spawn(async move {
            let _ = serve_packet_captures(ns_clone.ctx, provider_stream).await;
        });

        let rolling_params = fnet_debug::RollingPacketCaptureParams::default();
        let common_params = fnet_debug::CommonPacketCaptureParams {
            interfaces: Some(fnet_debug::InterfaceSpecifier::InterfaceIds(vec![loopback_id.get()])),
            ..fnet_debug::CommonPacketCaptureParams::default()
        };

        let rolling_proxy = provider_proxy
            .start_rolling(common_params, &rolling_params)
            .await
            .expect("start_rolling FIDL error")
            .expect("start_rolling error")
            .into_proxy();

        let capture_name = "test_capture";
        rolling_proxy.detach(capture_name).await.expect("detach FIDL").expect("detach");

        provider_scope.cancel().await;

        let state_lock = ns.ctx.bindings_ctx().packet_captures.state.lock();
        assert_matches::assert_matches!(*state_lock, RollingCaptureState::Empty);

        t
    }
}
