// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use fidl::endpoints::ServerEnd;
use fidl_fuchsia_recovery_ui::{
    ProgressRendererMarker, ProgressRendererProxyInterface, ProgressRendererRender2Request, Status,
};
use fuchsia_component::client;
use fuchsia_runtime::{take_startup_handle, HandleType};
use futures::future::{join, Future};
use ota_lib::ota::run_wellknown_ota;
use ota_lib::storage::wipe_storage;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use vfs::directory::entry_container::Directory;
use vfs::directory::immutable::simple::Simple;
use vfs::ToObjectRequest as _;
use {fidl_fuchsia_io as fio, fuchsia_async as fasync};

const SERVE_FLAGS: fio::Flags =
    fio::PERM_READABLE.union(fio::PERM_WRITABLE).union(fio::PERM_EXECUTABLE);

fn to_render2_error(err: fidl::Error) -> Error {
    anyhow::format_err!("Error encountered while calling render2: {:?}", err)
}

async fn main_internal<S, P, T, Fut, Fut2>(
    wipe_storage_fn: S,
    ota_progress_proxy: &P,
    out_dir: ServerEnd<fio::DirectoryMarker>,
    do_ota_fn: T,
) -> Result<(), Error>
where
    S: FnOnce() -> Fut2,
    P: ProgressRendererProxyInterface,
    T: FnOnce(fio::DirectoryProxy, Arc<Simple>) -> Fut,
    Fut: Future<Output = Result<(), Error>> + 'static,
    Fut2: Future<Output = Result<fio::DirectoryProxy, Error>> + 'static,
{
    let outgoing_dir_vfs = vfs::pseudo_directory! {};

    let blobfs_proxy = wipe_storage_fn().await.context("failed to wipe storage")?;

    let scope = vfs::execution_scope::ExecutionScope::new();
    vfs::directory::serve_on(outgoing_dir_vfs.clone(), SERVE_FLAGS, scope.clone(), out_dir);
    fasync::Task::local(async move { scope.wait().await }).detach();

    ota_progress_proxy
        .render2(&ProgressRendererRender2Request {
            status: Some(Status::Active),
            percent_complete: Some(0.0),
            ..Default::default()
        })
        .await
        .map_err(to_render2_error)?;

    let ota_done: &AtomicBool = &AtomicBool::new(false);
    let progress_future = async move {
        // TODO(b/245415603) Send false progress updates until actual progress is reported
        use fuchsia_async::MonotonicDuration;
        use futures::StreamExt;
        let duration = 7 * 60 * 1000; // 7 minutes (ms)
        let num_updates = 100;
        let mut interval_timer =
            fasync::Interval::new(MonotonicDuration::from_millis(duration / num_updates));

        let mut progress = 0;
        while let Some(_) = interval_timer.next().await {
            if ota_done.load(Ordering::Relaxed) || progress == 100 {
                return;
            }
            let _ = ota_progress_proxy
                .render2(&ProgressRendererRender2Request {
                    status: Some(Status::Active),
                    percent_complete: Some(progress as f32),
                    ..Default::default()
                })
                .await;
            progress += 1;
        }
    };
    let ota_future = async move {
        let result = do_ota_fn(blobfs_proxy, outgoing_dir_vfs).await;
        ota_done.store(true, Ordering::Relaxed);
        result
    };

    let (ota_result, _) = join(ota_future, progress_future).await;
    match ota_result {
        Ok(_) => {
            println!("OTA Success!");
            ota_progress_proxy
                .render2(&ProgressRendererRender2Request {
                    status: Some(Status::Complete),
                    percent_complete: Some(100.0),
                    ..Default::default()
                })
                .await
                .map_err(to_render2_error)
        }
        Err(e) => {
            println!("OTA Error..... {:?}", e);
            ota_progress_proxy
                .render2(&ProgressRendererRender2Request {
                    status: Some(Status::Error),
                    ..Default::default()
                })
                .await
                .map_err(to_render2_error)?;
            Err(e)
        }
    }
}

