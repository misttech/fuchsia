// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use discovery::emulator_watcher::EmulatorWatcher;
use discovery::fastboot_file_watcher::FastbootWatcher;
use discovery::query::TargetInfoQuery;
use discovery::{
    DiscoveryBuilder, DiscoverySources, TargetEvent, TargetHandle, TargetStream, TargetStreamConfig,
};
use ffx_config::EnvironmentContext;
use ffx_target::{Resolution, TargetResolver};
use fho::{FfxContext, Result};
use fidl_fuchsia_developer_ffx as ffx;
use futures::channel::mpsc;
use manual_targets::watcher::ManualTargetEvent;
use std::path::PathBuf;
use usb_fastboot_discovery::FastbootEvent;

#[derive(Default)]
pub struct DiagnosticsResolver {
    sources: DiscoverySources,
}

impl TargetResolver for DiagnosticsResolver {
    async fn discovered_targets(
        &self,
        query: TargetInfoQuery,
        ctx: EnvironmentContext,
    ) -> anyhow::Result<Vec<TargetHandle>> {
        let stream = build_discovery_stream(&ctx, self.sources)?;
        let discoverer = DiscoveryBuilder::default().build_with_stream(stream);
        discoverer.discover_devices(query).await.map_err(|e| anyhow::anyhow!(e))
    }

    async fn resolve_target_query(
        &self,
        query: TargetInfoQuery,
        ctx: &EnvironmentContext,
    ) -> anyhow::Result<Vec<TargetHandle>> {
        self.discovered_targets(query, ctx.clone()).await
    }

    async fn try_resolve_manual_target(
        &self,
        _name: &str,
        _ctx: &EnvironmentContext,
    ) -> anyhow::Result<Option<Resolution>> {
        // This is never going to be used in this implementation.
        unimplemented!();
    }
}

pub(crate) fn build_discovery_stream(
    ctx: &EnvironmentContext,
    sources: DiscoverySources,
) -> Result<TargetStream> {
    let emu_instance_root: PathBuf =
        ctx.get(emulator_instance::EMU_INSTANCE_ROOT_DIR).with_user_message(|| {
            format!("unable to get `{}`", emulator_instance::EMU_INSTANCE_ROOT_DIR)
        })?;
    let fastboot_file_path: Option<PathBuf> =
        ctx.get(fastboot_file_discovery::FASTBOOT_FILE_PATH).ok();

    let mut config = TargetStreamConfig::new();
    let (sender, queue) = mpsc::unbounded();

    if sources.contains(DiscoverySources::MDNS) {
        let mdns_sender = sender.clone();
        config.set_mdns_event_handler(move |res: ffx::MdnsEventType| {
            let event = TargetEvent::try_from(res).ok();
            if let Some(event) = event {
                let _ = mdns_sender.unbounded_send(event);
            }
        })
    }

    if sources.contains(DiscoverySources::USB_FASTBOOT) {
        let fastboot_sender = sender.clone();
        config.set_fastboot_event_handler(move |res: FastbootEvent| {
            let event = res.into();
            let _ = fastboot_sender.unbounded_send(event);
        })
    }

    if sources.contains(DiscoverySources::MANUAL) {
        let manual_targets_sender = sender.clone();
        config.set_manual_event_handler(move |res: ManualTargetEvent| {
            let event = res.into();
            let _ = manual_targets_sender.unbounded_send(event);
        })
    }

    if sources.contains(DiscoverySources::EMULATOR) {
        config.set_emulator_watcher(EmulatorWatcher::new(emu_instance_root, sender.clone()).bug()?)
    }

    if sources.contains(DiscoverySources::FASTBOOT_FILE) {
        if let Some(fastboot_devices_file) = fastboot_file_path {
            config.set_fastboot_file_watcher(
                FastbootWatcher::new(fastboot_devices_file, sender).bug()?,
            )
        }
    }

    Ok(TargetStream::new(config, queue))
}
