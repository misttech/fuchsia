// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::{DiscoverableProtocolMarker as _, Proxy as _};
use fidl_fuchsia_erofs::{ErofsMarker, ErofsProxy, ErofsServeRequest};
use fidl_fuchsia_io as fio;
use fuchsia_component_test::{Capability, ChildOptions, RealmBuilder, Ref, Route};
use fuchsia_fs::directory::{
    DirEntry, DirentKind, WatchEvent, Watcher, readdir, readdir_inclusive,
};
use futures::StreamExt as _;
use std::fs;
use std::io::Read as _;
use test_case::test_case;

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
async fn test_erofs_file_get_backing_memory() {
    let (root_client, _realm) = setup_erofs().await;

    let file = fuchsia_fs::directory::open_file(&root_client, "file1", fio::PERM_READABLE)
        .await
        .expect("Failed to open file1");

    let expected = fs::read("/pkg/data/simple/file1").expect("Failed to read file1 source");

    let (_, immut_attrs) = file
        .get_attributes(fio::NodeAttributesQuery::all())
        .await
        .expect("Failed to get attributes")
        .map_err(zx::Status::from_raw)
        .expect("get_attributes returned error");

    assert_eq!(immut_attrs.content_size, Some(expected.len() as u64));
    assert_eq!(
        immut_attrs.abilities,
        Some(fio::Operations::GET_ATTRIBUTES | fio::Operations::READ_BYTES)
    );
    assert!(immut_attrs.id.is_some());
    assert!(immut_attrs.id.unwrap() > 0);

    let paged_vmo = file
        .get_backing_memory(fio::VmoFlags::READ)
        .await
        .expect("get_backing_memory FIDL call failed")
        .map_err(zx::Status::from_raw)
        .expect("get_backing_memory returned error");

    let info = paged_vmo.info().expect("Failed to query VMO info");
    assert_eq!(info.committed_bytes, 0);

    let mut buf = vec![0u8; expected.len()];
    paged_vmo.read(&mut buf, 0).expect("Failed to read paged VMO");
    assert_eq!(buf, expected);
}

#[test_case("file1")]
#[test_case("photosynthesis")]
#[test_case("quantum")]
#[fuchsia::test]
async fn test_erofs_file_read(filename: &str) {
    let (root_client, _realm) = setup_erofs().await;

    let file = fuchsia_fs::directory::open_file(&root_client, filename, fio::PERM_READABLE)
        .await
        .expect("Failed to open file");

    let file_channel = file.into_channel().unwrap().into_zx_channel();
    let fd = fdio::create_fd(file_channel.into()).expect("Failed to create FD from VMO connection");
    let mut std_file: std::fs::File = fd.into();

    let mut content = Vec::new();
    std_file.read_to_end(&mut content).expect("Failed to read std_file using std::io::Read");

    let expected =
        fs::read(format!("/pkg/data/simple/{}", filename)).expect("Failed to read source file");
    assert_eq!(content, expected);
}

#[fuchsia::test]
async fn test_erofs_file_paging_after_close() {
    let (root_client, _realm) = setup_erofs().await;

    // Open "photosynthesis", which spans two pages (4128 bytes).
    let file = fuchsia_fs::directory::open_file(&root_client, "photosynthesis", fio::PERM_READABLE)
        .await
        .expect("Failed to open photosynthesis");

    // Request backing VMO memory
    let paged_vmo = file
        .get_backing_memory(fio::VmoFlags::READ)
        .await
        .expect("get_backing_memory FIDL call failed")
        .map_err(zx::Status::from_raw)
        .expect("get_backing_memory returned error");

    // Verify no pages are committed initially.
    let info = paged_vmo.info().expect("Failed to query VMO info");
    assert_eq!(info.committed_bytes, 0);

    // Close the connection to the file. This should drop the file proxy on our end, and on the
    // server, the VFS connection to the ErofsFile is dropped. If lifecycle tracking works
    // properly, the ErofsFile stays alive because of the active VMO child reference, and it will
    // continue to page in data.
    drop(file);

    // Read page 2 from the VMO (offset 4100). This forces a page-in.
    let mut buf = [0u8; 10];
    paged_vmo.read(&mut buf, 4100).expect("Failed to read VMO after closing file connection");
    assert_ne!(buf, [0u8; 10]); // The read should succeed and return actual data.
}

#[fuchsia::test]
async fn test_erofs_directory_watcher() {
    let (root_client, _realm) = setup_erofs().await;

    let mut watcher = Watcher::new(&root_client).await.expect("Failed to create watcher");

    let mut existing_files = std::collections::HashSet::new();

    while let Some(msg) = watcher.next().await {
        let msg = msg.expect("Watcher error");
        match msg.event {
            WatchEvent::EXISTING => {
                existing_files.insert(msg.filename);
            }
            WatchEvent::IDLE => {
                break;
            }
            event => panic!("Unexpected watch event: {:?}", event),
        }
    }

    let expected_files: std::collections::HashSet<_> =
        [".", "file1", "large_dir", "photosynthesis", "quantum"]
            .iter()
            .map(std::path::PathBuf::from)
            .collect();

    assert_eq!(existing_files, expected_files);
}
