// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use fidl::endpoints::ServiceMarker;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_wlan_tap as wlantap;
use fuchsia_component::client::Service;
use fuchsia_fs::directory::{WatchEvent, Watcher};
use futures::StreamExt;

pub struct Wlantap {
    proxy: wlantap::WlantapCtlProxy,
}

impl Wlantap {
    pub async fn open_from_namespace(prefix: &str) -> Result<Self, Error> {
        let test_ns_dir = fuchsia_fs::directory::open_in_namespace(prefix, fio::Flags::empty())?;

        let mut watcher = Watcher::new(&test_ns_dir).await?;
        while let Some(message) = watcher.next().await {
            let message = message?;
            if message.event == WatchEvent::ADD_FILE || message.event == WatchEvent::EXISTING {
                let filename = message.filename.to_str().unwrap();
                if filename == wlantap::ServiceMarker::SERVICE_NAME {
                    break;
                }
            }
        }

        let service = Service::open_from_dir(&test_ns_dir, wlantap::ServiceMarker)?;
        let service_proxy = service.watch_for_any().await?;
        let proxy = service_proxy.connect_to_wlantap_ctl()?;
        Ok(Self { proxy })
    }

    pub async fn create_phy(
        &self,
        config: wlantap::WlantapPhyConfig,
    ) -> Result<wlantap::WlantapPhyProxy, Error> {
        let Self { proxy } = self;
        let (ours, theirs) = fidl::endpoints::create_proxy();

        let status = proxy.create_phy(&config, theirs).await?;
        let () = zx::ok(status)?;

        Ok(ours)
    }
}
