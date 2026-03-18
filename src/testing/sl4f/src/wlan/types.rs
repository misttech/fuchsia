// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_wlan_ieee80211 as fidl_ieee80211;
use fidl_fuchsia_wlan_sme as fidl_sme;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(remote = "fidl_sme::Protection")]
pub(crate) enum ProtectionDef {
    Unknown = 0,
    Open = 1,
    Wep = 2,
    Wpa1 = 3,
    Wpa1Wpa2PersonalTkipOnly = 4,
    Wpa2PersonalTkipOnly = 5,
    Wpa1Wpa2Personal = 6,
    Wpa2Personal = 7,
    Wpa2Wpa3Personal = 8,
    Wpa3Personal = 9,
    Wpa2Enterprise = 10,
    Wpa3Enterprise = 11,
    Owe = 12,
    OpenOweTransition = 13,
}

// The following definitions derive Serialize and Deserialize for remote types, i.e. types
// defined in other crates. See https://serde.rs/remote-derive.html for more info.
#[derive(Serialize, Deserialize)]
#[repr(u32)]
pub(crate) enum ChannelBandwidthDef {
    Cbw20 = 0,
    Cbw40 = 1,
    Cbw40Below = 2,
    Cbw80 = 3,
    Cbw160 = 4,
    Cbw80P80 = 5,
    Unknown = u32::MAX,
}

impl From<fidl_ieee80211::ChannelBandwidth> for ChannelBandwidthDef {
    fn from(fidl_type: fidl_ieee80211::ChannelBandwidth) -> Self {
        match fidl_type {
            fidl_ieee80211::ChannelBandwidth::Cbw20 => Self::Cbw20,
            fidl_ieee80211::ChannelBandwidth::Cbw40 => Self::Cbw40,
            fidl_ieee80211::ChannelBandwidth::Cbw40Below => Self::Cbw40Below,
            fidl_ieee80211::ChannelBandwidth::Cbw80 => Self::Cbw80,
            fidl_ieee80211::ChannelBandwidth::Cbw160 => Self::Cbw160,
            fidl_ieee80211::ChannelBandwidth::Cbw80P80 => Self::Cbw80P80,
            fidl_ieee80211::ChannelBandwidthUnknown!() => Self::Unknown,
        }
    }
}

impl From<ChannelBandwidthDef> for fidl_ieee80211::ChannelBandwidth {
    fn from(serde_type: ChannelBandwidthDef) -> Self {
        match serde_type {
            ChannelBandwidthDef::Cbw20 => Self::Cbw20,
            ChannelBandwidthDef::Cbw40 => Self::Cbw40,
            ChannelBandwidthDef::Cbw40Below => Self::Cbw40Below,
            ChannelBandwidthDef::Cbw80 => Self::Cbw80,
            ChannelBandwidthDef::Cbw160 => Self::Cbw160,
            ChannelBandwidthDef::Cbw80P80 => Self::Cbw80P80,
            ChannelBandwidthDef::Unknown => Self::unknown(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WlanChannelDef {
    pub primary: u8,
    pub cbw: ChannelBandwidthDef,
    pub secondary80: u8,
}

impl From<fidl_ieee80211::WlanChannel> for WlanChannelDef {
    fn from(fidl_type: fidl_ieee80211::WlanChannel) -> Self {
        Self {
            primary: fidl_type.primary,
            cbw: fidl_type.cbw.into(),
            secondary80: fidl_type.secondary80,
        }
    }
}

impl From<WlanChannelDef> for fidl_ieee80211::WlanChannel {
    fn from(serde_type: WlanChannelDef) -> Self {
        Self {
            primary: serde_type.primary,
            cbw: serde_type.cbw.into(),
            secondary80: serde_type.secondary80,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct ServingApInfoDef {
    pub bssid: [u8; 6],
    pub ssid: Vec<u8>,
    pub rssi_dbm: i8,
    pub snr_db: i8,
    pub channel: WlanChannelDef,
    #[serde(with = "ProtectionDef")]
    pub protection: fidl_sme::Protection,
}

impl From<fidl_sme::ServingApInfo> for ServingApInfoDef {
    fn from(fidl_type: fidl_sme::ServingApInfo) -> Self {
        Self {
            bssid: fidl_type.bssid,
            ssid: fidl_type.ssid,
            rssi_dbm: fidl_type.rssi_dbm,
            snr_db: fidl_type.snr_db,
            channel: fidl_type.channel.into(),
            protection: fidl_type.protection,
        }
    }
}

#[derive(Serialize)]
#[serde(remote = "fidl_sme::Empty")]
pub(crate) struct SmeEmptyDef;

#[derive(Serialize)]
pub(crate) enum ClientStatusResponseDef {
    Connected(ServingApInfoDef),
    Connecting(Vec<u8>),
    Roaming(Vec<u8>),
    #[serde(with = "SmeEmptyDef")]
    Idle(fidl_sme::Empty),
}

impl From<fidl_sme::ClientStatusResponse> for ClientStatusResponseDef {
    fn from(fidl_type: fidl_sme::ClientStatusResponse) -> Self {
        match fidl_type {
            fidl_sme::ClientStatusResponse::Connected(info) => Self::Connected(info.into()),
            fidl_sme::ClientStatusResponse::Connecting(vec) => Self::Connecting(vec),
            fidl_sme::ClientStatusResponse::Roaming(bssid) => Self::Roaming(bssid.to_vec()),
            fidl_sme::ClientStatusResponse::Idle(empty) => Self::Idle(empty),
        }
    }
}
