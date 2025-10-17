// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::RecoveryMessages;
use anyhow::{Context as _, Error};
use fidl::endpoints::{DiscoverableProtocolMarker as _, create_proxy};
use fuchsia_component::client::connect_to_protocol;
use isolated_swd::updater::Updater;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use vfs::directory::helper::DirectlyMutable as _;
use {fidl_fuchsia_fxfs as ffxfs, fidl_fuchsia_io as fio};

pub async fn apply_update(
    url: &str,
    signature: &[u8],
    view_sender: &crate::view_sender::ViewSender,
    exposed_dir: Arc<vfs::directory::simple::Simple>,
    svc_dir: Arc<vfs::directory::simple::Simple>,
) -> Result<(), Error> {
    view_sender.queue_message(RecoveryMessages::Log(format!("Mounting system blob volume...")));
    let fshost_recovery = connect_to_protocol::<fidl_fuchsia_fshost::RecoveryMarker>()
        .context("connecting to fshost Recovery")?;
    let (blob_exposed_dir, blob_exposed_dir_server) = create_proxy::<fio::DirectoryMarker>();

    static FORMAT_BLOB_VOLUME: AtomicBool = AtomicBool::new(true);
    if FORMAT_BLOB_VOLUME.load(std::sync::atomic::Ordering::Relaxed) {
        // TODO(https://fxbug.dev/445907120): Use separate blob volume instead of formatting.
        fshost_recovery
            .format_system_blob_volume()
            .await
            .context("calling FormatSystemBlobVolume")?
            .map_err(zx::Status::from_raw)
            .context("formatting system blob volume")?;
    } else {
        log::info!("Skipping formatting of system blob volume");
    }
    fshost_recovery
        .mount_system_blob_volume(blob_exposed_dir_server)
        .await
        .context("calling MountSystemBlobVolume")?
        .map_err(zx::Status::from_raw)
        .context("mounting system blob volume")?;

    let blob_root = fuchsia_fs::directory::open_directory(
        &blob_exposed_dir,
        "root",
        fio::PERM_READABLE | fio::PERM_WRITABLE | fio::PERM_EXECUTABLE,
    )
    .await
    .context("opening blob root")?;
    exposed_dir
        .add_entry_may_overwrite("blob", vfs::remote::remote_dir(blob_root), true)
        .context("adding blob dir entry")?;

    let blob_svc =
        fuchsia_fs::directory::open_directory(&blob_exposed_dir, "svc", fio::PERM_READABLE)
            .await
            .context("opening blob svc")?;
    for protocol_name in
        [ffxfs::BlobCreatorMarker::PROTOCOL_NAME, ffxfs::BlobReaderMarker::PROTOCOL_NAME]
    {
        let blob_svc = Clone::clone(&blob_svc);
        svc_dir
            .add_entry_may_overwrite(
                protocol_name,
                vfs::service::endpoint(move |_scope, channel| {
                    if let Err(e) = blob_svc.open(
                        protocol_name,
                        fio::Flags::PROTOCOL_SERVICE,
                        &Default::default(),
                        channel.into(),
                    ) {
                        log::error!("Failed to call open on blob svc: {e}");
                    }
                }),
                true,
            )
            .with_context(|| format!("adding {protocol_name} entry"))?;
    }

    view_sender.queue_message(RecoveryMessages::Log(format!("Installing update...")));
    let mut updater = Updater::new().context("Failed to create updater")?;
    let res = updater
        .install_update(Some(&url.parse()?), Some(signature))
        .await
        .context("Failed to apply update");
    // Don't format blobs if we try to update again to allow resuming to maintain forward progress.
    FORMAT_BLOB_VOLUME.store(false, std::sync::atomic::Ordering::Relaxed);
    // Explicitly closing the `blob_exposed_dir` to let fshost shutdown the filesystem and destroy
    // the fxblob component, if not closed, this should still happen when `blob_exposed_dir` goes
    // out of scope. The `blob_root` and `blob_svc` handles are connected directly to the fxblob
    // component, and will be invalidated.
    blob_exposed_dir
        .close()
        .await
        .context("calling close")?
        .map_err(zx::Status::from_raw)
        .context("closing blob exposed dir")?;
    if let Err(e) = stop_pkg_recovery().await {
        log::error!("Failed to stop pkg-recovery: {e:#}");
    }
    res
}

async fn stop_pkg_recovery() -> Result<(), Error> {
    let lifecycle_controller =
        connect_to_protocol::<fidl_fuchsia_sys2::LifecycleControllerMarker>()
            .context("connecting to lifecycle controller")?;
    for moniker in
        ["./pkg-recovery/system-updater", "./pkg-recovery/pkg-resolver", "./pkg-recovery/pkg-cache"]
    {
        lifecycle_controller
            .stop_instance(moniker)
            .await
            .context("calling lifecycle controller")?
            .map_err(|e| anyhow::anyhow!("failed to stop {moniker}: {e:?}"))?;
    }
    Ok(())
}
