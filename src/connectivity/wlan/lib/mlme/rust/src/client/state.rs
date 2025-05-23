// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A state machine for associating a Client to a BSS.
//! Note: This implementation only supports simultaneous authentication with exactly one STA, the
//! AP. While 802.11 explicitly allows - and sometime requires - authentication with more than one
//! STA, Fuchsia does intentionally not yet support this use-case.

use crate::akm_algorithm as akm;
use crate::block_ack::{BlockAckState, Closed};
use crate::client::lost_bss::LostBssCounter;
use crate::client::{BoundClient, ParsedAssociateResp, TimedEvent};
use crate::ddk_converter::{get_rssi_dbm, softmac_key_configuration_from_mlme};
use crate::device::DeviceOps;
use crate::disconnect::LocallyInitiated;
use crate::error::Error;
use fuchsia_trace::Id as TraceId;
use ieee80211::{Bssid, MacAddr, MacAddrBytes};
use log::{debug, error, info, trace, warn};
use wlan_common::buffer_reader::BufferReader;
use wlan_common::capabilities::{intersect_with_ap_as_client, ApCapabilities, StaCapabilities};
use wlan_common::energy::DecibelMilliWatt;
use wlan_common::mac::{self, BeaconHdr};
use wlan_common::stats::SignalStrengthAverage;
use wlan_common::timer::{EventHandle, Timer};
use wlan_common::{ie, tim};
use wlan_statemachine::*;
use zerocopy::SplitByteSlice;
use {
    fidl_fuchsia_wlan_ieee80211 as fidl_ieee80211, fidl_fuchsia_wlan_internal as fidl_internal,
    fidl_fuchsia_wlan_mlme as fidl_mlme, fidl_fuchsia_wlan_softmac as fidl_softmac,
    wlan_trace as wtrace,
};

/// Reconnect timeout in Beacon periods.
/// If no association response was received from the BSS within this time window, an association is
/// considered to have failed.
const RECONNECT_TIMEOUT_BCN_PERIODS: u16 = 10;

/// Number of beacon intervals which beacon is not seen before we declare BSS as lost
pub const DEFAULT_AUTO_DEAUTH_TIMEOUT_BEACON_COUNT: u32 = 100;

/// Number of beacon intervals between association status check (signal report or auto-deatuh).
pub const ASSOCIATION_STATUS_TIMEOUT_BEACON_COUNT: u32 = 10;

/// Client joined a BSS (synchronized timers and prepared its underlying hardware).
/// At this point the Client is able to listen to frames on the BSS' channel.
#[derive(Debug)]
pub struct Joined;

impl Joined {
    /// Initiates an open authentication with the currently joined BSS.
    /// The returned state is unchanged in an error case. Otherwise, the state transitions into
    /// "Authenticating".
    /// Returns Ok(AkmAlgorithm) if authentication request was sent successfully, Err(()) otherwise.
    async fn start_authenticating<D: DeviceOps>(
        &self,
        sta: &mut BoundClient<'_, D>,
    ) -> Result<akm::AkmAlgorithm, ()> {
        let auth_type = sta.sta.connect_req.auth_type;
        let algorithm_candidate = match auth_type {
            fidl_mlme::AuthenticationTypes::OpenSystem => Ok(akm::AkmAlgorithm::OpenSupplicant),
            fidl_mlme::AuthenticationTypes::Sae => Ok(akm::AkmAlgorithm::Sae),
            _ => {
                error!("Unhandled authentication algorithm: {:?}", auth_type);
                Err(fidl_ieee80211::StatusCode::UnsupportedAuthAlgorithm)
            }
        };

        let result = match algorithm_candidate {
            Err(e) => Err(e),
            Ok(mut algorithm) => match algorithm.initiate(sta) {
                Ok(akm::AkmState::Failed) => {
                    Err(fidl_ieee80211::StatusCode::UnsupportedAuthAlgorithm)
                }
                Err(_) => Err(fidl_ieee80211::StatusCode::RefusedReasonUnspecified),
                Ok(_) => Ok(algorithm),
            },
        };

        match result {
            Ok(algorithm) => Ok(algorithm),
            Err(status_code) => {
                sta.send_connect_conf_failure(status_code);
                let _ = sta
                    .clear_association()
                    .await
                    .map_err(|e| error!("Failed to clear association: {}", e));
                Err(())
            }
        }
    }

    async fn on_sme_deauthenticate<D: DeviceOps>(&mut self, sta: &mut BoundClient<'_, D>) {
        let _ =
            sta.clear_association().await.map_err(|e| warn!("Failed to clear association: {}", e));
    }
}

#[derive(Debug)]
enum AuthProgress {
    Complete,
    InProgress,
    Failed,
}

/// Client issued an authentication request frame to its joined BSS prior to joining this state.
/// At this point the client is waiting for an authentication response frame from the client.
/// Note: This assumes Open System authentication.
#[derive(Debug)]
pub struct Authenticating {
    algorithm: akm::AkmAlgorithm,
}

impl Authenticating {
    fn new(algorithm: akm::AkmAlgorithm) -> Self {
        Self { algorithm }
    }

    async fn akm_state_update_notify_sme<D: DeviceOps>(
        &self,
        sta: &mut BoundClient<'_, D>,
        state: Result<akm::AkmState, anyhow::Error>,
    ) -> AuthProgress {
        match state {
            Ok(akm::AkmState::AuthComplete) => match self.start_associating(sta) {
                Ok(()) => AuthProgress::Complete,
                Err(()) => AuthProgress::Failed,
            },
            Ok(akm::AkmState::InProgress) => AuthProgress::InProgress,
            Ok(akm::AkmState::Failed) => {
                error!("authentication with BSS failed");
                // TODO(https://fxbug.dev/42164548): pass the status code from the original auth frame
                sta.send_connect_conf_failure(fidl_ieee80211::StatusCode::RefusedReasonUnspecified);
                let _ = sta
                    .clear_association()
                    .await
                    .map_err(|e| error!("Failed to clear association: {}", e));
                AuthProgress::Failed
            }
            Err(e) => {
                error!("Internal error while authenticating: {}", e);
                // TODO(https://fxbug.dev/42164548): pass the status code from the original auth frame
                sta.send_connect_conf_failure(fidl_ieee80211::StatusCode::RefusedReasonUnspecified);
                let _ = sta
                    .clear_association()
                    .await
                    .map_err(|e| error!("Failed to clear association: {}", e));
                AuthProgress::Failed
            }
        }
    }

    /// Initiates an association with the currently joined BSS.
    /// Returns Ok(()) if association request was sent successfully.
    /// Otherwise an Err(()) is returned and a CONNECT.confirm message to its SME peer.
    fn start_associating<D: DeviceOps>(&self, sta: &mut BoundClient<'_, D>) -> Result<(), ()> {
        sta.send_assoc_req_frame().map_err(|e| {
            error!("Error sending association request frame: {}", e);
            sta.send_connect_conf_failure(fidl_ieee80211::StatusCode::RefusedTemporarily);
        })
    }

    /// Processes an inbound authentication frame.
    /// SME will be notified via a CONNECT.confirm message if there's a failure.
    /// Returns AuthProgress::Complete if the authentication was successful and
    //  association has started.
    /// Returns AuthProgress::InProgress if authentication is still ongoing.
    /// Returns AuthProgress::Failed if failed to authenticate or start association request.
    async fn on_auth_frame<B: SplitByteSlice, D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        auth_frame: mac::AuthFrame<B>,
    ) -> AuthProgress {
        wtrace::duration!(c"Authenticating::on_auth_frame");

        let state = self.algorithm.handle_auth_frame(sta, auth_frame);
        self.akm_state_update_notify_sme(sta, state).await
    }

    /// Processes an SAE response from SME.
    /// This indicates that an SAE handshake has completed, successful or otherwise.
    /// On success, authentication is complete and association has started.
    async fn on_sme_sae_resp<D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        resp: fidl_mlme::SaeHandshakeResponse,
    ) -> AuthProgress {
        let state = self.algorithm.handle_sae_resp(sta, resp.status_code);
        self.akm_state_update_notify_sme(sta, state).await
    }

    /// Processes a request from SME to transmit an SAE authentication frame to a peer.
    async fn on_sme_sae_tx<D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        tx: fidl_mlme::SaeFrame,
    ) -> AuthProgress {
        let state =
            self.algorithm.handle_sme_sae_tx(sta, tx.seq_num, tx.status_code, &tx.sae_fields[..]);
        self.akm_state_update_notify_sme(sta, state).await
    }

    /// Processes an inbound deauthentication frame.
    /// This always results in an MLME-AUTHENTICATE.confirm message to MLME's SME peer.
    /// The pending authentication timeout will be canceled in this process.
    async fn on_deauth_frame<D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        deauth_hdr: &mac::DeauthHdr,
    ) {
        wtrace::duration!(c"Authenticating::on_deauth_frame");

        info!(
            "received spurious deauthentication frame while authenticating with BSS (unusual); \
             authentication failed: {:?}",
            { deauth_hdr.reason_code }
        );

        sta.sta.connect_timeout.take();
        sta.send_connect_conf_failure(fidl_ieee80211::StatusCode::SpuriousDeauthOrDisassoc);
        let _ =
            sta.clear_association().await.map_err(|e| error!("Failed to clear association: {}", e));
    }

    async fn on_sme_deauthenticate<D: DeviceOps>(&mut self, sta: &mut BoundClient<'_, D>) {
        sta.sta.connect_timeout.take();
        let _ =
            sta.clear_association().await.map_err(|e| error!("Failed to clear association: {}", e));
    }
}

#[derive(Debug)]
pub struct Authenticated;

impl Authenticated {
    fn on_sme_reconnect<D: DeviceOps>(
        &self,
        sta: &mut BoundClient<'_, D>,
        req: fidl_mlme::ReconnectRequest,
    ) -> Result<EventHandle, ()> {
        let peer_sta_address: Bssid = req.peer_sta_address.into();
        if peer_sta_address == sta.sta.connect_req.selected_bss.bssid {
            match sta.send_assoc_req_frame() {
                Ok(()) => {
                    // Setting timeout in term of beacon period allows us to adjust the realtime
                    // timeout in hw-sim, where we set a longer duration in case of a slowbot.
                    let duration = sta.sta.beacon_period() * RECONNECT_TIMEOUT_BCN_PERIODS;
                    Ok(sta.ctx.timer.schedule_after(duration, TimedEvent::Reassociating))
                }
                Err(e) => {
                    error!("Error sending association request frame: {}", e);
                    sta.send_connect_conf_failure(fidl_ieee80211::StatusCode::RefusedTemporarily);
                    Err(())
                }
            }
        } else {
            info!("received reconnect request for a different BSSID, ignoring");
            sta.send_connect_conf_failure_with_bssid(
                peer_sta_address,
                fidl_ieee80211::StatusCode::NotInSameBss,
            );
            Err(())
        }
    }
}

/// Client has sent an association request frame to the AP.
/// At this point, client is waiting for an association response frame from the AP.
#[derive(Debug, Default)]
pub struct Associating {
    /// This field is only populated when MLME is reconnecting after a disassociation.
    reconnect_timeout: Option<EventHandle>,
}

impl Associating {
    fn new_with_reconnect_timeout(reconnect_timeout: EventHandle) -> Self {
        Self { reconnect_timeout: Some(reconnect_timeout) }
    }

    /// Processes an inbound association response frame.
    /// SME will be notified via an MLME-ASSOCIATE.confirm message whether the association
    /// with the BSS was successful.
    /// Returns Ok(()) if the association was successful, otherwise Err(()).
    /// Note: The pending authentication timeout will be canceled in any case.
    async fn on_assoc_resp_frame<B: SplitByteSlice, D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        assoc_resp_frame: mac::AssocRespFrame<B>,
    ) -> Result<Association, ()> {
        wtrace::duration!(c"Associating::on_assoc_resp_frame");

        // TODO(https://fxbug.dev/42172907): All reserved values mapped to REFUSED_REASON_UNSPECIFIED.
        match Option::<fidl_ieee80211::StatusCode>::from(
            assoc_resp_frame.assoc_resp_hdr.status_code,
        )
        .unwrap_or(fidl_ieee80211::StatusCode::RefusedReasonUnspecified)
        {
            fidl_ieee80211::StatusCode::Success => (),
            status_code => {
                error!("association with BSS failed: {:?}", status_code);
                sta.send_connect_conf_failure(status_code);
                return Err(());
            }
        }

        let aid = assoc_resp_frame.assoc_resp_hdr.aid;
        let parsed_assoc_resp = ParsedAssociateResp::parse(&assoc_resp_frame);
        let ap_capabilities = ApCapabilities(StaCapabilities {
            capability_info: parsed_assoc_resp.capabilities,
            rates: parsed_assoc_resp.rates.clone(),
            ht_cap: parsed_assoc_resp.ht_cap.clone(),
            vht_cap: parsed_assoc_resp.vht_cap.clone(),
        });
        let negotiated_cap =
            match intersect_with_ap_as_client(&sta.sta.client_capabilities, &ap_capabilities) {
                Ok(cap) => cap,
                Err(e) => {
                    // This is unlikely to happen with any spec-compliant AP. In case the
                    // user somehow decided to connect to a malicious AP, reject and reset.
                    // Log at ERROR level to raise visibility of when this event occurs.
                    error!(
                        "Associate terminated because AP's capabilities in association \
                            response is different from beacon. Error: {}",
                        e
                    );
                    sta.send_connect_conf_failure(
                        fidl_ieee80211::StatusCode::RefusedCapabilitiesMismatch,
                    );
                    return Err(());
                }
            };

        let (ap_ht_op, ap_vht_op) = extract_ht_vht_op(&assoc_resp_frame);

        let main_channel = match sta.channel_state.get_main_channel() {
            Some(main_channel) => main_channel,
            None => {
                error!("MLME in associating state but no main channel is set");
                sta.send_connect_conf_failure(fidl_ieee80211::StatusCode::RefusedReasonUnspecified);
                return Err(());
            }
        };

        // TODO(https://fxbug.dev/42104064): Determine for each outbound data frame,
        // given the result of the dynamic capability negotiation, data frame
        // classification, and QoS policy.
        //
        // Aruba / Ubiquiti are confirmed to be compatible with QoS field for the
        // BlockAck session, independently of 40MHz operation.
        let qos = negotiated_cap.ht_cap.is_some();

        let assoc_cfg = fidl_softmac::WlanAssociationConfig {
            bssid: Some(sta.sta.bssid().to_array()),
            aid: Some(aid),
            // In the association request we sent out earlier, listen_interval is always set to 0,
            // indicating the client never enters power save mode.
            listen_interval: Some(0),
            channel: Some(main_channel),
            qos: Some(qos),
            wmm_params: None,
            rates: Some(negotiated_cap.rates.iter().map(|r| r.0).collect()),
            capability_info: Some(negotiated_cap.capability_info.raw()),
            ht_cap: negotiated_cap.ht_cap.map(Into::into),
            vht_cap: negotiated_cap.vht_cap.map(Into::into),
            ht_op: ap_ht_op.clone().map(From::from),
            vht_op: ap_vht_op.clone().map(From::from),
            ..Default::default()
        };

        if let Err(status) = sta.ctx.device.notify_association_complete(assoc_cfg).await {
            // Device cannot handle this association. Something is seriously wrong.
            error!("device failed to configure association: {}", status);
            sta.send_connect_conf_failure(fidl_ieee80211::StatusCode::RefusedReasonUnspecified);
            return Err(());
        }

        let (_, assoc_resp_body) = assoc_resp_frame.into_assoc_resp_body();
        sta.send_connect_conf_success(aid, assoc_resp_body.deref());
        let controlled_port_open = !sta.sta.eapol_required();
        if controlled_port_open {
            if let Err(e) = sta.ctx.device.set_ethernet_up().await {
                // TODO(https://fxbug.dev/42175857) - Consider returning an Err here.
                error!("Cannot set ethernet to UP. Status: {}", e);
            }
        }
        let lost_bss_counter = LostBssCounter::start(
            sta.sta.beacon_period(),
            DEFAULT_AUTO_DEAUTH_TIMEOUT_BEACON_COUNT,
        );

        let status_check_timeout =
            schedule_association_status_timeout(sta.sta.beacon_period(), &mut sta.ctx.timer);

        Ok(Association {
            aid,
            assoc_resp_ies: assoc_resp_body.to_vec(),
            controlled_port_open,
            ap_ht_op,
            ap_vht_op,
            qos: Qos::from(qos),
            lost_bss_counter,
            status_check_timeout,
            signal_strength_average: SignalStrengthAverage::new(),
            block_ack_state: StateMachine::new(BlockAckState::from(State::new(Closed))),
        })
    }

    /// Processes an inbound disassociation frame.
    /// Note: APs should never send disassociation frames without having established a valid
    /// association with the Client. However, to maximize interoperability disassociation frames
    /// are handled in this state as well and treated similar to unsuccessful association responses.
    /// This always results in an MLME-ASSOCIATE.confirm message to MLME's SME peer.
    fn on_disassoc_frame<D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        _disassoc_hdr: &mac::DisassocHdr,
    ) {
        wtrace::duration!(c"Associating::on_disassoc_frame");
        warn!("received unexpected disassociation frame while associating");
        sta.send_connect_conf_failure(fidl_ieee80211::StatusCode::SpuriousDeauthOrDisassoc);
    }

    /// Processes an inbound deauthentication frame.
    /// This always results in an MLME-ASSOCIATE.confirm message to MLME's SME peer.
    /// The pending association timeout will be canceled in this process.
    async fn on_deauth_frame<D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        deauth_hdr: &mac::DeauthHdr,
    ) {
        wtrace::duration!(c"Associating::on_deauth_frame");
        info!(
            "received spurious deauthentication frame while associating with BSS (unusual); \
             association failed: {:?}",
            { deauth_hdr.reason_code }
        );
        sta.send_connect_conf_failure(fidl_ieee80211::StatusCode::SpuriousDeauthOrDisassoc);
        let _ =
            sta.clear_association().await.map_err(|e| error!("Failed to clear association: {}", e));
    }

    async fn on_sme_deauthenticate<D: DeviceOps>(&mut self, sta: &mut BoundClient<'_, D>) {
        sta.sta.connect_timeout.take();
        let _ =
            sta.clear_association().await.map_err(|e| error!("Failed to clear association: {}", e));
    }
}

