// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(fuchsia_api_level_at_least = "HEAD")]

use crate::puppet::PuppetProxyExt;
use crate::test_topology;
use diagnostics_log_encoding::parse::parse_record;
use diagnostics_log_encoding::{Argument, Record};
use fidl_fuchsia_archivist_test as ftest;
use fidl_fuchsia_diagnostics as fdiagnostics;
use fidl_fuchsia_diagnostics_types as fdiagnostics_types;
use futures::StreamExt;

const PUPPET_NAME: &str = "puppet";

#[fuchsia::test]
async fn listen_with_log_stream() {
    let realm_proxy = test_topology::create_realm(ftest::RealmOptions {
        puppets: Some(vec![test_topology::PuppetDeclBuilder::new(PUPPET_NAME).into()]),
        ..Default::default()
    })
    .await
    .expect("create realm");

    let puppet = test_topology::connect_to_puppet(&realm_proxy, PUPPET_NAME).await.unwrap();
    puppet
        .log_messages(vec![
            (fdiagnostics_types::Severity::Info, "my msg: 10"),
            (fdiagnostics_types::Severity::Warn, "my other msg: 20"),
        ])
        .await;

    let (socket, s2) = zx::Socket::create_datagram();
    let socket = fuchsia_async::Socket::from_socket(socket);
    let log_stream =
        realm_proxy.connect_to_protocol::<fdiagnostics::LogStreamMarker>().await.unwrap();
    log_stream
        .connect(
            s2,
            &fdiagnostics::LogStreamOptions {
                mode: Some(fdiagnostics::StreamMode::SnapshotThenSubscribe),
                ..Default::default()
            },
        )
        .expect("connected socket");

    let mut datagrams = socket.into_datagram_stream();

    // 1. Manifest (Control record)
    let bytes = datagrams.next().await.unwrap().unwrap();
    let (record, _) = parse_record(&bytes).unwrap();

    let moniker = record.arguments.iter().find(|arg| arg.name() == "moniker").unwrap();
    let url = record.arguments.iter().find(|arg| arg.name() == "url").unwrap();

    if let Argument::Other { name: _, value: diagnostics_log_encoding::Value::Text(v) } = moniker {
        assert_eq!(v, "puppet");
    } else {
        panic!("Moniker not found or not text");
    }

    if let Argument::Other { name: _, value: diagnostics_log_encoding::Value::Text(v) } = url {
        assert_eq!(v, "puppet#meta/puppet.cm");
    } else {
        panic!("URL not found or not text");
    }

    // 2. Log 1
    let bytes = datagrams.next().await.unwrap().unwrap();
    let (record, _) = parse_record(&bytes).unwrap();
    assert_eq!(record.severity, fdiagnostics_types::Severity::Info.into_primitive());
    assert_eq!(message(&record), "my msg: 10");

    // 3. Log 2
    let bytes = datagrams.next().await.unwrap().unwrap();
    let (record, _) = parse_record(&bytes).unwrap();
    assert_eq!(record.severity, fdiagnostics_types::Severity::Warn.into_primitive());
    assert_eq!(message(&record), "my other msg: 20");
}

fn message<'a>(record: &'a Record<'_>) -> &'a str {
    record
        .arguments
        .iter()
        .filter_map(|arg| match arg {
            Argument::Message(m) => Some(m.as_ref()),
            _ => None,
        })
        .next()
        .unwrap()
}
