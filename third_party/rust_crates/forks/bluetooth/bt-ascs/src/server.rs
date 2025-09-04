// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_gatt::Characteristic;
use bt_gatt::server::{ReadResponder, WriteResponder};
use bt_gatt::types::{
    AttributePermissions, CharacteristicProperty, GattError, Handle, SecurityLevels,
};
use futures::task::{Poll, Waker};
use futures::{Future, Stream, stream::FusedStream};
use pin_project::pin_project;
use std::collections::HashMap;

use bt_common::{PeerId, Uuid};
use bt_gatt::server::{LocalService, Server, ServiceDefinition, ServiceId};

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

#[pin_project]
pub struct AudioStreamControlServiceServer<T: bt_gatt::ServerTypes> {
    service_def: ServiceDefinition,
    #[pin]
    local_service: LocalServiceState<T>,
    default_client_endpoints: ClientEndpoints,
    client_endpoints: HashMap<PeerId, ClientEndpoints>,
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
struct AudioStreamEndpoint {
    handle: Handle,
    direction: AudioDirection,
    ase_id: AseId,
    state: AseState,
    // TODO(b/433287917): Add Additional Parameters for other states.
    // Currently only works for Idle and Releasing states.
}

impl AudioStreamEndpoint {
    fn into_char_value(&self) -> Vec<u8> {
        let mut value = Vec::with_capacity(2);
        value.push(self.ase_id.into());
        value.push(self.state.into());
        // TODO: add the additional_ase_parameters for the other states
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
                        AudioStreamEndpoint { handle, ase_id, direction, state: AseState::Idle },
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

pub enum ServiceEvent {}

impl<T: bt_gatt::ServerTypes> Stream for AudioStreamControlServiceServer<T> {
    type Item = Result<ServiceEvent, Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let mut this = self.project();
        loop {
            let event = match futures::ready!(this.local_service.as_mut().poll_next(cx)) {
                None => return Poll::Ready(None),
                Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                Some(Ok(event)) => event,
            };
            use bt_gatt::server::ServiceEvent::*;
            let peer_id = event.peer_id();
            let peer_entry = this
                .client_endpoints
                .entry(peer_id)
                .or_insert_with(|| this.default_client_endpoints.clone_for_peer(peer_id));
            match event {
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
                    let _op = match AseControlOperation::try_from(value.to_owned()) {
                        Ok(op) => op,
                        Err(e) => {
                            let service_ref = this.local_service.as_ref();

                            let value = e.notify_value();
                            service_ref.service().unwrap().notify(
                                &CONTROL_POINT_HANDLE,
                                &value[..],
                                &[peer_id],
                            );
                            continue;
                        }
                    };
                    // TODO: Do the operation here, possibly notifying things
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
