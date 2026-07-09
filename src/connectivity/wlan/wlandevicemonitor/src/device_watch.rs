// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, format_err};
use fidl_fuchsia_io as fio;
use fuchsia_fs::directory::{WatchEvent, WatchMessage, Watcher};
use futures::stream::{BoxStream, StreamExt as _, TryStreamExt as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use crate::device::{PhyEvent, PhyProxy};

pub struct NewPhyDevice {
    pub id: u16,
    pub proxy: PhyProxy,
    pub event_stream: futures::stream::BoxStream<'static, Result<PhyEvent, anyhow::Error>>,
}

// Implement Debug manually because BoxStream doesn't implement Debug
impl std::fmt::Debug for NewPhyDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewPhyDevice").field("id", &self.id).field("proxy", &self.proxy).finish()
    }
}

pub async fn watch_phy_devices(
    device_directory: &str,
) -> Result<BoxStream<'static, Result<NewPhyDevice, anyhow::Error>>, anyhow::Error> {
    let devfs_dir =
        fuchsia_fs::directory::open_in_namespace(device_directory, fio::Flags::empty()).ok();
    let service =
        fuchsia_component::client::Service::open(fidl_fuchsia_wlan_phy::ServiceMarker).ok();
    watch_phy_devices_impl(devfs_dir, service).await
}

