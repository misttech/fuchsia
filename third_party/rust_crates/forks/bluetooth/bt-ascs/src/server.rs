// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_common::core::CodecId;
use bt_common::packet_encoding::Encodable;
use bt_common::{PeerId, Uuid};
use bt_gatt::server::{LocalService, Server, ServiceDefinition, ServiceId};
use bt_gatt::server::{ReadResponder, WriteResponder};
use bt_gatt::types::{
    AttributePermissions, CharacteristicProperty, GattError, Handle, SecurityLevels,
};
use bt_gatt::Characteristic;

use futures::channel::oneshot::{self, Canceled};
use futures::stream::{FusedStream, FuturesUnordered};
use futures::task::{Poll, Waker};
use futures::{Future, Stream, StreamExt};
use pin_project::pin_project;
use std::collections::{HashMap, VecDeque};

use crate::types::*;

#[pin_project(project = LocalServiceProj)]
enum LocalServiceState<T: bt_gatt::ServerTypes> {
    NotPublished {
        waker: Option<Waker>,
    },
    Preparing {
        #[pin]
        fut: T::LocalServiceFut,
    },
    Published {
        service: T::LocalService,
        #[pin]
        events: T::ServiceEventStream,
    },
    Terminated,
}

impl<T: bt_gatt::ServerTypes> Default for LocalServiceState<T> {
    fn default() -> Self {
        Self::NotPublished { waker: None }
    }
}

impl<T: bt_gatt::ServerTypes> LocalServiceState<T> {
    fn service(&self) -> Option<&T::LocalService> {
        let Self::Published { service, .. } = self else {
            return None;
        };
        Some(service)
    }

    fn notify(&self, characteristic: &Handle, data: &[u8], peers: &[PeerId]) -> bool {
        let Some(service) = self.service() else {
            return false;
        };
        service.notify(characteristic, data, peers);
        true
    }
}

impl<T: bt_gatt::ServerTypes> Stream for LocalServiceState<T> {
    type Item = Result<bt_gatt::server::ServiceEvent<T>, Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        // SAFETY:
        //  - Wakers are Unpin
        //  - We re-pin the structurally pinned futures in Preparing and Published
        //    (service is untouched)
        //  - Terminated is empty
        loop {
            match self.as_mut().project() {
                LocalServiceProj::Terminated => return Poll::Ready(None),
                LocalServiceProj::NotPublished { .. } => {
                    self.as_mut()
                        .set(LocalServiceState::NotPublished { waker: Some(cx.waker().clone()) });
                    return Poll::Pending;
                }
                LocalServiceProj::Preparing { fut } => {
                    let service_result = futures::ready!(fut.poll(cx));
                    let Ok(service) = service_result else {
                        self.as_mut().set(LocalServiceState::Terminated);
                        return Poll::Ready(Some(Err(Error::PublishError(
                            service_result.err().unwrap(),
                        ))));
                    };
                    let events = service.publish();
                    self.as_mut().set(LocalServiceState::Published { service, events });
                    continue;
                }
                LocalServiceProj::Published { service: _, events } => {
                    let item = futures::ready!(events.poll_next(cx));
                    let Some(gatt_result) = item else {
                        self.as_mut().set(LocalServiceState::Terminated);
                        return Poll::Ready(Some(Err(Error::PublishError(
                            "GATT server terminated".into(),
                        ))));
                    };
                    let Ok(event) = gatt_result else {
                        self.as_mut().set(LocalServiceState::Terminated);
                        return Poll::Ready(Some(Err(Error::PublishError(
                            gatt_result.err().unwrap(),
                        ))));
                    };
                    return Poll::Ready(Some(Ok(event)));
                }
            }
        }
    }
}

impl<T: bt_gatt::ServerTypes> FusedStream for LocalServiceState<T> {
    fn is_terminated(&self) -> bool {
        match self {
            Self::Terminated => true,
            _ => false,
        }
    }
}

impl<T: bt_gatt::ServerTypes> LocalServiceState<T> {
    fn is_not_published(&self) -> bool {
        matches!(self, LocalServiceState::NotPublished { .. })
    }
}

#[derive(Debug, Clone)]
struct AseControlOperationAction {
    peer_id: PeerId,
    opcode: Option<AseControlPointOpcode>,
    response_codes: Vec<ResponseCode>,
    new_endpoints: Vec<AudioStreamEndpoint>,
}

