// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
pub use crate::fidl_pipe::{FidlPipe, create_overnet_socket};
use crate::info::{self, TargetInfo};
pub use crate::resolve::{
    DefaultTargetResolver, Resolution, TargetResolver, get_discovery_stream,
    maybe_locally_resolve_target_spec, resolve_target_address,
};
use crate::{KnockCriticalError, KnockError, KnockNonCriticalError, TargetInfoQuery};
use addr::TargetAddr;
use anyhow::Result;
use ffx_config::EnvironmentContext;
use fuchsia_async::TimeoutExt;
use futures::StreamExt;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

const DEFAULT_SSH_TIMEOUT_MS: u64 = 10000;
async fn try_get_target_info(
    spec: TargetInfoQuery,
    context: &EnvironmentContext,
) -> Result<
    (info::RemoteControlState, Option<String>, Option<String>, Option<u64>, Option<String>),
    KnockError,
> {
    // We want to make sure to provide an up-to-date list, so don't rely on the cache
    let resolution = resolve_target_address(&spec, false, context)
        .await
        .map_err(|e| KnockError::Critical(KnockCriticalError::TargetError(format!("{:?}", e))))?;
    let (rcs_state, pc, bc, bi, sn) = match resolution.identify(context).await {
        Ok(id_result) => (
            info::RemoteControlState::Up,
            id_result.product_config,
            id_result.board_config,
            id_result.boot_id,
            id_result.serial_number,
        ),
        _ => (info::RemoteControlState::Down, None, None, None, None),
    };
    Ok((rcs_state, pc, bc, bi, sn))
}

async fn get_target_info(
    context: &EnvironmentContext,
    addrs: &[addr::TargetAddr],
) -> Result<(info::RemoteControlState, Option<String>, Option<String>, Option<u64>, Option<String>)>
{
    let ssh_timeout: u64 =
        context.get("target.host_pipe_ssh_timeout").unwrap_or(DEFAULT_SSH_TIMEOUT_MS);
    let ssh_timeout = Duration::from_millis(ssh_timeout);
    for addr in addrs {
        let query = TargetInfoQuery::from(*addr);
        log::debug!("Trying to make a connection to query {query:?}");
        match try_get_target_info(query, context)
            .on_timeout(ssh_timeout, || {
                Err(KnockError::NonCritical(KnockNonCriticalError::Timeout {
                    detail: "knock_rcs() timed out".to_string(),
                }))
            })
            .await
        {
            Ok(res) => {
                return Ok(res);
            }
            Err(KnockError::NonCritical(e)) => {
                log::debug!("Could not connect to {addr:?}: {e:?}");
                continue;
            }
            e => {
                log::debug!("Got error {e:?} when trying to connect to {addr:?}");
                return Ok((info::RemoteControlState::Unknown, None, None, None, None));
            }
        }
    }
    Ok((info::RemoteControlState::Down, None, None, None, None))
}

// Convert the handle to a TargetInfo, filling in the information from the target if we are
// asked to make a connection to RCS.
async fn handle_to_info(
    context: &EnvironmentContext,
    handle: discovery::TargetHandle,
    connect_to_target: bool,
    query: TargetInfoQuery,
) -> Result<TargetInfo> {
    let (rcs_state, product_config, board_config, boot_id, serial_number) =
        if let discovery::TargetState::Product { ref addrs, .. } = handle.state {
            // A let-chain would be cleaner, but they are only available in Rust 2024
            if connect_to_target {
                get_target_info(context, addrs).await?
            } else {
                (info::RemoteControlState::Unknown, None, None, None, None)
            }
        } else {
            (info::RemoteControlState::Unknown, None, None, None, None)
        };
    let info: TargetInfo = handle.into();
    let is_default = Some(info.match_query(&query));
    Ok(TargetInfo {
        rcs_state,
        board_config,
        product_config,
        boot_id,
        is_default,
        serial_number: serial_number.or_else(|| info.serial_number.clone()),
        ..info
    })
}

async fn handles_to_infos(
    stream: impl futures::Stream<Item = discovery::TargetHandle>,
    ctx: &EnvironmentContext,
    connect: bool,
) -> Result<Vec<TargetInfo>> {
    let default = TargetInfoQuery::try_from(crate::get_target_specifier(ctx)?)?;

    let info_futures = stream.then(|t| handle_to_info(ctx, t, connect, default.clone()));
    let infos: Vec<Result<TargetInfo>> = info_futures.collect().await;
    let targets = infos.into_iter().collect::<Result<Vec<_>>>()?;
    let targets = merge_target_addrs(targets);
    Ok(targets)
}