/// Extract HT Operation and VHT Operation IEs from the association response frame.
/// If either IE is of an incorrect length, it will be ignored.
fn extract_ht_vht_op<B: SplitByteSlice>(
    assoc_resp_frame: &mac::AssocRespFrame<B>,
) -> (Option<ie::HtOperation>, Option<ie::VhtOperation>) {
    let mut ht_op = None;
    let mut vht_op = None;
    for (id, body) in assoc_resp_frame.ies() {
        match id {
            ie::Id::HT_OPERATION => match ie::parse_ht_operation(body) {
                Ok(parsed_ht_op) => ht_op = Some(zerocopy::Ref::read(&parsed_ht_op)),
                Err(e) => {
                    error!("Invalid HT Operation: {}", e);
                    continue;
                }
            },
            ie::Id::VHT_OPERATION => match ie::parse_vht_operation(body) {
                Ok(parsed_vht_op) => vht_op = Some(zerocopy::Ref::read(&parsed_vht_op)),
                Err(e) => {
                    error!("Invalid VHT Operation: {}", e);
                    continue;
                }
            },
            _ => (),
        }
    }
    (ht_op, vht_op)
}

pub fn schedule_association_status_timeout(
    beacon_period: zx::MonotonicDuration,
    timer: &mut Timer<TimedEvent>,
) -> StatusCheckTimeout {
    let duration = beacon_period * ASSOCIATION_STATUS_TIMEOUT_BEACON_COUNT;
    StatusCheckTimeout {
        next_event: Some(timer.schedule_after(duration, TimedEvent::AssociationStatusCheck)),
    }
}

#[derive(Debug, PartialEq)]
pub enum Qos {
    Enabled,
    Disabled,
}

impl From<bool> for Qos {
    fn from(b: bool) -> Self {
        if b {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }
}

impl Qos {
    fn is_enabled(&self) -> bool {
        *self == Self::Enabled
    }
}

#[derive(Debug)]
pub struct StatusCheckTimeout {
    next_event: Option<EventHandle>,
}

#[derive(Debug)]
pub struct Association {
    pub aid: mac::Aid,
    pub assoc_resp_ies: Vec<u8>,

    /// Represents an 802.1X controlled port.
    /// A closed controlled port only processes EAP frames while an open one processes any frames.
    pub controlled_port_open: bool,

    // TODO(https://fxbug.dev/335283785): Remove or explain unused code.
    #[allow(dead_code)]
    pub ap_ht_op: Option<ie::HtOperation>,
    // TODO(https://fxbug.dev/335283785): Remove or explain unused code.
    #[allow(dead_code)]
    pub ap_vht_op: Option<ie::VhtOperation>,

    /// Whether to set QoS bit when MLME constructs an outgoing WLAN data frame.
    /// Currently, QoS is enabled if the associated PHY is HT or VHT.
    pub qos: Qos,

    /// `lost_bss_counter` is used to determine if the BSS is still alive nearby. It is started
    /// when the client is associated.
    pub lost_bss_counter: LostBssCounter,

    /// |timeout| is the timeout that is scheduled for the association status check, which includes
    /// a) sending signal strength report to SME and b) triggering auto-deauth if necessary.
    /// It will be cancelled when the client go off-channel for scanning and scheduled again when
    /// back on channel.
    pub status_check_timeout: StatusCheckTimeout,
    pub signal_strength_average: SignalStrengthAverage,

    // TODO(https://fxbug.dev/335283785): Remove or explain unused code.
    #[allow(dead_code)]
    pub block_ack_state: StateMachine<BlockAckState>,
}

/// Client received a "successful" association response from the BSS.
#[derive(Debug)]
pub struct Associated(pub Association);

impl Associated {
    /// Processes an inbound disassociation frame.
    /// This always results in an MLME-DISASSOCIATE.indication message to MLME's SME peer.
    async fn on_disassoc_frame<D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        disassoc_hdr: &mac::DisassocHdr,
    ) {
        wtrace::duration!(c"Associated::on_disassoc_frame");
        self.pre_leaving_associated_state(sta).await;
        let reason_code = fidl_ieee80211::ReasonCode::from_primitive(disassoc_hdr.reason_code.0)
            .unwrap_or(fidl_ieee80211::ReasonCode::UnspecifiedReason);
        sta.send_disassoc_ind(reason_code, LocallyInitiated(false));
    }

    /// Sends an MLME-DEAUTHENTICATE.indication message to MLME's SME peer.
    async fn on_deauth_frame<D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        deauth_hdr: &mac::DeauthHdr,
    ) {
        wtrace::duration!(c"Associated::on_deauth_frame");
        self.pre_leaving_associated_state(sta).await;
        let reason_code = fidl_ieee80211::ReasonCode::from_primitive(deauth_hdr.reason_code.0)
            .unwrap_or(fidl_ieee80211::ReasonCode::UnspecifiedReason);
        sta.send_deauthenticate_ind(reason_code, LocallyInitiated(false));
        let _ =
            sta.clear_association().await.map_err(|e| error!("Failed to clear association: {}", e));
    }

    /// Process every inbound management frame before its being handed off to a more specific
    /// handler.
    fn on_any_mgmt_frame<D: DeviceOps>(
        &self,
        sta: &mut BoundClient<'_, D>,
        mgmt_hdr: &mac::MgmtHdr,
    ) {
        self.request_bu_if_available(sta, mgmt_hdr.frame_ctrl, mgmt_hdr.addr1);
    }

    /// Sends PS-POLL requests if the FrameControl's more_data bit is set, and the received frame
    /// was addressed for this STA. No-op if the controlled port is closed.
    fn request_bu_if_available<D: DeviceOps>(
        &self,
        sta: &mut BoundClient<'_, D>,
        fc: mac::FrameControl,
        dst_addr: MacAddr,
    ) {
        if !self.0.controlled_port_open {
            return;
        }
        // IEEE Std. 802.11-2016, 9.2.4.1.8
        if fc.more_data() && dst_addr == sta.sta.iface_mac {
            let _result = sta.send_ps_poll_frame(self.0.aid);
        }
    }

    fn extract_and_record_signal_dbm(&mut self, rx_info: fidl_softmac::WlanRxInfo) {
        get_rssi_dbm(rx_info)
            .map(|rssi_dbm| self.0.signal_strength_average.add(DecibelMilliWatt(rssi_dbm)));
    }

    /// Process and inbound beacon frame.
    /// Resets LostBssCounter, check buffered frame if available.
    async fn on_beacon_frame<B: SplitByteSlice, D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        header: &BeaconHdr,
        elements: B,
    ) {
        wtrace::duration!(c"Associated::on_beacon_frame");
        self.0.lost_bss_counter.reset();
        // TODO(b/253637931): Add metrics to track channel switch counts and success rates.
        if let Err(e) =
            sta.channel_state.bind(sta.ctx, sta.scanner).handle_beacon(header, &elements[..]).await
        {
            warn!("Failed to handle channel switch announcement: {}", e);
        }
        for (id, body) in ie::Reader::new(elements) {
            match id {
                ie::Id::TIM => match ie::parse_tim(body) {
                    Ok(ie::TimView { header, bitmap }) => {
                        if tim::is_traffic_buffered(header.bmp_ctrl.offset(), &bitmap, self.0.aid) {
                            let _result = sta.send_ps_poll_frame(self.0.aid);
                        }
                    }
                    _ => (),
                },

                _ => (),
            }
        }
    }

    /// Extracts aggregated and non-aggregated MSDUs from the data frame.
    /// Handles all data subtypes.
    /// EAPoL MSDUs are forwarded to SME via an MLME-EAPOL.indication message independent of the
    /// STA's current controlled port status.
    /// All other MSDUs are converted into Ethernet II frames and forwarded via the device to
    /// Fuchsia's Netstack if the STA's controlled port is open.
    /// NULL-Data frames are interpreted as "Keep Alive" requests and responded with NULL data
    /// frames if the STA's controlled port is open.
    fn on_data_frame<B: SplitByteSlice, D: DeviceOps>(
        &self,
        sta: &mut BoundClient<'_, D>,
        data_frame: mac::DataFrame<B>,
        async_id: TraceId,
    ) {
        const MSDU_TRACE_NAME: &'static std::ffi::CStr = c"States::on_data_frame => MSDU";

        wtrace::duration!(c"States::on_data_frame");

        self.request_bu_if_available(
            sta,
            data_frame.frame_ctrl(),
            mac::data_dst_addr(&data_frame.fixed_fields),
        );

        // Handle NULL data frames independently of the controlled port's status.
        if data_frame.data_subtype().null() {
            if let Err(e) = sta.send_keep_alive_resp_frame() {
                error!("error sending keep alive frame: {}", e);
            }
        }

        // Handle aggregated and non-aggregated MSDUs.
        for msdu in data_frame {
            wtrace::duration_begin!(MSDU_TRACE_NAME);

            match msdu.llc_frame.hdr.protocol_id.to_native() {
                // Forward EAPoL frames to SME independently of the controlled port's status.
                mac::ETHER_TYPE_EAPOL => {
                    let mac::Msdu { dst_addr, src_addr, llc_frame } = msdu;
                    if let Err(e) =
                        sta.send_eapol_indication(src_addr, dst_addr, &llc_frame.body[..])
                    {
                        wtrace::duration_end!(
                            MSDU_TRACE_NAME,
                            "status" => "failure sending EAPOL indication",
                        );
                        error!("error sending MLME-EAPOL.indication: {}", e);
                    } else {
                        wtrace::duration_end!(
                            MSDU_TRACE_NAME,
                            "status" => "sent EAPOL indication",
                        );
                    }
                }
                // Deliver non-EAPoL MSDUs only if the controlled port is open.
                _ if self.0.controlled_port_open => {
                    if let Err(e) = sta.deliver_msdu(msdu) {
                        wtrace::duration_end!(
                            MSDU_TRACE_NAME,
                            "status" => "failure delivering MSDU",
                        );
                        error!("error while handling data frame: {}", e);
                    } else {
                        wtrace::duration_end!(
                            MSDU_TRACE_NAME,
                            "status" => "delivered MSDU",
                        );
                    }
                }
                // Drop all non-EAPoL MSDUs if the controlled port is closed.
                _ => {
                    wtrace::duration_end!(
                        MSDU_TRACE_NAME,
                        "status" => "dropping MSDU. controlled port closed.",
                    );
                }
            }
        }
        wtrace::async_end_wlansoftmac_rx(async_id, "completed data frame processing");
    }

    fn on_eth_frame<B: SplitByteSlice, D: DeviceOps>(
        &self,
        sta: &mut BoundClient<'_, D>,
        frame: B,
        async_id: TraceId,
    ) -> Result<(), Error> {
        wtrace::duration!(c"Associated::on_eth_frame");
        let mac::EthernetFrame { hdr, body } = match mac::EthernetFrame::parse(frame) {
            Some(eth_frame) => eth_frame,
            None => {
                return Err(Error::Status(
                    format!("Ethernet frame too short"),
                    zx::Status::IO_DATA_INTEGRITY,
                ));
            }
        };

        if !self.0.controlled_port_open {
            return Err(Error::Status(
                format!("Ethernet dropped. RSN not established"),
                zx::Status::BAD_STATE,
            ));
        }

        sta.send_data_frame(
            hdr.sa,
            hdr.da,
            sta.sta.eapol_required(),
            self.0.qos.is_enabled(),
            hdr.ether_type.to_native(),
            &body,
            Some(async_id),
        )
    }

    fn on_block_ack_frame<B: SplitByteSlice, D>(
        &mut self,
        _sta: &mut BoundClient<'_, D>,
        _action: mac::BlockAckAction,
        _body: B,
    ) {
        // TODO(https://fxbug.dev/42104687): Handle BlockAck frames. The following code has been disabled as a
        //                        fix for https://fxbug.dev/42180615. Without this code, the BlockAck state
        //                        machine is dormant and, importantly, never transmits BlockAck
        //                        frames.
        //self.0.block_ack_state.replace_state(|state| state.on_block_ack_frame(sta, action, body));
    }

    async fn on_spectrum_mgmt_frame<B: SplitByteSlice, D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        action: mac::SpectrumMgmtAction,
        body: B,
    ) {
        match action {
            mac::SpectrumMgmtAction::CHANNEL_SWITCH_ANNOUNCEMENT => {
                if let Err(e) = sta
                    .channel_state
                    .bind(sta.ctx, sta.scanner)
                    .handle_announcement_frame(&body[..])
                    .await
                {
                    warn!("Failed to handle channel switch announcement: {}", e);
                }
            }
            _ => (),
        }
    }

    fn on_sme_eapol<D: DeviceOps>(
        &self,
        sta: &mut BoundClient<'_, D>,
        req: fidl_mlme::EapolRequest,
    ) {
        // Drop EAPoL frame if it is not a protected network.
        if !sta.sta.eapol_required() {
            error!("Unexpected MLME-EAPOL.request message: BSS not protected");
            return;
        }
        // There may be more EAPoL frames (such as key rotation) coming after EAPoL established.
        // They need to be protected.
        let protected = sta.sta.eapol_required() && self.0.controlled_port_open;
        sta.send_eapol_frame(req.src_addr.into(), req.dst_addr.into(), protected, &req.data);
    }

    async fn on_sme_set_keys<D: DeviceOps>(
        &self,
        sta: &mut BoundClient<'_, D>,
        req: fidl_mlme::SetKeysRequest,
    ) {
        if !sta.sta.eapol_required() {
            error!("Unexpected MLME-SetKeys.request message: BSS not protected");
            return;
        }
        let mut results = Vec::with_capacity(req.keylist.len());
        for key_descriptor in req.keylist {
            let key_id = key_descriptor.key_id;

            match sta
                .ctx
                .device
                .install_key(&softmac_key_configuration_from_mlme(key_descriptor))
                .await
            {
                Ok(()) => results
                    .push(fidl_mlme::SetKeyResult { key_id, status: zx::Status::OK.into_raw() }),
                Err(e) => {
                    error!("failed to set key: {}", e);
                    results.push(fidl_mlme::SetKeyResult { key_id, status: e.into_raw() })
                }
            }
        }
        if let Err(e) = sta.ctx.device.send_mlme_event(fidl_mlme::MlmeEvent::SetKeysConf {
            conf: fidl_mlme::SetKeysConfirm { results },
        }) {
            error!("Error sending SetKeysConfirm: {}", e);
        }
    }

    async fn on_sme_set_controlled_port<D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        req: fidl_mlme::SetControlledPortRequest,
    ) {
        if !sta.sta.eapol_required() {
            error!("Unexpected MLME-SetControlledPort.request message: BSS not protected.");
            return;
        }
        let should_open_controlled_port = req.state == fidl_mlme::ControlledPortState::Open;
        if should_open_controlled_port == self.0.controlled_port_open {
            return;
        }
        self.0.controlled_port_open = should_open_controlled_port;
        if let Err(e) = sta.ctx.device.set_ethernet_status(req.state.into()).await {
            error!(
                "Error setting Ethernet port to {}: {}",
                if should_open_controlled_port { "OPEN" } else { "CLOSED" },
                e
            );
        }
    }

    async fn on_sme_deauthenticate<D: DeviceOps>(
        &mut self,
        sta: &mut BoundClient<'_, D>,
        req: fidl_mlme::DeauthenticateRequest,
    ) {
        if let Err(e) = sta.send_deauth_frame(mac::ReasonCode(req.reason_code.into_primitive())) {
            error!("Error sending deauthentication frame to BSS: {}", e);
        }

        self.pre_leaving_associated_state(sta).await;
        let _ =
            sta.clear_association().await.map_err(|e| error!("Failed to clear association: {}", e));

        if let Err(e) = sta.ctx.device.send_mlme_event(fidl_mlme::MlmeEvent::DeauthenticateConf {
            resp: fidl_mlme::DeauthenticateConfirm { peer_sta_address: sta.sta.bssid().to_array() },
        }) {
            error!("Error sending MLME-DEAUTHENTICATE.confirm: {}", e)
        }
    }

    async fn pre_leaving_associated_state<D: DeviceOps>(&mut self, sta: &mut BoundClient<'_, D>) {
        self.0.status_check_timeout.next_event.take();
        self.0.controlled_port_open = false;
        if let Err(e) = sta.ctx.device.set_ethernet_down().await {
            error!("Error disabling ethernet device offline: {}", e);
        }
    }

    #[must_use]
    /// Reports average signal strength to SME and check if auto deauthentication is due.
    /// Returns true if there auto deauthentication is triggered by lack of beacon frames.
    async fn on_timeout<D: DeviceOps>(&mut self, sta: &mut BoundClient<'_, D>) -> bool {
        if let Err(e) = sta.ctx.device.send_mlme_event(fidl_mlme::MlmeEvent::SignalReport {
            ind: fidl_internal::SignalReportIndication {
                rssi_dbm: self.0.signal_strength_average.avg_dbm().0,
                snr_db: 0,
            },
        }) {
            error!("Error sending MLME-SignalReport: {}", e)
        }

        let auto_deauth = self.0.lost_bss_counter.should_deauthenticate();
        if auto_deauth {
            sta.send_deauthenticate_ind(
                fidl_ieee80211::ReasonCode::LeavingNetworkDeauth,
                LocallyInitiated(true),
            );
            if let Err(e) =
                sta.send_deauth_frame(fidl_ieee80211::ReasonCode::LeavingNetworkDeauth.into())
            {
                warn!("Failed sending deauth frame {:?}", e);
            }
            self.pre_leaving_associated_state(sta).await;
        } else {
            // Always check should_deauthenticate() first since even if Client receives a beacon,
            // it would still add a full association status check interval to the lost BSS counter.
            self.0.lost_bss_counter.add_beacon_interval(ASSOCIATION_STATUS_TIMEOUT_BEACON_COUNT);
            self.0.status_check_timeout =
                schedule_association_status_timeout(sta.sta.beacon_period(), &mut sta.ctx.timer);
        }
        auto_deauth
    }
}

