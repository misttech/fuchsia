// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::DiscoverableProtocolMarker as _;
use fidl_fuchsia_erofs::{ErofsMarker, ErofsProxy, ErofsServeRequest};
use fidl_fuchsia_io as fio;
use fuchsia_component_test::{Capability, ChildOptions, RealmBuilder, Ref, Route};

#[fuchsia::test]
async fn test_erofs_server_skeleton() {
    let builder = RealmBuilder::new().await.expect("Failed to create RealmBuilder");

    let erofs = builder
        .add_child("erofs", "#meta/erofs.cm", ChildOptions::new())
        .await
        .expect("Failed to add erofs child");

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name(ErofsMarker::PROTOCOL_NAME))
                .from(&erofs)
                .to(Ref::parent()),
        )
        .await
        .expect("Failed to add route");

    let realm = builder.build().await.expect("Failed to build realm");

    let erofs_server: ErofsProxy =
        realm.root.connect_to_protocol_at_exposed_dir().expect("Failed to connect to Erofs");

    // It doesn't matter what size the vmo is yet, and once the real parsing starts we will be
    // copying in a golden file which will have its own size.
    let vmo = zx::Vmo::create(1024).expect("Failed to create VMO");

    let (root_client, root_server) = fidl::endpoints::create_endpoints::<fio::DirectoryMarker>();
    let payload =
        ErofsServeRequest { backing_vmo: Some(vmo), root: Some(root_server), ..Default::default() };

    let () = erofs_server
        .serve(payload)
        .await
        .expect("Failed to call Serve")
        .expect("Serve returned an error");

    // Since the server skeleton stubs out the root directory by dropping it, we expect the
    // root_client channel to be closed.
    let channel = root_client.into_channel();
    let signals = fuchsia_async::OnSignals::new(&channel, zx::Signals::CHANNEL_PEER_CLOSED).await;
    assert!(signals.is_ok());
}
