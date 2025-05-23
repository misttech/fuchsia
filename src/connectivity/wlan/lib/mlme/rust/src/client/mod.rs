// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod channel_switch;
mod convert_beacon;
mod lost_bss;
mod scanner;
mod state;
#[cfg(test)]
mod test_utils;

use crate::block_ack::BlockAckTx;
use crate::device::{self, DeviceOps};
use crate::disconnect::LocallyInitiated;
use crate::error::Error;
use crate::{akm_algorithm, ddk_converter};
use anyhow::format_err;
use channel_switch::ChannelState;
use fdf::{Arena, ArenaBox, ArenaStaticBox};
use ieee80211::{Bssid, MacAddr, MacAddrBytes, Ssid};
use log::{error, warn};
use scanner::Scanner;
use state::States;
use std::mem;
use std::ptr::NonNull;
use wlan_common::append::Append;
use wlan_common::bss::BssDescription;
use wlan_common::buffer_writer::BufferWriter;
use wlan_common::capabilities::{derive_join_capabilities, ClientCapabilities};
use wlan_common::channel::Channel;
use wlan_common::ie::rsn::rsne;
use wlan_common::ie::{self, Id};
use wlan_common::mac::{self, Aid, CapabilityInfo};
use wlan_common::sequence::SequenceManager;
use wlan_common::time::TimeUnit;
use wlan_common::timer::{EventHandle, Timer};
use wlan_common::{data_writer, mgmt_writer, wmm};
use wlan_frame_writer::{append_frame_to, write_frame, write_frame_with_fixed_slice};
use zerocopy::SplitByteSlice;
use {
    fidl_fuchsia_wlan_common as fidl_common, fidl_fuchsia_wlan_ieee80211 as fidl_ieee80211,
    fidl_fuchsia_wlan_minstrel as fidl_minstrel, fidl_fuchsia_wlan_mlme as fidl_mlme,
    fidl_fuchsia_wlan_softmac as fidl_softmac, fuchsia_trace as trace, wlan_trace as wtrace,
};

pub use scanner::ScanError;

#[derive(Debug, Clone, PartialEq)]
pub enum TimedEvent {
    /// Connecting to AP timed out.
    Connecting,
    /// Timeout for reassociating after a disassociation.
    Reassociating,
    /// Association status update includes checking for auto deauthentication due to beacon loss
    /// and report signal strength
    AssociationStatusCheck,
    /// The delay for a scheduled channel switch has elapsed.
    ChannelSwitch,
}

#[cfg(test)]
impl TimedEvent {
    fn class(&self) -> TimedEventClass {
        match self {
            Self::Connecting => TimedEventClass::Connecting,
            Self::Reassociating => TimedEventClass::Reassociating,
            Self::AssociationStatusCheck => TimedEventClass::AssociationStatusCheck,
            Self::ChannelSwitch => TimedEventClass::ChannelSwitch,
        }
    }
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum TimedEventClass {
    Connecting,
    Reassociating,
    AssociationStatusCheck,
    ChannelSwitch,
}

/// ClientConfig affects time duration used for different timeouts.
/// Originally added to more easily control behavior in tests.
#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct ClientConfig {
    pub ensure_on_channel_time: zx::sys::zx_duration_t,
}

pub struct Context<D> {
    _config: ClientConfig,
    device: D,
    timer: Timer<TimedEvent>,
    seq_mgr: SequenceManager,
}

pub struct ClientMlme<D> {
    sta: Option<Client>,
    ctx: Context<D>,
    scanner: Scanner,
    channel_state: ChannelState,
}

impl<D: DeviceOps> crate::MlmeImpl for ClientMlme<D> {
    type Config = ClientConfig;
    type Device = D;
    type TimerEvent = TimedEvent;
    async fn new(
        config: Self::Config,
        device: Self::Device,
        timer: Timer<TimedEvent>,
    ) -> Result<Self, anyhow::Error> {
        Self::new(config, device, timer).await.map_err(From::from)
    }
    async fn handle_mlme_request(
        &mut self,
        req: wlan_sme::MlmeRequest,
    ) -> Result<(), anyhow::Error> {
        Self::handle_mlme_req(self, req).await.map_err(From::from)
    }
    async fn handle_mac_frame_rx(
        &mut self,
        bytes: &[u8],
        rx_info: fidl_softmac::WlanRxInfo,
        async_id: trace::Id,
    ) {
        wtrace::duration!(c"ClientMlme::handle_mac_frame_rx");
        Self::on_mac_frame_rx(self, bytes, rx_info, async_id).await
    }
    fn handle_eth_frame_tx(
        &mut self,
        bytes: &[u8],
        async_id: trace::Id,
    ) -> Result<(), anyhow::Error> {
        wtrace::duration!(c"ClientMlme::handle_eth_frame_tx");
        Self::on_eth_frame_tx(self, bytes, async_id).map_err(From::from)
    }
    async fn handle_scan_complete(&mut self, status: zx::Status, scan_id: u64) {
        Self::handle_scan_complete(self, status, scan_id).await;
    }
    async fn handle_timeout(&mut self, event: TimedEvent) {
        Self::handle_timed_event(self, event).await
    }
    fn access_device(&mut self) -> &mut Self::Device {
        &mut self.ctx.device
    }
}

impl<D> ClientMlme<D> {
    pub fn seq_mgr(&mut self) -> &mut SequenceManager {
        &mut self.ctx.seq_mgr
    }

    fn on_sme_get_iface_stats(
        &self,
        responder: wlan_sme::responder::Responder<fidl_mlme::GetIfaceStatsResponse>,
    ) -> Result<(), Error> {
        // TODO(https://fxbug.dev/42119762): Implement stats
        let resp = fidl_mlme::GetIfaceStatsResponse::ErrorStatus(zx::sys::ZX_ERR_NOT_SUPPORTED);
        responder.respond(resp);
        Ok(())
    }

    fn on_sme_get_iface_histogram_stats(
        &self,
        responder: wlan_sme::responder::Responder<fidl_mlme::GetIfaceHistogramStatsResponse>,
    ) -> Result<(), Error> {
        // TODO(https://fxbug.dev/42119762): Implement stats
        let resp =
            fidl_mlme::GetIfaceHistogramStatsResponse::ErrorStatus(zx::sys::ZX_ERR_NOT_SUPPORTED);
        responder.respond(resp);
        Ok(())
    }

    fn on_sme_list_minstrel_peers(
        &self,
        responder: wlan_sme::responder::Responder<fidl_mlme::MinstrelListResponse>,
    ) -> Result<(), Error> {
        // TODO(https://fxbug.dev/42159791): Implement once Minstrel is in Rust.
        error!("ListMinstrelPeers is not supported.");
        let peers = fidl_minstrel::Peers { addrs: vec![] };
        let resp = fidl_mlme::MinstrelListResponse { peers };
        responder.respond(resp);
        Ok(())
    }

    fn on_sme_get_minstrel_stats(
        &self,
        responder: wlan_sme::responder::Responder<fidl_mlme::MinstrelStatsResponse>,
        _addr: &MacAddr,
    ) -> Result<(), Error> {
        // TODO(https://fxbug.dev/42159791): Implement once Minstrel is in Rust.
        error!("GetMinstrelStats is not supported.");
        let resp = fidl_mlme::MinstrelStatsResponse { peer: None };
        responder.respond(resp);
        Ok(())
    }
}

impl<D: DeviceOps> ClientMlme<D> {
    pub async fn new(
        config: ClientConfig,
        mut device: D,
        timer: Timer<TimedEvent>,
    ) -> Result<Self, Error> {
        let iface_mac = device::try_query_iface_mac(&mut device).await?;
        Ok(Self {
            sta: None,
            ctx: Context { _config: config, device, timer, seq_mgr: SequenceManager::new() },
            scanner: Scanner::new(iface_mac.into()),
            channel_state: Default::default(),
        })
    }

    pub async fn set_main_channel(
        &mut self,
        channel: fidl_common::WlanChannel,
    ) -> Result<(), zx::Status> {
        self.channel_state.bind(&mut self.ctx, &mut self.scanner).set_main_channel(channel).await
    }

    pub async fn on_mac_frame_rx(
        &mut self,
        frame: &[u8],
        rx_info: fidl_softmac::WlanRxInfo,
        async_id: trace::Id,
    ) {
        wtrace::duration!(c"ClientMlme::on_mac_frame_rx");
        // TODO(https://fxbug.dev/42120906): Send the entire frame to scanner.
        if let Some(mgmt_frame) = mac::MgmtFrame::parse(frame, false) {
            let bssid = Bssid::from(mgmt_frame.mgmt_hdr.addr3);
            match mgmt_frame.try_into_mgmt_body().1 {
                Some(mac::MgmtBody::Beacon { bcn_hdr, elements }) => {
                    wtrace::duration!(c"MgmtBody::Beacon");
                    self.scanner.bind(&mut self.ctx).handle_ap_advertisement(
                        bssid,
                        bcn_hdr.beacon_interval,
                        bcn_hdr.capabilities,
                        elements,
                        rx_info.clone(),
                    );
                }
                Some(mac::MgmtBody::ProbeResp { probe_resp_hdr, elements }) => {
                    wtrace::duration!(c"MgmtBody::ProbeResp");
                    self.scanner.bind(&mut self.ctx).handle_ap_advertisement(
                        bssid,
                        probe_resp_hdr.beacon_interval,
                        probe_resp_hdr.capabilities,
                        elements,
                        rx_info.clone(),
                    )
                }
                _ => (),
            }
        }

        if let Some(sta) = self.sta.as_mut() {
            // Only pass the frame to a BoundClient under the following conditions:
            //   - ChannelState currently has a main channel.
            //   - ClientMlme received the frame on the main channel.
            match self.channel_state.get_main_channel() {
                Some(main_channel) if main_channel.primary == rx_info.channel.primary => {
                    sta.bind(&mut self.ctx, &mut self.scanner, &mut self.channel_state)
                        .on_mac_frame(frame, rx_info, async_id)
                        .await
                }
                Some(_) => {
                    wtrace::async_end_wlansoftmac_rx(async_id, "off main channel");
                }
                // TODO(https://fxbug.dev/42075118): This is only reachable because the Client state machine
                // returns to the Joined state and clears the main channel upon deauthentication.
                None => {
                    error!(
                        "Received MAC frame on channel {:?} while main channel is not set.",
                        rx_info.channel
                    );
                    wtrace::async_end_wlansoftmac_rx(async_id, "main channel not set");
                }
            }
        } else {
            wtrace::async_end_wlansoftmac_rx(async_id, "no bound client");
        }
    }

    pub async fn handle_mlme_req(&mut self, req: wlan_sme::MlmeRequest) -> Result<(), Error> {
        use wlan_sme::MlmeRequest as Req;

        match req {
            // Handle non station specific MLME messages first (Join, Scan, etc.)
            Req::Scan(req) => Ok(self.on_sme_scan(req).await),
            Req::Connect(req) => self.on_sme_connect(req).await,
            Req::GetIfaceStats(responder) => self.on_sme_get_iface_stats(responder),
            Req::GetIfaceHistogramStats(responder) => {
                self.on_sme_get_iface_histogram_stats(responder)
            }
            Req::QueryDeviceInfo(responder) => self.on_sme_query_device_info(responder).await,
            Req::QueryMacSublayerSupport(responder) => {
                self.on_sme_query_mac_sublayer_support(responder).await
            }
            Req::QuerySecuritySupport(responder) => {
                self.on_sme_query_security_support(responder).await
            }
            Req::QuerySpectrumManagementSupport(responder) => {
                self.on_sme_query_spectrum_management_support(responder).await
            }
            Req::ListMinstrelPeers(responder) => self.on_sme_list_minstrel_peers(responder),
            Req::GetMinstrelStats(req, responder) => {
                self.on_sme_get_minstrel_stats(responder, &req.peer_addr.into())
            }
            other_message => match &mut self.sta {
                None => {
                    if let Req::Reconnect(req) = other_message {
                        self.ctx.device.send_mlme_event(fidl_mlme::MlmeEvent::ConnectConf {
                            resp: fidl_mlme::ConnectConfirm {
                                peer_sta_address: req.peer_sta_address,
                                result_code: fidl_ieee80211::StatusCode::DeniedNoAssociationExists,
                                association_id: 0,
                                association_ies: vec![],
                            },
                        })?;
                    }
                    Err(Error::Status(
                        format!(
                            "Failed to handle {} MLME request when this ClientMlme has no sta.",
                            other_message.name()
                        ),
                        zx::Status::BAD_STATE,
                    ))
                }
                Some(sta) => Ok(sta
                    .bind(&mut self.ctx, &mut self.scanner, &mut self.channel_state)
                    .handle_mlme_req(other_message)
                    .await),
            },
        }
    }

    async fn on_sme_scan(&mut self, req: fidl_mlme::ScanRequest) {
        let txn_id = req.txn_id;
        let _ = self.scanner.bind(&mut self.ctx).on_sme_scan(req).await.map_err(|e| {
            error!("Scan failed in MLME: {:?}", e);
            let code = match e {
                Error::ScanError(scan_error) => scan_error.into(),
                _ => fidl_mlme::ScanResultCode::InternalError,
            };
            self.ctx
                .device
                .send_mlme_event(fidl_mlme::MlmeEvent::OnScanEnd {
                    end: fidl_mlme::ScanEnd { txn_id, code },
                })
                .unwrap_or_else(|e| error!("error sending MLME ScanEnd: {}", e));
        });
    }

    pub async fn handle_scan_complete(&mut self, status: zx::Status, scan_id: u64) {
        self.scanner.bind(&mut self.ctx).handle_scan_complete(status, scan_id).await;
    }