impl AseControlOperationAction {
    fn notify_control_point_value(&self) -> Option<Vec<u8>> {
        if self.opcode.is_none() || self.response_codes.is_empty() {
            return None;
        }
        let mut notification = Vec::with_capacity(2 + self.response_codes.len() * 3);
        // Opcode and Number_of_ASEs
        notification.push(self.opcode.unwrap().into());
        if self.response_codes[0].ase_id_value() == 0x00 {
            // UnsupportedOpcode or InvalidLength. Number_of_ASEs shall be set to 0xFF
            // See ASCS v1.0.1 Table 4.7.  We only include the first response_code.
            notification.push(0xFF);
            notification.extend(self.response_codes[0].notify_value());
            return Some(notification);
        }
        notification.push(self.response_codes.len() as u8);
        for response in &self.response_codes {
            notification.extend(response.notify_value());
        }
        Some(notification)
    }
}

struct AseControlOperationFut {
    peer_id: PeerId,
    /// The opcode of the operation that this future is for, if there is one.
    opcode: Option<AseControlPointOpcode>,
    /// The current set of response codes gathered for this operation.
    /// Empty if there are no responses to send.
    current_response_codes: Vec<ResponseCode>,
    /// New Endpoint States after the operation is complete
    endpoints: Vec<AudioStreamEndpoint>,
    /// Queue of responses we are still waiting on from the operation.
    waiting: FuturesUnordered<
        futures::channel::oneshot::Receiver<Result<AudioStreamEndpoint, ResponseCode>>,
    >,
}

impl From<&AseControlOperationFut> for AseControlOperationAction {
    fn from(value: &AseControlOperationFut) -> Self {
        Self {
            peer_id: value.peer_id,
            opcode: value.opcode,
            response_codes: value.current_response_codes.clone(),
            new_endpoints: value.endpoints.clone(),
        }
    }
}

impl Future for AseControlOperationFut {
    type Output = AseControlOperationAction;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        loop {
            if self.waiting.is_terminated() {
                let this = std::pin::Pin::into_inner(self);
                return Poll::Ready((&*this).into());
            }
            let update = futures::ready!(self.waiting.poll_next_unpin(cx));
            match update {
                None => continue,
                Some(Err(Canceled)) => {
                    log::warn!("Detected dropped responder!");
                    // TODO: maybe figure out how to determine which AseId got canceled here
                    self.current_response_codes
                        .push(ResponseCode::UnspecifiedError { ase_id: AseId(0x00) });
                    // Bail on the rest of them.
                    self.waiting.clear();
                }
                Some(Ok(Ok(endpoint))) => {
                    self.current_response_codes
                        .push(ResponseCode::Success { ase_id: endpoint.ase_id });
                    self.endpoints.push(endpoint);
                }
                Some(Ok(Err(response_code))) => self.current_response_codes.push(response_code),
            }
        }
    }
}

#[pin_project]
pub struct AudioStreamControlServiceServer<T: bt_gatt::ServerTypes> {
    service_def: ServiceDefinition,
    #[pin]
    local_service: LocalServiceState<T>,
    default_client_endpoints: ClientEndpoints,
    client_endpoints: HashMap<PeerId, ClientEndpoints>,
    outgoing_events: VecDeque<ServiceEvent>,
    // TODO: maybe these should be FuturesOrdered if the operations should be FIFOed
    // Not pinned as AseControlOperationFut is Unpin.
    responses: futures::stream::FuturesUnordered<AseControlOperationFut>,
}

const CONTROL_POINT_HANDLE: Handle = Handle(1);
const BASE_ENDPOINT_HANDLE: Handle = Handle(2);

pub const ASCS_UUID: Uuid = Uuid::from_u16(0x184E);

// As only one ASCS service is allowed on a host, define an arbitrary ServiceId
// that every AudioStreamControlServiceServer will attempt to use so publishing
// multiple will fail.
pub(crate) const ASCS_SERVICE_ID: ServiceId = ServiceId::new(1123901);

impl<T: bt_gatt::ServerTypes> AudioStreamControlServiceServer<T> {
    pub fn new(source_count: u8, sink_count: u8) -> Self {
        let default_client_endpoints = ClientEndpoints::new(source_count, sink_count);
        let mut chars: Vec<Characteristic> = (&default_client_endpoints).into();
        chars.push(Self::build_control_point());
        let mut service_def = ServiceDefinition::new(
            ASCS_SERVICE_ID,
            ASCS_UUID,
            bt_gatt::types::ServiceKind::Primary,
        );
        for c in chars {
            service_def.add_characteristic(c).unwrap();
        }
        Self {
            service_def,
            local_service: Default::default(),
            default_client_endpoints,
            client_endpoints: Default::default(),
            outgoing_events: VecDeque::new(),
            responses: FuturesUnordered::new(),
        }
    }

