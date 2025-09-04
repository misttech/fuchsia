// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_common::{PeerId, Uuid};
use bt_gatt::test_utils::{FakeServer, FakeServerEvent, FakeTypes};
use bt_gatt::types::Handle;

use futures::channel::mpsc::UnboundedReceiver;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::server::AudioStreamControlServiceServer;
use crate::server::ServiceEvent;
use crate::server::{ASCS_SERVICE_ID, ASCS_UUID};
use crate::*;

#[track_caller]
fn expect_service_event(events: &mut UnboundedReceiver<FakeServerEvent>) -> FakeServerEvent {
    match events.poll_next_unpin(&mut futures_test::task::noop_context()) {
        Poll::Ready(Some(event)) => event,
        x => panic!("Expected fake server event, got {x:?}"),
    }
}

#[test]
fn publishes() {
    let mut ascs_server = std::pin::pin!(AudioStreamControlServiceServer::<FakeTypes>::new(1, 1));

    let (count_waker, woken_count) = futures_test::task::new_count_waker();

    // Polling the server before it's published should result in Poll::Pending
    let poll_result = ascs_server.as_mut().poll_next(&mut Context::from_waker(&count_waker));

    assert!(poll_result.is_pending());
    assert_eq!(woken_count.get(), 0);

    let (fake_server, mut events) = FakeServer::new();

    let result = ascs_server.publish(&fake_server);
    assert!(result.is_ok());

    let already_published = ascs_server.publish(&fake_server);

    assert!(already_published.is_err());

    assert_eq!(woken_count.get(), 1);

    let poll_result = ascs_server.poll_next(&mut Context::from_waker(&count_waker));

    match expect_service_event(&mut events) {
        FakeServerEvent::Published { id: _, definition } => {
            assert_eq!(definition.uuid(), ASCS_UUID);
            assert_eq!(
                definition.characteristics().filter(|c| c.uuid == Uuid::from_u16(0x2BC4)).count(),
                1
            );
            assert_eq!(
                definition.characteristics().filter(|c| c.uuid == Uuid::from_u16(0x2BC5)).count(),
                1
            );
        }
        x => panic!("Expected published event, got {x:?}"),
    };

    // Should still be pending, even though we had some work to do.
    assert!(poll_result.is_pending());
}

fn published_server() -> (
    Pin<Box<AudioStreamControlServiceServer<FakeTypes>>>,
    FakeServer,
    UnboundedReceiver<FakeServerEvent>,
) {
    let mut ascs_server = Box::pin(AudioStreamControlServiceServer::<FakeTypes>::new(1, 1));
    let (fake_server, events) = FakeServer::new();

    let result = ascs_server.publish(&fake_server);
    assert!(result.is_ok());

    assert!(ascs_server.poll_next_unpin(&mut futures_test::task::noop_context()).is_pending());

    (ascs_server, fake_server, events)
}

fn poll_server(
    server: &mut Pin<Box<AudioStreamControlServiceServer<FakeTypes>>>,
) -> Poll<Option<core::result::Result<ServiceEvent, Error>>> {
    server.poll_next_unpin(&mut futures_test::task::noop_context())
}

// Ignored because we currently do nothing with operations.
#[ignore]
#[test]
fn peers_are_separated() {
    let (mut ascs_server, fake_server, mut server_events) = published_server();

    // Read the sink uuid
    fake_server.incoming_read(PeerId(1), ASCS_SERVICE_ID, Handle(2), 0);

    // Poll the ascs server, should not result in an ASCS event
    assert!(poll_server(&mut ascs_server).is_pending());

    // Should have the response
    let peer_one_value;
    match server_events.poll_next_unpin(&mut futures_test::task::noop_context()) {
        Poll::Ready(Some(FakeServerEvent::ReadResponded { service_id, handle: _, value })) => {
            assert_eq!(service_id, ASCS_SERVICE_ID);
            peer_one_value = value.unwrap();
        }
        x => panic!("Expected the read to be responded to got {x:?}"),
    };

    let ase_id = peer_one_value[0];

    // Codec Configure the first peer ase_id
    fake_server.incoming_write(
        PeerId(1),
        ASCS_SERVICE_ID,
        Handle(1),
        0,
        vec![0x01, ase_id, 0x01, 0x01, 0x06, 0x00],
    );

    // Still shouldn't have any event
    assert!(poll_server(&mut ascs_server).is_pending());

    // Read the sink id from another peer
    fake_server.incoming_read(PeerId(2), ASCS_SERVICE_ID, Handle(2), 0);

    let peer_two_value;
    match server_events.poll_next_unpin(&mut futures_test::task::noop_context()) {
        Poll::Ready(Some(FakeServerEvent::ReadResponded { service_id, handle: _, value })) => {
            assert_eq!(service_id, ASCS_SERVICE_ID);
            peer_two_value = value.unwrap();
        }
        x => panic!("Expected the read to be responded to got {x:?}"),
    };

    let _ase_id = peer_two_value[0];
    assert!(peer_one_value[1] != peer_two_value[1]);
}