    async fn on_sme_connect(&mut self, req: fidl_mlme::ConnectRequest) -> Result<(), Error> {
        // Cancel any ongoing scan so that it doesn't conflict with the connect request
        // TODO(b/254290448): Use enable/disable scanning for better guarantees.
        if let Err(e) = self.scanner.bind(&mut self.ctx).cancel_ongoing_scan().await {
            warn!("Failed to cancel ongoing scan before connect: {}.", e);
        }

        let bssid = req.selected_bss.bssid;
        let result = match req.selected_bss.try_into() {
            Ok(bss) => {
                let req = ParsedConnectRequest {
                    selected_bss: bss,
                    connect_failure_timeout: req.connect_failure_timeout,
                    auth_type: req.auth_type,
                    sae_password: req.sae_password,
                    wep_key: req.wep_key.map(|k| *k),
                    security_ie: req.security_ie,
                };
                self.join_device(&req.selected_bss).await.map(|cap| (req, cap))
            }
            Err(e) => Err(Error::Status(
                format!("Error parsing BssDescription: {:?}", e),
                zx::Status::IO_INVALID,
            )),
        };

        match result {
            Ok((req, client_capabilities)) => {
                self.sta.replace(Client::new(
                    req,
                    device::try_query_iface_mac(&mut self.ctx.device).await?,
                    client_capabilities,
                ));
                if let Some(sta) = &mut self.sta {
                    sta.bind(&mut self.ctx, &mut self.scanner, &mut self.channel_state)
                        .start_connecting()
                        .await;
                }
                Ok(())
            }
            Err(e) => {
                error!("Error setting up device for join: {}", e);
                // TODO(https://fxbug.dev/42120718): Only one failure code defined in IEEE 802.11-2016 6.3.4.3
                // Can we do better?
                self.ctx.device.send_mlme_event(fidl_mlme::MlmeEvent::ConnectConf {
                    resp: fidl_mlme::ConnectConfirm {
                        peer_sta_address: bssid,
                        result_code: fidl_ieee80211::StatusCode::JoinFailure,
                        association_id: 0,
                        association_ies: vec![],
                    },
                })?;
                Err(e)
            }
        }
    }

    async fn join_device(&mut self, bss: &BssDescription) -> Result<ClientCapabilities, Error> {
        let info = ddk_converter::mlme_device_info_from_softmac(
            device::try_query(&mut self.ctx.device).await?,
        )?;
        let join_caps = derive_join_capabilities(Channel::from(bss.channel), bss.rates(), &info)
            .map_err(|e| {
                Error::Status(
                    format!("Failed to derive join capabilities: {:?}", e),
                    zx::Status::NOT_SUPPORTED,
                )
            })?;

        self.set_main_channel(bss.channel.into())
            .await
            .map_err(|status| Error::Status(format!("Error setting device channel"), status))?;

        let join_bss_request = fidl_common::JoinBssRequest {
            bssid: Some(bss.bssid.to_array()),
            bss_type: Some(fidl_common::BssType::Infrastructure),
            remote: Some(true),
            beacon_period: Some(bss.beacon_period),
            ..Default::default()
        };

        // Configure driver to pass frames from this BSS to MLME. Otherwise they will be dropped.
        self.ctx
            .device
            .join_bss(&join_bss_request)
            .await
            .map(|()| join_caps)
            .map_err(|status| Error::Status(format!("Error setting BSS in driver"), status))
    }

    async fn on_sme_query_device_info(
        &mut self,
        responder: wlan_sme::responder::Responder<fidl_mlme::DeviceInfo>,
    ) -> Result<(), Error> {
        let info = ddk_converter::mlme_device_info_from_softmac(
            device::try_query(&mut self.ctx.device).await?,
        )?;
        responder.respond(info);
        Ok(())
    }

    async fn on_sme_query_mac_sublayer_support(
        &mut self,
        responder: wlan_sme::responder::Responder<fidl_common::MacSublayerSupport>,
    ) -> Result<(), Error> {
        let support = device::try_query_mac_sublayer_support(&mut self.ctx.device).await?;
        responder.respond(support);
        Ok(())
    }

    async fn on_sme_query_security_support(
        &mut self,
        responder: wlan_sme::responder::Responder<fidl_common::SecuritySupport>,
    ) -> Result<(), Error> {
        let support = device::try_query_security_support(&mut self.ctx.device).await?;
        responder.respond(support);
        Ok(())
    }

    async fn on_sme_query_spectrum_management_support(
        &mut self,
        responder: wlan_sme::responder::Responder<fidl_common::SpectrumManagementSupport>,
    ) -> Result<(), Error> {
        let support = device::try_query_spectrum_management_support(&mut self.ctx.device).await?;
        responder.respond(support);
        Ok(())
    }

    pub fn on_eth_frame_tx<B: SplitByteSlice>(
        &mut self,
        bytes: B,
        async_id: trace::Id,
    ) -> Result<(), Error> {
        wtrace::duration!(c"ClientMlme::on_eth_frame_tx");
        match self.sta.as_mut() {
            None => Err(Error::Status(
                format!("Ethernet frame dropped (Client does not exist)."),
                zx::Status::BAD_STATE,
            )),
            Some(sta) => sta
                .bind(&mut self.ctx, &mut self.scanner, &mut self.channel_state)
                .on_eth_frame_tx(bytes, async_id),
        }
    }

    /// Called when a previously scheduled `TimedEvent` fired.
    /// Return true if auto-deauth has triggered. Return false otherwise.
    pub async fn handle_timed_event(&mut self, event: TimedEvent) {
        if let Some(sta) = self.sta.as_mut() {
            return sta
                .bind(&mut self.ctx, &mut self.scanner, &mut self.channel_state)
                .handle_timed_event(event)
                .await;
        }
    }
}

/// A STA running in Client mode.
/// The Client STA is in its early development process and does not yet manage its internal state
/// machine or track negotiated capabilities.
pub struct Client {
    state: Option<States>,
    pub connect_req: ParsedConnectRequest,
    pub iface_mac: MacAddr,
    pub client_capabilities: ClientCapabilities,
    pub connect_timeout: Option<EventHandle>,
}

impl Client {
    pub fn new(
        connect_req: ParsedConnectRequest,
        iface_mac: MacAddr,
        client_capabilities: ClientCapabilities,
    ) -> Self {
        Self {
            state: Some(States::new_initial()),
            connect_req,
            iface_mac,
            client_capabilities,
            connect_timeout: None,
        }
    }

    pub fn ssid(&self) -> &Ssid {
        &self.connect_req.selected_bss.ssid
    }

    pub fn bssid(&self) -> Bssid {
        self.connect_req.selected_bss.bssid
    }

    pub fn beacon_period(&self) -> zx::MonotonicDuration {
        zx::MonotonicDuration::from(TimeUnit(self.connect_req.selected_bss.beacon_period))
    }

    pub fn eapol_required(&self) -> bool {
        self.connect_req.selected_bss.rsne().is_some()
        // TODO(https://fxbug.dev/42139266): Add detection of WPA1 in softmac for testing
        // purposes only. In particular, connect-to-wpa1-network relies
        // on this half of the OR statement.
            || self.connect_req.selected_bss.find_wpa_ie().is_some()
    }

    pub fn bind<'a, D>(
        &'a mut self,
        ctx: &'a mut Context<D>,
        scanner: &'a mut Scanner,
        channel_state: &'a mut ChannelState,
    ) -> BoundClient<'a, D> {
        BoundClient { sta: self, ctx, scanner, channel_state }
    }

    /// Only management and data frames should be processed. Furthermore, the source address should
    /// be the BSSID the client associated to and the receiver address should either be non-unicast
    /// or the client's MAC address.
    fn should_handle_frame<B: SplitByteSlice>(&self, mac_frame: &mac::MacFrame<B>) -> bool {
        wtrace::duration!(c"Client::should_handle_frame");

        // Technically, |transmitter_addr| and |receiver_addr| would be more accurate but using src
        // src and dst to be consistent with |data_dst_addr()|.
        let (src_addr, dst_addr) = match mac_frame {
            mac::MacFrame::Mgmt(mac::MgmtFrame { mgmt_hdr, .. }) => {
                (Some(mgmt_hdr.addr3), mgmt_hdr.addr1)
            }
            mac::MacFrame::Data(mac::DataFrame { fixed_fields, .. }) => {
                (mac::data_bssid(&fixed_fields), mac::data_dst_addr(&fixed_fields))
            }
            // Control frames are not supported. Drop them.
            _ => return false,
        };
        src_addr.is_some_and(|src_addr| src_addr == self.bssid().into())
            && (!dst_addr.is_unicast() || dst_addr == self.iface_mac)
    }
}

pub struct BoundClient<'a, D> {
    sta: &'a mut Client,
    // TODO(https://fxbug.dev/42120453): pull everything out of Context and plop them here.
    ctx: &'a mut Context<D>,
    scanner: &'a mut Scanner,
    channel_state: &'a mut ChannelState,
}

impl<'a, D: DeviceOps> akm_algorithm::AkmAction for BoundClient<'a, D> {
    fn send_auth_frame(
        &mut self,
        auth_type: mac::AuthAlgorithmNumber,
        seq_num: u16,
        result_code: mac::StatusCode,
        auth_content: &[u8],
    ) -> Result<(), anyhow::Error> {
        self.send_auth_frame(auth_type, seq_num, result_code, auth_content).map_err(|e| e.into())
    }

    fn forward_sme_sae_rx(
        &mut self,
        seq_num: u16,
        status_code: fidl_ieee80211::StatusCode,
        sae_fields: Vec<u8>,
    ) {
        self.forward_sae_frame_rx(seq_num, status_code, sae_fields)
    }

    fn forward_sae_handshake_ind(&mut self) {
        self.forward_sae_handshake_ind()
    }
}

impl<'a, D: DeviceOps> BoundClient<'a, D> {
    /// Delivers a single MSDU to the STA's underlying device. The MSDU is delivered as an
    /// Ethernet II frame.
    /// Returns Err(_) if writing or delivering the Ethernet II frame failed.
    fn deliver_msdu<B: SplitByteSlice>(&mut self, msdu: mac::Msdu<B>) -> Result<(), Error> {
        let mac::Msdu { dst_addr, src_addr, llc_frame } = msdu;

        let mut packet = [0u8; mac::MAX_ETH_FRAME_LEN];
        let (frame_start, frame_end) = write_frame_with_fixed_slice!(&mut packet[..], {
            headers: {
                mac::EthernetIIHdr: &mac::EthernetIIHdr {
                    da: dst_addr,
                    sa: src_addr,
                    ether_type: llc_frame.hdr.protocol_id,
                },
            },
            payload: &llc_frame.body,
        })?;
        self.ctx
            .device
            .deliver_eth_frame(&packet[frame_start..frame_end])
            .map_err(|s| Error::Status(format!("could not deliver Ethernet II frame"), s))
    }

    pub fn send_auth_frame(
        &mut self,
        auth_type: mac::AuthAlgorithmNumber,
        seq_num: u16,
        result_code: mac::StatusCode,
        auth_content: &[u8],
    ) -> Result<(), Error> {
        let buffer = write_frame!({
            headers: {
                mac::MgmtHdr: &mgmt_writer::mgmt_hdr_to_ap(
                    mac::FrameControl(0)
                        .with_frame_type(mac::FrameType::MGMT)
                        .with_mgmt_subtype(mac::MgmtSubtype::AUTH),
                    self.sta.bssid(),
                    self.sta.iface_mac,
                    mac::SequenceControl(0)
                        .with_seq_num(self.ctx.seq_mgr.next_sns1(&self.sta.bssid().into()) as u16)
                ),
                mac::AuthHdr: &mac::AuthHdr {
                    auth_alg_num: auth_type,
                    auth_txn_seq_num: seq_num,
                    status_code: result_code,
                },
            },
            body: auth_content,
        })?;
        self.send_mgmt_or_ctrl_frame(buffer)
            .map_err(|s| Error::Status(format!("error sending open auth frame"), s))
    }

    /// Sends an authentication frame using Open System authentication.
    pub fn send_open_auth_frame(&mut self) -> Result<(), Error> {
        self.send_auth_frame(
            mac::AuthAlgorithmNumber::OPEN,
            1,
            fidl_ieee80211::StatusCode::Success.into(),
            &[],
        )
    }

    /// Sends an association request frame based on device capability.
    pub fn send_assoc_req_frame(&mut self) -> Result<(), Error> {
        let ssid = self.sta.ssid().clone();
        let cap = &self.sta.client_capabilities.0;
        let capability_info = cap.capability_info.0;
        let rates: Vec<u8> = cap.rates.iter().map(|r| r.rate()).collect();
        let ht_cap = cap.ht_cap;
        let vht_cap = cap.vht_cap;
        let security_ie = self.sta.connect_req.security_ie.clone();

        let rsne = (!security_ie.is_empty() && security_ie[0] == ie::Id::RSNE.0)
            .then(|| match rsne::from_bytes(&security_ie[..]) {
                Ok((_, x)) => Ok(x),
                Err(e) => Err(format_err!("error parsing rsne {:?} : {:?}", security_ie, e)),
            })
            .transpose()?;
        let buffer = write_frame!({
            headers: {
                mac::MgmtHdr: &mgmt_writer::mgmt_hdr_to_ap(
                    mac::FrameControl(0)
                        .with_frame_type(mac::FrameType::MGMT)
                        .with_mgmt_subtype(mac::MgmtSubtype::ASSOC_REQ),
                    self.sta.bssid(),
                    self.sta.iface_mac,
                    mac::SequenceControl(0)
                        .with_seq_num(self.ctx.seq_mgr.next_sns1(&self.sta.bssid().into()) as u16)
                ),
                mac::AssocReqHdr: &mac::AssocReqHdr {
                    capabilities: mac::CapabilityInfo(capability_info),
                    listen_interval: 0,
                },
            },
            ies: {
                ssid: ssid,
                supported_rates: rates,
                extended_supported_rates: {/* continue rates */},
                rsne?: rsne,
                ht_cap?: ht_cap,
                vht_cap?: vht_cap,
            },
        })?;
        self.send_mgmt_or_ctrl_frame(buffer)
            .map_err(|s| Error::Status(format!("error sending assoc req frame"), s))
    }