    fn build_control_point() -> Characteristic {
        let properties = CharacteristicProperty::Write
            | CharacteristicProperty::WriteWithoutResponse
            | CharacteristicProperty::Notify;
        let permissions =
            AttributePermissions::with_levels(&properties, &SecurityLevels::encryption_required());
        Characteristic {
            handle: CONTROL_POINT_HANDLE,
            uuid: Uuid::from_u16(0x2BC6),
            properties,
            permissions,
            descriptors: Vec::new(),
        }
    }

    pub fn publish(&mut self, server: &T::Server) -> Result<(), Error> {
        if !self.local_service.is_not_published() {
            return Err(Error::AlreadyPublished);
        }
        let LocalServiceState::NotPublished { waker } = std::mem::replace(
            &mut self.local_service,
            LocalServiceState::Preparing { fut: server.prepare(self.service_def.clone()) },
        ) else {
            unreachable!();
        };
        waker.map(Waker::wake);
        Ok(())
    }

    pub fn release(&mut self, _id: AseId) -> Result<(), Error> {
        unimplemented!()
    }

    fn queue_operation(self: std::pin::Pin<&mut Self>, peer_id: PeerId, op: AseControlOperation) {
        let this = self.project();
        let (events, fut) =
            op.apply(peer_id, this.client_endpoints.get(&peer_id).unwrap().endpoints.clone());
        this.outgoing_events.extend(events);
        this.responses.push(fut);
    }