// Merge targets that have the same boot_id. Having any boot_id at all means
// the target was in Product mode, and we're going to assume that all the
// information other than the addresses is the same. So we just need to combine
// the addresses together.
fn merge_target_addrs(targets: Vec<TargetInfo>) -> Vec<TargetInfo> {
    let mut merged_map: HashMap<u64, TargetInfo> = HashMap::with_capacity(targets.len());
    let mut result = HashSet::<TargetInfo>::with_capacity(targets.len());
    for mut t in targets {
        let aset: HashSet<TargetAddr> = t.addresses.into_iter().collect();
        t.addresses = aset.into_iter().collect();
        if let Some(boot_id) = t.boot_id {
            match merged_map.entry(boot_id) {
                Entry::Occupied(mut entry) => {
                    let addresses = &mut entry.get_mut().addresses;
                    let mut aset: HashSet<TargetAddr> = addresses.clone().into_iter().collect();
                    aset.extend(t.addresses);
                    *addresses = aset.into_iter().collect();
                }
                Entry::Vacant(entry) => {
                    entry.insert(t);
                }
            }
        } else {
            result.insert(t);
        }
    }
    result.extend(merged_map.into_values());
    result.into_iter().collect()
}

pub async fn list_targets(
    ctx: &EnvironmentContext,
    query: TargetInfoQuery,
    include_usb: bool,
    include_mdns: bool,
    connect: bool,
) -> std::result::Result<Vec<TargetInfo>, crate::FfxTargetCrateError> {
    // When explicitly listing all targets, we don't want to use the
    // cache, for a couple reasons:
    // * explicitly listing the targets probably warrants accurate results
    // * if we get back a stale target, we don't want to waste time trying
    //   to connect to RCS
    let stream = get_discovery_stream(query, include_usb, include_mdns, ctx)?;
    Ok(handles_to_infos(stream, ctx, connect).await?)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::info::{RemoteControlState, TargetState};
    use addr::TargetAddr;
    use std::collections::HashSet;

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
        let info =
            handle_to_info(&env.context, handle, false, TargetInfoQuery::First).await.unwrap();
        let addrs = info.addresses;
        assert_eq!(addrs.len(), 2);
        let addrs: Vec<addr::TargetAddr> = addrs.into_iter().map(|a| a.into()).collect();
        // The link-local address should come first.
        assert_eq!(addrs[0], link_local_addr);
        assert_eq!(addrs[1], non_link_local_addr);
    }

    fn make_target_info(addr: TargetAddr, boot_id: Option<u64>) -> TargetInfo {
        TargetInfo {
            nodename: Some("t".to_string()),
            addresses: vec![addr],
            rcs_state: RemoteControlState::Up,
            target_state: TargetState::Product,
            product_config: Some("product".to_string()),
            board_config: Some("board".to_string()),
            serial_number: Some("serial".to_string()),
            is_manual: false,
            boot_id,
            is_default: None,
        }
    }

    #[fuchsia::test]
    fn test_merge_target_ip_addrs() {
        let addr1: addr::TargetAddr = "[fe80::1]:1".parse().unwrap();
        let t1 = make_target_info(addr1, Some(999));
        let addr2: addr::TargetAddr = "[fe80::1]:2".parse().unwrap();
        let t2 = make_target_info(addr2, Some(999));
        let targets = merge_target_addrs(vec![t1, t2]);
        assert_eq!(targets.len(), 1);
        let merged = vec![addr1, addr2];
        let target0 = targets[0].clone();
        assert_eq!(
            HashSet::<TargetAddr>::from_iter(target0.addresses.into_iter()),
            HashSet::from_iter(merged.into_iter())
        );
    }

    #[fuchsia::test]
    fn test_merge_target_duplicate_addrs() {
        let addr1: addr::TargetAddr = "[fe80::1]:1".parse().unwrap();
        let t1 = make_target_info(addr1, Some(999));
        let t2 = make_target_info(addr1, Some(999));
        let targets = merge_target_addrs(vec![t1, t2]);
        assert_eq!(targets.len(), 1);
        let target0 = targets[0].clone();
        assert_eq!(target0.addresses.len(), 1);
        assert_eq!(target0.addresses[0], addr1);
    }

    #[fuchsia::test]
    fn test_merge_target_non_ip_addrs() {
        let addr1: addr::TargetAddr = "[fe80::1]:1".parse().unwrap();
        let t1 = make_target_info(addr1, Some(999));
        let addr2: addr::TargetAddr = addr::TargetAddr::VSockCtx(123);
        let t2 = make_target_info(addr2, Some(999));
        let targets = merge_target_addrs(vec![t1, t2]);
        assert_eq!(targets.len(), 1);
        let merged = vec![addr1, addr2];
        let target0 = targets[0].clone();
        assert_eq!(
            HashSet::<TargetAddr>::from_iter(target0.addresses.into_iter()),
            HashSet::from_iter(merged.into_iter())
        );
    }

    #[fuchsia::test]
    fn test_merge_target_distinct_bootids() {
        let addr1: addr::TargetAddr = "[fe80::1]:1".parse().unwrap();
        let t1 = make_target_info(addr1, Some(888));
        let addr2: addr::TargetAddr = "[fe80::1]:2".parse().unwrap();
        let t2 = make_target_info(addr2, Some(999));
        let targets = merge_target_addrs(vec![t1, t2]);
        assert_eq!(targets.len(), 2);
    }

    #[fuchsia::test]
    fn test_merge_target_no_bootids() {
        let addr1: addr::TargetAddr = "[fe80::1]:1".parse().unwrap();
        let t1 = make_target_info(addr1, None);
        let addr2: addr::TargetAddr = "[fe80::1]:2".parse().unwrap();
        let t2 = make_target_info(addr2, None);
        let targets = merge_target_addrs(vec![t1, t2]);
        assert_eq!(targets.len(), 2);
    }

    #[fuchsia::test]
    fn test_merge_target_one_bootid() {
        let addr1: addr::TargetAddr = "[fe80::1]:1".parse().unwrap();
        let t1 = make_target_info(addr1, Some(999));
        let addr2: addr::TargetAddr = "[fe80::1]:2".parse().unwrap();
        let t2 = make_target_info(addr2, None);
        let targets = merge_target_addrs(vec![t1, t2]);
        assert_eq!(targets.len(), 2);
    }

    #[fuchsia::test]
    fn test_merge_target_duplicate_targets_no_bootid() {
        let addr1: addr::TargetAddr = "127.0.0.1:1".parse().unwrap();
        let t1 = make_target_info(addr1, None);
        let t2 = make_target_info(addr1, None);
        let t3 = make_target_info(addr1, None);
        let targets = merge_target_addrs(vec![t1, t2, t3]);
        assert_eq!(targets.len(), 1);
    }

    #[fuchsia::test]
    async fn test_handle_to_info_is_default() {
        let env = ffx_config::test_init().unwrap();
        let matching_nodename = "matching-node".to_string();
        let non_matching_nodename = "non-matching-node".to_string();
        let query = TargetInfoQuery::try_from(matching_nodename.clone()).unwrap();

        // Test with a matching target
        let matching_handle = discovery::TargetHandle {
            node_name: Some(matching_nodename.clone()),
            state: discovery::TargetState::Product { addrs: vec![], serial: None },
            manual: false,
        };
        let info =
            handle_to_info(&env.context, matching_handle, false, query.clone()).await.unwrap();
        assert_eq!(info.is_default, Some(true));

        // Test with a non-matching target
        let non_matching_handle = discovery::TargetHandle {
            node_name: Some(non_matching_nodename.clone()),
            state: discovery::TargetState::Product { addrs: vec![], serial: None },
            manual: false,
        };
        let info =
            handle_to_info(&env.context, non_matching_handle, false, query.clone()).await.unwrap();
        assert_eq!(info.is_default, Some(false));
    }

    #[fuchsia::test]
    async fn test_handle_to_info_serial_number() {
        let env = ffx_config::test_init().unwrap();
        let handle = discovery::TargetHandle {
            node_name: Some("test-node".to_string()),
            state: discovery::TargetState::Fastboot(discovery::FastbootTargetState {
                serial_number: "fastboot_serial".to_string(),
                connection_state: discovery::FastbootConnectionState::Usb,
            }),
            manual: false,
        };
        let query = TargetInfoQuery::First;
        let info = handle_to_info(&env.context, handle, false, query).await.unwrap();
        assert_eq!(info.serial_number, Some("fastboot_serial".to_string()));
    }
}
