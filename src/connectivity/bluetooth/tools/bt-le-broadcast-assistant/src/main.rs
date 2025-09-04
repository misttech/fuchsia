// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod address_lookup;
mod assistant;
mod commands;
mod repl;

use anyhow::{Context, Error, anyhow};
use bt_broadcast_assistant::debug::AssistantDebug;
use bt_common::PeerId;
use bt_common::core::{Address, AddressType};
use bt_gatt::pii::GetPeerAddr;
use bt_gatt_fuchsia::FuchsiaTypes;
use bt_gatt_fuchsia::pii::FuchsiaPeerAddr;
use fidl_fuchsia_bluetooth_le::CentralMarker;
use fidl_fuchsia_bluetooth_sys::AddressLookupMarker;

use crate::address_lookup::LocalPeerAddrCache;

struct AppConfig {
    use_static_address: bool,
}

fn parse_args(args: &[String]) -> Result<AppConfig, Error> {
    let mut use_static_address = false;

    let mut args_iter = args.iter();
    while let Some(arg) = args_iter.next() {
        match arg.as_str() {
            "--use-static-address" => {
                use_static_address = true;
            }
            _ => {
                let usage = "Usage: bt-le-broadcast-assistant [--use-static-address]";
                return Err(anyhow!("Unknown argument: {}\n{}", arg, usage));
            }
        }
    }

    Ok(AppConfig { use_static_address })
}

/// An enum that holds one of the concrete types that implement the `GetPeerAddr` trait.
/// This is used to avoid dynamic dispatch, which is not possible with the `GetPeerAddr` trait.
enum BroadcastSourceAddressGetter {
    Fuchsia(FuchsiaPeerAddr),
    Local(LocalPeerAddrCache),
}

impl GetPeerAddr for BroadcastSourceAddressGetter {
    async fn get_peer_address(&self, peer_id: PeerId) -> bt_gatt::Result<(Address, AddressType)> {
        match self {
            BroadcastSourceAddressGetter::Fuchsia(f) => f.get_peer_address(peer_id).await,
            BroadcastSourceAddressGetter::Local(l) => l.get_peer_address(peer_id).await,
        }
    }
}

#[fuchsia::main(logging_tags=["bt-le-broadcast-assistant"])]
async fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = parse_args(&args).inspect_err(|e| println!("{e:?}"))?;

    let central_svc = fuchsia_component::client::connect_to_protocol::<CentralMarker>()
        .context("Failed to connect to BLE Central service")?;
    let central = bt_gatt_fuchsia::Central::new(central_svc);

    let local_cache_for_repl =
        if config.use_static_address { Some(LocalPeerAddrCache::new()) } else { None };

    let pii_getter_for_debug = if let Some(cache) = &local_cache_for_repl {
        BroadcastSourceAddressGetter::Local(cache.clone())
    } else {
        let addr_lookup_svc =
            fuchsia_component::client::connect_to_protocol::<AddressLookupMarker>()
                .context("Failed to connect to AddressLookup service")?;
        BroadcastSourceAddressGetter::Fuchsia(FuchsiaPeerAddr::new(addr_lookup_svc))
    };

    let debug = AssistantDebug::<FuchsiaTypes, _>::new(central, pii_getter_for_debug);

    // The REPL loop is now the main driver of the application.
    crate::repl::start_command_loop(debug, local_cache_for_repl).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_args_no_args() {
        let args: Vec<String> = vec![];
        let config = parse_args(&args).unwrap();
        assert!(!config.use_static_address);
    }

    #[test]
    fn test_parse_args_use_static_address() {
        let args: Vec<String> = vec!["--use-static-address".to_string()];
        let config = parse_args(&args).unwrap();
        assert!(config.use_static_address);
    }

    #[test]
    fn test_parse_args_unknown_arg() {
        let args: Vec<String> = vec!["--foo".to_string()];
        assert!(parse_args(&args).is_err());
    }
}