statemachine!(
    /// Client state machine.
    pub enum States,
    // Regular successful flow:
    () => Joined,
    Joined => Authenticating,
    Authenticating => Associating,
    Associating => Associated,

    // Timeout:
    Authenticating => Joined,
    Associating => Joined,

    // Deauthentication:
    Authenticating => Joined,
    Associating => Joined,
    Associated => Joined,

    // Disassociation:
    Associating => Authenticated, // Or failure to (re)associate
    Associated => Authenticated,

    // Reassociation:
    Authenticated => Associating,
);

impl States {
    /// Returns the STA's initial state.
    pub fn new_initial() -> States {
        States::from(State::new(Joined))
    }

    /// Begin the 802.11 connect operation, starting with authentication.
    /// This method only has an effect if the initial state is Joined.
    /// Otherwise it is no-op.
    pub async fn start_connecting<D: DeviceOps>(self, sta: &mut BoundClient<'_, D>) -> States {
        match self {
            States::Joined(state) => {
                // Setting timeout in term of beacon period allows us to adjust the realtime
                // timeout in hw-sim, where we set a longer duration in case of a slowbot.
                let duration =
                    sta.sta.beacon_period() * sta.sta.connect_req.connect_failure_timeout;
                let timeout = sta.ctx.timer.schedule_after(duration, TimedEvent::Connecting);
                sta.sta.connect_timeout.replace(timeout);
                match state.start_authenticating(sta).await {
                    Ok(algorithm) => state.transition_to(Authenticating::new(algorithm)).into(),
                    Err(()) => state.transition_to(Joined).into(),
                }
            }
            other => {
                warn!("Attempting to connect from a post-Joined state. Connect request ignored");
                other
            }
        }
    }

    /// Callback to process arbitrary IEEE 802.11 frames.
    /// Frames are dropped if:
    /// - frames are corrupted (too short)
    /// - frames' frame class is not yet permitted
    /// - frames are from a foreign BSS
    /// - frames are unicast but destined for a MAC address that is different from this STA.
    // TODO(https://fxbug.dev/42119762): Implement a packet counter and add tests to verify frames are dropped correctly.
    pub async fn on_mac_frame<B: SplitByteSlice, D: DeviceOps>(
        mut self,
        sta: &mut BoundClient<'_, D>,
        bytes: B,
        rx_info: fidl_softmac::WlanRxInfo,
        async_id: TraceId,
    ) -> States {
        wtrace::duration!(c"States::on_mac_frame");

        let body_aligned = (rx_info.rx_flags & fidl_softmac::WlanRxInfoFlags::FRAME_BODY_PADDING_4)
            != fidl_softmac::WlanRxInfoFlags::empty();

        // Parse mac frame. Drop corrupted ones.
        trace!("Parsing MAC frame:\n  {:02x?}", bytes.deref());
        let mac_frame = match mac::MacFrame::parse(bytes, body_aligned) {
            Some(mac_frame) => mac_frame,
            None => {
                debug!("Dropping corrupt MAC frame.");
                wtrace::async_end_wlansoftmac_rx(async_id, "corrupt frame");
                return self;
            }
        };

        if !sta.sta.should_handle_frame(&mac_frame) {
            warn!("Mac frame is either from a foreign BSS or not destined for us. Dropped.");
            wtrace::async_end_wlansoftmac_rx(async_id, "foreign BSS frame");
            return self;
        }

        // Drop frames which are not permitted in the STA's current state.
        let frame_class = mac::FrameClass::from(&mac_frame);
        if !self.is_frame_class_permitted(frame_class) {
            debug!("Dropping MAC frame with prohibited frame class.");
            wtrace::async_end_wlansoftmac_rx(async_id, "frame with prohibited frame class");
            return self;
        }

        match mac_frame {
            mac::MacFrame::Mgmt(mgmt_frame) => {
                let states = self.on_mgmt_frame(sta, mgmt_frame, rx_info).await;
                wtrace::async_end_wlansoftmac_rx(
                    async_id,
                    "management frame successfully received",
                );
                states
            }
            mac::MacFrame::Data(data_frame) => {
                if let States::Associated(state) = &mut self {
                    state.on_data_frame(sta, data_frame, async_id);
                    state.extract_and_record_signal_dbm(rx_info);
                } else {
                    // Drop data frames in all other states
                    debug!("Dropping MAC data frame while not associated.");
                    wtrace::async_end_wlansoftmac_rx(async_id, "data frame while not associated");
                }
                self
            }
            // Control frames are not yet supported. Drop them.
            _ => {
                debug!("Dropping unsupported MAC control frame.");
                wtrace::async_end_wlansoftmac_rx(async_id, "unsupported control frame");
                self
            }
        }
    }

    /// Processes inbound management frames.
    /// Only frames from the joined BSS are processed. Frames from other STAs are dropped.
    async fn on_mgmt_frame<B: SplitByteSlice, D: DeviceOps>(
        self,
        sta: &mut BoundClient<'_, D>,
        mgmt_frame: mac::MgmtFrame<B>,
        rx_info: fidl_softmac::WlanRxInfo,
    ) -> States {
        wtrace::duration!(c"States::on_mgmt_frame");

        // Parse management frame. Drop corrupted ones.
        let (mgmt_hdr, mgmt_body) = match mgmt_frame.try_into_mgmt_body() {
            (mgmt_hdr, Some(mgmt_body)) => (mgmt_hdr, mgmt_body),
            (_, None) => return self,
        };

        match self {
            States::Authenticating(mut state) => match mgmt_body {
                mac::MgmtBody::Authentication(auth_frame) => match state
                    .on_auth_frame(sta, auth_frame)
                    .await
                {
                    AuthProgress::Complete => state.transition_to(Associating::default()).into(),
                    AuthProgress::InProgress => state.into(),
                    AuthProgress::Failed => state.transition_to(Joined).into(),
                },
                mac::MgmtBody::Deauthentication { deauth_hdr, .. } => {
                    state.on_deauth_frame(sta, &deauth_hdr).await;
                    state.transition_to(Joined).into()
                }
                _ => state.into(),
            },
            States::Associating(mut state) => match mgmt_body {
                mac::MgmtBody::AssociationResp(assoc_resp_frame) => {
                    match state.on_assoc_resp_frame(sta, assoc_resp_frame).await {
                        Ok(association) => state.transition_to(Associated(association)).into(),
                        Err(()) => state.transition_to(Joined).into(),
                    }
                }
                mac::MgmtBody::Deauthentication { deauth_hdr, .. } => {
                    state.on_deauth_frame(sta, &deauth_hdr).await;
                    state.transition_to(Joined).into()
                }
                // This case is highly unlikely and only added to improve interoperability with
                // buggy Access Points.
                mac::MgmtBody::Disassociation { disassoc_hdr, .. } => {
                    state.on_disassoc_frame(sta, &disassoc_hdr);
                    state.transition_to(Authenticated).into()
                }
                _ => state.into(),
            },
            States::Associated(mut state) => {
                state.extract_and_record_signal_dbm(rx_info);
                state.on_any_mgmt_frame(sta, &mgmt_hdr);
                match mgmt_body {
                    mac::MgmtBody::Beacon { bcn_hdr, elements } => {
                        state.on_beacon_frame(sta, &bcn_hdr, elements).await;
                        state.into()
                    }
                    mac::MgmtBody::Deauthentication { deauth_hdr, .. } => {
                        state.on_deauth_frame(sta, &deauth_hdr).await;
                        state.transition_to(Joined).into()
                    }
                    mac::MgmtBody::Disassociation { disassoc_hdr, .. } => {
                        state.on_disassoc_frame(sta, &disassoc_hdr).await;
                        state.transition_to(Authenticated).into()
                    }
                    mac::MgmtBody::Action(action_frame) => {
                        let mac::ActionBody { action_hdr, elements, .. } = action_frame.into_body();
                        match action_hdr.action {
                            mac::ActionCategory::BLOCK_ACK => {
                                let reader = BufferReader::new(elements);
                                if let Some(action) = reader.peek_unaligned::<mac::BlockAckAction>()
                                {
                                    state.on_block_ack_frame(
                                        sta,
                                        action.get(),
                                        reader.into_remaining(),
                                    );
                                }
                                state.into()
                            }
                            mac::ActionCategory::SPECTRUM_MGMT => {
                                let reader = BufferReader::new(elements);
                                if let Some(action) =
                                    reader.peek_unaligned::<mac::SpectrumMgmtAction>()
                                {
                                    state
                                        .on_spectrum_mgmt_frame(
                                            sta,
                                            action.get(),
                                            reader.into_remaining(),
                                        )
                                        .await;
                                }
                                state.into()
                            }
                            _ => state.into(),
                        }
                    }
                    _ => state.into(),
                }
            }
            _ => self,
        }
    }

