// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A Netstack3 worker to serve fuchsia.net.debug.PacketCaptureProvider API
//! requests.
use std::sync::atomic::Ordering;

use fidl_fuchsia_io as fio;
use fidl_fuchsia_net_debug as fnet_debug;
use fidl_fuchsia_posix_socket_packet as fppacket;
use fuchsia_async as fasync;

use chunked_ringbuf::RingBuffer;
use futures::TryStreamExt as _;
use log::warn;
use pcap::LinkType;
use vfs::file::File as _;

use crate::bindings::bpf::{SocketFilterProgram, ValidVerifiedProgram};
use crate::bindings::devices::HasDeviceName as _;
use crate::bindings::socket::packet::SocketState;
use crate::bindings::util::{RemoveResourceResultExt as _, ResultExt as _, ScopeExt as _};
use crate::bindings::{BindingId, BindingsCtx, Ctx};
use netstack3_core::device::DeviceId;
use netstack3_core::device_socket::{Protocol, SocketId, TargetDevice};

#[derive(Default)]
pub(crate) struct PacketCaptureState {
    active_rolling_captures: std::sync::atomic::AtomicBool,
}

pub(crate) type QuotaGuard = scopeguard::ScopeGuard<Ctx, fn(Ctx)>;

fn release_quota(ctx: Ctx) {
    ctx.bindings_ctx().packet_captures.active_rolling_captures.store(false, Ordering::Release);
}

pub(crate) async fn serve_packet_capture_rolling(
    _guard: QuotaGuard,
    mut ctx: Ctx,
    mut rs: fnet_debug::RollingPacketCaptureRequestStream,
    id: SocketId<BindingsCtx>,
    pcap_headers: Vec<u8>,
) -> Result<(), fidl::Error> {
    let mut id = Some(id);
    let mut pcap_headers = Some(pcap_headers);
    while let Some(req) = rs.try_next().await? {
        match req {
            fnet_debug::RollingPacketCaptureRequest::Detach { .. } => {
                // TODO(https://fxbug.dev/485274945): Add support for Detach.
                warn!("Detach requested but unimplemented");
                return Ok(());
            }
            fnet_debug::RollingPacketCaptureRequest::Discard { .. } => {
                // TODO(https://fxbug.dev/485274945): Add support for Discard.
                warn!("Discard requested but unimplemented");
                return Ok(());
            }
            fnet_debug::RollingPacketCaptureRequest::StopAndDownload { channel, .. } => {
                // TODO(https://fxbug.dev/485274945): Support repeated calls to
                // StopAndDownload.
                let (Some(id), Some(pcap_headers)) = (id.take(), pcap_headers.take()) else {
                    return channel.close_with_epitaph(zx::Status::BAD_STATE);
                };
                let weak = id.downgrade();
                let socket_state = ctx
                    .api()
                    .device_socket()
                    .remove(id)
                    .map_deferred(|d| d.into_future("packet socket", &weak, &ctx))
                    .into_future()
                    .await;
                let ring_buffer = socket_state.into_rolling_pcap_buffer();
                let file = std::sync::Arc::new(PcapFile::new(ring_buffer, pcap_headers));
                let scope = vfs::execution_scope::ExecutionScope::new();
                let mut object_request = vfs::object_request::ObjectRequest::new(
                    fio::PERM_READABLE,
                    &fio::Options::default(),
                    channel.into(),
                );
                match vfs::file::serve(
                    file,
                    scope.clone(),
                    &fio::PERM_READABLE,
                    &mut object_request,
                ) {
                    Ok(()) => {}
                    Err(e) => warn!("failed to serve rolling packet capture file: {e}"),
                }

                let _: fasync::JoinHandle<_> =
                    fasync::Scope::current().spawn(async move { scope.wait().await });
            }
        }
    }
    Ok(())
}

// Serve a stream of fuchsia.net.debug.PacketCaptureProvider API requests for a single
// channel (e.g. a single client connection).
pub(crate) fn handle_start_rolling(
    ctx: &mut Ctx,
    common_params: fnet_debug::CommonPacketCaptureParams,
    params: fnet_debug::RollingPacketCaptureParams,
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
    if let Err::<_, bool>(_) =
        ctx.bindings_ctx().packet_captures.active_rolling_captures.compare_exchange(
            false, /* current */
            true,  /* new */
            Ordering::Acquire,
            Ordering::Relaxed,
        )
    {
        return Err(fnet_debug::PacketCaptureStartError::QuotaExceeded);
    }

    let guard = scopeguard::guard(ctx.clone(), release_quota as fn(Ctx));

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
    fasync::Scope::current().spawn_request_stream_handler(request_stream, move |rs| {
        serve_packet_capture_rolling(guard, ctx_clone, rs, id, pcap_headers)
    });
    Ok(rolling_client)
}

pub(crate) async fn serve_packet_captures(
    mut ctx: Ctx,
    mut rs: fnet_debug::PacketCaptureProviderRequestStream,
) -> Result<(), fidl::Error> {
    while let Some(req) = rs.try_next().await? {
        match req {
            fnet_debug::PacketCaptureProviderRequest::ReconnectRolling { .. } => {
                // TODO(https://fxbug.dev/485274945): Add support for ReconnectRolling.
                warn!("ReconnectRolling requested but unimplemented");
                return Ok(());
            }
            fnet_debug::PacketCaptureProviderRequest::StartRolling {
                common_params,
                params,
                responder,
            } => {
                let result = handle_start_rolling(&mut ctx, common_params, params);
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
