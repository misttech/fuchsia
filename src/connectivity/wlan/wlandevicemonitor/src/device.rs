// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, format_err};
use fidl_fuchsia_wlan_device as fidl_wlan_dev;
use fidl_fuchsia_wlan_internal as fidl_internal;
use fidl_fuchsia_wlan_phy as fidl_wlan_phy;
use fidl_fuchsia_wlan_sme as fidl_wlan_sme;
use fuchsia_inspect_contrib::inspect_log;
use futures::channel::mpsc;
use futures::select;
use futures::stream::{FuturesUnordered, StreamExt};
use log::{error, info, warn};
use std::convert::Infallible;
use std::pin::pin;
use std::sync::Arc;

use crate::watchable_map::WatchableMap;
use crate::{device_watch, inspect};
use wlan_fidl_ext::{TryUnpack as _, WithName as _};

#[derive(Clone, Debug)]
pub enum PhyProxy {
    Old(fidl_wlan_dev::PhyProxy),
    New(fidl_wlan_phy::WlanPhyProxy),
}

#[expect(dead_code)]
#[derive(Debug)]
pub enum PhyEvent {
    OnCriticalError { reason_code: fidl_internal::CriticalErrorReason },
    OnCountryCodeChange { phy_country: [u8; 2] },
}

fn convert_reason_code(
    code: fidl_wlan_dev::CriticalErrorReason,
) -> fidl_internal::CriticalErrorReason {
    match code {
        fidl_wlan_dev::CriticalErrorReason::FwCrash => fidl_internal::CriticalErrorReason::FwCrash,
    }
}

impl From<fidl_wlan_dev::PhyEvent> for PhyEvent {
    fn from(event: fidl_wlan_dev::PhyEvent) -> Self {
        match event {
            fidl_wlan_dev::PhyEvent::OnCriticalError { reason_code } => {
                PhyEvent::OnCriticalError { reason_code: convert_reason_code(reason_code) }
            }
            fidl_wlan_dev::PhyEvent::OnCountryCodeChange { ind } => {
                let mut alpha2 = [0; 2];
                alpha2.copy_from_slice(&ind.alpha2);
                PhyEvent::OnCountryCodeChange { phy_country: alpha2 }
            }
        }
    }
}

impl TryFrom<fidl_wlan_phy::WlanPhyNotifyRequest> for PhyEvent {
    type Error = anyhow::Error;

    fn try_from(req: fidl_wlan_phy::WlanPhyNotifyRequest) -> Result<Self, Self::Error> {
        match req {
            fidl_wlan_phy::WlanPhyNotifyRequest::OnCriticalError {
                payload: fidl_wlan_phy::WlanPhyNotifyOnCriticalErrorRequest { reason_code, .. },
                responder,
            } => match reason_code {
                Some(reason_code) => {
                    let _ = responder.send(Ok(()));
                    Ok(PhyEvent::OnCriticalError { reason_code })
                }
                None => {
                    let _ = responder.send(Err(fidl_wlan_phy::WlanPhyNotifyError::InvalidArgs));
                    Err(format_err!("OnCriticalError request is missing reason_code"))
                }
            },
            fidl_wlan_phy::WlanPhyNotifyRequest::OnCountryCodeChange {
                payload: fidl_wlan_phy::WlanPhyNotifyOnCountryCodeChangeRequest { phy_country, .. },
                responder,
            } => match phy_country {
                Some(phy_country) => {
                    let _ = responder.send(Ok(()));
                    Ok(PhyEvent::OnCountryCodeChange { phy_country })
                }
                None => {
                    let _ = responder.send(Err(fidl_wlan_phy::WlanPhyNotifyError::InvalidArgs));
                    Err(format_err!("OnCountryCodeChange request is missing phy_country"))
                }
            },
            req => Err(format_err!("Unhandled WlanPhyNotifyRequest: {:?}", req)),
        }
    }
}

