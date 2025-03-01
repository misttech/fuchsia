// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use discovery::{
    wait_for_devices, DiscoverySources, FastbootConnectionState, TargetEvent, TargetFilter,
    TargetState,
};
use ffx_fastboot_interface::interface_factory::InterfaceFactoryError;
use futures::StreamExt;
use std::path::PathBuf;

pub(crate) async fn rediscover_helper<F, U>(
    fastboot_file_path: &Option<PathBuf>,
    target_name: &String,
    filter: F,
    cb: &mut U,
) -> Result<(), InterfaceFactoryError>
where
    F: TargetFilter,
    U: FnMut(FastbootConnectionState) -> Result<(), InterfaceFactoryError>,
{
    let mut device_stream = wait_for_devices(
        filter,
        None,
        fastboot_file_path.clone(),
        true,
        false,
        DiscoverySources::MDNS | DiscoverySources::MANUAL | DiscoverySources::FASTBOOT_FILE,
    )
    .await?;

    if let Some(Ok(event)) = device_stream.next().await {
        // This is the first event that matches our filter.
        // Mutate our internal understanding of the address
        // the target is at with the new address discovered
        match event {
            TargetEvent::Removed(_) => {
                return Err(InterfaceFactoryError::RediscoverTargetError(format!("When rediscovering target: {}, expected a target Added event but got a Removed event", target_name)))
            }
            TargetEvent::Added(handle) => match handle.state {
                TargetState::Fastboot(ts) => cb(ts.connection_state)?,
                state @ _ => return Err(InterfaceFactoryError::RediscoverTargetNotInFastboot(target_name.to_string(), state.to_string())),
            },
        }
        return Ok(());
    }
    Ok(())
}
