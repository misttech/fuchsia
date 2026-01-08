// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests for handling of key shredding.  These are in a separate test package so they can allow
//! error logs, which occur due to apparent corruption if keys are rotated and attempted to be
//! reused.

pub mod config;
use assert_matches::assert_matches;
use config::{data_fs_spec, data_fs_type, new_builder, volumes_spec};
use fidl_fuchsia_fshost::AdminProxy;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_storage_block::BlockMarker;
use fs_management::filesystem::Filesystem;
use fshost_test_fixture::disk_builder::{DataSpec, Disk};
use vmo_backed_block_server::{VmoBackedServer, VmoBackedServerTestingExt as _};
use zx::HandleBased as _;

#[fuchsia::test]
async fn shred_data_volume_when_mounted_keymint() {
    // This test verifies that fshost correctly rotates keymint's keys when ShredDataVolume is
    // called.  It works by restoring the old keybag (which is also shredded during FDR) and
    // verifying that the old keys cannot be replayed, triggering a second FDR.
    let mut builder = new_builder().with_crypt_policy(crypt_policy::Policy::Keymint);
    let data_spec = DataSpec { crypt_policy: crypt_policy::Policy::Keymint, ..data_fs_spec() };
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_spec);
    let fixture = builder.build().await;

    fuchsia_fs::directory::open_file(
        &fixture.dir("data", fio::PERM_READABLE | fio::PERM_WRITABLE),
        "test-file",
        fio::Flags::FLAG_MAYBE_CREATE,
    )
    .await
    .expect("open_file failed");

    let disk = fixture.tear_down().await.unwrap();

    // Manually mount fxfs and backup the keybag.
    let (vmo, _) = disk.into_vmo_and_type_guid().await;
    let keybag_backup = {
        let server = std::sync::Arc::new(VmoBackedServer::from_vmo(
            512,
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        ));
        let connector = Box::new(move |server_end: fidl::endpoints::ServerEnd<BlockMarker>| {
            server.connect_server(server_end.into_channel().into());
            Ok(())
        });
        let fs = Filesystem::from_boxed_config(connector, Box::new(fs_management::Fxfs::default()));
        let vol = fs.serve_multi_volume().await.unwrap();
        let unencrypted = vol
            .open_volume("unencrypted", fidl_fuchsia_fs_startup::MountOptions::default())
            .await
            .unwrap();
        let keys_dir =
            fuchsia_fs::directory::open_directory(unencrypted.root(), "keys", fio::PERM_READABLE)
                .await
                .unwrap();

        // For Keymint policy, the file is "keymint.0".
        let file = fuchsia_fs::directory::open_file(&keys_dir, "keymint.0", fio::PERM_READABLE)
            .await
            .unwrap();
        fuchsia_fs::file::read(&file).await.expect("failed to read keybag")
    };

    // Start fshost again and shred.
    let fixture = new_builder()
        .with_disk_from(Disk::Prebuilt(
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            None,
        ))
        .with_crypt_policy(crypt_policy::Policy::Keymint)
        .build()
        .await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let admin: AdminProxy = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir()
        .expect("connect_to_protcol_at_exposed_dir failed");
    admin
        .shred_data_volume()
        .await
        .expect("shred_data_volume FIDL failed")
        .expect("shred_data_volume failed");

    let disk = fixture.tear_down().await.unwrap();

    // Manually mount fxfs and restore the keybag.
    let (vmo, _) = disk.into_vmo_and_type_guid().await;
    {
        let server = std::sync::Arc::new(VmoBackedServer::from_vmo(
            512,
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        ));
        let connector = Box::new(move |server_end: fidl::endpoints::ServerEnd<BlockMarker>| {
            server.connect_server(server_end.into_channel().into());
            Ok(())
        });
        let fs = Filesystem::from_boxed_config(connector, Box::new(fs_management::Fxfs::default()));
        let vol = fs.serve_multi_volume().await.unwrap();
        let unencrypted = vol
            .open_volume("unencrypted", fidl_fuchsia_fs_startup::MountOptions::default())
            .await
            .unwrap();

        // Re-create keys dir if missing.
        let keys_dir = fuchsia_fs::directory::create_directory(
            unencrypted.root(),
            "keys",
            fio::PERM_READABLE | fio::PERM_WRITABLE,
        )
        .await
        .unwrap();

        let file = fuchsia_fs::directory::open_file(
            &keys_dir,
            "keymint.0",
            fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_WRITABLE,
        )
        .await
        .unwrap();
        fuchsia_fs::file::write(&file, &keybag_backup).await.unwrap();
    }

    // Start fshost again and verify /data is mounted, but it was wiped (test-file should not
    // exist).
    let mut fixture = new_builder()
        .with_disk_from(Disk::Prebuilt(vmo, None))
        .with_crypt_policy(crypt_policy::Policy::Keymint)
        .build()
        .await;
    assert_matches!(
        fuchsia_fs::directory::open_file(
            &fixture.dir("data", fio::PERM_READABLE),
            "test-file",
            fio::PERM_READABLE,
        )
        .await
        .expect_err("open_file failed"),
        fuchsia_fs::node::OpenError::OpenError(zx::Status::NOT_FOUND)
    );

    // We expect crash reports because fshost thought that the data was corrupted.
    fixture.wait_for_crash_reports(1, "fxfs", "fuchsia-fxfs-corruption").await;

    fixture.tear_down().await;
}
