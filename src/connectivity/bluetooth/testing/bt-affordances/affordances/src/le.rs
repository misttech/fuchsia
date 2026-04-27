// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::proxies::Proxies;
use crate::sys;
use anyhow::anyhow;
use async_utils::hanging_get::client::HangingGetStream;
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_bluetooth::PeerId;
use fidl_fuchsia_bluetooth_le::{
    AdvertisedPeripheralMarker, AdvertisedPeripheralRequest, AdvertisingParameters,
    ConnectionMarker, ConnectionOptions,
};
use fidl_fuchsia_bluetooth_sys::Peer;
use fuchsia_async::{Task, TimeoutExt};
use fuchsia_sync::Mutex;
use futures::channel::mpsc;
use futures::{FutureExt, StreamExt, select};
use std::sync::Arc;

// Send peer update of those for which the address is known and return the list of those for
// which the address is not known.
pub(crate) fn send_peer_update(
    peer_cache: Arc<Mutex<Vec<Peer>>>,
    sender: &mpsc::UnboundedSender<
        Vec<(fidl_fuchsia_bluetooth_le::Peer, Option<fidl_fuchsia_bluetooth::Address>)>,
    >,
    updated: Vec<fidl_fuchsia_bluetooth_le::Peer>,
) -> Result<Vec<fidl_fuchsia_bluetooth_le::Peer>, anyhow::Error> {
    let mut send_list = vec![];
    let mut missing_addr = vec![];

    for peer in updated {
        match peer_cache
            .lock()
            .iter()
            .find(|&cached_peer| cached_peer.id.unwrap() == peer.id.unwrap())
        {
            Some(cached_peer) => send_list.push((peer, Some(cached_peer.address.unwrap()))),
            None => missing_addr.push(peer),
        };
    }

    if let Err(err) = sender.unbounded_send(send_list) {
        sender.close_channel();
        return Err(anyhow!("LE scan stream closed with status: {err}"));
    }

    Ok(missing_addr)
}

pub(crate) async fn start_le_scan(
    proxies: &mut Proxies,
    peer_cache: Arc<Mutex<Vec<Peer>>>,
) -> Result<
    mpsc::UnboundedReceiver<
        Vec<(fidl_fuchsia_bluetooth_le::Peer, Option<fidl_fuchsia_bluetooth::Address>)>,
    >,
    anyhow::Error,
> {
    // Enable Discovery as well to ascertain dual mode peers.
    let (_token, discovery_session_server) = fidl::endpoints::create_proxy();
    if let Err(err) = proxies.access_proxy.start_discovery(discovery_session_server).await? {
        return Err(anyhow!("fuchsia.bluetooth.sys.Access/StartDiscovery error: {err:?}"));
    }

    let (sender, receiver) = mpsc::unbounded();

    let (scan_client, scan_server) =
        fidl::endpoints::create_proxy::<fidl_fuchsia_bluetooth_le::ScanResultWatcherMarker>();
    let options = fidl_fuchsia_bluetooth_le::ScanOptions {
        // Empty filter matches all LE peripherals and broadcasters.
        filters: Some(vec![fidl_fuchsia_bluetooth_le::Filter { ..Default::default() }]),
        ..Default::default()
    };
    proxies.central_proxy.scan(&options, scan_server).await?;
    let mut scan_result_watcher_stream = HangingGetStream::new(
        scan_client,
        fidl_fuchsia_bluetooth_le::ScanResultWatcherProxy::watch,
    );
    let mut peers_waiting_for_addr: Vec<fidl_fuchsia_bluetooth_le::Peer> = vec![];

    *proxies.le_scan_task.lock() = Some(Task::spawn(async move {
        // Connect new Access proxy to avoid MultipleObservers error.
        let access_proxy = fuchsia_component::client::connect_to_protocol::<
            fidl_fuchsia_bluetooth_sys::AccessMarker,
        >()
        .expect("Failed to connect fuchsia.bluetooth.sys/Access");
        let mut peer_watcher_stream = HangingGetStream::new_with_fn_ptr(
            access_proxy,
            fidl_fuchsia_bluetooth_sys::AccessProxy::watch_peers,
        );
        loop {
            select! {
                scan_result = scan_result_watcher_stream.select_next_some() => {
                    match scan_result {
                        Ok(updated) => {
                            match send_peer_update(peer_cache.clone(), &sender, updated) {
                                Ok(waiting) => peers_waiting_for_addr = waiting,
                                Err(err) => {
                                    eprintln!("{err}");
                                    return;
                                }
                            }
                        }

                        Err(err) => {
                            eprintln!("LE scan encountered error: {err}");
                            sender.close_channel();
                            return;
                        }
                    }
                }

                peer_result = peer_watcher_stream.select_next_some() => {
                    match peer_result {
                        Ok((updated, removed)) => {
                            sys::update_peer_cache(peer_cache.clone(), updated, removed);

                            match send_peer_update(
                                peer_cache.clone(),
                                &sender,
                                peers_waiting_for_addr,
                            ) {
                                Ok(still_waiting) => peers_waiting_for_addr = still_waiting,
                                Err(err) => {
                                    eprintln!("{err}");
                                    return;
                                }
                            }
                        }

                        Err(err) => {
                            eprintln!("PeerWatcher stream returned error: {err}");
                            sender.close_channel();
                            return;
                        }
                    }
                }
            }
        }
    }));

    Ok(receiver)
}

pub(crate) fn stop_le_scan(proxies: &Proxies) -> bool {
    proxies.le_scan_task.lock().take().is_some()
}

pub(crate) async fn connect_le(
    proxies: &mut Proxies,
    peer_id: &PeerId,
) -> Result<(), anyhow::Error> {
    let (le_client, le_server) = fidl::endpoints::create_proxy::<ConnectionMarker>();
    proxies.central_proxy.connect(peer_id, &ConnectionOptions::default(), le_server)?;
    let (client_proxy, client_server_end) =
        fidl::endpoints::create_proxy::<fidl_fuchsia_bluetooth_gatt2::ClientMarker>();
    le_client.request_gatt_client(client_server_end)?;
    *proxies.central_connection.lock() = Some(le_client);
    proxies.gatt_client = Some(client_proxy);
    Ok(())
}

pub(crate) async fn advertise_peripheral(
    proxies: &Proxies,
    parameters: AdvertisingParameters,
    timeout: std::time::Duration,
) -> Result<Option<(PeerId, ClientEnd<ConnectionMarker>)>, anyhow::Error> {
    let (client, mut request_stream) =
        fidl::endpoints::create_request_stream::<AdvertisedPeripheralMarker>();

    select! {
        result = proxies.peripheral_proxy.advertise(&parameters, client) => {
            return Err(anyhow!("LE advertisement finished with result: {result:?}"));
        }
        request = request_stream.next().on_timeout(timeout, || None).fuse() => {
            match request {
                Some(Ok(AdvertisedPeripheralRequest::OnConnected {
                    peer,
                    connection,
                    responder,
                })) => {
                    let _ = responder.send();
                    return Ok(Some((peer.id.unwrap(), connection)));
                }
                Some(Err(e)) => {
                    return Err(anyhow!("Error in AdvertisedPeripheral stream: {e:?}"));
                }
                None => {
                    println!("Peripheral advertisement ended without connection");
                    return Ok(None);
                }
            }
        }
    }
}