impl PhyProxy {
    pub async fn new(
        proxy: fidl_wlan_phy::WlanPhyProxy,
    ) -> Result<
        (Self, futures::stream::BoxStream<'static, Result<PhyEvent, anyhow::Error>>),
        anyhow::Error,
    > {
        let (client_end, request_stream) =
            fidl::endpoints::create_request_stream::<fidl_wlan_phy::WlanPhyNotifyMarker>();

        let req = fidl_wlan_phy::WlanPhyInitRequest {
            notify_client: Some(client_end),
            ..Default::default()
        };

        // Initialize the PHY with the notify client.
        proxy
            .init(req)
            .await
            .map_err(|e| anyhow::anyhow!("failed to initialize PHY: {:?}", e))?
            .map_err(|status| anyhow::anyhow!("PHY init failed with status: {}", status))?;

let event_stream = request_stream
            .filter_map(|r| async move {
                match r {
                    Ok(req) => match PhyEvent::try_from(req) {
                        Ok(converted) => Some(Ok(converted)),
                        Err(e) => {
                            warn!("Received unhandled or invalid event from new PHY: {:?}", e);
                            None
                        }
                    },
                    Err(e) => Some(Err(e.into())),
                }
            })
            .boxed();

        Ok((Self::New(proxy), event_stream))
    }

    pub fn old_event_stream(
        proxy: &fidl_wlan_dev::PhyProxy,
    ) -> futures::stream::BoxStream<'static, Result<PhyEvent, anyhow::Error>> {
        use futures::StreamExt as _;
        proxy
            .take_event_stream()
            .map(|r| r.map(PhyEvent::from).map_err(anyhow::Error::from))
            .boxed()
    }

    pub async fn get_country(&self) -> Result<Result<[u8; 2], zx::sys::zx_status_t>, fidl::Error> {
        match self {
            Self::Old(p) => p.get_country().await.map(|r| r.map(|c| c.alpha2)),
            Self::New(p) => p.get_country().await,
        }
    }

    pub async fn set_country(
        &self,
        country: &[u8; 2],
    ) -> Result<Result<(), zx::sys::zx_status_t>, fidl::Error> {
        match self {
            Self::Old(p) => {
                let req = fidl_wlan_dev::CountryCode { alpha2: *country };
                p.set_country(&req).await.map(
                    |status| {
                        if status == 0 { Ok(()) } else { Err(status) }
                    },
                )
            }
            Self::New(p) => p.set_country(country).await,
        }
    }

    pub async fn clear_country(&self) -> Result<Result<(), zx::sys::zx_status_t>, fidl::Error> {
        match self {
            Self::Old(p) => {
                p.clear_country().await.map(|status| if status == 0 { Ok(()) } else { Err(status) })
            }
            Self::New(p) => p.clear_country().await,
        }
    }

    pub async fn power_down(&self) -> Result<Result<(), zx::sys::zx_status_t>, fidl::Error> {
        match self {
            Self::Old(p) => p.power_down().await,
            Self::New(p) => p.power_down().await,
        }
    }

    pub async fn power_up(&self) -> Result<Result<(), zx::sys::zx_status_t>, fidl::Error> {
        match self {
            Self::Old(p) => p.power_up().await,
            Self::New(p) => p.power_up().await,
        }
    }

    pub async fn reset(&self) -> Result<Result<(), zx::sys::zx_status_t>, fidl::Error> {
        match self {
            Self::Old(p) => p.reset().await,
            Self::New(p) => p.reset().await,
        }
    }

    pub async fn get_power_state(
        &self,
    ) -> Result<
        Result<fidl_wlan_phy::WlanPhyGetPowerStateResponse, zx::sys::zx_status_t>,
        fidl::Error,
    > {
        match self {
            Self::Old(p) => p.get_power_state().await.map(|r| {
                r.map(|power_on| fidl_wlan_phy::WlanPhyGetPowerStateResponse {
                    power_on: Some(power_on),
                    ..Default::default()
                })
            }),
            Self::New(p) => p.get_power_state().await,
        }
    }

