// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
pub use crate::fidl_pipe::{FidlPipe, create_overnet_socket};
use crate::info::{self, TargetInfo};
pub use crate::resolve::{
    DefaultTargetResolver, Resolution, TargetResolver, get_discovery_stream,
    maybe_locally_resolve_target_spec, resolve_target_address, resolve_target_query,
    resolve_target_query_to_info,
};
use crate::{KnockError, TargetInfoQuery};
use anyhow::Result;
use ffx_config::EnvironmentContext;
use fuchsia_async::TimeoutExt;
use futures::StreamExt;
use std::time::Duration;

const DEFAULT_SSH_TIMEOUT_MS: u64 = 10000;
async fn try_get_target_info(
    spec: TargetInfoQuery,
    context: &EnvironmentContext,
) -> Result<(info::RemoteControlState, Option<String>, Option<String>), KnockError> {
    let resolution = resolve_target_address(&spec, context)
        .await
        .map_err(|e| KnockError::CriticalError(e.into()))?;
    let (rcs_state, pc, bc) = match resolution.identify(context).await {
        Ok(id_result) => {
            (info::RemoteControlState::Up, id_result.product_config, id_result.board_config)
        }
        _ => (info::RemoteControlState::Down, None, None),
    };
    Ok((rcs_state, pc, bc))
}

async fn get_target_info(
    context: &EnvironmentContext,
    addrs: &[addr::TargetAddr],
) -> Result<(info::RemoteControlState, Option<String>, Option<String>)> {
    let ssh_timeout: u64 =
        context.get("target.host_pipe_ssh_timeout").unwrap_or(DEFAULT_SSH_TIMEOUT_MS);
    let ssh_timeout = Duration::from_millis(ssh_timeout);
    for addr in addrs {
        // An address is, conveniently, a valid target spec as well
        let spec = if addr.port().filter(|x| *x != 0).is_none() {
            format!("{addr}")
        } else {
            format!("{addr}:{}", addr.port().unwrap())
        };
        log::debug!("Trying to make a connection to spec {spec:?}");
        match try_get_target_info(spec.into(), context)
            .on_timeout(ssh_timeout, || {
                Err(KnockError::NonCriticalError(anyhow::anyhow!("knock_rcs() timed out")))
            })
            .await
        {
            Ok(res) => {
                return Ok(res);
            }
            Err(KnockError::NonCriticalError(e)) => {
                log::debug!("Could not connect to {addr:?}: {e:?}");
                continue;
            }
            e => {
                log::debug!("Got error {e:?} when trying to connect to {addr:?}");
                return Ok((info::RemoteControlState::Unknown, None, None));
            }
        }
    }
    Ok((info::RemoteControlState::Down, None, None))
}

// Convert the handle to a TargetInfo, filling in the information from the target if we are
// asked to make a connection to RCS.
async fn handle_to_info(
    context: &EnvironmentContext,
    handle: discovery::TargetHandle,
    connect_to_target: bool,
) -> Result<TargetInfo> {
    let (rcs_state, product_config, board_config) =
        if let discovery::TargetState::Product { ref addrs, .. } = handle.state {
            // A let-chain would be cleaner, but they are only available in Rust 2024
            if connect_to_target {
                get_target_info(context, addrs).await?
            } else {
                (info::RemoteControlState::Unknown, None, None)
            }
        } else {
            (info::RemoteControlState::Unknown, None, None)
        };
    let info: TargetInfo = handle.into();
    Ok(TargetInfo { rcs_state, board_config, product_config, ..info })
}

async fn handles_to_infos(
    stream: impl futures::Stream<Item = discovery::TargetHandle>,
    ctx: &EnvironmentContext,
    connect: bool,
) -> Result<Vec<TargetInfo>> {
    let info_futures = stream.then(|t| handle_to_info(ctx, t, connect));
    let infos: Vec<Result<TargetInfo>> = info_futures.collect().await;
    let targets = infos.into_iter().collect::<Result<Vec<_>>>()?;
    Ok(targets)
}

pub async fn list_targets(
    ctx: &EnvironmentContext,
    nodename: Option<String>,
    include_usb: bool,
    include_mdns: bool,
    connect: bool,
) -> Result<Vec<TargetInfo>> {
    let query = TargetInfoQuery::from(nodename);
    // When explicitly listing all targets, we don't want to use the
    // cache, for a couple reasons:
    // * explicitly listing the targets probably warrants accurate results
    // * if we get back a stale target, we don't want to waste time trying
    //   to connect to RCS
    let stream = get_discovery_stream(query, include_usb, include_mdns, false, ctx)
        .map_err(anyhow::Error::from)?;
    let targets = handles_to_infos(stream, ctx, connect).await?;
    Ok(targets)
}
#[cfg(test)]
mod test {
    use super::*;

    #[fuchsia::test]
    async fn test_serial_addresses() {
        // USB targets should have an empty list of addresses
        let env = ffx_config::test_init().unwrap();
        let handle = discovery::TargetHandle {
            node_name: Some("nodename".to_string()),
            state: discovery::TargetState::Fastboot(discovery::FastbootTargetState {
                serial_number: "12345678".to_string(),
                connection_state: discovery::FastbootConnectionState::Usb,
            }),
            manual: false,
        };
        let stream = futures::stream::once(async { handle });
        let targets = handles_to_infos(stream, &env.context, true).await;
        let targets = targets.unwrap();
        assert!(targets[0].addresses.is_empty());
    }

    #[fuchsia::test]
    async fn test_handle_to_info_address_sorting() {
        let env = ffx_config::test_init().unwrap();
        let non_link_local_addr: addr::TargetAddr = "[2001:db8::1]:0".parse().unwrap();
        let link_local_addr: addr::TargetAddr = "[fe80::1]:0".parse().unwrap();
        let handle = discovery::TargetHandle {
            node_name: Some("test-node".to_string()),
            state: discovery::TargetState::Product {
                addrs: vec![non_link_local_addr.clone(), link_local_addr.clone()],
                serial: None,
            },
            manual: false,
        };
        let info = handle_to_info(&env.context, handle, false).await.unwrap();
        let addrs = info.addresses;
        assert_eq!(addrs.len(), 2);
        let addrs: Vec<addr::TargetAddr> = addrs.into_iter().map(|a| a.into()).collect();
        // The link-local address should come first.
        assert_eq!(addrs[0], link_local_addr);
        assert_eq!(addrs[1], non_link_local_addr);
    }
}