    /// Sends a "keep alive" response to the BSS. A keep alive response is a NULL data frame sent as
    /// a response to the AP transmitting NULL data frames to the client.
    // Note: This function was introduced to meet C++ MLME feature parity. However, there needs to
    // be some investigation, whether these "keep alive" frames are the right way of keeping a
    // client associated to legacy APs.
    fn send_keep_alive_resp_frame(&mut self) -> Result<(), Error> {
        let buffer = write_frame!({
            headers: {
                mac::FixedDataHdrFields: &data_writer::data_hdr_client_to_ap(
                    mac::FrameControl(0)
                        .with_frame_type(mac::FrameType::DATA)
                        .with_data_subtype(mac::DataSubtype(0).with_null(true)),
                    self.sta.bssid(),
                    self.sta.iface_mac,
                    mac::SequenceControl(0)
                        .with_seq_num(self.ctx.seq_mgr.next_sns1(&self.sta.bssid().into()) as u16)
                ),
            },
        })?;
        self.ctx
            .device
            .send_wlan_frame(buffer, fidl_softmac::WlanTxInfoFlags::empty(), None)
            .map_err(|s| Error::Status(format!("error sending keep alive frame"), s))
    }

    pub fn send_deauth_frame(&mut self, reason_code: mac::ReasonCode) -> Result<(), Error> {
        let buffer = write_frame!({
            headers: {
                mac::MgmtHdr: &mgmt_writer::mgmt_hdr_to_ap(
                    mac::FrameControl(0)
                        .with_frame_type(mac::FrameType::MGMT)
                        .with_mgmt_subtype(mac::MgmtSubtype::DEAUTH),
                    self.sta.bssid(),
                    self.sta.iface_mac,
                    mac::SequenceControl(0)
                        .with_seq_num(self.ctx.seq_mgr.next_sns1(&self.sta.bssid().into()) as u16)
                ),
                mac::DeauthHdr: &mac::DeauthHdr {
                    reason_code,
                },
            },
        })?;
        let result = self
            .send_mgmt_or_ctrl_frame(buffer)
            .map_err(|s| Error::Status(format!("error sending deauthenticate frame"), s));
        // Clear main_channel since there is no "main channel" after deauthenticating
        self.channel_state.bind(&mut self.ctx, &mut self.scanner).clear_main_channel();

        result
    }

    /// Sends the given |payload| as a data frame over the air. If the caller does not pass an |async_id| to
    /// this function, then this function will generate its own |async_id| and end the trace if an error
    /// occurs.
    pub fn send_data_frame(
        &mut self,
        src: MacAddr,
        dst: MacAddr,
        is_protected: bool,
        qos_ctrl: bool,
        ether_type: u16,
        payload: &[u8],
        async_id: Option<trace::Id>,
    ) -> Result<(), Error> {
        let async_id_provided = async_id.is_some();
        let async_id = async_id.unwrap_or_else(|| {
            let async_id = trace::Id::new();
            wtrace::async_begin_wlansoftmac_tx(async_id, "mlme");
            async_id
        });
        wtrace::duration!(c"BoundClient::send_data_frame");

        let qos_ctrl = if qos_ctrl {
            Some(
                wmm::derive_tid(ether_type, payload)
                    .map_or(mac::QosControl(0), |tid| mac::QosControl(0).with_tid(tid as u16)),
            )
        } else {
            None
        };

        // IEEE Std 802.11-2016, Table 9-26 specifies address field contents and their relation
        // to the addr fields.
        // TODO(https://fxbug.dev/42128470): Support A-MSDU address field contents.

        // We do not currently support RA other than the BSS.
        // TODO(https://fxbug.dev/42122401): Support to_ds = false and alternative RA for TDLS.
        let to_ds = true;
        let from_ds = src != self.sta.iface_mac;
        // Detect when SA != TA, in which case we use addr4.
        let addr1 = self.sta.bssid().into();
        let addr2 = self.sta.iface_mac;
        let addr3 = match (to_ds, from_ds) {
            (false, false) => self.sta.bssid().into(),
            (false, true) => src,
            (true, _) => dst,
        };
        let addr4 = if from_ds && to_ds { Some(src) } else { None };

        let tx_flags = match ether_type {
            mac::ETHER_TYPE_EAPOL => fidl_softmac::WlanTxInfoFlags::FAVOR_RELIABILITY,
            _ => fidl_softmac::WlanTxInfoFlags::empty(),
        };

        // TODO(https://fxbug.dev/353987692): Replace `header_room` with actual amount of space
        // for the header in a network device buffer. The MAX_HEADER_SIZE is arbitrarily extended
        // to emulate the extra room the network device buffer will likely always provide.
        const MAX_HEADER_SIZE: usize = mem::size_of::<mac::FixedDataHdrFields>()
            + mem::size_of::<MacAddr>()
            + mem::size_of::<mac::QosControl>()
            + mem::size_of::<mac::LlcHdr>();
        let header_room = MAX_HEADER_SIZE + 100;
        let arena = Arena::new();
        let mut buffer = arena.insert_default_slice(header_room + payload.len());

        // TODO(https://fxbug.dev/353987692): Remove this clone once we migrate to network device where
        // the buffer can be reused.
        let payload_start = buffer.len() - payload.len();
        buffer[payload_start..].clone_from_slice(&payload[..]);

        let (frame_start, _frame_end) =
            write_frame_with_fixed_slice!(&mut buffer[..payload_start], {
                fill_zeroes: (),
                headers: {
                    mac::FixedDataHdrFields: &mac::FixedDataHdrFields {
                        frame_ctrl: mac::FrameControl(0)
                            .with_frame_type(mac::FrameType::DATA)
                            .with_data_subtype(mac::DataSubtype(0).with_qos(qos_ctrl.is_some()))
                            .with_protected(is_protected)
                            .with_to_ds(to_ds)
                            .with_from_ds(from_ds),
                        duration: 0,
                        addr1,
                        addr2,
                        addr3,
                        seq_ctrl: mac::SequenceControl(0).with_seq_num(
                            match qos_ctrl.as_ref() {
                                None => self.ctx.seq_mgr.next_sns1(&dst),
                                Some(qos_ctrl) => self.ctx.seq_mgr.next_sns2(&dst, qos_ctrl.tid()),
                            } as u16
                        )
                    },
                    mac::Addr4?: addr4,
                    mac::QosControl?: qos_ctrl,
                    mac::LlcHdr: &data_writer::make_snap_llc_hdr(ether_type),
                },
            })
            .map_err(|e| {
                if !async_id_provided {
                    wtrace::async_end_wlansoftmac_tx(async_id, zx::Status::INTERNAL);
                }
                e
            })?;

        // Adjust the start of the slice stored in the ArenaBox.
        //
        // Safety: buffer is a valid pointer to a slice allocated in arena.
        let buffer = unsafe {
            arena.assume_unchecked(NonNull::new_unchecked(
                &mut ArenaBox::into_ptr(buffer).as_mut()[frame_start..],
            ))
        };
        let buffer = arena.make_static(buffer);
        self.ctx.device.send_wlan_frame(buffer, tx_flags, Some(async_id)).map_err(|s| {
            if !async_id_provided {
                wtrace::async_end_wlansoftmac_tx(async_id, s);
            }
            Error::Status(format!("error sending data frame"), s)
        })
    }

    /// Sends an MLME-EAPOL.indication to MLME's SME peer.
    /// Note: MLME-EAPOL.indication is a custom Fuchsia primitive and not defined in IEEE 802.11.
    fn send_eapol_indication(
        &mut self,
        src_addr: MacAddr,
        dst_addr: MacAddr,
        eapol_frame: &[u8],
    ) -> Result<(), Error> {
        self.ctx
            .device
            .send_mlme_event(fidl_mlme::MlmeEvent::EapolInd {
                ind: fidl_mlme::EapolIndication {
                    src_addr: src_addr.to_array(),
                    dst_addr: dst_addr.to_array(),
                    data: eapol_frame.to_vec(),
                },
            })
            .map_err(|e| e.into())
    }

    /// Sends an EAPoL frame over the air and reports transmission status to SME via an
    /// MLME-EAPOL.confirm message.
    pub fn send_eapol_frame(
        &mut self,
        src: MacAddr,
        dst: MacAddr,
        is_protected: bool,
        eapol_frame: &[u8],
    ) {
        // TODO(https://fxbug.dev/42110270): EAPoL frames can be send in QoS data frames. However, Fuchsia's old C++
        // MLME never sent EAPoL frames in QoS data frames. For feature parity do the same.
        let result = self.send_data_frame(
            src,
            dst,
            is_protected,
            false, /* don't use QoS */
            mac::ETHER_TYPE_EAPOL,
            eapol_frame,
            None,
        );
        let result_code = match result {
            Ok(()) => fidl_mlme::EapolResultCode::Success,
            Err(e) => {
                error!("error sending EAPoL frame: {}", e);
                fidl_mlme::EapolResultCode::TransmissionFailure
            }
        };

        // Report transmission result to SME.
        self.ctx
            .device
            .send_mlme_event(fidl_mlme::MlmeEvent::EapolConf {
                resp: fidl_mlme::EapolConfirm { result_code, dst_addr: dst.to_array() },
            })
            .unwrap_or_else(|e| error!("error sending MLME-EAPOL.confirm message: {}", e));
    }

    pub fn send_ps_poll_frame(&mut self, aid: Aid) -> Result<(), Error> {
        const PS_POLL_ID_MASK: u16 = 0b11000000_00000000;

        let buffer = write_frame!({
            headers: {
                mac::FrameControl: &mac::FrameControl(0)
                    .with_frame_type(mac::FrameType::CTRL)
                    .with_ctrl_subtype(mac::CtrlSubtype::PS_POLL),
                mac::PsPoll: &mac::PsPoll {
                    // IEEE 802.11-2016 9.3.1.5 states the ID in the PS-Poll frame is the
                    // association ID with the 2 MSBs set to 1.
                    masked_aid: aid | PS_POLL_ID_MASK,
                    bssid: self.sta.bssid(),
                    ta: self.sta.iface_mac,
                },
            },
        })?;
        self.send_mgmt_or_ctrl_frame(buffer)
            .map_err(|s| Error::Status(format!("error sending PS-Poll frame"), s))
    }

    /// Called when a previously scheduled `TimedEvent` fired.
    pub async fn handle_timed_event(&mut self, event: TimedEvent) {
        self.sta.state = Some(self.sta.state.take().unwrap().on_timed_event(self, event).await)
    }

    /// Called when an arbitrary frame was received over the air.
    pub async fn on_mac_frame<B: SplitByteSlice>(
        &mut self,
        bytes: B,
        rx_info: fidl_softmac::WlanRxInfo,
        async_id: trace::Id,
    ) {
        wtrace::duration!(c"BoundClient::on_mac_frame");
        // Safe: |state| is never None and always replaced with Some(..).
        self.sta.state =
            Some(self.sta.state.take().unwrap().on_mac_frame(self, bytes, rx_info, async_id).await);
    }

    pub fn on_eth_frame_tx<B: SplitByteSlice>(
        &mut self,
        frame: B,
        async_id: trace::Id,
    ) -> Result<(), Error> {
        wtrace::duration!(c"BoundClient::on_eth_frame_tx");
        // Safe: |state| is never None and always replaced with Some(..).
        let state = self.sta.state.take().unwrap();
        let result = state.on_eth_frame(self, frame, async_id);
        self.sta.state.replace(state);
        result
    }

    pub async fn start_connecting(&mut self) {
        // Safe: |state| is never None and always replaced with Some(..).
        let next_state = self.sta.state.take().unwrap().start_connecting(self).await;
        self.sta.state.replace(next_state);
    }

    pub async fn handle_mlme_req(&mut self, msg: wlan_sme::MlmeRequest) {
        // Safe: |state| is never None and always replaced with Some(..).
        let next_state = self.sta.state.take().unwrap().handle_mlme_req(self, msg).await;
        self.sta.state.replace(next_state);
    }

    fn send_connect_conf_failure(&mut self, result_code: fidl_ieee80211::StatusCode) {
        self.sta.connect_timeout.take();
        let bssid = self.sta.connect_req.selected_bss.bssid;
        self.send_connect_conf_failure_with_bssid(bssid, result_code);
    }

    /// Send ConnectConf failure with BSSID specified.
    /// The connect timeout is not cleared as this method may be called with a foreign BSSID.
    fn send_connect_conf_failure_with_bssid(
        &mut self,
        bssid: Bssid,
        result_code: fidl_ieee80211::StatusCode,
    ) {
        let connect_conf = fidl_mlme::ConnectConfirm {
            peer_sta_address: bssid.to_array(),
            result_code,
            association_id: 0,
            association_ies: vec![],
        };
        self.ctx
            .device
            .send_mlme_event(fidl_mlme::MlmeEvent::ConnectConf { resp: connect_conf })
            .unwrap_or_else(|e| error!("error sending MLME-CONNECT.confirm: {}", e));
    }

    fn send_connect_conf_success<B: SplitByteSlice>(
        &mut self,
        association_id: mac::Aid,
        association_ies: B,
    ) {
        self.sta.connect_timeout.take();
        let connect_conf = fidl_mlme::ConnectConfirm {
            peer_sta_address: self.sta.connect_req.selected_bss.bssid.to_array(),
            result_code: fidl_ieee80211::StatusCode::Success,
            association_id,
            association_ies: association_ies.to_vec(),
        };
        self.ctx
            .device
            .send_mlme_event(fidl_mlme::MlmeEvent::ConnectConf { resp: connect_conf })
            .unwrap_or_else(|e| error!("error sending MLME-CONNECT.confirm: {}", e));
    }

    /// Sends an MLME-DEAUTHENTICATE.indication message to the joined BSS.
    fn send_deauthenticate_ind(
        &mut self,
        reason_code: fidl_ieee80211::ReasonCode,
        locally_initiated: LocallyInitiated,
    ) {
        // Clear main_channel since there is no "main channel" after deauthenticating
        self.channel_state.bind(&mut self.ctx, &mut self.scanner).clear_main_channel();

        self.ctx
            .device
            .send_mlme_event(fidl_mlme::MlmeEvent::DeauthenticateInd {
                ind: fidl_mlme::DeauthenticateIndication {
                    peer_sta_address: self.sta.bssid().to_array(),
                    reason_code,
                    locally_initiated: locally_initiated.0,
                },
            })
            .unwrap_or_else(|e| error!("error sending MLME-DEAUTHENTICATE.indication: {}", e));
    }