async fn watch_phy_devices_impl(
    devfs_dir: Option<fio::DirectoryProxy>,
    service: Option<fuchsia_component::client::Service<fidl_fuchsia_wlan_phy::ServiceMarker>>,
) -> Result<BoxStream<'static, Result<NewPhyDevice, anyhow::Error>>, anyhow::Error> {
    let devfs_watcher = match &devfs_dir {
        Some(dir) => Watcher::new(dir).await.context("create watcher"),
        None => Err(anyhow::anyhow!("devfs directory not available")),
    };

    let service_stream = match service {
        Some(svc) => svc.watch().await.context("watch service"),
        None => Err(anyhow::anyhow!("service not available")),
    };

    if devfs_watcher.is_err() && service_stream.is_err() {
        return Err(anyhow::anyhow!(
            "Both devfs watcher and service watcher failed to initialize. Devfs: {:?}, Service: {:?}",
            devfs_watcher.err(),
            service_stream.err()
        ));
    }

    let next_id = Arc::new(AtomicU16::new(0));

    let devfs_stream = match (devfs_watcher, devfs_dir) {
        (Ok(watcher), Some(devfs_dir)) => {
            let next_id = Arc::clone(&next_id);
            watcher
                .then(move |result| {
                    let devfs_dir = Clone::clone(&devfs_dir);
                    let next_id = Arc::clone(&next_id);

                    async move {
                        let WatchMessage { event, filename } = match result {
                            Err(e) => {
                                return Err(format_err!("Error in devfs watcher stream {e:?}"));
                            }
                            Ok(x) => x,
                        };

                        if !matches!(event, WatchEvent::ADD_FILE | WatchEvent::EXISTING) {
                            // Ignore all other events since we only care about PHYs being added.
                            return Ok(None);
                        }
                        let filename = match filename.as_path().to_str() {
                            Some(filename) => filename,
                            None => {
                                return Err(format_err!(
                                    "Dropping PHY devfs instance that is not valid unicode: {}",
                                    filename.as_path().to_string_lossy()
                                ));
                            }
                        };

                        // Avoid trying to open '.', which is never valid
                        if filename == "." {
                            return Ok(None);
                        }
                        let (phy_proxy, server_end) = fidl::endpoints::create_proxy();
                        let connector =
                            match fuchsia_component::client::connect_to_named_protocol_at_dir_root::<
                                fidl_fuchsia_wlan_device::ConnectorMarker,
                            >(&devfs_dir, filename)
                            {
                                Err(e) => {
                                    return Err(format_err!(
                                        "Failed to connect to devfs instance: {devfs_dir:?}, {filename}, {e:?}"
                                    ));
                                }
                                Ok(x) => x,
                            };

                        if let Err(e) = connector.connect(server_end) {
                            return Err(format_err!("Error opening '{}': {}", filename, e));
                        }

                        let id = next_id.fetch_add(1, Ordering::Relaxed);
                        let event_stream = PhyProxy::old_event_stream(&phy_proxy);
                        Ok(Some(NewPhyDevice { id, proxy: PhyProxy::Old(phy_proxy), event_stream }))
                    }
                })
                // Using `.then` before `try_filter_map` surfaces all stream and item-level
                // errors explicitly in the async closure, allowing `try_filter_map` to filter
                // out `None` items while propagating any encountered `Err`.
                .try_filter_map(|x| futures::future::ready(Ok(x)))
                .boxed()
        }
        _ => futures::stream::empty::<Result<NewPhyDevice, anyhow::Error>>().boxed(),
    };

    let svc_stream = match service_stream {
        Ok(stream) => {
            let next_id = Arc::clone(&next_id);
            stream
                .then(move |result| {
                    let next_id = Arc::clone(&next_id);
                    async move {
                        let instance_proxy = match result {
                            Err(e) => {
                                return Err(format_err!("Error in service instance stream {e:?}"));
                            }
                            Ok(x) => x,
                        };

                        let phy_proxy = match instance_proxy.connect_to_device() {
                            Err(e) => {
                                return Err(format_err!("Error connecting to PHY service instance: {}", e));
                            }
                            Ok(x) => x,
                        };

                        let id = next_id.fetch_add(1, Ordering::Relaxed);
                        let (proxy, event_stream) = PhyProxy::new(phy_proxy).await?;
                        Ok(Some(NewPhyDevice { id, proxy, event_stream }))
                    }
                })
                // Using `.then` before `try_filter_map` surfaces all stream and item-level
                // errors explicitly in the async closure, allowing `try_filter_map` to filter
                // out `None` items while propagating any encountered `Err`.
                .try_filter_map(|x| futures::future::ready(Ok(x)))
                .boxed()
        }
        _ => futures::stream::empty::<Result<NewPhyDevice, anyhow::Error>>().boxed(),
    };

    let merged = futures::stream::select(devfs_stream, svc_stream);
    Ok(merged.boxed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fidl_fuchsia_wlan_device::{ConnectorRequest, ConnectorRequestStream};
    use fuchsia_async as fasync;
    use futures::poll;

    use futures::task::Poll;
    use log::info;
    use std::pin::pin;
    use std::sync::Arc;
    use vfs::pseudo_directory;
    use wlan_common::test_utils::ExpectWithin;

    fn serve_mock_wlan_phy() -> Arc<vfs::service::Service> {
        vfs::service::host(
            move |mut stream: fidl_fuchsia_wlan_phy::WlanPhyRequestStream| async move {
                use futures::StreamExt as _;
                while let Some(Ok(req)) = stream.next().await {
                    if let fidl_fuchsia_wlan_phy::WlanPhyRequest::Init { payload: _, responder } =
                        req
                    {
                        let _ = responder.send(Ok(()));
                    }
                }
            },
        )
    }

    fn serve_device_connector() -> Arc<vfs::service::Service> {
        vfs::service::host(move |mut stream: ConnectorRequestStream| async move {
            while let Some(request) = stream.next().await {
                match request {
                    Ok(ConnectorRequest::Connect { request: _request, .. }) => {
                        info!("device connector got connect request");
                    }
                    Err(e) => {
                        panic!("Unexpected error in device connector {e:?}");
                    }
                }
            }
        })
    }

    #[fasync::run_singlethreaded(test)]
    async fn watch_single_phy() {
        let fake_dir = pseudo_directory! {
            "123" => serve_device_connector(),
        };
        let dir_proxy =
            vfs::directory::serve_read_only(fake_dir, vfs::execution_scope::ExecutionScope::new());

        let mut phy_watcher = pin!(
            watch_phy_devices_impl(Some(dir_proxy), None)
                .await
                .expect("Failed to create phy_watcher")
        );

        phy_watcher
            .next()
            .expect_within(zx::MonotonicDuration::from_seconds(60), "phy_watcher did not respond")
            .await
            .expect("phy_watcher ended without yielding a phy")
            .expect("phy_watcher returned an error");

        assert_matches!(poll!(phy_watcher.next()), Poll::Pending);
    }

    #[fasync::run_singlethreaded(test)]
    async fn watch_multiple_phys() {
        let fake_dir = pseudo_directory! {
            "123" => serve_device_connector(),
            "456" => serve_device_connector(),
        };
        let dir_proxy =
            vfs::directory::serve_read_only(fake_dir, vfs::execution_scope::ExecutionScope::new());

        let mut phy_watcher = pin!(
            watch_phy_devices_impl(Some(dir_proxy), None)
                .await
                .expect("Failed to create phy_watcher")
        );

        for _ in 0..2 {
            phy_watcher
                .next()
                .expect_within(
                    zx::MonotonicDuration::from_seconds(60),
                    "phy_watcher did not respond",
                )
                .await
                .expect("phy_watcher ended without yielding a phy")
                .expect("phy_watcher returned an error");
        }

        assert_matches!(poll!(phy_watcher.next()), Poll::Pending);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_watch_neither_available() {
        let res = watch_phy_devices_impl(None, None).await;
        assert!(res.is_err());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_watch_only_service_available() {
        let fake_svc_dir = pseudo_directory! {
            "fuchsia.wlan.phy.Service" => pseudo_directory! {
                "default" => pseudo_directory! {
                    "device" => serve_mock_wlan_phy(),
                }
            }
        };
        let dir_proxy = vfs::directory::serve_read_only(
            fake_svc_dir,
            vfs::execution_scope::ExecutionScope::new(),
        );
        let service = fuchsia_component::client::Service::open_from_dir(
            dir_proxy,
            fidl_fuchsia_wlan_phy::ServiceMarker,
        )
        .expect("open_from_dir failed");

        let phy_watcher =
            watch_phy_devices_impl(None, Some(service)).await.expect("failed to start watcher");
        let mut phy_watcher = pin!(phy_watcher);

        let new_phy =
            phy_watcher.next().await.expect("stream ended").expect("watcher returned error");
        assert_eq!(new_phy.id, 0);

        assert_matches!(poll!(phy_watcher.next()), Poll::Pending);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_watch_only_devfs_available() {
        let fake_dir = pseudo_directory! {
            "123" => serve_device_connector(),
        };
        let dir_proxy =
            vfs::directory::serve_read_only(fake_dir, vfs::execution_scope::ExecutionScope::new());

        let phy_watcher =
            watch_phy_devices_impl(Some(dir_proxy), None).await.expect("failed to start watcher");
        let mut phy_watcher = pin!(phy_watcher);

        let new_phy =
            phy_watcher.next().await.expect("stream ended").expect("watcher returned error");
        assert_eq!(new_phy.id, 0);

        assert_matches!(poll!(phy_watcher.next()), Poll::Pending);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_watch_both_available() {
        let fake_dir = pseudo_directory! {
            "123" => serve_device_connector(),
        };
        let dir_proxy =
            vfs::directory::serve_read_only(fake_dir, vfs::execution_scope::ExecutionScope::new());

        let fake_svc_dir = pseudo_directory! {
            "fuchsia.wlan.phy.Service" => pseudo_directory! {
                "default" => pseudo_directory! {
                    "device" => serve_mock_wlan_phy(),
                }
            }
        };
        let svc_dir_proxy = vfs::directory::serve_read_only(
            fake_svc_dir,
            vfs::execution_scope::ExecutionScope::new(),
        );
        let service = fuchsia_component::client::Service::open_from_dir(
            svc_dir_proxy,
            fidl_fuchsia_wlan_phy::ServiceMarker,
        )
        .expect("open_from_dir failed");

        let phy_watcher = watch_phy_devices_impl(Some(dir_proxy), Some(service))
            .await
            .expect("failed to start watcher");
        let mut phy_watcher = pin!(phy_watcher);

        let phy1 = phy_watcher.next().await.expect("stream ended").expect("watcher returned error");
        let phy2 = phy_watcher.next().await.expect("stream ended").expect("watcher returned error");

        let mut ids = vec![phy1.id, phy2.id];
        ids.sort();
        assert_eq!(ids, vec![0, 1]);

        assert_matches!(poll!(phy_watcher.next()), Poll::Pending);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_watch_3_phys_2_devfs_1_service() {
        let fake_dir = pseudo_directory! {
            "123" => serve_device_connector(),
            "456" => serve_device_connector(),
        };
        let dir_proxy =
            vfs::directory::serve_read_only(fake_dir, vfs::execution_scope::ExecutionScope::new());

        let fake_svc_dir = pseudo_directory! {
            "fuchsia.wlan.phy.Service" => pseudo_directory! {
                "default" => pseudo_directory! {
                    "device" => serve_mock_wlan_phy(),
                }
            }
        };
        let svc_dir_proxy = vfs::directory::serve_read_only(
            fake_svc_dir,
            vfs::execution_scope::ExecutionScope::new(),
        );
        let service = fuchsia_component::client::Service::open_from_dir(
            svc_dir_proxy,
            fidl_fuchsia_wlan_phy::ServiceMarker,
        )
        .expect("open_from_dir failed");

        let phy_watcher = watch_phy_devices_impl(Some(dir_proxy), Some(service))
            .await
            .expect("failed to start watcher");
        let mut phy_watcher = pin!(phy_watcher);

        let phy1 = phy_watcher.next().await.expect("stream ended").expect("watcher returned error");
        let phy2 = phy_watcher.next().await.expect("stream ended").expect("watcher returned error");
        let phy3 = phy_watcher.next().await.expect("stream ended").expect("watcher returned error");

        let mut ids = vec![phy1.id, phy2.id, phy3.id];
        ids.sort();
        assert_eq!(ids, vec![0, 1, 2]);

        assert_matches!(poll!(phy_watcher.next()), Poll::Pending);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_watch_3_phys_1_devfs_2_service() {
        let fake_dir = pseudo_directory! {
            "123" => serve_device_connector(),
        };
        let dir_proxy =
            vfs::directory::serve_read_only(fake_dir, vfs::execution_scope::ExecutionScope::new());

        let fake_svc_dir = pseudo_directory! {
            "fuchsia.wlan.phy.Service" => pseudo_directory! {
                "inst1" => pseudo_directory! {
                    "device" => serve_mock_wlan_phy(),
                },
                "inst2" => pseudo_directory! {
                    "device" => serve_mock_wlan_phy(),
                }
            }
        };
        let svc_dir_proxy = vfs::directory::serve_read_only(
            fake_svc_dir,
            vfs::execution_scope::ExecutionScope::new(),
        );
        let service = fuchsia_component::client::Service::open_from_dir(
            svc_dir_proxy,
            fidl_fuchsia_wlan_phy::ServiceMarker,
        )
        .expect("open_from_dir failed");

        let phy_watcher = watch_phy_devices_impl(Some(dir_proxy), Some(service))
            .await
            .expect("failed to start watcher");
        let mut phy_watcher = pin!(phy_watcher);

        let phy1 = phy_watcher.next().await.expect("stream ended").expect("watcher returned error");
        let phy2 = phy_watcher.next().await.expect("stream ended").expect("watcher returned error");
        let phy3 = phy_watcher.next().await.expect("stream ended").expect("watcher returned error");

        let mut ids = vec![phy1.id, phy2.id, phy3.id];
        ids.sort();
        assert_eq!(ids, vec![0, 1, 2]);

        assert_matches!(poll!(phy_watcher.next()), Poll::Pending);
    }
}