#[fuchsia::main(logging = true)]
async fn main() -> Result<(), Error> {
    stdout_to_debuglog::init().await.unwrap_or_else(|error| {
        eprintln!("Failed to initialize debuglog: {:?}", error);
    });

    println!("recovery-ota: started");
    let ota_progress_proxy = client::connect_to_protocol::<ProgressRendererMarker>()?;
    let directory_handle = take_startup_handle(HandleType::DirectoryRequest.into())
        .expect("cannot take startup handle");

    main_internal(
        wipe_storage,
        &ota_progress_proxy,
        zx::Channel::from(directory_handle).into(),
        move |blobfs_proxy, outgoing_dir| run_wellknown_ota(blobfs_proxy, outgoing_dir),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::format_err;
    use assert_matches::assert_matches;
    use fidl::endpoints::{create_endpoints, create_proxy, create_proxy_and_stream};
    use fidl_fuchsia_recovery_ui::ProgressRendererRequest;
    use futures::stream::StreamExt;
    use vfs::file::vmo::read_only;

    async fn fake_wipe_storage() -> Result<fio::DirectoryProxy, Error> {
        let (client, server) = create_endpoints::<fio::DirectoryMarker>();
        let scope = vfs::execution_scope::ExecutionScope::new();
        let dir = vfs::pseudo_directory! {
            "testfile" => read_only("test1")
        };
        vfs::directory::serve_on(dir, SERVE_FLAGS, scope.clone(), server);
        fasync::Task::local(async move { scope.wait().await }).detach();
        Ok(client.into_proxy())
    }

    #[fuchsia::test]
    async fn test_main_internal_reports_ota_success() {
        let (progress_proxy, mut progress_stream) =
            create_proxy_and_stream::<ProgressRendererMarker>();
        let (_dir_proxy, dir_server) = create_proxy::<fio::DirectoryMarker>();

        fasync::Task::local(async move {
            assert_matches!(progress_stream.next().await.unwrap().unwrap(), ProgressRendererRequest::Render2 { payload, responder } => {
                assert_eq!(payload.status.unwrap(), Status::Active);
                assert_eq!(payload.percent_complete.unwrap(), 0.0);
                responder.send().unwrap();
            });

            assert_matches!(progress_stream.next().await.unwrap().unwrap(), ProgressRendererRequest::Render2 { payload, responder } => {
                assert_eq!(payload.status.unwrap(), Status::Complete);
                assert_eq!(payload.percent_complete.unwrap(), 100.0);
                responder.send().unwrap();
            });

            // Expect nothing more.
            assert!(progress_stream.next().await.is_none());
        })
        .detach();

        main_internal(fake_wipe_storage, &progress_proxy, dir_server, |_storage, _outgoing_dir| {
            futures::future::ready(Ok(()))
        })
        .await
        .unwrap();
    }

    #[fuchsia::test]
    async fn test_main_internal_sends_error_when_ota_fails() {
        let (progress_proxy, mut progress_stream) =
            create_proxy_and_stream::<ProgressRendererMarker>();
        let (_dir_proxy, dir_server) = create_proxy::<fio::DirectoryMarker>();

        fasync::Task::local(async move {
            assert_matches!(progress_stream.next().await.unwrap().unwrap(), ProgressRendererRequest::Render2 { payload, responder } => {
                assert_eq!(payload.status.unwrap(), Status::Active);
                assert_eq!(payload.percent_complete.unwrap(), 0.0);
                responder.send().unwrap();
            });

            assert_matches!(progress_stream.next().await.unwrap().unwrap(), ProgressRendererRequest::Render2 { payload, responder } => {
                assert_eq!(payload.status.unwrap(), Status::Error);
                responder.send().unwrap();
            });

            // Expect nothing more.
            assert!(progress_stream.next().await.is_none());
        })
        .detach();

        main_internal(fake_wipe_storage, &progress_proxy, dir_server, |_storage, _outgoing_dir| {
            futures::future::ready(Err(format_err!("ota failed")))
        })
        .await
        .unwrap_err();
    }
}