    pub async fn set_power_save_mode(
        &self,
        req: &fidl_wlan_phy::WlanPhySetPowerSaveModeRequest,
    ) -> Result<Result<(), zx::sys::zx_status_t>, fidl::Error> {
        match self {
            Self::Old(p) => {
                let ps_mode = match req.ps_mode.with_name("ps_mode").try_unpack() {
                    Ok(m) => m,
                    Err(_) => return Ok(Err(zx::sys::ZX_ERR_INVALID_ARGS)),
                };
                p.set_power_save_mode(ps_mode)
                    .await
                    .map(|status| if status == 0 { Ok(()) } else { Err(status) })
            }
            Self::New(p) => p.set_power_save_mode(req).await,
        }
    }

    pub async fn get_power_save_mode(
        &self,
    ) -> Result<
        Result<fidl_wlan_phy::WlanPhyGetPowerSaveModeResponse, zx::sys::zx_status_t>,
        fidl::Error,
    > {
        match self {
            Self::Old(p) => p.get_power_save_mode().await.map(|r| {
                r.map(|ps_mode| fidl_wlan_phy::WlanPhyGetPowerSaveModeResponse {
                    ps_mode: Some(ps_mode),
                    ..Default::default()
                })
            }),
            Self::New(p) => p.get_power_save_mode().await,
        }
    }

    pub async fn get_supported_mac_roles(
        &self,
    ) -> Result<
        Result<fidl_wlan_phy::WlanPhyGetSupportedMacRolesResponse, zx::sys::zx_status_t>,
        fidl::Error,
    > {
        match self {
            Self::Old(p) => p.get_supported_mac_roles().await.map(|r| {
                r.map(|supported_mac_roles| fidl_wlan_phy::WlanPhyGetSupportedMacRolesResponse {
                    supported_mac_roles: Some(supported_mac_roles),
                    ..Default::default()
                })
            }),
            Self::New(p) => p.get_supported_mac_roles().await,
        }
    }

    pub async fn set_bt_coexistence_mode(
        &self,
        req: &fidl_wlan_phy::WlanPhySetBtCoexistenceModeRequest,
    ) -> Result<Result<(), zx::sys::zx_status_t>, fidl::Error> {
        match self {
            Self::Old(p) => {
                let mode = match req.mode.with_name("mode").try_unpack() {
                    Ok(m) => m,
                    Err(_) => return Ok(Err(zx::sys::ZX_ERR_INVALID_ARGS)),
                };
                p.set_bt_coexistence_mode(mode).await
            }
            Self::New(p) => p.set_bt_coexistence_mode(req).await,
        }
    }

    pub async fn set_tx_power_scenario(
        &self,
        req: &fidl_wlan_phy::WlanPhySetTxPowerScenarioRequest,
    ) -> Result<Result<(), zx::sys::zx_status_t>, fidl::Error> {
        match self {
            Self::Old(p) => {
                let scenario = match req.scenario.with_name("scenario").try_unpack() {
                    Ok(s) => s,
                    Err(_) => return Ok(Err(zx::sys::ZX_ERR_INVALID_ARGS)),
                };
                p.set_tx_power_scenario(scenario).await
            }
            Self::New(p) => p.set_tx_power_scenario(req).await,
        }
    }

    pub async fn reset_tx_power_scenario(
        &self,
    ) -> Result<Result<(), zx::sys::zx_status_t>, fidl::Error> {
        match self {
            Self::Old(p) => p.reset_tx_power_scenario().await,
            Self::New(p) => p.reset_tx_power_scenario().await,
        }
    }

    pub async fn get_tx_power_scenario(
        &self,
    ) -> Result<
        Result<fidl_fuchsia_wlan_internal::TxPowerScenario, zx::sys::zx_status_t>,
        fidl::Error,
    > {
        match self {
            Self::Old(p) => p.get_tx_power_scenario().await,
            Self::New(p) => p.get_tx_power_scenario().await,
        }
    }