    pub fn on_eth_frame<B: SplitByteSlice, D: DeviceOps>(
        &self,
        sta: &mut BoundClient<'_, D>,
        frame: B,
        async_id: TraceId,
    ) -> Result<(), Error> {
        wtrace::duration!(c"States::on_eth_frame");
        match self {
            States::Associated(state) => state.on_eth_frame(sta, frame, async_id),
            _ => Err(Error::Status(
                format!("Not associated. Ethernet dropped"),
                zx::Status::BAD_STATE,
            )),
        }
    }

    /// Callback when a previously scheduled event fired.
    pub async fn on_timed_event<D: DeviceOps>(
        self,
        sta: &mut BoundClient<'_, D>,
        event: TimedEvent,
    ) -> States {
        match event {
            TimedEvent::Connecting => {
                sta.sta.connect_timeout.take();
                sta.send_connect_conf_failure(fidl_ieee80211::StatusCode::RejectedSequenceTimeout);
                let _ = sta
                    .clear_association()
                    .await
                    .map_err(|e| error!("Failed to clear association: {}", e));
                match self {
                    States::Authenticating(state) => state.transition_to(Joined).into(),
                    States::Associating(state) => state.transition_to(Joined).into(),
                    States::Associated(state) => state.transition_to(Joined).into(),
                    _ => self,
                }
            }
            TimedEvent::Reassociating => match self {
                States::Associating(mut state) => {
                    state.reconnect_timeout.take();
                    sta.send_connect_conf_failure(
                        fidl_ieee80211::StatusCode::RejectedSequenceTimeout,
                    );
                    state.transition_to(Authenticated).into()
                }
                _ => self,
            },
            TimedEvent::AssociationStatusCheck => match self {
                States::Associated(mut state) => {
                    let should_auto_deauth = state.on_timeout(sta).await;
                    match should_auto_deauth {
                        true => state.transition_to(Joined).into(),
                        false => state.into(),
                    }
                }
                _ => self,
            },
            TimedEvent::ChannelSwitch => {
                if let Err(e) = sta
                    .channel_state
                    .bind(sta.ctx, sta.scanner)
                    .handle_channel_switch_timeout()
                    .await
                {
                    error!("ChannelSwitch timeout handler failed: {}", e);
                }
                self
            }
        }
    }

    pub async fn handle_mlme_req<D: DeviceOps>(
        self,
        sta: &mut BoundClient<'_, D>,
        req: wlan_sme::MlmeRequest,
    ) -> States {
        use wlan_sme::MlmeRequest as MlmeReq;

        match self {
            States::Joined(mut state) => match req {
                MlmeReq::Deauthenticate(_) => {
                    state.on_sme_deauthenticate(sta).await;
                    state.into()
                }
                MlmeReq::Reconnect(req) => {
                    sta.send_connect_conf_failure_with_bssid(
                        req.peer_sta_address.into(),
                        fidl_ieee80211::StatusCode::DeniedNoAssociationExists,
                    );
                    state.into()
                }
                _ => state.into(),
            },
            States::Authenticating(mut state) => match req {
                MlmeReq::SaeHandshakeResp(resp) => match state.on_sme_sae_resp(sta, resp).await {
                    AuthProgress::Complete => state.transition_to(Associating::default()).into(),
                    AuthProgress::InProgress => state.into(),
                    AuthProgress::Failed => state.transition_to(Joined).into(),
                },
                MlmeReq::SaeFrameTx(frame) => match state.on_sme_sae_tx(sta, frame).await {
                    AuthProgress::Complete => state.transition_to(Associating::default()).into(),
                    AuthProgress::InProgress => state.into(),
                    AuthProgress::Failed => state.transition_to(Joined).into(),
                },
                MlmeReq::Deauthenticate(_) => {
                    state.on_sme_deauthenticate(sta).await;
                    state.transition_to(Joined).into()
                }
                MlmeReq::Reconnect(req) => {
                    sta.send_connect_conf_failure_with_bssid(
                        req.peer_sta_address.into(),
                        fidl_ieee80211::StatusCode::DeniedNoAssociationExists,
                    );
                    state.into()
                }
                _ => state.into(),
            },
            States::Authenticated(state) => match req {
                MlmeReq::Reconnect(req) => match state.on_sme_reconnect(sta, req) {
                    Ok(timeout) => {
                        state.transition_to(Associating::new_with_reconnect_timeout(timeout)).into()
                    }
                    Err(()) => state.into(),
                },
                _ => state.into(),
            },
            States::Associating(mut state) => match req {
                MlmeReq::Deauthenticate(_) => {
                    state.on_sme_deauthenticate(sta).await;
                    state.transition_to(Joined).into()
                }
                MlmeReq::Reconnect(req) => {
                    let peer_sta_address: Bssid = req.peer_sta_address.into();
                    if peer_sta_address != sta.sta.connect_req.selected_bss.bssid {
                        sta.send_connect_conf_failure_with_bssid(
                            peer_sta_address,
                            fidl_ieee80211::StatusCode::NotInSameBss,
                        );
                    }
                    state.into()
                }
                _ => state.into(),
            },
            States::Associated(mut state) => match req {
                MlmeReq::Eapol(req) => {
                    state.on_sme_eapol(sta, req);
                    state.into()
                }
                MlmeReq::SetKeys(req) => {
                    state.on_sme_set_keys(sta, req).await;
                    state.into()
                }
                MlmeReq::SetCtrlPort(req) => {
                    state.on_sme_set_controlled_port(sta, req).await;
                    state.into()
                }
                MlmeReq::Deauthenticate(req) => {
                    state.on_sme_deauthenticate(sta, req).await;
                    state.transition_to(Joined).into()
                }
                MlmeReq::Reconnect(req) => {
                    let peer_sta_address: Bssid = req.peer_sta_address.into();
                    if peer_sta_address != sta.sta.connect_req.selected_bss.bssid {
                        sta.send_connect_conf_failure_with_bssid(
                            peer_sta_address,
                            fidl_ieee80211::StatusCode::NotInSameBss,
                        );
                    } else {
                        sta.send_connect_conf_success(state.0.aid, &state.0.assoc_resp_ies[..]);
                    }
                    state.into()
                }
                _ => state.into(),
            },
        }
    }

    /// Returns |true| iff a given FrameClass is permitted to be processed in the current state.
    fn is_frame_class_permitted(&self, class: mac::FrameClass) -> bool {
        wtrace::duration!(c"State::is_frame_class_permitted");
        match self {
            States::Joined(_) | States::Authenticating(_) => class == mac::FrameClass::Class1,
            States::Authenticated(_) | States::Associating(_) => class <= mac::FrameClass::Class2,
            States::Associated(_) => class <= mac::FrameClass::Class3,
        }
    }
}

#[cfg(test)]
mod free_function_tests {
    use super::*;
    use wlan_common::mac::IntoBytesExt as _;

    fn assoc_resp_frame_from_ies(elements: &[u8]) -> mac::AssocRespFrame<&[u8]> {
        mac::AssocRespFrame {
            assoc_resp_hdr: mac::AssocRespHdr {
                capabilities: mac::CapabilityInfo(0u16),
                status_code: mac::StatusCode(0u16),
                aid: 0u16,
            }
            .as_bytes_ref(),
            elements,
        }
    }

    #[test]
    fn test_extract_ht_vht_op_success() {
        let mut buffer = Vec::new();
        ie::write_ht_operation(&mut buffer, &ie::fake_ht_operation()).expect("valid HT Op");
        ie::write_vht_operation(&mut buffer, &ie::fake_vht_operation()).expect("valid VHT Op");
        let (ht_operation, vht_operation) =
            extract_ht_vht_op(&assoc_resp_frame_from_ies(&buffer[..]));
        assert_eq!(ht_operation.unwrap(), ie::fake_ht_operation());
        assert_eq!(vht_operation.unwrap(), ie::fake_vht_operation());
    }

    #[test]
    fn test_extract_ht_op_too_short() {
        let mut buffer = Vec::<u8>::new();
        ie::write_ht_operation(&mut buffer, &ie::fake_ht_operation()).expect("valid HT Op");
        buffer[1] -= 1; // Make length shorter
        buffer.truncate(buffer.len() - 1);
        ie::write_vht_operation(&mut buffer, &ie::fake_vht_operation()).expect("valid VHT Op");
        let (ht_operation, vht_operation) =
            extract_ht_vht_op(&assoc_resp_frame_from_ies(&buffer[..]));
        assert_eq!(ht_operation, None);
        assert_eq!(vht_operation.unwrap(), ie::fake_vht_operation());
    }

