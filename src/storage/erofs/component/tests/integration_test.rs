// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::DiscoverableProtocolMarker as _;
use fidl_fuchsia_erofs::{ErofsMarker, ErofsProxy, ErofsServeRequest};
use fidl_fuchsia_io as fio;
use fuchsia_component_test::{Capability, ChildOptions, RealmBuilder, Ref, Route};
use fuchsia_fs::directory::{DirEntry, DirentKind, readdir, readdir_inclusive};
use std::fs;

async fn setup_realm() -> (ErofsProxy, fuchsia_component_test::RealmInstance) {
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

    (erofs_server, realm)
}

async fn setup_erofs() -> (fio::DirectoryProxy, fuchsia_component_test::RealmInstance) {
    let (erofs_server, realm) = setup_realm().await;

    let erofs_image = fs::read("/pkg/data/simple.erofs").expect("Failed to read simple.erofs");
    let vmo = zx::Vmo::create(erofs_image.len() as u64).expect("Failed to create VMO");
    vmo.write(&erofs_image, 0).expect("Failed to write VMO");

    let (root_client, root_server) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();

    let payload =
        ErofsServeRequest { backing_vmo: Some(vmo), root: Some(root_server), ..Default::default() };

    let () = erofs_server
        .serve(payload)
        .await
        .expect("Failed to call Serve")
        .expect("Serve returned an error");

    (root_client, realm)
}

#[fuchsia::test]
async fn test_erofs_directory_traversal() {
    let (root_client, _realm) = setup_erofs().await;

    let entries = readdir_inclusive(&root_client).await.expect("Failed to readdir root");

    let expected_entries = [
        DirEntry { name: ".".to_string(), kind: DirentKind::Directory },
        DirEntry { name: "..".to_string(), kind: DirentKind::Directory },
        DirEntry { name: "file1".to_string(), kind: DirentKind::File },
        DirEntry { name: "large_dir".to_string(), kind: DirentKind::Directory },
        DirEntry { name: "photosynthesis".to_string(), kind: DirentKind::File },
        DirEntry { name: "quantum".to_string(), kind: DirentKind::File },
    ];
    assert_eq!(entries, expected_entries);

    let large_dir =
        fuchsia_fs::directory::open_directory(&root_client, "large_dir", fio::PERM_READABLE)
            .await
            .expect("Failed to open large_dir");

    let large_entries = readdir(&large_dir).await.expect("Failed to readdir large_dir");
    assert!(large_entries.len() > 0);
    for entry in &large_entries {
        if entry.name == ".." {
            assert_eq!(entry.kind, DirentKind::Directory);
            continue;
        }
        assert!(entry.name.starts_with("file_number_"));
        assert_eq!(entry.kind, DirentKind::File);
    }

    // Assert lookup non-existent file returns NOT_FOUND
    match fuchsia_fs::directory::open_file(&root_client, "non_existent", fio::PERM_READABLE).await {
        Err(fuchsia_fs::node::OpenError::OpenError(zx::Status::NOT_FOUND)) => (),
        res => panic!("Expected OpenError(NOT_FOUND), got {:?}", res),
    }
}

#[fuchsia::test]
async fn test_erofs_file_stub() {
    let (root_client, _realm) = setup_erofs().await;

    let file = fuchsia_fs::directory::open_file(&root_client, "file1", fio::PERM_READABLE)
        .await
        .expect("Failed to open file1");

    let read_result = file.read(100).await.expect("FIDL call to read failed");
    assert_eq!(read_result, Err(zx::Status::NOT_SUPPORTED.into_raw()));

    let (_, immut_attrs) = file
        .get_attributes(fio::NodeAttributesQuery::all())
        .await
        .expect("Failed to get attributes")
        .map_err(zx::Status::from_raw)
        .expect("get_attributes returned error");

    assert_eq!(immut_attrs.content_size, Some(15)); // "this is a file\n" is 15 bytes
    assert_eq!(
        immut_attrs.abilities,
        Some(fio::Operations::GET_ATTRIBUTES | fio::Operations::READ_BYTES)
    );
    assert!(immut_attrs.id.is_some());
    assert!(immut_attrs.id.unwrap() > 0);
}
