// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use assert_matches::assert_matches;
use fidl::Error::ClientChannelClosed;
use netstack_testing_common::realms::{Netstack3, TestSandboxExt as _};
use netstack_testing_macros::netstack_test;
use {
    fidl_fuchsia_net as fnet, fidl_fuchsia_net_matchers as fnet_matchers,
    fidl_fuchsia_net_sockets as fnet_sockets,
};

#[netstack_test]
async fn no_results_when_no_sockets(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox.create_netstack_realm::<Netstack3, _>(name).expect("create realm");
    let diagnostics = realm
        .connect_to_protocol::<fnet_sockets::DiagnosticsMarker>()
        .expect("connect to protocol");

    let (proxy, server_end) = fidl::endpoints::create_proxy::<fnet_sockets::IpIteratorMarker>();
    assert_matches!(
        diagnostics
            .iterate_ip(
                server_end,
                fnet_sockets::Extensions::empty(),
                &[fnet_sockets::IpSocketMatcher::Family(fnet::IpVersion::V4)]
            )
            .await
            .expect("failed to call fidl"),
        fnet_sockets::IterateIpResult::Ok(fnet_sockets::Empty)
    );

    let (sockets, has_more) = proxy.next().await.unwrap();
    assert!(sockets.is_empty());
    assert!(!has_more);
    assert_matches!(proxy.next().await, Err(ClientChannelClosed { .. }));
}

#[netstack_test]
async fn invalid_matcher(name: &str) {
    let good_matcher = fnet_sockets::IpSocketMatcher::Family(fnet::IpVersion::V4);
    let invalid_matcher = fnet_sockets::IpSocketMatcher::BoundInterface(
        fnet_matchers::BoundInterface::Bound(fnet_matchers::Interface::Id(0)),
    );

    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox.create_netstack_realm::<Netstack3, _>(name).expect("create realm");
    let diagnostics = realm
        .connect_to_protocol::<fnet_sockets::DiagnosticsMarker>()
        .expect("connect to protocol");

    let (proxy, server_end) = fidl::endpoints::create_proxy::<fnet_sockets::IpIteratorMarker>();
    assert_matches!(
        diagnostics
            .iterate_ip(
                server_end,
                fnet_sockets::Extensions::empty(),
                &[invalid_matcher.clone(), good_matcher.clone(), good_matcher.clone(),]
            )
            .await
            .expect("failed to call fidl"),
        fnet_sockets::IterateIpResult::MatcherError(fnet_sockets::IterateIpMatcherError {
            index: Some(0),
            ..
        })
    );
    assert_matches!(proxy.next().await, Err(ClientChannelClosed { .. }));

    let (proxy, server_end) = fidl::endpoints::create_proxy::<fnet_sockets::IpIteratorMarker>();
    assert_matches!(
        diagnostics
            .iterate_ip(
                server_end,
                fnet_sockets::Extensions::empty(),
                &[good_matcher.clone(), good_matcher, invalid_matcher,]
            )
            .await
            .expect("failed to call fidl"),
        fnet_sockets::IterateIpResult::MatcherError(fnet_sockets::IterateIpMatcherError {
            index: Some(2),
            ..
        })
    );
    assert_matches!(proxy.next().await, Err(ClientChannelClosed { .. }));
}