    pub async fn create_iface(
        &self,
        req: fidl_wlan_phy::WlanPhyCreateIfaceRequest,
    ) -> Result<Result<fidl_wlan_phy::WlanPhyCreateIfaceResponse, zx::sys::zx_status_t>, fidl::Error>
    {
        match self {
            Self::Old(p) => {
                let (role, mlme_channel) =
                    match (req.role.with_name("role"), req.mlme_channel.with_name("mlme_channel"))
                        .try_unpack()
                    {
                        Ok(val) => val,
                        Err(_) => return Ok(Err(zx::sys::ZX_ERR_INVALID_ARGS)),
                    };
                let init_sta_addr = req.init_sta_addr.unwrap_or([0; 6]);
                let old_req = fidl_wlan_dev::CreateIfaceRequest {
                    role,
                    mlme_channel: Some(mlme_channel),
                    init_sta_addr,
                };
                p.create_iface(old_req).await.map(|r| {
                    r.map(|iface_id| fidl_wlan_phy::WlanPhyCreateIfaceResponse {
                        iface_id: Some(iface_id),
                        ..Default::default()
                    })
                })
            }
            Self::New(p) => p.create_iface(req).await,
        }
    }

    pub async fn destroy_iface(
        &self,
        req: &fidl_wlan_phy::WlanPhyDestroyIfaceRequest,
    ) -> Result<Result<(), zx::sys::zx_status_t>, fidl::Error> {
        match self {
            Self::Old(p) => {
                let id = match req.iface_id.with_name("iface_id").try_unpack() {
                    Ok(i) => i,
                    Err(_) => return Ok(Err(zx::sys::ZX_ERR_INVALID_ARGS)),
                };
                let old_req = fidl_wlan_dev::DestroyIfaceRequest { id };
                p.destroy_iface(&old_req).await
            }
            Self::New(p) => p.destroy_iface(req).await,
        }
    }
}

/// Iface's PHY information.
#[derive(Debug, PartialEq, Clone)]
pub struct PhyOwnership {
    // Iface's global PHY ID.
    pub phy_id: u16,
    // Local ID assigned by this iface's PHY.
    pub phy_assigned_id: u16,
}

#[derive(Debug, Clone)]
pub struct NewIface {
    // Global, unique iface ID.
    pub id: u16,
    // Information about this iface's PHY.
    pub phy_ownership: PhyOwnership,
    // The handle for connecting channels to this iface's SME.
    pub generic_sme: fidl_wlan_sme::GenericSmeProxy,
}

pub struct PhyDevice {
    pub proxy: PhyProxy,
}

pub struct IfaceDevice {
    pub phy_ownership: PhyOwnership,
    pub generic_sme: fidl_wlan_sme::GenericSmeProxy,
}

pub type PhyMap = WatchableMap<u16, PhyDevice>;
pub type IfaceMap = WatchableMap<u16, IfaceDevice>;

/// Handles newly-discovered PHYs.
///
/// When new PHYs are discovered, the `device_watch` module produces a `NewPhyDevice`.  This struct
/// contains a PHY id and proxy.
pub async fn serve_phys(
    phys: Arc<PhyMap>,
    inspect_tree: Arc<inspect::WlanMonitorTree>,
    device_directory: &str,
    phy_event_sink: mpsc::Sender<(u16, PhyEvent)>,
) -> Result<Infallible, Error> {
    let new_phys = device_watch::watch_phy_devices(device_directory).await?.fuse();
    let mut new_phys = pin!(new_phys);
    let mut active_phys = FuturesUnordered::new();
    loop {
        select! {
            new_phy = new_phys.next() => match new_phy {
                None => return Err(format_err!("new phy stream unexpectedly finished")),
                Some(Err(e)) => return Err(format_err!("new phy stream returned an error: {}", e)),
                Some(Ok(new_phy)) => {
                    let fut = serve_phy(&phys, new_phy, inspect_tree.clone(), phy_event_sink.clone());
                    active_phys.push(fut);
                }
            },
            () = active_phys.select_next_some() => {},
        }
    }
}

