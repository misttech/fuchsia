// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use wlan_legacy_metrics_registry as metrics;

pub fn convert_security_type(
    protection: &wlan_common::bss::Protection,
) -> metrics::SuccessfulConnectBreakdownBySecurityTypeMetricDimensionSecurityType {
    use metrics::SuccessfulConnectBreakdownBySecurityTypeMetricDimensionSecurityType::*;
    match protection {
        wlan_common::bss::Protection::Unknown => Unknown,
        wlan_common::bss::Protection::Open => Open,
        wlan_common::bss::Protection::Wep => Wep,
        wlan_common::bss::Protection::Wpa1 => Wpa1,
        wlan_common::bss::Protection::Wpa1Wpa2PersonalTkipOnly => Wpa1Wpa2PersonalTkipOnly,
        wlan_common::bss::Protection::Wpa2PersonalTkipOnly => Wpa2PersonalTkipOnly,
        wlan_common::bss::Protection::Wpa1Wpa2Personal => Wpa1Wpa2Personal,
        wlan_common::bss::Protection::Wpa2Personal => Wpa2Personal,
        wlan_common::bss::Protection::Wpa2Wpa3Personal => Wpa2Wpa3Personal,
        wlan_common::bss::Protection::Wpa3Personal => Wpa3Personal,
        wlan_common::bss::Protection::Wpa2Enterprise => Wpa2Enterprise,
        wlan_common::bss::Protection::Wpa3Enterprise => Wpa3Enterprise,
        wlan_common::bss::Protection::Owe => Owe,
        wlan_common::bss::Protection::OpenOweTransition => OpenOweTransition,
    }
}

pub fn convert_channel_band(
    primary_channel: u8,
) -> metrics::SuccessfulConnectBreakdownByChannelBandMetricDimensionChannelBand {
    use metrics::SuccessfulConnectBreakdownByChannelBandMetricDimensionChannelBand::*;
    if primary_channel > 14 { Band5Ghz } else { Band2Dot4Ghz }
}
