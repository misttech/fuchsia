// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use discovery::{
    DiscoveryBuilder, DiscoverySources, FastbootConnectionState, TargetDiscovery, TargetEvent,
    TargetHandle, TargetState,
};
use ffx_fastboot_interface::interface_factory::InterfaceFactoryError;
use futures::StreamExt;
use std::path::PathBuf;

pub(crate) async fn rediscover_helper<F, U>(
    fastboot_file_path: &Option<PathBuf>,
    target_name: &String,
    mut filter: F,
    cb: &mut U,
) -> Result<(), InterfaceFactoryError>
where
    F: FnMut(&TargetHandle) -> bool,
    U: FnMut(FastbootConnectionState) -> Result<(), InterfaceFactoryError>,
{
    let discovery = DiscoveryBuilder::default()
        .notify_added(true)
        .set_source(DiscoverySources::MDNS | DiscoverySources::MANUAL)
        .with_fastboot_devices_file_path(fastboot_file_path.clone())
        .build();
    let query = discovery::query::TargetInfoQuery::NodenameOrSerial(target_name.clone());
    let mut device_stream = discovery.discover_devices(query).map_err(anyhow::Error::from)?;

    while let Some(event) = device_stream.next().await {
        if filter(event.as_handle()) {
            // This is the first event that matches our name.
            // Mutate our internal understanding of the address
            // the target is at with the new address discovered
            match event {
                TargetEvent::Removed(_) => {
                    return Err(InterfaceFactoryError::RediscoverTargetError(format!(
                        "When rediscovering target: {}, expected a target Added event but got a Removed event",
                        target_name
                    )));
                }
                TargetEvent::Added(handle) => match handle.state {
                    TargetState::Fastboot(ts) => cb(ts.connection_state)?,
                    state @ _ => {
                        return Err(InterfaceFactoryError::RediscoverTargetNotInFastboot(
                            target_name.to_string(),
                            state.to_string(),
                        ));
                    }
                },
            }
            return Ok(());
        }
    }
    Ok(())
}
