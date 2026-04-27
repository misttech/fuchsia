// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::proxies::Proxies;
use anyhow::anyhow;
use fidl_fuchsia_bluetooth::{ChannelMode, ChannelParameters, PeerId};
use fidl_fuchsia_bluetooth_bredr::{
    ConnectParameters, ConnectionReceiverMarker, ConnectionReceiverRequest,
    ConnectionReceiverRequestStream, DataElement, L2capParameters, ProtocolDescriptor,
    ProtocolIdentifier, ServiceDefinition,
};
use fuchsia_async::TimeoutExt;
use fuchsia_bluetooth::types::{Channel, Uuid as BtUuid};
use futures::StreamExt;

pub(crate) async fn connect_l2cap(
    proxies: &Proxies,
    peer_id: &PeerId,
    psm: u16,
) -> Result<Channel, anyhow::Error> {
    match proxies
        .profile_proxy
        .connect(
            peer_id,
            &ConnectParameters::L2cap(L2capParameters { psm: Some(psm), ..Default::default() }),
        )
        .await
    {
        Ok(Ok(channel_res)) => Ok(channel_res
            .try_into()
            .map_err(|err| anyhow!("Couldn't convert FIDL to BT channel: {err:?}"))?),
        Ok(Err(sapphire_err)) => {
            Err(anyhow!("fuchsia.bluetooth.bredr.Profile/Connect error: {sapphire_err:?}"))
        }
        Err(fidl_err) => Err(anyhow!("fuchsia.bluetooth.bredr.Profile/Connect error: {fidl_err}")),
    }
}

pub(crate) async fn advertise_service(
    proxies: &Proxies,
    psm: u16,
) -> Result<ConnectionReceiverRequestStream, anyhow::Error> {
    let (connect_client, connect_server) =
        fidl::endpoints::create_request_stream::<ConnectionReceiverMarker>();

    let service_def = ServiceDefinition {
        service_class_uuids: Some(vec![BtUuid::new16(0x1401).into()]), // Non-reserved ID
        protocol_descriptor_list: Some(vec![ProtocolDescriptor {
            protocol: Some(ProtocolIdentifier::L2Cap),
            params: Some(vec![DataElement::Uint16(psm)]),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let _ = proxies
        .profile_proxy
        .advertise(fidl_fuchsia_bluetooth_bredr::ProfileAdvertiseRequest {
            services: Some(vec![service_def]),
            receiver: Some(connect_client),
            parameters: Some(ChannelParameters {
                channel_mode: Some(ChannelMode::EnhancedRetransmission),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await?
        .map_err(|e| anyhow!("fuchsia.bluetooth.bredr.Profile/Advertise error: {:?}", e))?;

    Ok(connect_server)
}

pub(crate) async fn serve_connection_receiver(
    mut connection_receiver_stream: ConnectionReceiverRequestStream,
    l2cap_channel: &mut Option<Channel>,
    timeout: std::time::Duration,
) -> Result<Option<PeerId>, anyhow::Error> {
    match connection_receiver_stream.next().on_timeout(timeout, || None).await {
        Some(Ok(ConnectionReceiverRequest::Connected {
            peer_id,
            channel,
            protocol: _,
            control_handle: _,
        })) => {
            *l2cap_channel = Some(channel.try_into().unwrap());
            Ok(Some(peer_id))
        }
        None => Ok(None),
        Some(Err(err)) => {
            Err(anyhow!("fuchsia.bluetooth.bredr.ConnectionReceiver reported error: {err}"))
        }
        Some(Ok(_)) => {
            Err(anyhow!("fuchsia.bluetooth.bredr.ConnectionReceiver received unexpected request"))
        }
    }
}