    /// Sends an MLME-DISASSOCIATE.indication message to the joined BSS.
    fn send_disassoc_ind(
        &mut self,
        reason_code: fidl_ieee80211::ReasonCode,
        locally_initiated: LocallyInitiated,
    ) {
        self.ctx
            .device
            .send_mlme_event(fidl_mlme::MlmeEvent::DisassociateInd {
                ind: fidl_mlme::DisassociateIndication {
                    peer_sta_address: self.sta.bssid().to_array(),
                    reason_code,
                    locally_initiated: locally_initiated.0,
                },
            })
            .unwrap_or_else(|e| error!("error sending MLME-DISASSOCIATE.indication: {}", e));
    }

    async fn clear_association(&mut self) -> Result<(), zx::Status> {
        self.ctx
            .device
            .clear_association(&fidl_softmac::WlanSoftmacBaseClearAssociationRequest {
                peer_addr: Some(self.sta.bssid().to_array()),
                ..Default::default()
            })
            .await
    }

    /// Sends an sae frame rx message to the SME.
    fn forward_sae_frame_rx(
        &mut self,
        seq_num: u16,
        status_code: fidl_ieee80211::StatusCode,
        sae_fields: Vec<u8>,
    ) {
        self.ctx
            .device
            .send_mlme_event(fidl_mlme::MlmeEvent::OnSaeFrameRx {
                frame: fidl_mlme::SaeFrame {
                    peer_sta_address: self.sta.bssid().to_array(),
                    seq_num,
                    status_code,
                    sae_fields,
                },
            })
            .unwrap_or_else(|e| error!("error sending OnSaeFrameRx: {}", e));
    }

    fn forward_sae_handshake_ind(&mut self) {
        self.ctx
            .device
            .send_mlme_event(fidl_mlme::MlmeEvent::OnSaeHandshakeInd {
                ind: fidl_mlme::SaeHandshakeIndication {
                    peer_sta_address: self.sta.bssid().to_array(),
                },
            })
            .unwrap_or_else(|e| error!("error sending OnSaeHandshakeInd: {}", e));
    }

    fn send_mgmt_or_ctrl_frame(&mut self, buffer: ArenaStaticBox<[u8]>) -> Result<(), zx::Status> {
        self.ctx.device.send_wlan_frame(buffer, fidl_softmac::WlanTxInfoFlags::empty(), None)
    }
}

pub struct ParsedConnectRequest {
    pub selected_bss: BssDescription,
    pub connect_failure_timeout: u32,
    pub auth_type: fidl_mlme::AuthenticationTypes,
    pub sae_password: Vec<u8>,
    pub wep_key: Option<fidl_mlme::SetKeyDescriptor>,
    pub security_ie: Vec<u8>,
}

pub struct ParsedAssociateResp {
    pub association_id: u16,
    pub capabilities: CapabilityInfo,
    pub rates: Vec<ie::SupportedRate>,
    pub ht_cap: Option<ie::HtCapabilities>,
    pub vht_cap: Option<ie::VhtCapabilities>,
}

impl ParsedAssociateResp {
    pub fn parse<B: SplitByteSlice>(assoc_resp_frame: &mac::AssocRespFrame<B>) -> Self {
        let mut parsed = ParsedAssociateResp {
            association_id: assoc_resp_frame.assoc_resp_hdr.aid,
            capabilities: assoc_resp_frame.assoc_resp_hdr.capabilities,
            rates: vec![],
            ht_cap: None,
            vht_cap: None,
        };
        for (id, body) in assoc_resp_frame.ies() {
            match id {
                Id::SUPPORTED_RATES => match ie::parse_supported_rates(body) {
                    Err(e) => warn!("invalid Supported Rates: {}", e),
                    Ok(supported_rates) => {
                        // safe to unwrap because supported rate is 1-byte long thus always aligned
                        parsed.rates.extend(supported_rates.iter());
                    }
                },
                Id::EXTENDED_SUPPORTED_RATES => match ie::parse_extended_supported_rates(body) {
                    Err(e) => warn!("invalid Extended Supported Rates: {}", e),
                    Ok(supported_rates) => {
                        // safe to unwrap because supported rate is 1-byte long thus always aligned
                        parsed.rates.extend(supported_rates.iter());
                    }
                },
                Id::HT_CAPABILITIES => match ie::parse_ht_capabilities(body) {
                    Err(e) => warn!("invalid HT Capabilities: {}", e),
                    Ok(ht_cap) => {
                        parsed.ht_cap = Some(*ht_cap);
                    }
                },
                Id::VHT_CAPABILITIES => match ie::parse_vht_capabilities(body) {
                    Err(e) => warn!("invalid VHT Capabilities: {}", e),
                    Ok(vht_cap) => {
                        parsed.vht_cap = Some(*vht_cap);
                    }
                },
                // TODO(https://fxbug.dev/42120297): parse vendor ID and include WMM param if exists
                _ => {}
            }
        }
        parsed
    }
}

impl<'a, D: DeviceOps> BlockAckTx for BoundClient<'a, D> {
    /// Sends a BlockAck frame to the associated AP.
    ///
    /// BlockAck frames are described by 802.11-2016, section 9.6.5.2, 9.6.5.3, and 9.6.5.4.
    fn send_block_ack_frame(&mut self, n: usize, body: &[u8]) -> Result<(), Error> {
        let arena = Arena::new();
        let buffer = arena.insert_default_slice::<u8>(n);
        let mut buffer = arena.make_static(buffer);
        let mut writer = BufferWriter::new(&mut buffer[..]);
        write_block_ack_hdr(
            &mut writer,
            self.sta.bssid(),
            self.sta.iface_mac,
            &mut self.ctx.seq_mgr,
        )
        .and_then(|_| writer.append_bytes(body).map_err(Into::into))?;
        self.send_mgmt_or_ctrl_frame(buffer)
            .map_err(|status| Error::Status(format!("error sending BlockAck frame"), status))
    }
}

/// Writes the header of the management frame for BlockAck frames to the given buffer.
///
/// The address may be that of the originator or recipient. The frame formats are described by IEEE
/// Std 802.11-2016, 9.6.5.
fn write_block_ack_hdr<B: Append>(
    buffer: &mut B,
    bssid: Bssid,
    addr: MacAddr,
    seq_mgr: &mut SequenceManager,
) -> Result<(), Error> {
    // The management header differs for APs and clients. The frame control and management header
    // are constructed here, but AP and client STAs share the code that constructs the body. See
    // the `block_ack` module.
    Ok(append_frame_to!(
        buffer,
        {
            headers: {
                mac::MgmtHdr: &mgmt_writer::mgmt_hdr_to_ap(
                    mac::FrameControl(0)
                        .with_frame_type(mac::FrameType::MGMT)
                        .with_mgmt_subtype(mac::MgmtSubtype::ACTION),
                    bssid,
                    addr,
                    mac::SequenceControl(0)
                        .with_seq_num(seq_mgr.next_sns1(&bssid.into()) as u16),
                ),
            },
        }
    )
    .map(|_buffer| {})?)
}