    #[test]
    fn test_extract_vht_op_too_short() {
        let mut buffer = Vec::new();
        ie::write_ht_operation(&mut buffer, &ie::fake_ht_operation()).expect("valid HT Op");
        let ht_end = buffer.len();
        ie::write_vht_operation(&mut buffer, &ie::fake_vht_operation()).expect("valid VHT Op");
        buffer[ht_end + 1] -= 1; // Make VHT operation shorter.
        buffer.truncate(buffer.len() - 1);
        let (ht_operation, vht_operation) =
            extract_ht_vht_op(&assoc_resp_frame_from_ies(&buffer[..]));
        assert_eq!(ht_operation.unwrap(), ie::fake_ht_operation());
        assert_eq!(vht_operation, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_ack::{write_addba_req_body, ADDBA_REQ_FRAME_LEN};
    use crate::client::channel_switch::ChannelState;
    use crate::client::scanner::Scanner;
    use crate::client::test_utils::drain_timeouts;
    use crate::client::{Client, Context, ParsedConnectRequest, TimedEventClass};
    use crate::device::{FakeDevice, FakeDeviceState};
    use crate::test_utils::{fake_set_keys_req, fake_wlan_channel, MockWlanRxInfo};
    use akm::AkmAlgorithm;
    use fuchsia_sync::Mutex;
    use lazy_static::lazy_static;
    use std::sync::Arc;
    use test_case::test_case;
    use wlan_common::buffer_writer::BufferWriter;
    use wlan_common::ie::IeType;
    use wlan_common::mac::IntoBytesExt as _;
    use wlan_common::sequence::SequenceManager;
    use wlan_common::test_utils::fake_capabilities::fake_client_capabilities;
    use wlan_common::test_utils::fake_frames::*;
    use wlan_common::test_utils::fake_stas::IesOverrides;
    use wlan_common::timer::{self, create_timer};
    use wlan_common::{assert_variant, fake_bss_description, mgmt_writer};
    use wlan_frame_writer::append_frame_to;
    use {fidl_fuchsia_wlan_common as fidl_common, wlan_statemachine as statemachine};

    lazy_static! {
        static ref BSSID: Bssid = [6u8; 6].into();
        static ref IFACE_MAC: MacAddr = [3u8; 6].into();
    }

    struct MockObjects {
        fake_device: FakeDevice,
        fake_device_state: Arc<Mutex<FakeDeviceState>>,
        timer: Option<Timer<TimedEvent>>,
        time_stream: timer::EventStream<TimedEvent>,
        scanner: Scanner,
        channel_state: ChannelState,
    }

    impl MockObjects {
        // TODO(https://fxbug.dev/327499461): This function is async to ensure MLME functions will
        // run in an async context and not call `wlan_common::timer::Timer::now` without an
        // executor.
        async fn new() -> Self {
            let (timer, time_stream) = create_timer();
            let (fake_device, fake_device_state) = FakeDevice::new().await;
            Self {
                fake_device,
                fake_device_state,
                timer: Some(timer),
                time_stream,
                scanner: Scanner::new(*IFACE_MAC),
                channel_state: ChannelState::new_with_main_channel(fake_wlan_channel().into()),
            }
        }

        async fn make_ctx(&mut self) -> Context<FakeDevice> {
            self.fake_device
                .set_channel(fake_wlan_channel().into())
                .await
                .expect("fake device is obedient");
            self.make_base_ctx()
        }

        async fn make_ctx_with_bss(&mut self) -> Context<FakeDevice> {
            self.fake_device
                .set_channel(fake_wlan_channel().into())
                .await
                .expect("fake device is obedient");
            self.fake_device
                .join_bss(&fidl_common::JoinBssRequest {
                    bssid: Some([1, 2, 3, 4, 5, 6]),
                    bss_type: Some(fidl_common::BssType::Personal),
                    remote: Some(true),
                    beacon_period: Some(100),
                    ..Default::default()
                })
                .await
                .expect("error configuring bss");
            self.make_base_ctx()
        }

        fn make_base_ctx(&mut self) -> Context<FakeDevice> {
            Context {
                _config: Default::default(),
                device: self.fake_device.clone(),
                timer: self.timer.take().unwrap(),
                seq_mgr: SequenceManager::new(),
            }
        }
    }

    fn make_client_station() -> Client {
        let connect_req = ParsedConnectRequest {
            selected_bss: fake_bss_description!(Open, bssid: BSSID.to_array()),
            connect_failure_timeout: 10,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![],
        };
        Client::new(connect_req, *IFACE_MAC, fake_client_capabilities())
    }

    fn make_protected_client_station() -> Client {
        let connect_req = ParsedConnectRequest {
            selected_bss: fake_bss_description!(Wpa2, bssid: BSSID.to_array()),
            connect_failure_timeout: 10,
            auth_type: fidl_mlme::AuthenticationTypes::OpenSystem,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![
                0x30, 0x14, //  ID and len
                1, 0, //  version
                0x00, 0x0f, 0xac, 0x04, //  group data cipher suite
                0x01, 0x00, //  pairwise cipher suite count
                0x00, 0x0f, 0xac, 0x04, //  pairwise cipher suite list
                0x01, 0x00, //  akm suite count
                0x00, 0x0f, 0xac, 0x02, //  akm suite list
                0xa8, 0x04, //  rsn capabilities
            ],
        };
        Client::new(connect_req, *IFACE_MAC, fake_client_capabilities())
    }

    fn connect_conf_failure(result_code: fidl_ieee80211::StatusCode) -> fidl_mlme::ConnectConfirm {
        fidl_mlme::ConnectConfirm {
            peer_sta_address: BSSID.to_array(),
            result_code,
            association_id: 0,
            association_ies: vec![],
        }
    }

    fn empty_association(sta: &mut BoundClient<'_, FakeDevice>) -> Association {
        let status_check_timeout =
            schedule_association_status_timeout(sta.sta.beacon_period(), &mut sta.ctx.timer);
        Association {
            controlled_port_open: false,
            aid: 0,
            assoc_resp_ies: vec![],
            ap_ht_op: None,
            ap_vht_op: None,
            lost_bss_counter: LostBssCounter::start(
                sta.sta.beacon_period(),
                DEFAULT_AUTO_DEAUTH_TIMEOUT_BEACON_COUNT,
            ),
            qos: Qos::Disabled,
            status_check_timeout,
            signal_strength_average: SignalStrengthAverage::new(),
            block_ack_state: StateMachine::new(BlockAckState::from(State::new(Closed))),
        }
    }

    fn fake_assoc_cfg() -> fidl_softmac::WlanAssociationConfig {
        fidl_softmac::WlanAssociationConfig {
            bssid: Some(BSSID.to_array()),
            aid: Some(42),
            channel: Some(fidl_common::WlanChannel {
                primary: 149,
                cbw: fidl_common::ChannelBandwidth::Cbw40,
                secondary80: 42,
            }),
            rates: None,
            capability_info: None,
            ..Default::default()
        }
    }

    fn fake_deauth_req() -> wlan_sme::MlmeRequest {
        wlan_sme::MlmeRequest::Deauthenticate(fidl_mlme::DeauthenticateRequest {
            peer_sta_address: BSSID.to_array(),
            reason_code: fidl_ieee80211::ReasonCode::LeavingNetworkDeauth,
        })
    }

    fn open_authenticating(sta: &mut BoundClient<'_, FakeDevice>) -> Authenticating {
        let mut auth = Authenticating::new(AkmAlgorithm::OpenSupplicant);
        auth.algorithm.initiate(sta).expect("Failed to initiate open auth");
        auth
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn connect_authenticate_tx_failure() {
        let mut m = MockObjects::new().await;
        m.fake_device_state.lock().config.send_wlan_frame_fails = true;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let state = Joined;
        let _state =
            state.start_authenticating(&mut sta).await.expect_err("should fail authenticating");

        // Verify no event was queued up in the timer.
        assert!(m.time_stream.try_next().is_err());

        // Verify MLME-CONNECT.confirm message was sent.
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("expect msg");
        assert_eq!(
            msg,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: BSSID.to_array(),
                result_code: fidl_ieee80211::StatusCode::RefusedReasonUnspecified,
                association_id: 0,
                association_ies: vec![],
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn joined_no_authentication_algorithm() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let connect_req = ParsedConnectRequest {
            selected_bss: fake_bss_description!(Open, bssid: BSSID.to_array()),
            connect_failure_timeout: 10,
            // use an unsupported AuthenticationType
            auth_type: fidl_mlme::AuthenticationTypes::SharedKey,
            sae_password: vec![],
            wep_key: None,
            security_ie: vec![],
        };
        let mut sta = Client::new(connect_req, *IFACE_MAC, fake_client_capabilities());
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = Joined;

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        let _state =
            state.start_authenticating(&mut sta).await.expect_err("should fail authenticating");

        // Verify MLME-CONNECT.confirm message was sent.
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("expect msg");
        assert_eq!(
            msg,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: [6, 6, 6, 6, 6, 6],
                result_code: fidl_ieee80211::StatusCode::UnsupportedAuthAlgorithm,
                association_id: 0,
                association_ies: vec![],
            }
        );

        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn authenticating_state_auth_rejected() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state = open_authenticating(&mut sta);

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        // Verify authentication failed.
        assert_variant!(
            state
                .on_auth_frame(
                    &mut sta,
                    mac::AuthFrame {
                        auth_hdr: mac::AuthHdr {
                            auth_alg_num: mac::AuthAlgorithmNumber::OPEN,
                            auth_txn_seq_num: 2,
                            status_code: fidl_ieee80211::StatusCode::NotInSameBss.into(),
                        }
                        .as_bytes_ref(),
                        elements: &[],
                    },
                )
                .await,
            AuthProgress::Failed
        );

        // Verify MLME-CONNECT.confirm message was sent.
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("expect msg");
        assert_eq!(
            msg,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: BSSID.to_array(),
                result_code: fidl_ieee80211::StatusCode::RefusedReasonUnspecified,
                association_id: 0,
                association_ies: vec![],
            }
        );
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn authenticating_state_deauth_frame() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state = open_authenticating(&mut sta);

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        state
            .on_deauth_frame(
                &mut sta,
                &mac::DeauthHdr { reason_code: fidl_ieee80211::ReasonCode::NoMoreStas.into() },
            )
            .await;

        // Verify MLME-CONNECT.confirm message was sent.
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("expect msg");
        assert_eq!(
            msg,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: BSSID.to_array(),
                result_code: fidl_ieee80211::StatusCode::SpuriousDeauthOrDisassoc,
                association_id: 0,
                association_ies: vec![],
            }
        );
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associating_success_unprotected() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        let mut state = Associating::default();
        let assoc_resp_ies = fake_bss_description!(Wpa2, ies_overrides: IesOverrides::new()
            .set(IeType::HT_CAPABILITIES, ie::fake_ht_cap_bytes().to_vec())
            .set(IeType::VHT_CAPABILITIES, ie::fake_vht_cap_bytes().to_vec())
        )
        .ies()
        .to_vec();
        let Association { aid, controlled_port_open, .. } = state
            .on_assoc_resp_frame(
                &mut sta,
                mac::AssocRespFrame {
                    assoc_resp_hdr: mac::AssocRespHdr {
                        aid: 42,
                        capabilities: mac::CapabilityInfo(52),
                        status_code: fidl_ieee80211::StatusCode::Success.into(),
                    }
                    .as_bytes_ref(),
                    elements: &assoc_resp_ies[..],
                },
            )
            .await
            .expect("failed processing association response frame");
        assert_eq!(aid, 42);
        assert_eq!(true, controlled_port_open);

        // Verify MLME-CONNECT.confirm message was sent.
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("no message");
        assert_eq!(
            msg,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: BSSID.to_array(),
                result_code: fidl_ieee80211::StatusCode::Success,
                association_id: 42,
                association_ies: assoc_resp_ies,
            }
        );
        assert!(m.fake_device_state.lock().join_bss_request.is_some());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associating_success_protected() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_protected_client_station();
        sta.client_capabilities.0.capability_info =
            mac::CapabilityInfo(0).with_ess(true).with_ibss(true);
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state = Associating::default();

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        let assoc_resp_ies =
            fake_bss_description!(Wpa2, bssid: BSSID.to_array(), ies_overrides: IesOverrides::new()
                .set(IeType::HT_CAPABILITIES, ie::fake_ht_cap_bytes().to_vec())
                .set(IeType::VHT_CAPABILITIES, ie::fake_vht_cap_bytes().to_vec())
            )
            .ies()
            .to_vec();
        let Association { aid, controlled_port_open, .. } = state
            .on_assoc_resp_frame(
                &mut sta,
                mac::AssocRespFrame {
                    assoc_resp_hdr: mac::AssocRespHdr {
                        aid: 42,
                        capabilities: mac::CapabilityInfo(0).with_ibss(true).with_cf_pollable(true),
                        status_code: fidl_ieee80211::StatusCode::Success.into(),
                    }
                    .as_bytes_ref(),
                    elements: &assoc_resp_ies[..],
                },
            )
            .await
            .expect("failed processing association response frame");
        assert_eq!(aid, 42);
        assert_eq!(false, controlled_port_open);

        // Verify association context is set
        assert_eq!(m.fake_device_state.lock().assocs.len(), 1);

        let assoc_cfg = m
            .fake_device_state
            .lock()
            .assocs
            .get(&(*BSSID).into())
            .expect("expect assoc ctx to be set")
            .clone();
        assert_eq!(assoc_cfg.aid, Some(42));
        assert_eq!(assoc_cfg.qos, Some(true));
        assert_eq!(
            assoc_cfg.rates,
            Some(vec![0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, 0x30, 0x48, 0x60, 0x6c])
        );
        assert_eq!(assoc_cfg.capability_info, Some(2));
        assert!(assoc_cfg.ht_cap.is_some());
        assert!(assoc_cfg.vht_cap.is_some());
        assert!(assoc_cfg.ht_op.is_some());
        assert!(assoc_cfg.vht_op.is_some());

        // Verify MLME-CONNECT.confirm message was sent.
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("no message");
        assert_eq!(
            msg,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: BSSID.to_array(),
                result_code: fidl_ieee80211::StatusCode::Success,
                association_id: 42,
                association_ies: assoc_resp_ies,
            }
        );
        assert!(m.fake_device_state.lock().join_bss_request.is_some());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associating_failure_due_to_failed_status_code() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let mut state = Associating::default();

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        // Verify authentication was considered successful.
        state
            .on_assoc_resp_frame(
                &mut sta,
                mac::AssocRespFrame {
                    assoc_resp_hdr: mac::AssocRespHdr {
                        aid: 42,
                        capabilities: mac::CapabilityInfo(52),
                        status_code: fidl_ieee80211::StatusCode::NotInSameBss.into(),
                    }
                    .as_bytes_ref(),
                    elements: &[][..],
                },
            )
            .await
            .expect_err("expected failure processing association response frame");

        // Verify MLME-CONNECT.confirm message was sent.
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("no message");
        assert_eq!(msg, connect_conf_failure(fidl_ieee80211::StatusCode::NotInSameBss));
        assert!(m.fake_device_state.lock().join_bss_request.is_some());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associating_failure_due_to_incompatibility() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let mut state = Associating::default();

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        state
            .on_assoc_resp_frame(
                &mut sta,
                mac::AssocRespFrame {
                    assoc_resp_hdr: mac::AssocRespHdr {
                        aid: 42,
                        capabilities: mac::CapabilityInfo(52),
                        status_code: fidl_ieee80211::StatusCode::Success.into(),
                    }
                    .as_bytes_ref(),
                    elements: fake_bss_description!(Wpa2, rates: vec![0x81]).ies(),
                },
            )
            .await
            .expect_err("expected failure processing association response frame");

        // Verify MLME-CONNECT.confirm message was sent.
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("no message");
        assert_eq!(
            msg,
            connect_conf_failure(fidl_ieee80211::StatusCode::RefusedCapabilitiesMismatch)
        );
        assert!(m.fake_device_state.lock().join_bss_request.is_some());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associating_deauth_frame() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let mut state = Associating::default();

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        state
            .on_deauth_frame(
                &mut sta,
                &mac::DeauthHdr { reason_code: fidl_ieee80211::ReasonCode::ApInitiated.into() },
            )
            .await;

        // Verify MLME-CONNECT.confirm message was sent.
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("no message");
        assert_eq!(msg, connect_conf_failure(fidl_ieee80211::StatusCode::SpuriousDeauthOrDisassoc));
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associating_disassociation() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let mut state = Associating::default();

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        state.on_disassoc_frame(
            &mut sta,
            &mac::DisassocHdr { reason_code: fidl_ieee80211::ReasonCode::ApInitiated.into() },
        );