/// Handles the lifetime of discovered PHY devices.
///
/// `serve_phy` takes newly discovered PHY devices and inserts them into a `WatchableMap`.
///
/// `serve_phy` then waits for the PHY device to be removed from the system.  When the PHY is
/// removed, `serve_phy` removes the device from the `WatchableMap`.
///
/// The `WatchableMap` produces events when elements are added to or removed from it.  These events
/// are consumed by another future that manages the `DeviceWatcher` protocol and notifies API
/// clients of PHY addition or removal.
async fn serve_phy(
    phys: &PhyMap,
    new_phy: device_watch::NewPhyDevice,
    inspect_tree: Arc<inspect::WlanMonitorTree>,
    mut phy_event_sink: mpsc::Sender<(u16, PhyEvent)>,
) {
    let msg = format!("new phy #{}", new_phy.id);
    info!("{}", msg);
    inspect_log!(inspect_tree.device_events.lock(), msg: msg);
    let id = new_phy.id;

    let mut event_stream = new_phy.event_stream;

    // Insert the newly discovered device into the `WatchableMap`.  This will trigger the watchable
    // map to produce an event so that the `DeviceWatcher` service can produce an update for API
    // consumers.
    phys.insert(id, PhyDevice { proxy: new_phy.proxy });

    let mut phy_stream_result = Ok(());
    while let Some(event) = event_stream.next().await {
        match event {
            Ok(event) => {
                info!("phy event: {:?}", event);
                if let Err(e) = phy_event_sink.try_send((id, event)) {
                    error!("failed to send phy event: {}", e);
                }
            }
            Err(e) => {
                error!("error in phy event stream: {}", e);
                phy_stream_result = Err(e);
                break;
            }
        }
    }

    // The event stream's end indicates that the PHY has been removed from the system.
    // Remove the PHY from the `WatchableMap`.  This will result in the `WatchableMap`
    // producing a removal event which will trigger the `DeviceWatcher` service to send a
    // notification to API consumers.
    phys.remove(&id);
    if let Err(e) = phy_stream_result {
        let msg = format!("error reading from FIDL channel of phy #{id}: {e}");
        error!("{}", msg);
        inspect_log!(inspect_tree.device_events.lock(), msg: msg);
    }
    info!("phy removed: #{}", id);
    inspect_log!(inspect_tree.device_events.lock(), msg: format!("phy removed: #{}", id));
}

#[derive(Clone, Copy)]
pub struct IfaceWrapper<'a> {
    pub(crate) ifaces: &'a IfaceMap,
    pub(crate) ifaces_tree: &'a inspect::IfacesTree,
}

impl<'a> IfaceWrapper<'a> {
    pub fn new(ifaces: &'a IfaceMap, ifaces_tree: &'a inspect::IfacesTree) -> Self {
        Self { ifaces, ifaces_tree }
    }

    pub fn insert(&self, id: u16, device: IfaceDevice, inspect_vmo: zx::Vmo) {
        self.ifaces.insert(id, device);
        self.ifaces_tree.add_iface(id, inspect_vmo);
    }