#[cfg(test)]
mod tests {
    use super::state::DEFAULT_AUTO_DEAUTH_TIMEOUT_BEACON_COUNT;
    use super::*;
    use crate::block_ack::{
        self, BlockAckState, Closed, ADDBA_REQ_FRAME_LEN, ADDBA_RESP_FRAME_LEN,
    };
    use crate::client::lost_bss::LostBssCounter;
    use crate::client::test_utils::drain_timeouts;
    use crate::device::{test_utils, FakeDevice, FakeDeviceConfig, FakeDeviceState, LinkStatus};
    use crate::test_utils::{fake_wlan_channel, MockWlanRxInfo};
    use fuchsia_sync::Mutex;
    use lazy_static::lazy_static;
    use std::sync::Arc;
    use wlan_common::capabilities::StaCapabilities;
    use wlan_common::channel::Cbw;
    use wlan_common::stats::SignalStrengthAverage;
    use wlan_common::test_utils::fake_capabilities::fake_client_capabilities;
    use wlan_common::test_utils::fake_frames::*;
    use wlan_common::timer::{self, create_timer};
    use wlan_common::{assert_variant, fake_bss_description, fake_fidl_bss_description};
    use wlan_sme::responder::Responder;
    use wlan_statemachine::*;
    use {fidl_fuchsia_wlan_common as fidl_common, fidl_fuchsia_wlan_internal as fidl_internal};
    lazy_static! {
        static ref BSSID: Bssid = [6u8; 6].into();
        static ref IFACE_MAC: MacAddr = [7u8; 6].into();
    }
    const RSNE: &[u8] = &[
        0x30, 0x14, //  ID and len
        1, 0, //  version
        0x00, 0x0f, 0xac, 0x04, //  group data cipher suite
        0x01, 0x00, //  pairwise cipher suite count
        0x00, 0x0f, 0xac, 0x04, //  pairwise cipher suite list
        0x01, 0x00, //  akm suite count
        0x00, 0x0f, 0xac, 0x02, //  akm suite list
        0xa8, 0x04, //  rsn capabilities
    ];
    const SCAN_CHANNEL_PRIMARY: u8 = 6;
    // Note: not necessarily valid beacon frame.
    #[rustfmt::skip]
    const BEACON_FRAME: &'static [u8] = &[
        // Mgmt header
        0b10000000, 0, // Frame Control
        0, 0, // Duration
        255, 255, 255, 255, 255, 255, // addr1
        6, 6, 6, 6, 6, 6, // addr2
        6, 6, 6, 6, 6, 6, // addr3
        0, 0, // Sequence Control
        // Beacon header:
        0, 0, 0, 0, 0, 0, 0, 0, // Timestamp
        10, 0, // Beacon interval
        33, 0, // Capabilities
        // IEs:
        0, 4, 0x73, 0x73, 0x69, 0x64, // SSID - "ssid"
        1, 8, 1, 2, 3, 4, 5, 6, 7, 8, // Supported rates
        3, 1, 11, // DSSS parameter set - channel 11
        5, 4, 0, 0, 0, 0, // TIM
    ];

    struct MockObjects {
        fake_device: FakeDevice,
        fake_device_state: Arc<Mutex<FakeDeviceState>>,
        timer: Option<Timer<super::TimedEvent>>,
        time_stream: timer::EventStream<super::TimedEvent>,
    }

    impl MockObjects {
        // TODO(https://fxbug.dev/327499461): This function is async to ensure MLME functions will
        // run in an async context and not call `wlan_common::timer::Timer::now` without an
        // executor.
        async fn new() -> Self {
            let (timer, time_stream) = create_timer();
            let (fake_device, fake_device_state) = FakeDevice::new_with_config(
                FakeDeviceConfig::default()
                    .with_mock_mac_role(fidl_common::WlanMacRole::Client)
                    .with_mock_sta_addr((*IFACE_MAC).to_array()),
            )
            .await;
            Self { fake_device, fake_device_state, timer: Some(timer), time_stream }
        }

        async fn make_mlme(&mut self) -> ClientMlme<FakeDevice> {
            let mut mlme = ClientMlme::new(
                Default::default(),
                self.fake_device.clone(),
                self.timer.take().unwrap(),
            )
            .await
            .expect("Failed to create client MLME.");
            mlme.set_main_channel(fake_wlan_channel().into())
                .await
                .expect("unable to set main channel");
            mlme
        }
    }

    fn scan_req() -> fidl_mlme::ScanRequest {
        fidl_mlme::ScanRequest {
            txn_id: 1337,
            scan_type: fidl_mlme::ScanTypes::Passive,
            channel_list: vec![SCAN_CHANNEL_PRIMARY],
            ssid_list: vec![Ssid::try_from("ssid").unwrap().into()],
            probe_delay: 0,
            min_channel_time: 100,
            max_channel_time: 300,
        }
    }

    fn make_client_station() -> Client {
        let connect_req = ParsedConnectRequest {
            selected_bss: fake_bss_description!(Open, bssid: BSSID.to_array()),
            connect_failure_timeout: 100,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![],
        };
        Client::new(connect_req, *IFACE_MAC, fake_client_capabilities())
    }

    fn make_client_station_protected() -> Client {
        let connect_req = ParsedConnectRequest {
            selected_bss: fake_bss_description!(Wpa2, bssid: BSSID.to_array()),
            connect_failure_timeout: 100,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: RSNE.to_vec(),
        };
        Client::new(connect_req, *IFACE_MAC, fake_client_capabilities())
    }

    impl ClientMlme<FakeDevice> {
        fn make_client_station(&mut self) {
            self.sta.replace(make_client_station());
        }

        fn make_client_station_protected(&mut self) {
            self.sta.replace(make_client_station_protected());
        }

        fn get_bound_client(&mut self) -> Option<BoundClient<'_, FakeDevice>> {
            match self.sta.as_mut() {
                None => None,
                Some(sta) => {
                    Some(sta.bind(&mut self.ctx, &mut self.scanner, &mut self.channel_state))
                }
            }
        }
    }

    impl BoundClient<'_, FakeDevice> {
        fn move_to_associated_state(&mut self) {
            use super::state::*;
            let status_check_timeout =
                schedule_association_status_timeout(self.sta.beacon_period(), &mut self.ctx.timer);
            let state =
                States::from(wlan_statemachine::testing::new_state(Associated(Association {
                    aid: 42,
                    assoc_resp_ies: vec![],
                    controlled_port_open: true,
                    ap_ht_op: None,
                    ap_vht_op: None,
                    qos: Qos::Disabled,
                    lost_bss_counter: LostBssCounter::start(
                        self.sta.beacon_period(),
                        DEFAULT_AUTO_DEAUTH_TIMEOUT_BEACON_COUNT,
                    ),
                    status_check_timeout,
                    signal_strength_average: SignalStrengthAverage::new(),
                    block_ack_state: StateMachine::new(BlockAckState::from(State::new(Closed))),
                })));
            self.sta.state.replace(state);
        }

        async fn close_controlled_port(&mut self) {
            self.handle_mlme_req(wlan_sme::MlmeRequest::SetCtrlPort(
                fidl_mlme::SetControlledPortRequest {
                    peer_sta_address: BSSID.to_array(),
                    state: fidl_mlme::ControlledPortState::Closed,
                },
            ))
            .await;
        }
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn spawns_new_sta_on_connect_request_from_sme() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        assert!(me.get_bound_client().is_none(), "MLME should not contain client, yet");
        me.on_sme_connect(fidl_mlme::ConnectRequest {
            selected_bss: fake_fidl_bss_description!(Open, ssid: Ssid::try_from("foo").unwrap()),
            connect_failure_timeout: 100,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![],
        })
        .await
        .expect("valid ConnectRequest should be handled successfully");
        me.get_bound_client().expect("client sta should have been created by now.");
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn fails_to_connect_if_channel_unknown() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        assert!(me.get_bound_client().is_none(), "MLME should not contain client, yet");
        let mut req = fidl_mlme::ConnectRequest {
            selected_bss: fake_fidl_bss_description!(Open, ssid: Ssid::try_from("foo").unwrap()),
            connect_failure_timeout: 100,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![],
        };

        req.selected_bss.channel.cbw = fidl_fuchsia_wlan_common::ChannelBandwidth::unknown();
        me.on_sme_connect(req)
            .await
            .expect_err("ConnectRequest with unknown channel should be rejected");
        assert!(me.get_bound_client().is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn rsn_ie_implies_sta_eapol_required() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        assert!(me.get_bound_client().is_none(), "MLME should not contain client, yet");
        me.on_sme_connect(fidl_mlme::ConnectRequest {
            selected_bss: fake_fidl_bss_description!(Wpa2, ssid: Ssid::try_from("foo").unwrap()),
            connect_failure_timeout: 100,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![],
        })
        .await
        .expect("valid ConnectRequest should be handled successfully");
        let client = me.get_bound_client().expect("client sta should have been created by now.");
        assert!(client.sta.eapol_required());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn wpa1_implies_sta_eapol_required() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        assert!(me.get_bound_client().is_none(), "MLME should not contain client, yet");
        me.on_sme_connect(fidl_mlme::ConnectRequest {
            selected_bss: fake_fidl_bss_description!(Wpa1, ssid: Ssid::try_from("foo").unwrap()),
            connect_failure_timeout: 100,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![],
        })
        .await
        .expect("valid ConnectRequest should be handled successfully");
        let client = me.get_bound_client().expect("client sta should have been created by now.");
        assert!(client.sta.eapol_required());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn no_wpa_or_rsn_ie_implies_sta_eapol_not_required() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        assert!(me.get_bound_client().is_none(), "MLME should not contain client, yet");
        me.on_sme_connect(fidl_mlme::ConnectRequest {
            selected_bss: fake_fidl_bss_description!(Open, ssid: Ssid::try_from("foo").unwrap()),
            connect_failure_timeout: 100,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![],
        })
        .await
        .expect("valid ConnectRequest should be handled successfully");
        let client = me.get_bound_client().expect("client sta should have been created by now.");
        assert!(!client.sta.eapol_required());
    }

    /// Consumes `TimedEvent` values from the `timer::EventStream` held by `mock_objects` and
    /// handles each `TimedEvent` value with `mlme`. This function makes the following assertions:
    ///
    ///   - The `timer::EventStream` held by `mock_objects` starts with one `StatusCheckTimeout`
    ///     pending.
    ///   - For the `beacon_count` specified, `mlme` will consume the current `StatusCheckTimeout`
    ///     and schedule the next.
    ///   - `mlme` produces a `fidl_mlme::SignalReportIndication` for each StatusCheckTimeout
    ///     consumed.
    async fn handle_association_status_checks_and_signal_reports(
        mock_objects: &mut MockObjects,
        mlme: &mut ClientMlme<FakeDevice>,
        beacon_count: u32,
    ) {
        for _ in 0..beacon_count / super::state::ASSOCIATION_STATUS_TIMEOUT_BEACON_COUNT {
            let (_, timed_event, _) = mock_objects
                .time_stream
                .try_next()
                .unwrap()
                .expect("Should have scheduled a timed event");
            mlme.handle_timed_event(timed_event.event).await;
            assert_eq!(mock_objects.fake_device_state.lock().wlan_queue.len(), 0);
            mock_objects
                .fake_device_state
                .lock()
                .next_mlme_msg::<fidl_internal::SignalReportIndication>()
                .expect("error reading SignalReport.indication");
        }
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_auto_deauth_uninterrupted_interval() {
        let mut mock_objects = MockObjects::new().await;
        let mut mlme = mock_objects.make_mlme().await;
        mlme.make_client_station();
        let mut client = mlme.get_bound_client().expect("client should be present");

        client.move_to_associated_state();

        // Verify timer is scheduled and move the time to immediately before auto deauth is triggered.
        handle_association_status_checks_and_signal_reports(
            &mut mock_objects,
            &mut mlme,
            DEFAULT_AUTO_DEAUTH_TIMEOUT_BEACON_COUNT,
        )
        .await;

        // One more timeout to trigger the auto deauth
        let (_, timed_event, _) = mock_objects
            .time_stream
            .try_next()
            .unwrap()
            .expect("Should have scheduled a timed event");

        // Verify that triggering event at deadline causes deauth
        mlme.handle_timed_event(timed_event.event).await;
        mock_objects
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_internal::SignalReportIndication>()
            .expect("error reading SignalReport.indication");
        assert_eq!(mock_objects.fake_device_state.lock().wlan_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(&mock_objects.fake_device_state.lock().wlan_queue[0].0[..], &[
            // Mgmt header:
            0b1100_00_00, 0b00000000, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // Sequence Control
            3, 0, // reason code
        ][..]);
        let deauth_ind = mock_objects
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::DeauthenticateIndication>()
            .expect("error reading DEAUTHENTICATE.indication");
        assert_eq!(
            deauth_ind,
            fidl_mlme::DeauthenticateIndication {
                peer_sta_address: BSSID.to_array(),
                reason_code: fidl_ieee80211::ReasonCode::LeavingNetworkDeauth,
                locally_initiated: true,
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn test_auto_deauth_received_beacon() {
        let mut mock_objects = MockObjects::new().await;
        let mut mlme = mock_objects.make_mlme().await;
        mlme.make_client_station();
        let mut client = mlme.get_bound_client().expect("client should be present");

        client.move_to_associated_state();

        // Move the countdown to just about to cause auto deauth.
        handle_association_status_checks_and_signal_reports(
            &mut mock_objects,
            &mut mlme,
            DEFAULT_AUTO_DEAUTH_TIMEOUT_BEACON_COUNT,
        )
        .await;

        // Receive beacon midway, so lost bss countdown is reset.
        // If this beacon is not received, the next timeout will trigger auto deauth.
        mlme.on_mac_frame_rx(
            BEACON_FRAME,
            fidl_softmac::WlanRxInfo {
                rx_flags: fidl_softmac::WlanRxInfoFlags::empty(),
                valid_fields: fidl_softmac::WlanRxInfoValid::empty(),
                phy: fidl_common::WlanPhyType::Dsss,
                data_rate: 0,
                channel: mlme.channel_state.get_main_channel().unwrap(),
                mcs: 0,
                rssi_dbm: 0,
                snr_dbh: 0,
            },
            0.into(),
        )
        .await;

        // Verify auto deauth is not triggered for the entire duration.
        handle_association_status_checks_and_signal_reports(
            &mut mock_objects,
            &mut mlme,
            DEFAULT_AUTO_DEAUTH_TIMEOUT_BEACON_COUNT,
        )
        .await;

        // Verify more timer is scheduled
        let (_, timed_event2, _) = mock_objects
            .time_stream
            .try_next()
            .unwrap()
            .expect("Should have scheduled a timed event");

        // Verify that triggering event at new deadline causes deauth
        mlme.handle_timed_event(timed_event2.event).await;
        mock_objects
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_internal::SignalReportIndication>()
            .expect("error reading SignalReport.indication");
        assert_eq!(mock_objects.fake_device_state.lock().wlan_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(&mock_objects.fake_device_state.lock().wlan_queue[0].0[..], &[
            // Mgmt header:
            0b1100_00_00, 0b00000000, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // Sequence Control
            3, 0, // reason code
        ][..]);
        let deauth_ind = mock_objects
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::DeauthenticateIndication>()
            .expect("error reading DEAUTHENTICATE.indication");
        assert_eq!(
            deauth_ind,
            fidl_mlme::DeauthenticateIndication {
                peer_sta_address: BSSID.to_array(),
                reason_code: fidl_ieee80211::ReasonCode::LeavingNetworkDeauth,
                locally_initiated: true,
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_open_auth_frame() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client.send_open_auth_frame().expect("error delivering WLAN frame");
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(&m.fake_device_state.lock().wlan_queue[0].0[..], &[
            // Mgmt header:
            0b1011_00_00, 0b00000000, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // Sequence Control
            // Auth header:
            0, 0, // auth algorithm
            1, 0, // auth txn seq num
            0, 0, // status code
        ][..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_assoc_req_frame() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        let connect_req = ParsedConnectRequest {
            selected_bss: fake_bss_description!(Wpa2,
                ssid: Ssid::try_from([11, 22, 33, 44]).unwrap(),
                bssid: BSSID.to_array(),
            ),
            connect_failure_timeout: 100,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: RSNE.to_vec(),
        };
        let client_capabilities = ClientCapabilities(StaCapabilities {
            capability_info: CapabilityInfo(0x1234),
            rates: vec![8u8, 7, 6, 5, 4, 3, 2, 1, 0].into_iter().map(ie::SupportedRate).collect(),
            ht_cap: ie::parse_ht_capabilities(&(0..26).collect::<Vec<u8>>()[..]).map(|h| *h).ok(),
            vht_cap: ie::parse_vht_capabilities(&(100..112).collect::<Vec<u8>>()[..])
                .map(|v| *v)
                .ok(),
        });
        me.sta.replace(Client::new(connect_req, *IFACE_MAC, client_capabilities));
        let mut client = me.get_bound_client().expect("client should be present");
        client.send_assoc_req_frame().expect("error delivering WLAN frame");
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        assert_eq!(
            &m.fake_device_state.lock().wlan_queue[0].0[..],
            &[
                // Mgmt header:
                0, 0, // FC
                0, 0, // Duration
                6, 6, 6, 6, 6, 6, // addr1
                7, 7, 7, 7, 7, 7, // addr2
                6, 6, 6, 6, 6, 6, // addr3
                0x10, 0, // Sequence Control
                // Association Request header:
                0x34, 0x12, // capability info
                0, 0, // listen interval
                // IEs
                0, 4, // SSID id and length
                11, 22, 33, 44, // SSID
                1, 8, // supp rates id and length
                8, 7, 6, 5, 4, 3, 2, 1, // supp rates
                50, 1, // ext supp rates and length
                0, // ext supp rates
                0x30, 0x14, // RSNE ID and len
                1, 0, // RSNE version
                0x00, 0x0f, 0xac, 0x04, // RSNE group data cipher suite
                0x01, 0x00, // RSNE pairwise cipher suite count
                0x00, 0x0f, 0xac, 0x04, // RSNE pairwise cipher suite list
                0x01, 0x00, // RSNE akm suite count
                0x00, 0x0f, 0xac, 0x02, // RSNE akm suite list
                0xa8, 0x04, // RSNE rsn capabilities
                45, 26, // HT Cap id and length
                0, 1, 2, 3, 4, 5, 6, 7, // HT Cap \
                8, 9, 10, 11, 12, 13, 14, 15, // HT Cap \
                16, 17, 18, 19, 20, 21, 22, 23, // HT Cap \
                24, 25, // HT Cap (26 bytes)
                191, 12, // VHT Cap id and length
                100, 101, 102, 103, 104, 105, 106, 107, // VHT Cap \
                108, 109, 110, 111, // VHT Cap (12 bytes)
            ][..]
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_keep_alive_resp_frame() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client.send_keep_alive_resp_frame().expect("error delivering WLAN frame");
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(&m.fake_device_state.lock().wlan_queue[0].0[..], &[
            // Data header:
            0b0100_10_00, 0b0000000_1, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // Sequence Control
        ][..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_data_frame() {
        let payload = vec![5; 8];
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client
            .send_data_frame(*IFACE_MAC, [4; 6].into(), false, false, 0x1234, &payload[..], None)
            .expect("error delivering WLAN frame");
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(&m.fake_device_state.lock().wlan_queue[0].0[..], &[
            // Data header:
            0b0000_10_00, 0b0000000_1, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            4, 4, 4, 4, 4, 4, // addr3
            0x10, 0, // Sequence Control
            // LLC header:
            0xAA, 0xAA, 0x03, // DSAP, SSAP, Control
            0, 0, 0, // OUI
            0x12, 0x34, // Protocol ID
            // Payload
            5, 5, 5, 5, 5, 5, 5, 5,
        ][..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_data_frame_ipv4_qos() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        let mut client = make_client_station();
        client
            .bind(&mut me.ctx, &mut me.scanner, &mut me.channel_state)
            .send_data_frame(
                *IFACE_MAC,
                [4; 6].into(),
                false,
                true,
                0x0800,              // IPv4
                &[1, 0xB0, 3, 4, 5], // DSCP = 0b101100 (i.e. VOICE-ADMIT)
                None,
            )
            .expect("error delivering WLAN frame");
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(&m.fake_device_state.lock().wlan_queue[0].0[..], &[
            // Data header:
            0b1000_10_00, 0b0000000_1, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            4, 4, 4, 4, 4, 4, // addr3
            0x10, 0, // Sequence Control
            0x06, 0, // QoS Control - TID = 6
            // LLC header:
            0xAA, 0xAA, 0x03, // DSAP, SSAP, Control
            0, 0, 0, // OUI
            0x08, 0x00, // Protocol ID
            // Payload
            1, 0xB0, 3, 4, 5,
        ][..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_data_frame_ipv6_qos() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        let mut client = make_client_station();
        client
            .bind(&mut me.ctx, &mut me.scanner, &mut me.channel_state)
            .send_data_frame(
                *IFACE_MAC,
                [4; 6].into(),
                false,
                true,
                0x86DD,                         // IPv6
                &[0b0101, 0b10000000, 3, 4, 5], // DSCP = 0b010110 (i.e. AF23)
                None,
            )
            .expect("error delivering WLAN frame");
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(&m.fake_device_state.lock().wlan_queue[0].0[..], &[
            // Data header:
            0b1000_10_00, 0b0000000_1, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            4, 4, 4, 4, 4, 4, // addr3
            0x10, 0, // Sequence Control
            0x03, 0, // QoS Control - TID = 3
            // LLC header:
            0xAA, 0xAA, 0x03, // DSAP, SSAP, Control
            0, 0, 0, // OUI
            0x86, 0xDD, // Protocol ID
            // Payload
            0b0101, 0b10000000, 3, 4, 5,
        ][..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_data_frame_from_ds() {
        let payload = vec![5; 8];
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client
            .send_data_frame([3; 6].into(), [4; 6].into(), false, false, 0x1234, &payload[..], None)
            .expect("error delivering WLAN frame");
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(&m.fake_device_state.lock().wlan_queue[0].0[..], &[
            // Data header:
            0b0000_10_00, 0b000000_11, // FC (ToDS=1, FromDS=1)
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2 = IFACE_MAC
            4, 4, 4, 4, 4, 4, // addr3
            0x10, 0, // Sequence Control
            3, 3, 3, 3, 3, 3, // addr4
            // LLC header:
            0xAA, 0xAA, 0x03, // DSAP, SSAP, Control
            0, 0, 0, // OUI
            0x12, 0x34, // Protocol ID
            // Payload
            5, 5, 5, 5, 5, 5, 5, 5,
        ][..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_deauthentication_notification() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");

        client
            .send_deauth_frame(fidl_ieee80211::ReasonCode::ApInitiated.into())
            .expect("error delivering WLAN frame");
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(&m.fake_device_state.lock().wlan_queue[0].0[..], &[
            // Mgmt header:
            0b1100_00_00, 0b00000000, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // Sequence Control
            47, 0, // reason code
        ][..]);
    }

    fn mock_rx_info<'a>(client: &BoundClient<'a, FakeDevice>) -> fidl_softmac::WlanRxInfo {
        let channel = client.channel_state.get_main_channel().unwrap();
        MockWlanRxInfo::with_channel(channel).into()
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn respond_to_keep_alive_request() {
        #[rustfmt::skip]
        let data_frame = vec![
            // Data header:
            0b0100_10_00, 0b000000_1_0, // FC
            0, 0, // Duration
            7, 7, 7, 7, 7, 7, // addr1
            6, 6, 6, 6, 6, 6, // addr2
            42, 42, 42, 42, 42, 42, // addr3
            0x10, 0, // Sequence Control
        ];
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client.move_to_associated_state();

        client.on_mac_frame(&data_frame[..], mock_rx_info(&client), 0.into()).await;

        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(&m.fake_device_state.lock().wlan_queue[0].0[..], &[
            // Data header:
            0b0100_10_00, 0b0000000_1, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // Sequence Control
        ][..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn data_frame_to_ethernet_single_llc() {
        let mut data_frame = make_data_frame_single_llc(None, None);
        data_frame[1] = 0b00000010; // from_ds = 1, to_ds = 0 when AP sends to client (us)
        data_frame[4..10].copy_from_slice(IFACE_MAC.as_array()); // addr1 - receiver - client (us)
        data_frame[10..16].copy_from_slice(BSSID.as_array()); // addr2 - bssid

        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client.move_to_associated_state();

        client.on_mac_frame(&data_frame[..], mock_rx_info(&client), 0.into()).await;

        assert_eq!(m.fake_device_state.lock().eth_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(m.fake_device_state.lock().eth_queue[0], [
            7, 7, 7, 7, 7, 7, // dst_addr
            5, 5, 5, 5, 5, 5, // src_addr
            9, 10, // ether_type
            11, 11, 11, // payload
        ]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn data_frame_to_ethernet_amsdu() {
        let mut data_frame = make_data_frame_amsdu();
        data_frame[1] = 0b00000010; // from_ds = 1, to_ds = 0 when AP sends to client (us)
        data_frame[4..10].copy_from_slice(IFACE_MAC.as_array()); // addr1 - receiver - client (us)
        data_frame[10..16].copy_from_slice(BSSID.as_array()); // addr2 - bssid

        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client.move_to_associated_state();

        client.on_mac_frame(&data_frame[..], mock_rx_info(&client), 0.into()).await;

        let queue = &m.fake_device_state.lock().eth_queue;
        assert_eq!(queue.len(), 2);
        #[rustfmt::skip]
        let mut expected_first_eth_frame = vec![
            0x78, 0x8a, 0x20, 0x0d, 0x67, 0x03, // dst_addr
            0xb4, 0xf7, 0xa1, 0xbe, 0xb9, 0xab, // src_addr
            0x08, 0x00, // ether_type
        ];
        expected_first_eth_frame.extend_from_slice(MSDU_1_PAYLOAD);
        assert_eq!(queue[0], &expected_first_eth_frame[..]);
        #[rustfmt::skip]
        let mut expected_second_eth_frame = vec![
            0x78, 0x8a, 0x20, 0x0d, 0x67, 0x04, // dst_addr
            0xb4, 0xf7, 0xa1, 0xbe, 0xb9, 0xac, // src_addr
            0x08, 0x01, // ether_type
        ];
        expected_second_eth_frame.extend_from_slice(MSDU_2_PAYLOAD);
        assert_eq!(queue[1], &expected_second_eth_frame[..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn data_frame_to_ethernet_amsdu_padding_too_short() {
        let mut data_frame = make_data_frame_amsdu_padding_too_short();
        data_frame[1] = 0b00000010; // from_ds = 1, to_ds = 0 when AP sends to client (us)
        data_frame[4..10].copy_from_slice(IFACE_MAC.as_array()); // addr1 - receiver - client (us)
        data_frame[10..16].copy_from_slice(BSSID.as_array()); // addr2 - bssid

        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client.move_to_associated_state();

        client.on_mac_frame(&data_frame[..], mock_rx_info(&client), 0.into()).await;

        let queue = &m.fake_device_state.lock().eth_queue;
        assert_eq!(queue.len(), 1);
        #[rustfmt::skip]
            let mut expected_first_eth_frame = vec![
            0x78, 0x8a, 0x20, 0x0d, 0x67, 0x03, // dst_addr
            0xb4, 0xf7, 0xa1, 0xbe, 0xb9, 0xab, // src_addr
            0x08, 0x00, // ether_type
        ];
        expected_first_eth_frame.extend_from_slice(MSDU_1_PAYLOAD);
        assert_eq!(queue[0], &expected_first_eth_frame[..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn data_frame_controlled_port_closed() {
        let mut data_frame = make_data_frame_single_llc(None, None);
        data_frame[1] = 0b00000010; // from_ds = 1, to_ds = 0 when AP sends to client (us)
        data_frame[4..10].copy_from_slice(IFACE_MAC.as_array()); // addr1 - receiver - client (us)
        data_frame[10..16].copy_from_slice(BSSID.as_array()); // addr2 - bssid

        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station_protected();
        let mut client = me.get_bound_client().expect("client should be present");
        client.move_to_associated_state();
        client.close_controlled_port().await;

        client.on_mac_frame(&data_frame[..], mock_rx_info(&client), 0.into()).await;

        // Verify frame was not sent to netstack.
        assert_eq!(m.fake_device_state.lock().eth_queue.len(), 0);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn eapol_frame_controlled_port_closed() {
        let (src_addr, dst_addr, mut eapol_frame) = make_eapol_frame(*IFACE_MAC);
        eapol_frame[1] = 0b00000010; // from_ds = 1, to_ds = 0 when AP sends to client (us)
        eapol_frame[4..10].copy_from_slice(IFACE_MAC.as_array()); // addr1 - receiver - client (us)
        eapol_frame[10..16].copy_from_slice(BSSID.as_array()); // addr2 - bssid

        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station_protected();
        let mut client = me.get_bound_client().expect("client should be present");
        client.move_to_associated_state();
        client.close_controlled_port().await;

        client.on_mac_frame(&eapol_frame[..], mock_rx_info(&client), 0.into()).await;

        // Verify EAPoL frame was not sent to netstack.
        assert_eq!(m.fake_device_state.lock().eth_queue.len(), 0);

        // Verify EAPoL frame was sent to SME.
        let eapol_ind = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::EapolIndication>()
            .expect("error reading EAPOL.indication");
        assert_eq!(
            eapol_ind,
            fidl_mlme::EapolIndication {
                src_addr: src_addr.to_array(),
                dst_addr: dst_addr.to_array(),
                data: EAPOL_PDU.to_vec()
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn eapol_frame_is_controlled_port_open() {
        let (src_addr, dst_addr, mut eapol_frame) = make_eapol_frame(*IFACE_MAC);
        eapol_frame[1] = 0b00000010; // from_ds = 1, to_ds = 0 when AP sends to client (us)
        eapol_frame[4..10].copy_from_slice(IFACE_MAC.as_array()); // addr1 - receiver - client (us)
        eapol_frame[10..16].copy_from_slice(BSSID.as_array()); // addr2 - bssid

        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client.move_to_associated_state();

        client.on_mac_frame(&eapol_frame[..], mock_rx_info(&client), 0.into()).await;

        // Verify EAPoL frame was not sent to netstack.
        assert_eq!(m.fake_device_state.lock().eth_queue.len(), 0);

        // Verify EAPoL frame was sent to SME.
        let eapol_ind = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::EapolIndication>()
            .expect("error reading EAPOL.indication");
        assert_eq!(
            eapol_ind,
            fidl_mlme::EapolIndication {
                src_addr: src_addr.to_array(),
                dst_addr: dst_addr.to_array(),
                data: EAPOL_PDU.to_vec()
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn send_eapol_ind_success() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client
            .send_eapol_indication([1; 6].into(), [2; 6].into(), &[5; 200])
            .expect("expected EAPOL.indication to be sent");
        let eapol_ind = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::EapolIndication>()
            .expect("error reading EAPOL.indication");
        assert_eq!(
            eapol_ind,
            fidl_mlme::EapolIndication {
                src_addr: [1; 6].into(),
                dst_addr: [2; 6].into(),
                data: vec![5; 200]
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn send_eapol_frame_success() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client.send_eapol_frame(*IFACE_MAC, (*BSSID).into(), false, &[5; 8]);

        // Verify EAPOL.confirm message was sent to SME.
        let eapol_confirm = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::EapolConfirm>()
            .expect("error reading EAPOL.confirm");
        assert_eq!(
            eapol_confirm,
            fidl_mlme::EapolConfirm {
                result_code: fidl_mlme::EapolResultCode::Success,
                dst_addr: BSSID.to_array(),
            }
        );

        // Verify EAPoL frame was sent over the air.
        #[rustfmt::skip]
        assert_eq!(&m.fake_device_state.lock().wlan_queue[0].0[..], &[
            // Data header:
            0b0000_10_00, 0b0000000_1, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // Sequence Control
            // LLC header:
            0xaa, 0xaa, 0x03, // dsap ssap ctrl
            0x00, 0x00, 0x00, // oui
            0x88, 0x8E, // protocol id (EAPOL)
            // EAPoL PDU:
            5, 5, 5, 5, 5, 5, 5, 5,
        ][..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn send_eapol_frame_failure() {
        let mut m = MockObjects::new().await;
        m.fake_device_state.lock().config.send_wlan_frame_fails = true;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client.send_eapol_frame([1; 6].into(), [2; 6].into(), false, &[5; 200]);

        // Verify EAPOL.confirm message was sent to SME.
        let eapol_confirm = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::EapolConfirm>()
            .expect("error reading EAPOL.confirm");
        assert_eq!(
            eapol_confirm,
            fidl_mlme::EapolConfirm {
                result_code: fidl_mlme::EapolResultCode::TransmissionFailure,
                dst_addr: [2; 6].into(),
            }
        );

        // Verify EAPoL frame was not sent over the air.
        assert!(m.fake_device_state.lock().wlan_queue.is_empty());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn send_keys() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station_protected();
        let mut client = me.get_bound_client().expect("client should be present");
        client.move_to_associated_state();

        assert!(m.fake_device_state.lock().keys.is_empty());
        client.handle_mlme_req(crate::test_utils::fake_set_keys_req((*BSSID).into())).await;
        assert_eq!(m.fake_device_state.lock().keys.len(), 1);

        let sent_key = crate::test_utils::fake_key((*BSSID).into());
        let received_key = &m.fake_device_state.lock().keys[0];
        assert_eq!(received_key.key, Some(sent_key.key));
        assert_eq!(received_key.key_idx, Some(sent_key.key_id as u8));
        assert_eq!(received_key.key_type, Some(fidl_ieee80211::KeyType::Pairwise));
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn send_addba_req_frame() {
        let mut mock = MockObjects::new().await;
        let mut mlme = mock.make_mlme().await;
        mlme.make_client_station();
        let mut client = mlme.get_bound_client().expect("client should be present");

        let mut body = [0u8; 16];
        let mut writer = BufferWriter::new(&mut body[..]);
        block_ack::write_addba_req_body(&mut writer, 1).expect("failed writing addba frame");
        client
            .send_block_ack_frame(ADDBA_REQ_FRAME_LEN, writer.into_written())
            .expect("failed sending addba frame");
        assert_eq!(
            &mock.fake_device_state.lock().wlan_queue[0].0[..],
            &[
                // Mgmt header 1101 for action frame
                0b11010000, 0b00000000, // frame control
                0, 0, // duration
                6, 6, 6, 6, 6, 6, // addr1
                7, 7, 7, 7, 7, 7, // addr2
                6, 6, 6, 6, 6, 6, // addr3
                0x10, 0, // sequence control
                // Action frame header (Also part of ADDBA request frame)
                0x03, // Action Category: block ack (0x03)
                0x00, // block ack action: ADDBA request (0x00)
                1,    // block ack dialog token
                0b00000011, 0b00010000, // block ack parameters (u16)
                0, 0, // block ack timeout (u16) (0: disabled)
                0b00010000, 0, // block ack starting sequence number: fragment 0, sequence 1
            ][..]
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn send_addba_resp_frame() {
        let mut mock = MockObjects::new().await;
        let mut mlme = mock.make_mlme().await;
        mlme.make_client_station();
        let mut client = mlme.get_bound_client().expect("client should be present");

        let mut body = [0u8; 16];
        let mut writer = BufferWriter::new(&mut body[..]);
        block_ack::write_addba_resp_body(&mut writer, 1).expect("failed writing addba frame");
        client
            .send_block_ack_frame(ADDBA_RESP_FRAME_LEN, writer.into_written())
            .expect("failed sending addba frame");
        assert_eq!(
            &mock.fake_device_state.lock().wlan_queue[0].0[..],
            &[
                // Mgmt header 1101 for action frame
                0b11010000, 0b00000000, // frame control
                0, 0, // duration
                6, 6, 6, 6, 6, 6, // addr1
                7, 7, 7, 7, 7, 7, // addr2
                6, 6, 6, 6, 6, 6, // addr3
                0x10, 0, // sequence control
                // Action frame header (Also part of ADDBA response frame)
                0x03, // Action Category: block ack (0x03)
                0x01, // block ack action: ADDBA response (0x01)
                1,    // block ack dialog token
                0, 0, // status
                0b00000011, 0b00010000, // block ack parameters (u16)
                0, 0, // block ack timeout (u16) (0: disabled)
            ][..]
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_successful_connect_conf() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");

        client.send_connect_conf_success(42, &[0, 5, 3, 4, 5, 6, 7][..]);
        let connect_conf = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("error reading Connect.confirm");
        assert_eq!(
            connect_conf,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: BSSID.to_array(),
                result_code: fidl_ieee80211::StatusCode::Success,
                association_id: 42,
                association_ies: vec![0, 5, 3, 4, 5, 6, 7],
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_failed_connect_conf() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();
        let mut client = me.get_bound_client().expect("client should be present");
        client.send_connect_conf_failure(fidl_ieee80211::StatusCode::DeniedNoMoreStas);
        let connect_conf = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("error reading Connect.confirm");
        assert_eq!(
            connect_conf,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: BSSID.to_array(),
                result_code: fidl_ieee80211::StatusCode::DeniedNoMoreStas,
                association_id: 0,
                association_ies: vec![],
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_scan_end_on_mlme_scan_busy() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();

        // Issue a second scan before the first finishes
        me.on_sme_scan(scan_req()).await;
        me.on_sme_scan(fidl_mlme::ScanRequest { txn_id: 1338, ..scan_req() }).await;

        let scan_end = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ScanEnd>()
            .expect("error reading MLME ScanEnd");
        assert_eq!(
            scan_end,
            fidl_mlme::ScanEnd { txn_id: 1338, code: fidl_mlme::ScanResultCode::NotSupported }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_scan_end_on_scan_busy() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        me.make_client_station();

        // Issue a second scan before the first finishes
        me.on_sme_scan(scan_req()).await;
        me.on_sme_scan(fidl_mlme::ScanRequest { txn_id: 1338, ..scan_req() }).await;

        let scan_end = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ScanEnd>()
            .expect("error reading MLME ScanEnd");
        assert_eq!(
            scan_end,
            fidl_mlme::ScanEnd { txn_id: 1338, code: fidl_mlme::ScanResultCode::NotSupported }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_scan_end_on_mlme_scan_invalid_args() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;

        me.make_client_station();
        me.on_sme_scan(fidl_mlme::ScanRequest {
            txn_id: 1337,
            scan_type: fidl_mlme::ScanTypes::Passive,
            channel_list: vec![], // empty channel list
            ssid_list: vec![Ssid::try_from("ssid").unwrap().into()],
            probe_delay: 0,
            min_channel_time: 100,
            max_channel_time: 300,
        })
        .await;
        let scan_end = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ScanEnd>()
            .expect("error reading MLME ScanEnd");
        assert_eq!(
            scan_end,
            fidl_mlme::ScanEnd { txn_id: 1337, code: fidl_mlme::ScanResultCode::InvalidArgs }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_scan_end_on_scan_invalid_args() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;

        me.make_client_station();
        me.on_sme_scan(fidl_mlme::ScanRequest {
            txn_id: 1337,
            scan_type: fidl_mlme::ScanTypes::Passive,
            channel_list: vec![6],
            ssid_list: vec![Ssid::try_from("ssid").unwrap().into()],
            probe_delay: 0,
            min_channel_time: 300, // min > max
            max_channel_time: 100,
        })
        .await;
        let scan_end = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ScanEnd>()
            .expect("error reading MLME ScanEnd");
        assert_eq!(
            scan_end,
            fidl_mlme::ScanEnd { txn_id: 1337, code: fidl_mlme::ScanResultCode::InvalidArgs }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn client_send_scan_end_on_passive_scan_fails() {
        let mut m = MockObjects::new().await;
        m.fake_device_state.lock().config.start_passive_scan_fails = true;
        let mut me = m.make_mlme().await;

        me.make_client_station();
        me.on_sme_scan(scan_req()).await;
        let scan_end = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ScanEnd>()
            .expect("error reading MLME ScanEnd");
        assert_eq!(
            scan_end,
            fidl_mlme::ScanEnd { txn_id: 1337, code: fidl_mlme::ScanResultCode::NotSupported }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_respond_to_query_device_info() {
        let mut mock_objects = MockObjects::new().await;
        let mut mlme = mock_objects.make_mlme().await;

        let (responder, receiver) = Responder::new();
        mlme.handle_mlme_req(wlan_sme::MlmeRequest::QueryDeviceInfo(responder))
            .await
            .expect("Failed to send MlmeRequest::Connect");
        assert_eq!(
            receiver.await.unwrap(),
            fidl_mlme::DeviceInfo {
                sta_addr: IFACE_MAC.to_array(),
                role: fidl_common::WlanMacRole::Client,
                bands: test_utils::fake_mlme_band_caps(),
                softmac_hardware_capability: 0,
                qos_capable: false,
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_respond_to_query_mac_sublayer_support() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;

        let (responder, receiver) = Responder::new();
        me.handle_mlme_req(wlan_sme::MlmeRequest::QueryMacSublayerSupport(responder))
            .await
            .expect("Failed to send MlmeRequest::Connect");
        let resp = receiver.await.unwrap();
        assert_eq!(resp.rate_selection_offload.supported, false);
        assert_eq!(resp.data_plane.data_plane_type, fidl_common::DataPlaneType::EthernetDevice);
        assert_eq!(resp.device.is_synthetic, true);
        assert_eq!(
            resp.device.mac_implementation_type,
            fidl_common::MacImplementationType::Softmac
        );
        assert_eq!(resp.device.tx_status_report_supported, true);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_respond_to_query_security_support() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;

        let (responder, receiver) = Responder::new();
        assert_variant!(
            me.handle_mlme_req(wlan_sme::MlmeRequest::QuerySecuritySupport(responder)).await,
            Ok(())
        );
        let resp = receiver.await.unwrap();
        assert_eq!(resp.mfp.supported, false);
        assert_eq!(resp.sae.driver_handler_supported, false);
        assert_eq!(resp.sae.sme_handler_supported, false);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_respond_to_query_spectrum_management_support() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;

        let (responder, receiver) = Responder::new();
        me.handle_mlme_req(wlan_sme::MlmeRequest::QuerySpectrumManagementSupport(responder))
            .await
            .expect("Failed to send MlmeRequest::QuerySpectrumManagementSupport");
        assert_eq!(receiver.await.unwrap().dfs.supported, true);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_connect_unprotected_happy_path() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        let channel = Channel::new(6, Cbw::Cbw40);
        let connect_req = fidl_mlme::ConnectRequest {
            selected_bss: fake_fidl_bss_description!(Open,
                ssid: Ssid::try_from("ssid").unwrap().into(),
                bssid: BSSID.to_array(),
                channel: channel.clone(),
            ),
            connect_failure_timeout: 100,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![],
        };
        me.handle_mlme_req(wlan_sme::MlmeRequest::Connect(connect_req))
            .await
            .expect("Failed to send MlmeRequest::Connect");

        // Verify an event was queued up in the timer.
        assert_variant!(drain_timeouts(&mut m.time_stream).get(&TimedEventClass::Connecting), Some(ids) => {
            assert_eq!(ids.len(), 1);
        });

        // Verify authentication frame was sent to AP.
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        let (frame, _txflags) = m.fake_device_state.lock().wlan_queue.remove(0);
        #[rustfmt::skip]
        let expected = vec![
            // Mgmt Header:
            0b1011_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // Addr1
            7, 7, 7, 7, 7, 7, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Auth Header:
            0, 0, // Algorithm Number (Open)
            1, 0, // Txn Sequence Number
            0, 0, // Status Code
        ];
        assert_eq!(&frame[..], &expected[..]);

        // Mock auth frame response from the AP
        #[rustfmt::skip]
        let auth_resp_success = vec![
            // Mgmt Header:
            0b1011_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            7, 7, 7, 7, 7, 7, // Addr1
            7, 7, 7, 7, 7, 7, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Auth Header:
            0, 0, // Algorithm Number (Open)
            2, 0, // Txn Sequence Number
            0, 0, // Status Code
        ];
        me.on_mac_frame_rx(
            &auth_resp_success[..],
            MockWlanRxInfo::with_channel(channel.into()).into(),
            0.into(),
        )
        .await;

        // Verify association request frame was went to AP
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        let (frame, _txflags) = m.fake_device_state.lock().wlan_queue.remove(0);
        #[rustfmt::skip]
        let expected = vec![
            // Mgmt header:
            0, 0, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x20, 0, // Sequence Control
            // Association Request header:
            0x01, 0x00, // capability info
            0, 0, // listen interval
            // IEs
            0, 4, // SSID id and length
            0x73, 0x73, 0x69, 0x64, // SSID
            1, 8, // supp rates id and length
            2, 4, 11, 22, 12, 18, 24, 36, // supp rates
            50, 4, // ext supp rates and length
            48, 72, 96, 108, // ext supp rates
            45, 26, // HT Cap id and length
            0x63, 0, 0x17, 0xff, 0, 0, 0, // HT Cap \
            0, 0, 0, 0, 0, 0, 0, 0, 1, // HT Cap \
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // HT Cap
        ];
        assert_eq!(&frame[..], &expected[..]);

        // Mock assoc resp frame from the AP
        #[rustfmt::skip]
        let assoc_resp_success = vec![
            // Mgmt Header:
            0b0001_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            7, 7, 7, 7, 7, 7, // Addr1 == IFACE_MAC
            7, 7, 7, 7, 7, 7, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x20, 0, // Sequence Control
            // Assoc Resp Header:
            0, 0, // Capabilities
            0, 0, // Status Code
            42, 0, // AID
            // IEs
            // Basic Rates
            0x01, 0x08, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24,
            // HT Capabilities
            0x2d, 0x1a, 0xef, 0x09, // HT capabilities info
            0x17, // A-MPDU parameters
            0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // VHT Capabilities
            0xbf, 0x0c, 0x91, 0x59, 0x82, 0x0f, // VHT capabilities info
            0xea, 0xff, 0x00, 0x00, 0xea, 0xff, 0x00, 0x00, // VHT supported MCS set
        ];
        me.on_mac_frame_rx(
            &assoc_resp_success[..],
            MockWlanRxInfo::with_channel(channel.into()).into(),
            0.into(),
        )
        .await;

        // Verify a successful connect conf is sent
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("expect ConnectConf");
        assert_eq!(
            msg,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: BSSID.to_array(),
                result_code: fidl_ieee80211::StatusCode::Success,
                association_id: 42,
                association_ies: vec![
                    // IEs
                    // Basic Rates
                    0x01, 0x08, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24,
                    // HT Capabilities
                    0x2d, 0x1a, 0xef, 0x09, // HT capabilities info
                    0x17, // A-MPDU parameters
                    0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, // VHT Capabilities
                    0xbf, 0x0c, 0x91, 0x59, 0x82, 0x0f, // VHT capabilities info
                    0xea, 0xff, 0x00, 0x00, 0xea, 0xff, 0x00, 0x00, // VHT supported MCS set
                ],
            }
        );

        // Verify eth link is up
        assert_eq!(m.fake_device_state.lock().link_status, LinkStatus::UP);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_connect_protected_happy_path() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        let channel = Channel::new(6, Cbw::Cbw40);
        let connect_req = fidl_mlme::ConnectRequest {
            selected_bss: fake_fidl_bss_description!(Wpa2,
                ssid: Ssid::try_from("ssid").unwrap().into(),
                bssid: BSSID.to_array(),
                channel: channel.clone(),
            ),
            connect_failure_timeout: 100,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![
                48, 18, // RSNE header
                1, 0, // Version
                0x00, 0x0F, 0xAC, 4, // Group Cipher: CCMP-128
                1, 0, 0x00, 0x0F, 0xAC, 4, // 1 Pairwise Cipher: CCMP-128
                1, 0, 0x00, 0x0F, 0xAC, 2, // 1 AKM: PSK
            ],
        };
        me.handle_mlme_req(wlan_sme::MlmeRequest::Connect(connect_req))
            .await
            .expect("Failed to send MlmeRequest::Connect");

        // Verify an event was queued up in the timer.
        assert_variant!(drain_timeouts(&mut m.time_stream).get(&TimedEventClass::Connecting), Some(ids) => {
            assert_eq!(ids.len(), 1);
        });

        // Verify authentication frame was sent to AP.
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        let (frame, _txflags) = m.fake_device_state.lock().wlan_queue.remove(0);
        #[rustfmt::skip]
        let expected = vec![
            // Mgmt Header:
            0b1011_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // Addr1
            7, 7, 7, 7, 7, 7, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Auth Header:
            0, 0, // Algorithm Number (Open)
            1, 0, // Txn Sequence Number
            0, 0, // Status Code
        ];
        assert_eq!(&frame[..], &expected[..]);

        // Mock auth frame response from the AP
        #[rustfmt::skip]
        let auth_resp_success = vec![
            // Mgmt Header:
            0b1011_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            7, 7, 7, 7, 7, 7, // Addr1
            7, 7, 7, 7, 7, 7, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Auth Header:
            0, 0, // Algorithm Number (Open)
            2, 0, // Txn Sequence Number
            0, 0, // Status Code
        ];
        me.on_mac_frame_rx(
            &auth_resp_success[..],
            MockWlanRxInfo::with_channel(channel.into()).into(),
            0.into(),
        )
        .await;

        // Verify association request frame was went to AP
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        let (frame, _txflags) = m.fake_device_state.lock().wlan_queue.remove(0);
        #[rustfmt::skip]
        let expected = vec![
            // Mgmt header:
            0, 0, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x20, 0, // Sequence Control
            // Association Request header:
            0x01, 0x00, // capability info
            0, 0, // listen interval
            // IEs
            0, 4, // SSID id and length
            0x73, 0x73, 0x69, 0x64, // SSID
            1, 8, // supp rates id and length
            2, 4, 11, 22, 12, 18, 24, 36, // supp rates
            50, 4, // ext supp rates and length
            48, 72, 96, 108, // ext supp rates
            48, 18, // RSNE id and length
            1, 0, // RSN \
            0x00, 0x0F, 0xAC, 4, // RSN \
            1, 0, 0x00, 0x0F, 0xAC, 4, // RSN \
            1, 0, 0x00, 0x0F, 0xAC, 2, // RSN
            45, 26, // HT Cap id and length
            0x63, 0, 0x17, 0xff, 0, 0, 0, // HT Cap \
            0, 0, 0, 0, 0, 0, 0, 0, 1, // HT Cap \
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // HT Cap
        ];
        assert_eq!(&frame[..], &expected[..]);

        // Mock assoc resp frame from the AP
        #[rustfmt::skip]
        let assoc_resp_success = vec![
            // Mgmt Header:
            0b0001_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            7, 7, 7, 7, 7, 7, // Addr1 == IFACE_MAC
            7, 7, 7, 7, 7, 7, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x20, 0, // Sequence Control
            // Assoc Resp Header:
            0, 0, // Capabilities
            0, 0, // Status Code
            42, 0, // AID
            // IEs
            // Basic Rates
            0x01, 0x08, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24,
            // RSN
            0x30, 18, 1, 0, // RSN header and version
            0x00, 0x0F, 0xAC, 4, // Group Cipher: CCMP-128
            1, 0, 0x00, 0x0F, 0xAC, 4, // 1 Pairwise Cipher: CCMP-128
            1, 0, 0x00, 0x0F, 0xAC, 2, // 1 AKM: PSK
            // HT Capabilities
            0x2d, 0x1a, 0xef, 0x09, // HT capabilities info
            0x17, // A-MPDU parameters
            0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // Other HT Cap fields
            // VHT Capabilities
            0xbf, 0x0c, 0x91, 0x59, 0x82, 0x0f, // VHT capabilities info
            0xea, 0xff, 0x00, 0x00, 0xea, 0xff, 0x00, 0x00, // VHT supported MCS set
        ];
        me.on_mac_frame_rx(
            &assoc_resp_success[..],
            MockWlanRxInfo::with_channel(channel.into()).into(),
            0.into(),
        )
        .await;

        // Verify a successful connect conf is sent
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("expect ConnectConf");
        assert_eq!(
            msg,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: BSSID.to_array(),
                result_code: fidl_ieee80211::StatusCode::Success,
                association_id: 42,
                association_ies: vec![
                    // IEs
                    // Basic Rates
                    0x01, 0x08, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, // RSN
                    0x30, 18, 1, 0, // RSN header and version
                    0x00, 0x0F, 0xAC, 4, // Group Cipher: CCMP-128
                    1, 0, 0x00, 0x0F, 0xAC, 4, // 1 Pairwise Cipher: CCMP-128
                    1, 0, 0x00, 0x0F, 0xAC, 2, // 1 AKM: PSK
                    // HT Capabilities
                    0x2d, 0x1a, 0xef, 0x09, // HT capabilities info
                    0x17, // A-MPDU parameters
                    0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, // Other HT Cap fields
                    // VHT Capabilities
                    0xbf, 0x0c, 0x91, 0x59, 0x82, 0x0f, // VHT capabilities info
                    0xea, 0xff, 0x00, 0x00, 0xea, 0xff, 0x00, 0x00, // VHT supported MCS set
                ],
            }
        );

        // Verify that link is still down
        assert_eq!(m.fake_device_state.lock().link_status, LinkStatus::DOWN);

        // Send a request to open controlled port
        me.handle_mlme_req(wlan_sme::MlmeRequest::SetCtrlPort(
            fidl_mlme::SetControlledPortRequest {
                peer_sta_address: BSSID.to_array(),
                state: fidl_mlme::ControlledPortState::Open,
            },
        ))
        .await
        .expect("expect sending msg to succeed");

        // Verify that link is now up
        assert_eq!(m.fake_device_state.lock().link_status, LinkStatus::UP);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_connect_vht() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        let channel = Channel::new(36, Cbw::Cbw40);
        let connect_req = fidl_mlme::ConnectRequest {
            selected_bss: fake_fidl_bss_description!(Open,
                ssid: Ssid::try_from("ssid").unwrap().into(),
                bssid: BSSID.to_array(),
                channel: channel.clone(),
            ),
            connect_failure_timeout: 100,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![],
        };
        me.handle_mlme_req(wlan_sme::MlmeRequest::Connect(connect_req))
            .await
            .expect("Failed to send MlmeRequest::Connect.");

        // Verify an event was queued up in the timer.
        assert_variant!(drain_timeouts(&mut m.time_stream).get(&TimedEventClass::Connecting), Some(ids) => {
            assert_eq!(ids.len(), 1);
        });

        // Auth frame
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        let (_frame, _txflags) = m.fake_device_state.lock().wlan_queue.remove(0);

        // Mock auth frame response from the AP
        #[rustfmt::skip]
        let auth_resp_success = vec![
            // Mgmt Header:
            0b1011_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            7, 7, 7, 7, 7, 7, // Addr1
            7, 7, 7, 7, 7, 7, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Auth Header:
            0, 0, // Algorithm Number (Open)
            2, 0, // Txn Sequence Number
            0, 0, // Status Code
        ];
        me.on_mac_frame_rx(
            &auth_resp_success[..],
            MockWlanRxInfo::with_channel(channel.into()).into(),
            0.into(),
        )
        .await;

        // Verify association request frame was went to AP
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        let (frame, _txflags) = m.fake_device_state.lock().wlan_queue.remove(0);
        #[rustfmt::skip]
        let expected = vec![
            // Mgmt header:
            0, 0, // FC
            0, 0, // Duration
            6, 6, 6, 6, 6, 6, // addr1
            7, 7, 7, 7, 7, 7, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x20, 0, // Sequence Control
            // Association Request header:
            0x01, 0x00, // capability info
            0, 0, // listen interval
            // IEs
            0, 4, // SSID id and length
            0x73, 0x73, 0x69, 0x64, // SSID
            1, 6, // supp rates id and length
            2, 4, 11, 22, 48, 96, // supp rates
            45, 26, // HT Cap id and length
            0x63, 0, 0x17, 0xff, 0, 0, 0, // HT Cap \
            0, 0, 0, 0, 0, 0, 0, 0, 1, // HT Cap \
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // HT Cap
            191, 12, // VHT Cap id and length
            50, 80, 128, 15, 254, 255, 0, 0, 254, 255, 0, 0, // VHT Cap
        ];
        assert_eq!(&frame[..], &expected[..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_connect_timeout() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;
        let connect_req = fidl_mlme::ConnectRequest {
            selected_bss: fake_fidl_bss_description!(Open, bssid: BSSID.to_array()),
            connect_failure_timeout: 100,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![],
        };
        me.handle_mlme_req(wlan_sme::MlmeRequest::Connect(connect_req))
            .await
            .expect("Failed to send MlmeRequest::Connect.");

        // Verify an event was queued up in the timer.
        let (event, _id) = assert_variant!(drain_timeouts(&mut m.time_stream).get(&TimedEventClass::Connecting), Some(events) => {
            assert_eq!(events.len(), 1);
            events[0].clone()
        });

        // Quick check that a frame was sent (this is authentication frame).
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        let (_frame, _txflags) = m.fake_device_state.lock().wlan_queue.remove(0);

        // Send connect timeout
        me.handle_timed_event(event).await;

        // Verify a connect confirm message was sent
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("expect msg");
        assert_eq!(
            msg,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: BSSID.to_array(),
                result_code: fidl_ieee80211::StatusCode::RejectedSequenceTimeout,
                association_id: 0,
                association_ies: vec![],
            },
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_reconnect_no_sta() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;

        let reconnect_req = fidl_mlme::ReconnectRequest { peer_sta_address: [1, 2, 3, 4, 5, 6] };
        let result = me.handle_mlme_req(wlan_sme::MlmeRequest::Reconnect(reconnect_req)).await;
        assert_variant!(result, Err(Error::Status(_, zx::Status::BAD_STATE)));

        // Verify a connect confirm message was sent
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("expect msg");
        assert_eq!(
            msg,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: [1, 2, 3, 4, 5, 6],
                result_code: fidl_ieee80211::StatusCode::DeniedNoAssociationExists,
                association_id: 0,
                association_ies: vec![],
            },
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_respond_to_get_iface_stats_with_error_status() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;

        let (responder, receiver) = Responder::new();
        me.handle_mlme_req(wlan_sme::MlmeRequest::GetIfaceStats(responder))
            .await
            .expect("Failed to send MlmeRequest::GetIfaceStats.");
        assert_eq!(
            receiver.await,
            Ok(fidl_mlme::GetIfaceStatsResponse::ErrorStatus(zx::sys::ZX_ERR_NOT_SUPPORTED))
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_respond_to_get_iface_histogram_stats_with_error_status() {
        let mut m = MockObjects::new().await;
        let mut me = m.make_mlme().await;

        let (responder, receiver) = Responder::new();
        me.handle_mlme_req(wlan_sme::MlmeRequest::GetIfaceHistogramStats(responder))
            .await
            .expect("Failed to send MlmeRequest::GetIfaceHistogramStats");
        assert_eq!(
            receiver.await,
            Ok(fidl_mlme::GetIfaceHistogramStatsResponse::ErrorStatus(
                zx::sys::ZX_ERR_NOT_SUPPORTED
            ))
        );
    }

    #[test]
    fn drop_mgmt_frame_wrong_bssid() {
        let frame = [
            // Mgmt header 1101 for action frame
            0b11010000, 0b00000000, // frame control
            0, 0, // duration
            7, 7, 7, 7, 7, 7, // addr1
            6, 6, 6, 6, 6, 6, // addr2
            0, 0, 0, 0, 0, 0, // addr3 (bssid should have been [6; 6])
            0x10, 0, // sequence control
        ];
        let frame = mac::MacFrame::parse(&frame[..], false).unwrap();
        assert_eq!(false, make_client_station().should_handle_frame(&frame));
    }

    #[test]
    fn drop_mgmt_frame_wrong_dst_addr() {
        let frame = [
            // Mgmt header 1101 for action frame
            0b11010000, 0b00000000, // frame control
            0, 0, // duration
            0, 0, 0, 0, 0, 0, // addr1 (dst_addr should have been [7; 6])
            6, 6, 6, 6, 6, 6, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // sequence control
        ];
        let frame = mac::MacFrame::parse(&frame[..], false).unwrap();
        assert_eq!(false, make_client_station().should_handle_frame(&frame));
    }

    #[test]
    fn mgmt_frame_ok_broadcast() {
        let frame = [
            // Mgmt header 1101 for action frame
            0b11010000, 0b00000000, // frame control
            0, 0, // duration
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // addr1 (dst_addr is broadcast)
            6, 6, 6, 6, 6, 6, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // sequence control
        ];
        let frame = mac::MacFrame::parse(&frame[..], false).unwrap();
        assert_eq!(true, make_client_station().should_handle_frame(&frame));
    }

    #[test]
    fn mgmt_frame_ok_client_addr() {
        let frame = [
            // Mgmt header 1101 for action frame
            0b11010000, 0b00000000, // frame control
            0, 0, // duration
            7, 7, 7, 7, 7, 7, // addr1 (dst_addr should have been [7; 6])
            6, 6, 6, 6, 6, 6, // addr2
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // sequence control
        ];
        let frame = mac::MacFrame::parse(&frame[..], false).unwrap();
        assert_eq!(true, make_client_station().should_handle_frame(&frame));
    }

    #[test]
    fn drop_data_frame_wrong_bssid() {
        let frame = [
            // Data header 0100
            0b01001000,
            0b00000010, // frame control. right 2 bits of octet 2: from_ds(1), to_ds(0)
            0, 0, // duration
            7, 7, 7, 7, 7, 7, // addr1 (dst_addr)
            0, 0, 0, 0, 0, 0, // addr2 (bssid should have been [6; 6])
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // sequence control
        ];
        let frame = mac::MacFrame::parse(&frame[..], false).unwrap();
        assert_eq!(false, make_client_station().should_handle_frame(&frame));
    }

    #[test]
    fn drop_data_frame_wrong_dst_addr() {
        let frame = [
            // Data header 0100
            0b01001000,
            0b00000010, // frame control. right 2 bits of octet 2: from_ds(1), to_ds(0)
            0, 0, // duration
            0, 0, 0, 0, 0, 0, // addr1 (dst_addr should have been [7; 6])
            6, 6, 6, 6, 6, 6, // addr2 (bssid)
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // sequence control
        ];
        let frame = mac::MacFrame::parse(&frame[..], false).unwrap();
        assert_eq!(false, make_client_station().should_handle_frame(&frame));
    }

    #[test]
    fn data_frame_ok_broadcast() {
        let frame = [
            // Data header 0100
            0b01001000,
            0b00000010, // frame control. right 2 bits of octet 2: from_ds(1), to_ds(0)
            0, 0, // duration
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // addr1 (dst_addr is broadcast)
            6, 6, 6, 6, 6, 6, // addr2 (bssid)
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // sequence control
        ];
        let frame = mac::MacFrame::parse(&frame[..], false).unwrap();
        assert_eq!(true, make_client_station().should_handle_frame(&frame));
    }

    #[test]
    fn data_frame_ok_client_addr() {
        let frame = [
            // Data header 0100
            0b01001000,
            0b00000010, // frame control. right 2 bits of octet 2: from_ds(1), to_ds(0)
            0, 0, // duration
            7, 7, 7, 7, 7, 7, // addr1 (dst_addr)
            6, 6, 6, 6, 6, 6, // addr2 (bssid)
            6, 6, 6, 6, 6, 6, // addr3
            0x10, 0, // sequence control
        ];
        let frame = mac::MacFrame::parse(&frame[..], false).unwrap();
        assert_eq!(true, make_client_station().should_handle_frame(&frame));
    }
}
