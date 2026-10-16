// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use discovery::{
    DiscoveryBuilder, DiscoverySources, FastbootConnectionState, TargetHandle, TargetState,
    TargetStateFilter,
};
use fastboot::command::{ClientVariable, Command};
use fastboot::reply::Reply;
use fastboot::{FastbootContext, send};
use ffx_config::EnvironmentContext;
use ffx_fastboot_interface::interface_factory::InterfaceFactoryError;
use fuchsia_async::TimeoutExt;
use futures::io::{AsyncRead, AsyncWrite};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

const LIVE_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Verifies that a newly opened network fastboot interface can actually communicate
/// with the target bootloader over the Fastboot protocol (and is not merely connected
/// to a local proxy whose remote USB target is mid-reboot).
pub(crate) async fn verify_fastboot_live<T: AsyncRead + AsyncWrite + Unpin>(
    interface: &mut T,
    addr: &SocketAddr,
) -> bool {
    match send(FastbootContext::new(), Command::GetVar(ClientVariable::Version), interface)
        .on_timeout(LIVE_CHECK_TIMEOUT, || {
            Err(fastboot::FastbootError::Read(fastboot::ReadError::Timeout))
        })
        .await
    {
        Ok(Reply::Okay(version)) => {
            log::debug!("Fastboot target {addr} is live (version: {version})");
            true
        }
        Ok(Reply::Fail(message)) => {
            log::warn!(
                "Failed to get variable \"version\" from {addr} with message: \"{message}\", \
                 but communicated over Fastboot protocol... continuing"
            );
            true
        }
        Err(e) => {
            log::debug!(
                "Fastboot target {addr}: could not communicate over Fastboot protocol: {e:#?}"
            );
            false
        }
        other => {
            log::debug!(
                "Fastboot target {addr}: got unexpected response getting variable: {other:#?}"
            );
            false
        }
    }
}

/// Converts the target identifier stored on a TCP/UDP Fastboot factory into a
/// [`discovery::query::TargetInfoQuery`] for mid-flash rediscovery.
pub(crate) fn query_for_target_name(target_name: &str) -> discovery::query::TargetInfoQuery {
    if target_name.is_empty() || target_name.parse::<SocketAddr>().is_ok() {
        discovery::query::TargetInfoQuery::First
    } else if let Some(serial) =
        target_name.strip_prefix("id:").or_else(|| target_name.strip_prefix("serial:"))
    {
        if serial.is_empty() {
            discovery::query::TargetInfoQuery::First
        } else {
            discovery::query::TargetInfoQuery::Id(serial.to_string())
        }
    } else {
        discovery::query::TargetInfoQuery::NodenameOrId(target_name.to_string())
    }
}

pub(crate) async fn rediscover_helper<F, U>(
    context: &EnvironmentContext,
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
        .set_source(
            DiscoverySources::MDNS | DiscoverySources::MANUAL | DiscoverySources::FASTBOOT_FILE,
        )
        .with_fastboot_devices_file_path(fastboot_file_path.clone())
        .with_state_filter(TargetStateFilter::FASTBOOT)
        .with_short_circuit_on_first(true)
        .build(context);
    let query = query_for_target_name(target_name);
    let targets = discovery.discover_devices(query).await.map_err(anyhow::Error::from)?;
    for handle in targets {
        if filter(&handle) {
            // This is the first event that matches our name.
            // Mutate our internal understanding of the address
            // the target is at with the new address discovered
            match handle.state {
                TargetState::Fastboot(ts) => cb(ts.connection_state)?,
                state @ _ => {
                    return Err(InterfaceFactoryError::RediscoverTargetNotInFastboot(
                        target_name.to_string(),
                        state.to_string(),
                    ));
                }
            };
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use discovery::query::TargetInfoQuery;

    #[fuchsia::test]
    fn test_query_for_target_name_empty_or_socket_addr_returns_first() {
        assert!(matches!(query_for_target_name(""), TargetInfoQuery::First));
        assert!(matches!(query_for_target_name("id:"), TargetInfoQuery::First));
        assert!(matches!(query_for_target_name("serial:"), TargetInfoQuery::First));
        assert!(matches!(query_for_target_name("127.0.0.1:53027"), TargetInfoQuery::First));
        assert!(matches!(query_for_target_name("[::1]:5554"), TargetInfoQuery::First));
    }

    #[fuchsia::test]
    fn test_query_for_target_name_prefix_returns_id() {
        assert_eq!(query_for_target_name("id:serial123"), TargetInfoQuery::Id("serial123".into()));
        assert_eq!(
            query_for_target_name("serial:serial456"),
            TargetInfoQuery::Id("serial456".into())
        );
    }

    #[fuchsia::test]
    fn test_query_for_target_name_nodename_returns_nodename_or_id() {
        match query_for_target_name("fuchsia-5254-0063-5e7a") {
            TargetInfoQuery::NodenameOrId(name) => assert_eq!(name, "fuchsia-5254-0063-5e7a"),
            other => panic!("Expected NodenameOrId, got {other:?}"),
        }
    }
}