    /// Applies operations that are ready to complete (all outstanding events
    /// have been responded to)
    fn poll_operations(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) {
        let this = self.project();
        loop {
            let Poll::Ready(Some(operation)) = this.responses.poll_next_unpin(cx) else {
                return;
            };
            if let Some(notification) = operation.notify_control_point_value() {
                this.local_service.notify(
                    &CONTROL_POINT_HANDLE,
                    &notification,
                    &[operation.peer_id],
                );
            }
            if operation.new_endpoints.is_empty() {
                return;
            }
            // Apply the endpoint changes, and notify each endpoint
            let Some(current_endpoints) = this.client_endpoints.get_mut(&operation.peer_id) else {
                log::warn!(
                    "PeerId {peer_id} has disppeared while an operation was happening, ignoring..",
                    peer_id = operation.peer_id
                );
                continue;
            };
            for endpoint in operation.new_endpoints {
                this.local_service.notify(
                    &endpoint.handle,
                    endpoint.into_char_value().as_slice(),
                    &[operation.peer_id],
                );
                let _ = current_endpoints.endpoints.insert(endpoint.ase_id, endpoint);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AudioDirection {
    Sink,
    Source,
}

impl From<&AudioDirection> for bt_common::Uuid {
    fn from(value: &AudioDirection) -> Self {
        match value {
            AudioDirection::Sink => Uuid::from_u16(0x2BC4),
            AudioDirection::Source => Uuid::from_u16(0x2BC5),
        }
    }
}

#[derive(Debug, Clone)]
enum AseAdditionalParameters {
    /// When in states with no additional parameters: Idle, Releasing
    None,
    /// When CodecConfigured
    CodecConfigured {
        framing: Framing,
        preferred_phys: Vec<Phy>,
        preferred_retransmission_number: u8,
        max_transport_latency: MaxTransportLatency,
        presentation_delay_range: PresentationDelayRange,
        codec_id: CodecId,
        codec_config: Vec<u8>,
    },
}

impl AseAdditionalParameters {
    fn char_size(&self) -> usize {
        match self {
            AseAdditionalParameters::None => 0,
            AseAdditionalParameters::CodecConfigured { codec_config, .. } => {
                23 + codec_config.len()
            }
        }
    }
    fn into_char_value(&self) -> Vec<u8> {
        match self {
            AseAdditionalParameters::None => Vec::new(),
            AseAdditionalParameters::CodecConfigured {
                framing,
                preferred_phys,
                preferred_retransmission_number,
                max_transport_latency,
                presentation_delay_range,
                codec_id,
                codec_config,
            } => {
                let mut value = Vec::with_capacity(self.char_size());
                value.resize(self.char_size() - codec_config.len(), 0);
                value[0] = (*framing) as u8;
                value[1] = Phy::to_bits(preferred_phys.iter());
                value[2] = *preferred_retransmission_number;
                max_transport_latency.encode(&mut value[3..]).unwrap();
                presentation_delay_range.encode(&mut value[5..]).unwrap();
                codec_id.encode(&mut value[17..]).unwrap();
                value[22] = codec_config.len() as u8;
                value.extend(codec_config.clone());
                value
            }
        }
    }
}

#[derive(Debug, Clone)]
struct AudioStreamEndpoint {
    handle: Handle,
    direction: AudioDirection,
    ase_id: AseId,
    state: AseState,
    additional: AseAdditionalParameters,
}

impl AudioStreamEndpoint {
    fn into_char_value(&self) -> Vec<u8> {
        let mut value = Vec::with_capacity(2 + self.additional.char_size());
        value.push(self.ase_id.into());
        value.push(self.state.into());
        value.extend(self.additional.into_char_value());
        value
    }
}

impl From<&AudioStreamEndpoint> for Characteristic {
    fn from(value: &AudioStreamEndpoint) -> Self {
        let properties = CharacteristicProperty::Read | CharacteristicProperty::Notify;
        let permissions =
            AttributePermissions::with_levels(&properties, &SecurityLevels::encryption_required());
        Characteristic {
            handle: value.handle,
            uuid: (&value.direction).into(),
            properties,
            permissions,
            descriptors: Vec::new(),
        }
    }
}

struct ClientEndpoints {
    endpoints: HashMap<AseId, AudioStreamEndpoint>,
    handles: HashMap<Handle, AseId>,
}

impl ClientEndpoints {
    fn new(source_count: u8, sink_count: u8) -> Self {
        let dir_iter = std::iter::repeat(AudioDirection::Source)
            .take(source_count as usize)
            .chain(std::iter::repeat(AudioDirection::Sink).take(sink_count as usize));
        // AseIds shall not have an id of 0
        let (endpoints, handles) = (1..)
            .zip(dir_iter)
            .map(|(raw_ase_id, direction)| {
                let handle = Handle(BASE_ENDPOINT_HANDLE.0 + raw_ase_id as u64);
                let ase_id = AseId(raw_ase_id);
                (
                    (
                        ase_id,
                        AudioStreamEndpoint {
                            handle,
                            ase_id,
                            direction,
                            state: AseState::Idle,
                            additional: AseAdditionalParameters::None,
                        },
                    ),
                    (handle, ase_id),
                )
            })
            .unzip();
        Self { endpoints, handles }
    }

    fn clone_for_peer(&self, _peer_id: PeerId) -> Self {
        // TODO: Randomize the ASE_IDs.  Handles need to stay the same.
        Self { endpoints: self.endpoints.clone(), handles: self.handles.clone() }
    }
}

impl From<&ClientEndpoints> for Vec<Characteristic> {
    fn from(value: &ClientEndpoints) -> Self {
        value.endpoints.values().map(Into::into).collect()
    }
}

#[derive(Debug)]
pub struct CodecConfigureResponder {
    endpoint: AudioStreamEndpoint,
    codec_id: CodecId,
    codec_config: Vec<u8>,
    sender: futures::channel::oneshot::Sender<Result<AudioStreamEndpoint, ResponseCode>>,
}

impl CodecConfigureResponder {
    pub fn reject(self, err: ResponseCode) {
        let _ = self.sender.send(Err(err));
    }

    pub fn accept(
        mut self,
        framing: Framing,
        preferred_phys: Vec<Phy>,
        preferred_retransmission_number: u8,
        max_transport_latency: MaxTransportLatency,
        presentation_delay_range: PresentationDelayRange,
    ) {
        self.endpoint.state = AseState::CodecConfigured;
        self.endpoint.additional = AseAdditionalParameters::CodecConfigured {
            framing,
            preferred_phys,
            preferred_retransmission_number,
            max_transport_latency,
            presentation_delay_range,
            codec_id: self.codec_id,
            codec_config: self.codec_config,
        };
        let _ = self.sender.send(Ok(self.endpoint));
    }
}

#[derive(Debug)]
pub enum ServiceEvent {
    CodecConfigure { configuration: CodecConfiguration, responder: CodecConfigureResponder },
}

impl CodecConfiguration {
    fn into_event(
        self,
        endpoint: AudioStreamEndpoint,
        sender: oneshot::Sender<Result<AudioStreamEndpoint, ResponseCode>>,
    ) -> crate::server::ServiceEvent {
        ServiceEvent::CodecConfigure {
            configuration: self.clone(),
            responder: crate::server::CodecConfigureResponder {
                endpoint,
                codec_id: self.codec_id,
                codec_config: self.codec_specific_configuration.clone(),
                sender,
            },
        }
    }
}

impl AseControlOperation {
    fn apply(
        self,
        peer_id: PeerId,
        endpoint_map: HashMap<AseId, AudioStreamEndpoint>,
    ) -> (Vec<crate::server::ServiceEvent>, AseControlOperationFut) {
        let mut current_response_codes = Vec::new();
        let mut waiting = Vec::new();
        let mut events: Vec<crate::server::ServiceEvent> = Vec::new();
        let opcode: Option<AseControlPointOpcode> = (&self).try_into().ok();
        match self {
            Self::ConfigCodec { codec_configurations, mut responses } => {
                current_response_codes.append(&mut responses);
                for codec_configuration in codec_configurations {
                    let ase_id = codec_configuration.ase_id;
                    let Some(endpoint) = endpoint_map.get(&ase_id) else {
                        current_response_codes
                            .push(ResponseCode::InvalidAseId { value: ase_id.into() });
                        continue;
                    };
                    if !opcode.unwrap().allowed_in_state(&endpoint.state) {
                        current_response_codes
                            .push(ResponseCode::InvalidAseStateMachineTransition { ase_id });
                        continue;
                    }
                    let (send, recv) = oneshot::channel();
                    waiting.push(recv);
                    events.push(codec_configuration.into_event(endpoint.clone(), send));
                }
            }
            _ => todo!(),
        }
        (
            events,
            AseControlOperationFut {
                peer_id,
                opcode,
                current_response_codes,
                endpoints: Vec::new(),
                waiting: waiting.into_iter().collect(),
            },
        )
    }
}

impl<T: bt_gatt::ServerTypes> Stream for AudioStreamControlServiceServer<T> {
    type Item = Result<ServiceEvent, Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            self.as_mut().poll_operations(cx);
            let mut this = self.as_mut().project();
            if let Some(event) = this.outgoing_events.pop_front() {
                return Poll::Ready(Some(Ok(event)));
            }
            let gatt_event = match futures::ready!(this.local_service.as_mut().poll_next(cx)) {
                None => return Poll::Ready(None),
                Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                Some(Ok(event)) => event,
            };
            use bt_gatt::server::ServiceEvent::*;
            let peer_id = gatt_event.peer_id();
            let peer_entry = this
                .client_endpoints
                .entry(peer_id)
                .or_insert_with(|| this.default_client_endpoints.clone_for_peer(peer_id));
            match gatt_event {
                Read { handle, offset, responder, .. } => {
                    let offset = offset as usize;
                    if handle == CONTROL_POINT_HANDLE {
                        responder.error(GattError::ReadNotPermitted);
                        continue;
                    }
                    let Some(ase_id) = peer_entry.handles.get(&handle) else {
                        responder.error(GattError::InvalidHandle);
                        continue;
                    };
                    let Some(endpoint) = peer_entry.endpoints.get(ase_id) else {
                        responder.error(GattError::UnlikelyError);
                        continue;
                    };
                    let value = endpoint.into_char_value();
                    if offset > value.len() {
                        responder.error(GattError::InvalidOffset);
                        continue;
                    }
                    responder.respond(&value[offset..]);
                    continue;
                }
                Write { peer_id, handle, offset, value, responder } => {
                    if handle != CONTROL_POINT_HANDLE {
                        responder.error(GattError::WriteNotPermitted);
                        continue;
                    }
                    if offset != 0 {
                        // Offset write isn't allowed by the service?
                        // TODO: determine if partial writes should be allowed
                        responder.error(GattError::InvalidOffset);
                        continue;
                    }
                    responder.acknowledge();
                    match AseControlOperation::try_from(value.to_owned()) {
                        Ok(op) => self.as_mut().queue_operation(peer_id, op),
                        Err(response_code) => {
                            this.local_service.notify(
                                &CONTROL_POINT_HANDLE,
                                &response_code.error_notify_value(),
                                &[peer_id],
                            );
                        }
                    };
                    continue;
                }
                ClientConfiguration { peer_id, handle, notification_type } => {
                    log::info!(
                        "ASCS Got ClientConfig for {peer_id:?}: {handle:?} {notification_type:?}"
                    );
                }
                PeerInfo { peer_id, mtu, connected, .. } => {
                    log::info!(
                        "ASCS got PeerInfo {peer_id:?}: mtu {mtu:?}, connected: {connected:?}"
                    );
                }
                _ => continue,
            }
        }
    }
}

impl<T: bt_gatt::ServerTypes> FusedStream for AudioStreamControlServiceServer<T> {
    fn is_terminated(&self) -> bool {
        self.local_service.is_terminated()
    }
}