        // Verify MLME-CONNECT.confirm message was sent.
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("no message");
        assert_eq!(msg, connect_conf_failure(fidl_ieee80211::StatusCode::SpuriousDeauthOrDisassoc));
        assert!(m.fake_device_state.lock().join_bss_request.is_some());
    }

    fn mock_rx_info<'a>(client: &BoundClient<'a, FakeDevice>) -> fidl_softmac::WlanRxInfo {
        let channel = client.channel_state.get_main_channel().unwrap();
        MockWlanRxInfo::with_channel(channel).into()
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_block_ack_frame() {
        let mut mock = MockObjects::new().await;
        let mut ctx = mock.make_ctx().await;
        let mut station = make_client_station();
        let mut client = station.bind(&mut ctx, &mut mock.scanner, &mut mock.channel_state);

        let frame = {
            let mut buffer = [0u8; ADDBA_REQ_FRAME_LEN];
            let writer = BufferWriter::new(&mut buffer[..]);
            let mut writer = append_frame_to!(
                writer,
                {
                    headers: {
                        mac::MgmtHdr: &mgmt_writer::mgmt_hdr_from_ap(
                            mac::FrameControl(0)
                                .with_frame_type(mac::FrameType::MGMT)
                                .with_mgmt_subtype(mac::MgmtSubtype::ACTION),
                            client.sta.iface_mac,
                            client.sta.bssid(),
                            mac::SequenceControl(0)
                                .with_seq_num(client.ctx.seq_mgr.next_sns1(&client.sta.bssid().into()) as u16),
                        ),
                    },
                }
            )
            .unwrap();
            write_addba_req_body(&mut writer, 1).unwrap();
            buffer
        };

        let state = States::from(statemachine::testing::new_state(Associated(empty_association(
            &mut client,
        ))));
        let rx_info = mock_rx_info(&client);
        match state.on_mac_frame(&mut client, &frame[..], rx_info, 0.into()).await {
            States::Associated(state) => {
                let (_, associated) = state.release_data();
                // TODO(https://fxbug.dev/42104687): Handle BlockAck frames. The following code has been
                //                        altered as part of a fix for https://fxbug.dev/42180615. This check
                //                        should ensure that the state has transitioned to
                //                        `Established`, but since the state machine has been
                //                        disabled it instead checks that the state has remained
                //                        `Closed`.
                match *associated.0.block_ack_state.as_ref() {
                    BlockAckState::Closed(_) => {}
                    _ => panic!("client has transitioned BlockAck"),
                }
            }
            _ => panic!("client no longer associated"),
        }
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_deauth_frame() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state = Associated(empty_association(&mut sta));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());

        // Association configuration will be cleared when MLME receives deauth frame.
        sta.ctx
            .device
            .notify_association_complete(fake_assoc_cfg())
            .await
            .expect("valid assoc_cfg should succeed");
        assert_eq!(1, m.fake_device_state.lock().assocs.len());

        sta.ctx.device.set_ethernet_up().await.expect("should succeed");
        assert_eq!(m.fake_device_state.lock().link_status, crate::device::LinkStatus::UP);

        let _joined = state
            .on_deauth_frame(
                &mut sta,
                &mac::DeauthHdr { reason_code: fidl_ieee80211::ReasonCode::ApInitiated.into() },
            )
            .await;

        // Verify MLME-ASSOCIATE.confirm message was sent.
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::DeauthenticateIndication>()
            .expect("no message");
        assert_eq!(
            msg,
            fidl_mlme::DeauthenticateIndication {
                peer_sta_address: BSSID.to_array(),
                reason_code: fidl_ieee80211::ReasonCode::ApInitiated,
                locally_initiated: false,
            }
        );
        // Verify ethernet port is shut down.
        assert_eq!(m.fake_device_state.lock().link_status, crate::device::LinkStatus::DOWN);
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_disassociation() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state = Associated(empty_association(&mut sta));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        state.0.controlled_port_open = true;

        sta.ctx
            .device
            .notify_association_complete(fake_assoc_cfg())
            .await
            .expect("valid assoc_cfg should succeed");
        assert_eq!(1, m.fake_device_state.lock().assocs.len());

        sta.ctx.device.set_ethernet_up().await.expect("should succeed");
        assert_eq!(m.fake_device_state.lock().link_status, crate::device::LinkStatus::UP);

        let _authenticated = state
            .on_disassoc_frame(
                &mut sta,
                &mac::DisassocHdr { reason_code: fidl_ieee80211::ReasonCode::ApInitiated.into() },
            )
            .await;

        // Verify MLME-ASSOCIATE.confirm message was sent.
        let msg = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::DisassociateIndication>()
            .expect("no message");
        assert_eq!(
            msg,
            fidl_mlme::DisassociateIndication {
                peer_sta_address: BSSID.to_array(),
                reason_code: fidl_ieee80211::ReasonCode::ApInitiated,
                locally_initiated: false,
            }
        );

        // Verify ethernet port is shut down.
        assert_eq!(m.fake_device_state.lock().link_status, crate::device::LinkStatus::DOWN);
        assert!(m.fake_device_state.lock().join_bss_request.is_some());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_move_data_closed_controlled_port() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = Associated(empty_association(&mut sta));

        let bytes = make_data_frame_single_llc(None, None);
        let data_frame = mac::DataFrame::parse(bytes.as_slice(), false).unwrap();
        state.on_data_frame(&mut sta, data_frame, 0.into());

        // Verify data frame was dropped.
        assert_eq!(m.fake_device_state.lock().eth_queue.len(), 0);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_move_data_opened_controlled_port() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state =
            Associated(Association { controlled_port_open: true, ..empty_association(&mut sta) });

        let bytes = make_data_frame_single_llc(None, None);
        let data_frame = mac::DataFrame::parse(bytes.as_slice(), false).unwrap();
        state.on_data_frame(&mut sta, data_frame, 0.into());

        // Verify data frame was processed.
        assert_eq!(m.fake_device_state.lock().eth_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(m.fake_device_state.lock().eth_queue[0], [
            3, 3, 3, 3, 3, 3, // dst_addr
            4, 4, 4, 4, 4, 4, // src_addr
            9, 10, // ether_type
            11, 11, 11, // payload
        ]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_skip_empty_data() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state =
            Associated(Association { controlled_port_open: true, ..empty_association(&mut sta) });

        let bytes = make_data_frame_single_llc_payload(None, None, &[]);
        let data_frame = mac::DataFrame::parse(bytes.as_slice(), false).unwrap();
        state.on_data_frame(&mut sta, data_frame, 0.into());

        // Verify data frame was discarded.
        assert!(m.fake_device_state.lock().eth_queue.is_empty());
    }

    #[test_case(true, true; "port open and protected")]
    #[test_case(false, true; "port closed and protected")]
    #[test_case(true, false; "port open and unprotected")]
    #[test_case(false, false; "port closed and unprotected (not a typical state)")]
    #[fuchsia::test(allow_stalls = false)]
    async fn associated_send_keep_alive_after_null_data_frame(
        controlled_port_open: bool,
        protected: bool,
    ) {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta =
            if protected { make_protected_client_station() } else { make_client_station() };
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = Associated(Association { controlled_port_open, ..empty_association(&mut sta) });

        let bytes = make_null_data_frame();
        let data_frame = mac::DataFrame::parse(bytes.as_slice(), false).unwrap();
        state.on_data_frame(&mut sta, data_frame, 0.into());

        // Verify data frame was not forwarded up.
        assert!(m.fake_device_state.lock().eth_queue.is_empty());
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        let bytes = &m.fake_device_state.lock().wlan_queue[0].0;
        let data_frame = mac::DataFrame::parse(bytes.as_slice(), false).unwrap();
        let frame_ctrl = data_frame.frame_ctrl();
        assert_eq!(frame_ctrl.to_ds(), true);
        assert_eq!(frame_ctrl.from_ds(), false);
        assert!(data_frame.body.is_empty());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_handle_eapol_closed_controlled_port() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_protected_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = Associated(empty_association(&mut sta));

        let (src_addr, dst_addr, bytes) = make_eapol_frame(*IFACE_MAC);
        let data_frame = mac::DataFrame::parse(bytes.as_slice(), false).unwrap();
        state.on_data_frame(&mut sta, data_frame, 0.into());

        // Verify EAPOL frame was not sent to netstack.
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
    async fn associated_handle_eapol_open_controlled_port() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_protected_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = Associated(empty_association(&mut sta));

        let (src_addr, dst_addr, bytes) = make_eapol_frame(*IFACE_MAC);
        let data_frame = mac::DataFrame::parse(bytes.as_slice(), false).unwrap();
        state.on_data_frame(&mut sta, data_frame, 0.into());

        // Verify EAPOL frame was not sent to netstack.
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
    async fn associated_handle_amsdus_open_controlled_port() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_protected_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state =
            Associated(Association { controlled_port_open: true, ..empty_association(&mut sta) });

        let bytes = make_data_frame_amsdu();
        let data_frame = mac::DataFrame::parse(bytes.as_slice(), false).unwrap();
        state.on_data_frame(&mut sta, data_frame, 0.into());

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
    async fn associated_request_bu_data_frame() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = Associated(Association {
            aid: 42,
            controlled_port_open: true,
            ..empty_association(&mut sta)
        });

        let mut bytes = make_data_frame_single_llc(None, None);
        let mut data_frame = mac::DataFrame::parse(bytes.as_mut_slice(), false).unwrap();
        data_frame.fixed_fields.frame_ctrl =
            data_frame.fixed_fields.frame_ctrl.with_more_data(true);
        state.on_data_frame(&mut sta, data_frame, 0.into());

        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(&m
            .fake_device_state.lock().wlan_queue[0].0[..], &[
            // Frame Control:
            0b10100100, 0b00000000, // FC
            42, 0b11_000000, // Id
            6, 6, 6, 6, 6, 6, // addr1
            3, 3, 3, 3, 3, 3, // addr2
        ][..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_request_bu_mgmt_frame() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = Associated(Association {
            aid: 42,
            controlled_port_open: true,
            ..empty_association(&mut sta)
        });

        state.on_any_mgmt_frame(
            &mut sta,
            &mac::MgmtHdr {
                frame_ctrl: mac::FrameControl(0)
                    .with_frame_type(mac::FrameType::MGMT)
                    .with_mgmt_subtype(mac::MgmtSubtype::BEACON)
                    .with_more_data(true),
                duration: 0,
                addr1: [3; 6].into(),
                addr2: (*BSSID).into(),
                addr3: (*BSSID).into(),
                seq_ctrl: mac::SequenceControl(0),
            },
        );

        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        #[rustfmt::skip]
        assert_eq!(&m
            .fake_device_state.lock().wlan_queue[0].0[..], &[
            // Frame Control:
            0b10100100, 0b00000000, // FC
            42, 0b11_000000, // Id
            6, 6, 6, 6, 6, 6, // addr1
            3, 3, 3, 3, 3, 3, // addr2
        ][..]);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_no_bu_request() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        // Closed Controlled port
        let state = Associated(empty_association(&mut sta));
        let mut bytes = make_data_frame_single_llc(None, None);
        let mut data_frame = mac::DataFrame::parse(bytes.as_mut_slice(), false).unwrap();
        data_frame.fixed_fields.frame_ctrl =
            data_frame.fixed_fields.frame_ctrl.with_more_data(true);
        state.on_data_frame(&mut sta, data_frame, 0.into());
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 0);

        // Foreign management frame
        let state = States::from(statemachine::testing::new_state(Associated(Association {
            controlled_port_open: true,
            ..empty_association(&mut sta)
        })));
        #[rustfmt::skip]
        let beacon = vec![
            // Mgmt Header:
            0b1000_00_00, 0b00100000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1
            7, 7, 7, 7, 7, 7, // Addr2
            5, 5, 5, 5, 5, 5, // Addr3
            0x10, 0, // Sequence Control
            // Omit IEs
        ];
        let rx_info = mock_rx_info(&sta);
        state.on_mac_frame(&mut sta, &beacon[..], rx_info, 0.into()).await;
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 0);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_drop_foreign_data_frames() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        // Foreign data frame
        let state = States::from(statemachine::testing::new_state(Associated(Association {
            aid: 42,
            controlled_port_open: true,
            ..empty_association(&mut sta)
        })));
        let fc = mac::FrameControl(0)
            .with_frame_type(mac::FrameType::DATA)
            .with_data_subtype(mac::DataSubtype(0))
            .with_from_ds(true);
        let fc = fc.0.to_le_bytes();
        // Send data frame from an address other than the BSSID([6u8; 6]).
        let bytes = vec![
            // Data Header
            fc[0], fc[1], // fc
            2, 2, // duration
            3, 3, 3, 3, 3, 3, // addr1
            4, 4, 4, 4, 4, 4, // addr2
            5, 5, 5, 5, 5, 5, // addr3
            6, 6, // sequence control
            // LLC Header
            7, 7, 7, // DSAP, SSAP & control
            8, 8, 8, // OUI
            9, 10, // eth type
            // Trailing bytes
            11, 11, 11,
        ];
        let rx_info = mock_rx_info(&sta);
        state.on_mac_frame(&mut sta, &bytes[..], rx_info, 0.into()).await;
        assert_eq!(m.fake_device_state.lock().eth_queue.len(), 0);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_joined_state_reconnect_denied() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state = States::from(statemachine::testing::new_state(Joined));

        // (sme->mlme) Send a reconnect request
        let reconnect_req = wlan_sme::MlmeRequest::Reconnect(fidl_mlme::ReconnectRequest {
            peer_sta_address: [1, 2, 3, 4, 5, 6],
        });
        state = state.handle_mlme_req(&mut sta, reconnect_req).await;

        assert_variant!(state, States::Joined(_), "not in joined state");

        // Verify MLME-CONNECT.confirm message was sent.
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
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_authing_success() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state =
            States::from(statemachine::testing::new_state(open_authenticating(&mut sta)));

        // Successful: Joined > Authenticating > Associating
        #[rustfmt::skip]
        let auth_resp_success = vec![
            // Mgmt Header:
            0b1011_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Auth Header:
            0, 0, // Algorithm Number (Open)
            2, 0, // Txn Sequence Number
            0, 0, // Status Code
        ];
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &auth_resp_success[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Associating(_), "not in associating state");
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_authing_failure() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state =
            States::from(statemachine::testing::new_state(open_authenticating(&mut sta)));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        // Failure: Joined > Authenticating > Joined
        #[rustfmt::skip]
        let auth_resp_failure = vec![
            // Mgmt Header:
            0b1011_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Auth Header:
            0, 0, // Algorithm Number (Open)
            2, 0, // Txn Sequence Number
            42, 0, // Status Code
        ];
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &auth_resp_failure[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Joined(_), "not in joined state");
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_authing_deauth() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state =
            States::from(statemachine::testing::new_state(open_authenticating(&mut sta)));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        // Deauthenticate: Authenticating > Joined
        #[rustfmt::skip]
        let deauth = vec![
            // Mgmt Header:
            0b1100_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Deauth Header:
            5, 0, // Algorithm Number (Open)
        ];
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &deauth[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Joined(_), "not in joined state");
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_foreign_auth_resp() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state =
            States::from(statemachine::testing::new_state(open_authenticating(&mut sta)));

        // Send foreign auth response. State should not change.
        #[rustfmt::skip]
        let auth_resp_success = vec![
            // Mgmt Header:
            0b1011_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            5, 5, 5, 5, 5, 5, // Addr1
            3, 3, 3, 3, 3, 3, // Addr2
            5, 5, 5, 5, 5, 5, // Addr3
            0x10, 0, // Sequence Control
            // Auth Header:
            0, 0, // Algorithm Number (Open)
            2, 0, // Txn Sequence Number
            0, 0, // Status Code
        ];
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &auth_resp_success[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Authenticating(_), "not in authenticating state");

        // Verify that an authentication response from the joined BSS still moves the Client
        // forward.
        #[rustfmt::skip]
        let auth_resp_success = vec![
            // Mgmt Header:
            0b1011_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Auth Header:
            0, 0, // Algorithm Number (Open)
            2, 0, // Txn Sequence Number
            0, 0, // Status Code
        ];
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &auth_resp_success[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Associating(_), "not in associating state");
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_authing_state_reconnect_denied() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state =
            States::from(statemachine::testing::new_state(open_authenticating(&mut sta)));

        // (sme->mlme) Send a reconnect request
        let reconnect_req = wlan_sme::MlmeRequest::Reconnect(fidl_mlme::ReconnectRequest {
            peer_sta_address: [1, 2, 3, 4, 5, 6],
        });
        state = state.handle_mlme_req(&mut sta, reconnect_req).await;

        assert_variant!(state, States::Authenticating(_), "not in authenticating state");

        // Verify MLME-CONNECT.confirm message was sent.
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
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_authing_state_wrong_algorithm() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state =
            States::from(statemachine::testing::new_state(open_authenticating(&mut sta)));

        #[rustfmt::skip]
        let auth_resp_wrong = vec![
            // Mgmt Header:
            0b1011_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Auth Header:
            8, 0, // Algorithm Number (wrong algorithm: SAE)
            2, 0, // Txn Sequence Number
            0, 0, // Status Code
        ];
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &auth_resp_wrong[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Joined(_), "not in joined state");
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_associng_success() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state = States::from(statemachine::testing::new_state(Associating::default()));

        // Successful: Associating > Associated
        #[rustfmt::skip]
        let assoc_resp_success = vec![
            // Mgmt Header:
            0b0001_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Assoc Resp Header:
            0, 0, // Capabilities
            0, 0, // Status Code
            0, 0, // AID
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
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &assoc_resp_success[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Associated(_), "not in associated state");
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_associng_failure() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state = States::from(statemachine::testing::new_state(Associating::default()));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        // Failure: Associating > Joined
        #[rustfmt::skip]
        let assoc_resp_failure = vec![
            // Mgmt Header:
            0b0001_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Assoc Resp Header:
            0, 0, // Capabilities
            2, 0, // Status Code (Failed)
            0, 0, // AID
        ];
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &assoc_resp_failure[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Joined(_), "not in joined state");
        assert!(m.fake_device_state.lock().join_bss_request.is_some());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_associng_deauthing() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state = States::from(statemachine::testing::new_state(Associating::default()));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        // Deauthentication: Associating > Joined
        #[rustfmt::skip]
        let deauth = vec![
            // Mgmt Header:
            0b1100_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Deauth Header:
            4, 0, // Reason Code
        ];
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &deauth[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Joined(_), "not in joined state");
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_associng_reconnect_no_op() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state = States::from(statemachine::testing::new_state(Associating::default()));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        // (sme->mlme) Send a reconnect request
        let reconnect_req = wlan_sme::MlmeRequest::Reconnect(fidl_mlme::ReconnectRequest {
            peer_sta_address: BSSID.to_array(),
        });
        state = state.handle_mlme_req(&mut sta, reconnect_req).await;
        assert_variant!(state, States::Associating(_), "not in associating state");
        assert!(m.fake_device_state.lock().join_bss_request.is_some());

        // Verify no connect conf is sent
        m.fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect_err("unexpected Connect.confirm");
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_associng_reconnect_denied() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state = States::from(statemachine::testing::new_state(Associating::default()));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        // (sme->mlme) Send a reconnect request
        let sus_bssid = [b's', b'u', b's', b'r', b'e', b'q'];
        let reconnect_req = wlan_sme::MlmeRequest::Reconnect(fidl_mlme::ReconnectRequest {
            peer_sta_address: sus_bssid,
        });
        state = state.handle_mlme_req(&mut sta, reconnect_req).await;
        assert_variant!(state, States::Associating(_), "not in associating state");
        assert!(m.fake_device_state.lock().join_bss_request.is_some());

        // Verify a connect conf was sent
        let connect_conf = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("error reading Connect.confirm");
        assert_eq!(
            connect_conf,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: sus_bssid,
                result_code: fidl_ieee80211::StatusCode::NotInSameBss,
                association_id: 0,
                association_ies: vec![],
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_assoced_disassoc_connect_success() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        // Disassociation: Associated > Associating
        #[rustfmt::skip]
        let disassoc = vec![
            // Mgmt Header:
            0b1010_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Deauth Header:
            4, 0, // Reason Code
        ];
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &disassoc[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Authenticated(_), "not in auth'd state");
        assert!(m.fake_device_state.lock().join_bss_request.is_some());

        // Verify a disassoc ind was sent
        let disassoc_ind = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::DisassociateIndication>()
            .expect("error reading Disassociate.ind");
        assert_eq!(
            disassoc_ind,
            fidl_mlme::DisassociateIndication {
                peer_sta_address: BSSID.to_array(),
                reason_code: fidl_ieee80211::ReasonCode::ReasonInactivity,
                locally_initiated: false,
            }
        );

        // (sme->mlme) Send a reconnect request
        let reconnect_req = wlan_sme::MlmeRequest::Reconnect(fidl_mlme::ReconnectRequest {
            peer_sta_address: BSSID.to_array(),
        });
        state = state.handle_mlme_req(&mut sta, reconnect_req).await;
        assert_variant!(state, States::Associating(_), "not in associating state");

        // Verify associate request frame was sent
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        assert_eq!(
            &m.fake_device_state.lock().wlan_queue[0].0[..22],
            &[
                // Mgmt header:
                0, 0, // FC
                0, 0, // Duration
                6, 6, 6, 6, 6, 6, // addr1
                3, 3, 3, 3, 3, 3, // addr2
                6, 6, 6, 6, 6, 6, // addr3
            ][..]
        );

        // Successful: Associating > Associated
        #[rustfmt::skip]
        let assoc_resp_success = vec![
            // Mgmt Header:
            0b0001_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Assoc Resp Header:
            0, 0, // Capabilities
            0, 0, // Status Code
            11, 0, // AID
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
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &assoc_resp_success[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Associated(_), "not in associated state");

        // Verify a successful connect conf is sent
        let connect_conf = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("error reading Connect.confirm");
        assert_eq!(&connect_conf.peer_sta_address, BSSID.as_array());
        assert_eq!(connect_conf.result_code, fidl_ieee80211::StatusCode::Success);
        assert_eq!(connect_conf.association_id, 11);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_assoced_disassoc_reconnect_timeout() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        // Disassociation: Associated > Associating
        #[rustfmt::skip]
        let disassoc = vec![
            // Mgmt Header:
            0b1010_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Deauth Header:
            4, 0, // Reason Code
        ];
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &disassoc[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Authenticated(_), "not in auth'd state");
        assert!(m.fake_device_state.lock().join_bss_request.is_some());

        // Verify a disassoc ind was sent
        let _disassoc_ind = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::DisassociateIndication>()
            .expect("error reading Disassociate.ind");

        // (sme->mlme) Send a reconnect request
        let reconnect_req = wlan_sme::MlmeRequest::Reconnect(fidl_mlme::ReconnectRequest {
            peer_sta_address: BSSID.to_array(),
        });
        state = state.handle_mlme_req(&mut sta, reconnect_req).await;
        assert_variant!(state, States::Associating(_), "not in associating state");

        // Verify an event was queued up in the timer.
        let (event, _id) = assert_variant!(drain_timeouts(&mut m.time_stream).get(&TimedEventClass::Reassociating), Some(ids) => {
            assert_eq!(ids.len(), 1);
            ids[0].clone()
        });

        // Notify reconnecting timeout
        let state = state.on_timed_event(&mut sta, event).await;
        assert_variant!(state, States::Authenticated(_), "not in auth'd state");
        assert!(m.fake_device_state.lock().join_bss_request.is_some());

        // Verify a connect conf was sent
        let connect_conf = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("error reading Connect.confirm");
        assert_eq!(
            connect_conf,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: BSSID.to_array(),
                result_code: fidl_ieee80211::StatusCode::RejectedSequenceTimeout,
                association_id: 0,
                association_ies: vec![],
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_assoced_disassoc_reconnect_denied() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        // Disassociation: Associated > Associating
        #[rustfmt::skip]
        let disassoc = vec![
            // Mgmt Header:
            0b1010_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Deauth Header:
            4, 0, // Reason Code
        ];
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &disassoc[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Authenticated(_), "not in auth'd state");
        assert!(m.fake_device_state.lock().join_bss_request.is_some());

        // Verify a disassoc ind was sent
        let _disassoc_ind = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::DisassociateIndication>()
            .expect("error reading Disassociate.ind");

        // (sme->mlme) Send a reconnect request with a different BSSID
        let sus_bssid = [b's', b'u', b's', b'r', b'e', b'q'];
        let reconnect_req = wlan_sme::MlmeRequest::Reconnect(fidl_mlme::ReconnectRequest {
            peer_sta_address: sus_bssid,
        });
        state = state.handle_mlme_req(&mut sta, reconnect_req).await;
        assert_variant!(state, States::Authenticated(_), "not in auth'd state");

        // Verify a connect conf was sent
        let connect_conf = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("error reading Connect.confirm");
        assert_eq!(
            connect_conf,
            fidl_mlme::ConnectConfirm {
                peer_sta_address: sus_bssid,
                result_code: fidl_ieee80211::StatusCode::NotInSameBss,
                association_id: 0,
                association_ies: vec![],
            }
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_assoced_reconnect_no_op() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let association = Association {
            aid: 42,
            assoc_resp_ies: vec![
                // Basic Rates
                0x01, 0x08, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24,
            ],
            ..empty_association(&mut sta)
        };
        let mut state = States::from(statemachine::testing::new_state(Associated(association)));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());

        // (sme->mlme) Send a reconnect request
        let reconnect_req = wlan_sme::MlmeRequest::Reconnect(fidl_mlme::ReconnectRequest {
            peer_sta_address: BSSID.to_array(),
        });
        state = state.handle_mlme_req(&mut sta, reconnect_req).await;
        assert_variant!(state, States::Associated(_), "not in associated state");
        assert!(m.fake_device_state.lock().join_bss_request.is_some());

        // Verify a successful connect conf is sent
        let connect_conf = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::ConnectConfirm>()
            .expect("error reading Connect.confirm");
        assert_eq!(&connect_conf.peer_sta_address, BSSID.as_array());
        assert_eq!(connect_conf.result_code, fidl_ieee80211::StatusCode::Success);
        assert_eq!(connect_conf.association_id, 42);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn state_transitions_assoced_deauthing() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let mut state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        // Deauthentication: Associated > Joined
        #[rustfmt::skip]
        let deauth = vec![
            // Mgmt Header:
            0b1100_00_00, 0b00000000, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            3, 3, 3, 3, 3, 3, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0x10, 0, // Sequence Control
            // Deauth Header:
            4, 0, // Reason Code
        ];
        let rx_info = mock_rx_info(&sta);
        state = state.on_mac_frame(&mut sta, &deauth[..], rx_info, 0.into()).await;
        assert_variant!(state, States::Joined(_), "not in joined state");
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[test_case(false, false; "unprotected bss, not scanning")]
    #[test_case(true, false; "protected bss, not scanning")]
    #[test_case(false, true; "unprotected bss, scanning")]
    #[test_case(true, true; "protected bss, scanning")]
    #[fuchsia::test(allow_stalls = false)]
    async fn assoc_send_eth_frame_becomes_data_frame(protected: bool, scanning: bool) {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta =
            if protected { make_protected_client_station() } else { make_client_station() };
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = States::from(statemachine::testing::new_state(Associated(Association {
            controlled_port_open: true,
            ..empty_association(&mut sta)
        })));

        if scanning {
            let mut bound_scanner = sta.scanner.bind(sta.ctx);
            bound_scanner
                .on_sme_scan(fidl_mlme::ScanRequest {
                    txn_id: 1337,
                    scan_type: fidl_mlme::ScanTypes::Passive,
                    channel_list: vec![1],
                    ssid_list: vec![],
                    probe_delay: 0,
                    min_channel_time: 100,
                    max_channel_time: 300,
                })
                .await
                .expect("Failed to start scan");
            assert!(sta.scanner.is_scanning());
        }

        let eth_frame = [
            1, 2, 3, 4, 5, 6, // dst_addr
            3, 3, 3, 3, 3, 3, // src_addr == IFACE_MAC
            0x0d, 0x05, // ether_type
            21, 22, 23, 24, 25, 26, 27, 28, // payload
            29, // more payload
        ];

        state.on_eth_frame(&mut sta, &eth_frame[..], 0.into()).expect("all good");

        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        let (data_frame, _tx_flags) = m.fake_device_state.lock().wlan_queue.remove(0);
        let mut fc_byte_2 = 0b00000001;
        if protected {
            fc_byte_2 |= 0b01000000;
        }
        assert_eq!(
            &data_frame[..],
            &[
                // Data header
                0b00001000, fc_byte_2, // Frame Control
                0, 0, // Duration
                6, 6, 6, 6, 6, 6, // addr1
                3, 3, 3, 3, 3, 3, // addr2 (from src_addr above)
                1, 2, 3, 4, 5, 6, // addr3 (from dst_addr above)
                0x10, 0, // Sequence Control
                // LLC header
                0xAA, 0xAA, 0x03, // DSAP, SSAP, Control, OUI
                0, 0, 0, // OUI
                0x0d, 0x05, // Protocol ID (from ether_type above)
                21, 22, 23, 24, 25, 26, 27, 28, // Payload
                29, // More payload
            ][..]
        )
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn eth_frame_dropped_when_off_channel() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));

        sta.ctx
            .device
            .set_channel(fidl_common::WlanChannel {
                primary: 42,
                cbw: fidl_common::ChannelBandwidth::Cbw20,
                secondary80: 0,
            })
            .await
            .expect("fake device is obedient");
        let eth_frame = &[100; 14]; // An ethernet frame must be at least 14 bytes long.

        let error = state
            .on_eth_frame(&mut sta, &eth_frame[..], 0.into())
            .expect_err("Ethernet frame is dropped when client is off channel");
        assert_variant!(error, Error::Status(_str, status) =>
            assert_eq!(status, zx::Status::BAD_STATE),
            "error should contain a status"
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn assoc_eth_frame_too_short_dropped() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));

        let eth_frame = &[100; 13]; // Needs at least 14 bytes for header.

        let error = state
            .on_eth_frame(&mut sta, &eth_frame[..], 0.into())
            .expect_err("Ethernet frame is too short");
        assert_variant!(error, Error::Status(_str, status) =>
            assert_eq!(status, zx::Status::IO_DATA_INTEGRITY),
            "error should contain a status"
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn assoc_controlled_port_closed_eth_frame_dropped() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));

        let eth_frame = &[100; 14]; // long enough for ethernet header.

        let error = state
            .on_eth_frame(&mut sta, &eth_frame[..], 0.into())
            .expect_err("Ethernet frame canot be sent when controlled port is closed");
        assert_variant!(error, Error::Status(_str, status) =>
            assert_eq!(status, zx::Status::BAD_STATE),
            "Error should contain status"
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn not_assoc_eth_frame_dropped() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = States::from(statemachine::testing::new_state(Joined));

        let eth_frame = &[100; 14]; // long enough for ethernet header.

        let error = state
            .on_eth_frame(&mut sta, &eth_frame[..], 0.into())
            .expect_err("Ethernet frame cannot be sent in Joined state");
        assert_variant !(error, Error::Status(_str, status) =>
            assert_eq!(status, zx::Status::BAD_STATE),
            "Error should contain status"
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn joined_sme_deauth() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = States::from(statemachine::testing::new_state(Joined));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        let state = state.handle_mlme_req(&mut sta, fake_deauth_req()).await;
        assert_variant!(state, States::Joined(_), "Joined should stay in Joined");
        // No MLME message was sent because MLME already deauthenticated.
        m.fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::DeauthenticateIndication>()
            .expect_err("should be no outgoing message");
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn authenticating_sme_deauth() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = States::from(statemachine::testing::new_state(open_authenticating(&mut sta)));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        let state = state.handle_mlme_req(&mut sta, fake_deauth_req()).await;

        assert_variant!(state, States::Joined(_), "should transition to Joined");

        // No need to notify SME since it already deauthenticated
        m.fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::DeauthenticateConfirm>()
            .expect_err("should not see more MLME messages");
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associating_sme_deauth() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = States::from(statemachine::testing::new_state(Associating::default()));

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        let state = state.handle_mlme_req(&mut sta, fake_deauth_req()).await;

        assert_variant!(state, States::Joined(_), "should transition to Joined");

        // No need to notify SME since it already deauthenticated
        m.fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::DeauthenticateConfirm>()
            .expect_err("should not see more MLME messages");
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_sme_deauth() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx_with_bss().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = States::from(statemachine::testing::new_state(Associated(Association {
            controlled_port_open: true,
            ..empty_association(&mut sta)
        })));

        sta.ctx
            .device
            .notify_association_complete(fake_assoc_cfg())
            .await
            .expect("valid assoc ctx should not fail");
        assert_eq!(1, m.fake_device_state.lock().assocs.len());

        assert!(m.fake_device_state.lock().join_bss_request.is_some());
        sta.ctx.device.set_ethernet_up().await.expect("should succeed");
        assert_eq!(crate::device::LinkStatus::UP, m.fake_device_state.lock().link_status);

        let state = state.handle_mlme_req(&mut sta, fake_deauth_req()).await;
        assert_variant!(state, States::Joined(_), "should transition to Joined");

        // Should accept the deauthentication request and send back confirm.
        let deauth_conf = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::DeauthenticateConfirm>()
            .expect("should see deauth conf");
        assert_eq!(
            deauth_conf,
            fidl_mlme::DeauthenticateConfirm { peer_sta_address: BSSID.to_array() }
        );
        m.fake_device_state
            .lock()
            .next_mlme_msg::<fidl_mlme::DeauthenticateConfirm>()
            .expect_err("should not see more MLME messages");
        // Verify association context cleared.
        assert_eq!(0, m.fake_device_state.lock().assocs.len());
        // Verify ethernet link status is down.
        assert_eq!(crate::device::LinkStatus::DOWN, m.fake_device_state.lock().link_status);
        assert!(m.fake_device_state.lock().join_bss_request.is_none());
    }

    fn fake_eapol_req() -> wlan_sme::MlmeRequest {
        wlan_sme::MlmeRequest::Eapol(fidl_mlme::EapolRequest {
            dst_addr: BSSID.to_array(),
            src_addr: IFACE_MAC.to_array(),
            data: vec![1, 2, 3, 4],
        })
    }

    #[allow(deprecated)]
    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_eapol_not_associated() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_protected_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let state = States::from(statemachine::testing::new_state(Joined));
        let _state = state.handle_mlme_req(&mut sta, fake_eapol_req()).await;
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 0);

        let state = States::from(statemachine::testing::new_state(open_authenticating(&mut sta)));
        m.fake_device_state.lock().wlan_queue.clear();
        let _state = state.handle_mlme_req(&mut sta, fake_eapol_req()).await;
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 0);

        let state = States::from(statemachine::testing::new_state(Associating::default()));
        let _state = state.handle_mlme_req(&mut sta, fake_eapol_req()).await;
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 0);
    }

    #[allow(deprecated)]
    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_eapol_associated_not_protected() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));
        let _state = state.handle_mlme_req(&mut sta, fake_eapol_req()).await;
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 0);
    }

    #[allow(deprecated)]
    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_eapol_associated() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_protected_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));
        let _state = state.handle_mlme_req(&mut sta, fake_eapol_req()).await;
        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        assert_eq!(
            &m.fake_device_state.lock().wlan_queue[0].0[..],
            &[
                // Data header (EAPoL frames are data frames)
                0b00001000, 0b00000001, // Frame Control
                0, 0, // Duration
                6, 6, 6, 6, 6, 6, // addr1 - BSSID
                3, 3, 3, 3, 3, 3, // addr2 - IFACE_MAC
                6, 6, 6, 6, 6, 6, // addr3 - BSSID
                0x10, 0, // Sequence Control
                // LLC header
                0xAA, 0xAA, 0x03, // DSAP, SSAP, Control, OUI
                0, 0, 0, // OUI
                0x88, 0x8E, // Protocol ID (EAPoL is 0x888E)
                1, 2, 3, 4, // Payload
            ][..]
        );
    }

    #[allow(deprecated)]
    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_set_keys_not_associated() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_protected_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let state = States::from(statemachine::testing::new_state(Joined));
        let _state = state.handle_mlme_req(&mut sta, fake_set_keys_req((*BSSID).into())).await;
        assert_eq!(m.fake_device_state.lock().keys.len(), 0);

        let state = States::from(statemachine::testing::new_state(open_authenticating(&mut sta)));
        let _state = state.handle_mlme_req(&mut sta, fake_set_keys_req((*BSSID).into())).await;
        assert_eq!(m.fake_device_state.lock().keys.len(), 0);

        let state = States::from(statemachine::testing::new_state(Associating::default()));
        let _state = state.handle_mlme_req(&mut sta, fake_set_keys_req((*BSSID).into())).await;
        assert_eq!(m.fake_device_state.lock().keys.len(), 0);
    }

    #[allow(deprecated)]
    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_set_keys_associated_not_protected() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));
        let _state = state.handle_mlme_req(&mut sta, fake_set_keys_req((*BSSID).into())).await;
        assert_eq!(m.fake_device_state.lock().keys.len(), 0);
    }

    #[allow(deprecated)]
    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_set_keys_associated() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_protected_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));
        let _state = state.handle_mlme_req(&mut sta, fake_set_keys_req((*BSSID).into())).await;
        assert_eq!(m.fake_device_state.lock().keys.len(), 1);
        let conf = assert_variant!(m.fake_device_state.lock().next_mlme_msg::<fidl_mlme::SetKeysConfirm>(), Ok(conf) => conf);
        assert_eq!(conf.results.len(), 1);
        assert_eq!(
            conf.results[0],
            fidl_mlme::SetKeyResult { key_id: 6, status: zx::Status::OK.into_raw() }
        );

        assert_eq!(
            m.fake_device_state.lock().keys,
            vec![fidl_softmac::WlanKeyConfiguration {
                protection: Some(fidl_softmac::WlanProtection::RxTx),
                cipher_oui: Some([1, 2, 3]),
                cipher_type: Some(4),
                key_type: Some(fidl_ieee80211::KeyType::Pairwise),
                peer_addr: Some((*BSSID).to_array()),
                key_idx: Some(6),
                key: Some(vec![1, 2, 3, 4, 5, 6, 7]),
                rsc: Some(8),
                ..Default::default()
            }]
        );
    }

    #[allow(deprecated)]
    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_set_keys_failure() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_protected_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));
        m.fake_device_state.lock().install_key_results.push_back(Err(zx::Status::BAD_STATE));
        m.fake_device_state.lock().install_key_results.push_back(Ok(()));
        // Create a SetKeysReq with one success and one failure.
        let mut set_keys_req = fake_set_keys_req((*BSSID).into());
        match &mut set_keys_req {
            wlan_sme::MlmeRequest::SetKeys(req) => {
                req.keylist
                    .push(fidl_mlme::SetKeyDescriptor { key_id: 4, ..req.keylist[0].clone() });
            }
            _ => panic!(),
        }
        let _state = state.handle_mlme_req(&mut sta, set_keys_req).await;
        let conf = assert_variant!(m.fake_device_state.lock().next_mlme_msg::<fidl_mlme::SetKeysConfirm>(), Ok(conf) => conf);
        assert_eq!(conf.results.len(), 2);
        assert_eq!(
            conf.results[0],
            fidl_mlme::SetKeyResult { key_id: 6, status: zx::Status::BAD_STATE.into_raw() }
        );
        assert_eq!(
            conf.results[1],
            fidl_mlme::SetKeyResult { key_id: 4, status: zx::Status::OK.into_raw() }
        );
    }

    fn fake_set_ctrl_port_open(open: bool) -> wlan_sme::MlmeRequest {
        wlan_sme::MlmeRequest::SetCtrlPort(fidl_mlme::SetControlledPortRequest {
            peer_sta_address: BSSID.to_array(),
            state: match open {
                true => fidl_mlme::ControlledPortState::Open,
                false => fidl_mlme::ControlledPortState::Closed,
            },
        })
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_set_controlled_port_not_associated() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_protected_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let state = States::from(statemachine::testing::new_state(Joined));
        let _state = state.handle_mlme_req(&mut sta, fake_set_ctrl_port_open(true)).await;
        assert_eq!(m.fake_device_state.lock().link_status, crate::device::LinkStatus::DOWN);

        let state = States::from(statemachine::testing::new_state(open_authenticating(&mut sta)));
        let _state = state.handle_mlme_req(&mut sta, fake_set_ctrl_port_open(true)).await;
        assert_eq!(m.fake_device_state.lock().link_status, crate::device::LinkStatus::DOWN);

        let state = States::from(statemachine::testing::new_state(Associating::default()));
        let _state = state.handle_mlme_req(&mut sta, fake_set_ctrl_port_open(true)).await;
        assert_eq!(m.fake_device_state.lock().link_status, crate::device::LinkStatus::DOWN);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_set_controlled_port_associated_not_protected() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));
        let _state = state.handle_mlme_req(&mut sta, fake_set_ctrl_port_open(true)).await;
        assert_eq!(m.fake_device_state.lock().link_status, crate::device::LinkStatus::DOWN);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn mlme_set_controlled_port_associated() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_protected_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let state =
            States::from(statemachine::testing::new_state(Associated(empty_association(&mut sta))));
        assert_eq!(m.fake_device_state.lock().link_status, crate::device::LinkStatus::DOWN);
        let state = state.handle_mlme_req(&mut sta, fake_set_ctrl_port_open(true)).await;
        assert_eq!(m.fake_device_state.lock().link_status, crate::device::LinkStatus::UP);
        let _state = state.handle_mlme_req(&mut sta, fake_set_ctrl_port_open(false)).await;
        assert_eq!(m.fake_device_state.lock().link_status, crate::device::LinkStatus::DOWN);
    }

    #[test_case(true; "while scanning")]
    #[test_case(false; "while not scanning")]
    #[fuchsia::test(allow_stalls = false)]
    async fn associated_rx_succeeds(scanning: bool) {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = States::from(statemachine::testing::new_state(Associated(Association {
            aid: 1,
            controlled_port_open: true,
            ..empty_association(&mut sta)
        })));

        if scanning {
            let mut bound_scanner = sta.scanner.bind(sta.ctx);
            bound_scanner
                .on_sme_scan(fidl_mlme::ScanRequest {
                    txn_id: 1337,
                    scan_type: fidl_mlme::ScanTypes::Passive,
                    channel_list: vec![1],
                    ssid_list: vec![],
                    probe_delay: 0,
                    min_channel_time: 100,
                    max_channel_time: 300,
                })
                .await
                .expect("Failed to start scan");
            assert!(sta.scanner.is_scanning());
        }

        let fc = mac::FrameControl(0)
            .with_frame_type(mac::FrameType::DATA)
            .with_data_subtype(mac::DataSubtype(0))
            .with_from_ds(true);
        let fc = fc.0.to_le_bytes();

        let data_frame = vec![
            // Data header:
            fc[0], fc[1], // FC
            0, 0, // Duration
            7, 7, 7, 7, 7, 7, // addr1
            6, 6, 6, 6, 6, 6, // addr2
            42, 42, 42, 42, 42, 42, // addr3
            0x10, 0, // Sequence Control
            // LLC Header
            7, 7, 7, // DSAP, SSAP & control
            8, 8, 8, // OUI
            9, 10, // eth type
            1, 2, 3, 4, 5, 6, 7, 8, // payload
        ];

        let rx_info = mock_rx_info(&sta);
        state.on_mac_frame(&mut sta, &data_frame[..], rx_info, 0.into()).await;
        assert_eq!(m.fake_device_state.lock().eth_queue.len(), 1);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_rx_with_wrong_cbw_succeeds() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = States::from(statemachine::testing::new_state(Associated(Association {
            aid: 1,
            controlled_port_open: true,
            ..empty_association(&mut sta)
        })));

        let fc = mac::FrameControl(0)
            .with_frame_type(mac::FrameType::DATA)
            .with_data_subtype(mac::DataSubtype(0))
            .with_from_ds(true);
        let fc = fc.0.to_le_bytes();

        let data_frame = vec![
            // Data header:
            fc[0], fc[1], // FC
            0, 0, // Duration
            7, 7, 7, 7, 7, 7, // addr1
            6, 6, 6, 6, 6, 6, // addr2
            42, 42, 42, 42, 42, 42, // addr3
            0x10, 0, // Sequence Control
            // LLC Header
            7, 7, 7, // DSAP, SSAP & control
            8, 8, 8, // OUI
            9, 10, // eth type
            1, 2, 3, 4, 5, 6, 7, 8, // payload
        ];

        let mut rx_info = mock_rx_info(&sta);
        // We deliberately ignore the cbw, since it isn't important and not all
        // drivers report it consistently.
        rx_info.channel.cbw = fidl_common::ChannelBandwidth::Cbw80;
        state.on_mac_frame(&mut sta, &data_frame[..], rx_info, 0.into()).await;
        assert_eq!(m.fake_device_state.lock().eth_queue.len(), 1);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_request_bu_if_tim_indicates_buffered_frame() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = States::from(statemachine::testing::new_state(Associated(Association {
            aid: 1,
            ..empty_association(&mut sta)
        })));

        let beacon = [
            // Mgmt header
            0b10000000, 0, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            7, 7, 7, 7, 7, 7, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0, 0, // Sequence Control
            // Beacon header:
            0, 0, 0, 0, 0, 0, 0, 0, // Timestamp
            10, 0, // Beacon interval
            33, 0, // Capabilities
            5, 4, 0, 0, 0, 0b00000010, // Tim IE: bit 1 in the last octet indicates AID 1
        ];

        let rx_info = mock_rx_info(&sta);
        state.on_mac_frame(&mut sta, &beacon[..], rx_info, 0.into()).await;

        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 1);
        assert_eq!(
            &m.fake_device_state.lock().wlan_queue[0].0[..],
            &[
                0b10100100, 0, // Frame control
                1, 0b11000000, // ID (2 MSBs are set to 1 from the AID)
                6, 6, 6, 6, 6, 6, // BSSID
                3, 3, 3, 3, 3, 3, // TA
            ][..]
        );
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn associated_does_not_request_bu_if_tim_indicates_no_buffered_frame() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);
        let state = States::from(statemachine::testing::new_state(Associated(Association {
            aid: 1,
            ..empty_association(&mut sta)
        })));

        let beacon = [
            // Mgmt header
            0b10000000, 0, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            7, 7, 7, 7, 7, 7, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0, 0, // Sequence Control
            // Beacon header:
            0, 0, 0, 0, 0, 0, 0, 0, // Timestamp
            10, 0, // Beacon interval
            33, 0, // Capabilities
            5, 4, 0, 0, 0, 0, // Tim IE: No buffered frame for any client.
        ];
        let rx_info = mock_rx_info(&sta);
        state.on_mac_frame(&mut sta, &beacon[..], rx_info, 0.into()).await;

        assert_eq!(m.fake_device_state.lock().wlan_queue.len(), 0);
    }

    fn rx_info_with_dbm<'a>(
        client: &BoundClient<'a, FakeDevice>,
        rssi_dbm: i8,
    ) -> fidl_softmac::WlanRxInfo {
        let mut rx_info = fidl_softmac::WlanRxInfo { rssi_dbm, ..mock_rx_info(client) };
        rx_info.valid_fields |= fidl_softmac::WlanRxInfoValid::RSSI;
        rx_info
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn signal_report() {
        let mut m = MockObjects::new().await;
        let mut ctx = m.make_ctx().await;
        let mut sta = make_protected_client_station();
        let mut sta = sta.bind(&mut ctx, &mut m.scanner, &mut m.channel_state);

        let state = States::from(State::from(statemachine::testing::new_state(Associated(
            empty_association(&mut sta),
        ))));

        let (_, timed_event, _) =
            m.time_stream.try_next().unwrap().expect("Should have scheduled signal report timeout");
        let state = state.on_timed_event(&mut sta, timed_event.event).await;

        let signal_ind = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_internal::SignalReportIndication>()
            .expect("should see a signal report");

        // -128 is the default value, equivalent to 0 watt.
        assert_eq!(signal_ind.rssi_dbm, -128);

        let beacon = [
            // Mgmt header
            0b10000000, 0, // Frame Control
            0, 0, // Duration
            3, 3, 3, 3, 3, 3, // Addr1 == IFACE_MAC
            7, 7, 7, 7, 7, 7, // Addr2
            6, 6, 6, 6, 6, 6, // Addr3
            0, 0, // Sequence Control
            // Beacon header:
            0, 0, 0, 0, 0, 0, 0, 0, // Timestamp
            10, 0, // Beacon interval
            33, 0, // Capabilities
        ];

        const EXPECTED_DBM: i8 = -32;
        let rx_info = rx_info_with_dbm(&sta, EXPECTED_DBM);
        let state = state.on_mac_frame(&mut sta, &beacon[..], rx_info, 0.into()).await;

        let (_, timed_event, _) =
            m.time_stream.try_next().unwrap().expect("Should have scheduled signal report timeout");
        let _state = state.on_timed_event(&mut sta, timed_event.event).await;

        let signal_ind = m
            .fake_device_state
            .lock()
            .next_mlme_msg::<fidl_internal::SignalReportIndication>()
            .expect("should see a signal report");

        assert_eq!(signal_ind.rssi_dbm, EXPECTED_DBM);
    }
}