    pub fn remove(&self, id: u16, inspect_vmo: Option<zx::Vmo>) {
        let existed = self.ifaces.get(&id).is_some();
        self.ifaces.remove(&id);
        if existed {
            self.ifaces_tree.record_destroyed_iface(id, inspect_vmo);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watchable_map;
    use assert_matches::assert_matches;
    use fidl::endpoints::create_proxy;
    use fuchsia_async as fasync;
    use fuchsia_inspect::{Inspector, InspectorConfig};
    use futures::channel::mpsc;
    use futures::task::Poll;
    use wlan_common::test_utils::ExpectWithin;

    use test_case::test_case;

    #[derive(Debug, Clone, Copy)]
    enum TestPhyType {
        Old,
        New,
    }

    #[fuchsia::test]
    fn test_serve_phys_exits_when_watching_devices_fails() {
        let mut exec = fasync::TestExecutor::new();
        let (sender, _) = mpsc::channel(5);
        let (phys, _phy_events) = PhyMap::new();
        let phys = Arc::new(phys);
        let inspector = Inspector::new(InspectorConfig::default().size(inspect::VMO_SIZE_BYTES));
        let inspect_tree = Arc::new(inspect::WlanMonitorTree::new(inspector));
        let fut = serve_phys(phys.clone(), inspect_tree, "/wrong/path", sender);
        let mut fut = pin!(fut);

        assert_matches!(exec.run_singlethreaded(&mut fut), Err(_));
    }

    #[test_case(TestPhyType::Old)]
    #[test_case(TestPhyType::New)]
    fn test_serve_phy_adds_and_removes_phy(phy_type: TestPhyType) {
        let mut exec = fasync::TestExecutor::new();
        let (sender, _) = mpsc::channel(5);
        let (phys, mut phy_events) = PhyMap::new();
        let phys = Arc::new(phys);
        let inspector = Inspector::new(InspectorConfig::default().size(inspect::VMO_SIZE_BYTES));
        let inspect_tree = Arc::new(inspect::WlanMonitorTree::new(inspector));

        let (proxy, event_stream, _server) = match phy_type {
            TestPhyType::Old => {
                let (phy_proxy, phy_server) = create_proxy::<fidl_wlan_dev::PhyMarker>();
                let event_stream = PhyProxy::old_event_stream(&phy_proxy);
                (
                    PhyProxy::Old(phy_proxy),
                    event_stream,
                    Box::new(phy_server) as Box<dyn std::any::Any>,
                )
            }
            TestPhyType::New => {
                let (phy_proxy, phy_server) = create_proxy::<fidl_wlan_phy::WlanPhyMarker>();
                let mut stream = phy_server.into_stream();
                let mut new_fut = pin!(PhyProxy::new(phy_proxy));
                match exec.run_until_stalled(&mut new_fut) {
                    Poll::Pending => {}
                    _ => panic!("expected PhyProxy::new to be pending"),
                }
                let notify_client_holder = {
                    let mut stream_next = pin!(stream.next());
                    match exec.run_until_stalled(&mut stream_next) {
                        Poll::Ready(Some(Ok(fidl_wlan_phy::WlanPhyRequest::Init {
                            payload,
                            responder,
                        }))) => {
                            let _ = responder.send(Ok(()));
                            payload.notify_client
                        }
                        other => panic!("expected Init request, got {:?}", other),
                    }
                };
                let (proxy, event_stream) = match exec.run_until_stalled(&mut new_fut) {
                    Poll::Ready(Ok(val)) => val,
                    _ => panic!("expected PhyProxy::new to resolve"),
                };
                (
                    proxy,
                    event_stream,
                    Box::new((stream, notify_client_holder)) as Box<dyn std::any::Any>,
                )
            }
        };
        let new_phy = device_watch::NewPhyDevice { id: 0, proxy, event_stream };

        let fut = serve_phy(&phys, new_phy, inspect_tree, sender);
        let mut fut = pin!(fut);

        // Run the PHY service to pick up the new PHY.
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Pending);
        match exec.run_until_stalled(&mut pin!(phy_events.next().expect_within(
            zx::MonotonicDuration::from_seconds(60),
            "phy_watcher did not observe device addition",
        ))) {
            Poll::Ready(Some(event)) => match event {
                watchable_map::MapEvent::KeyInserted(key) => {
                    assert_eq!(key, 0)
                }
                _ => panic!("unexpected watcher event"),
            },
            Poll::Ready(None) => panic!("watcher events ended unexpectedly"),
            Poll::Pending => panic!("no pending watcher events"),
        }
        assert!(phys.get(&0).is_some());

        // Now drop the other end of the PHY and observe that the PHY is removed from the map.
        drop(_server);
        assert_matches!(exec.run_until_stalled(&mut fut), Poll::Ready(()));
        match exec.run_until_stalled(&mut pin!(phy_events.next().expect_within(
            zx::MonotonicDuration::from_seconds(60),
            "phy_watcher did not observe device removal",
        ))) {
            Poll::Ready(Some(event)) => match event {
                watchable_map::MapEvent::KeyRemoved(key) => {
                    assert_eq!(key, 0)
                }
                _ => panic!("unexpected watcher event"),
            },
            Poll::Ready(None) => panic!("watcher events ended unexpectedly"),
            Poll::Pending => panic!("no pending watcher events"),
        }
        assert!(phys.get(&0).is_none());
    }
}
