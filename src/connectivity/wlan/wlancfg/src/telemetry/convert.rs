// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::client::roaming::lib::RoamReason;
use fidl_fuchsia_wlan_sme as fidl_sme;
use wlan_common::bss::Protection as BssProtection;
use wlan_common::channel::Channel;
use wlan_metrics_registry as metrics;

pub fn convert_disconnect_source(
    source: &fidl_sme::DisconnectSource,
) -> metrics::ConnectivityWlanMetricDimensionDisconnectSource {
    use metrics::ConnectivityWlanMetricDimensionDisconnectSource::*;
    match source {
        fidl_sme::DisconnectSource::Ap(..) => Ap,
        fidl_sme::DisconnectSource::User(..) => User,
        fidl_sme::DisconnectSource::Mlme(..) => Mlme,
    }
}

pub fn convert_user_wait_time(
    duration: zx::MonotonicDuration,
) -> metrics::ConnectivityWlanMetricDimensionWaitTime {
    use metrics::ConnectivityWlanMetricDimensionWaitTime::*;
    match duration {
        x if x < zx::MonotonicDuration::from_seconds(1) => LessThan1Second,
        x if x < zx::MonotonicDuration::from_seconds(3) => LessThan3Seconds,
        x if x < zx::MonotonicDuration::from_seconds(5) => LessThan5Seconds,
        x if x < zx::MonotonicDuration::from_seconds(8) => LessThan8Seconds,
        x if x < zx::MonotonicDuration::from_seconds(15) => LessThan15Seconds,
        _ => AtLeast15Seconds,
    }
}

pub fn convert_is_multi_bss(
    multiple_bss_candidates: bool,
) -> metrics::SuccessfulConnectBreakdownByIsMultiBssMetricDimensionIsMultiBss {
    use metrics::SuccessfulConnectBreakdownByIsMultiBssMetricDimensionIsMultiBss::*;
    match multiple_bss_candidates {
        true => Yes,
        false => No,
    }
}

pub fn convert_security_type(
    protection: &BssProtection,
) -> metrics::SuccessfulConnectBreakdownBySecurityTypeMetricDimensionSecurityType {
    use metrics::SuccessfulConnectBreakdownBySecurityTypeMetricDimensionSecurityType::*;
    match protection {
        BssProtection::Unknown => Unknown,
        BssProtection::Open => Open,
        BssProtection::Wep => Wep,
        BssProtection::Wpa1 => Wpa1,
        BssProtection::Wpa1Wpa2PersonalTkipOnly => Wpa1Wpa2PersonalTkipOnly,
        BssProtection::Wpa2PersonalTkipOnly => Wpa2PersonalTkipOnly,
        BssProtection::Wpa1Wpa2Personal => Wpa1Wpa2Personal,
        BssProtection::Wpa2Personal => Wpa2Personal,
        BssProtection::Wpa2Wpa3Personal => Wpa2Wpa3Personal,
        BssProtection::Wpa3Personal => Wpa3Personal,
        BssProtection::Wpa2Enterprise => Wpa2Enterprise,
        BssProtection::Wpa3Enterprise => Wpa3Enterprise,
        BssProtection::Owe => Owe,
        BssProtection::OpenOweTransition => OpenOweTransition,
    }
}

pub fn convert_channel_band(
    primary_channel: u8,
) -> metrics::SuccessfulConnectBreakdownByChannelBandMetricDimensionChannelBand {
    use metrics::SuccessfulConnectBreakdownByChannelBandMetricDimensionChannelBand::*;
    if primary_channel > 14 { Band5Ghz } else { Band2Dot4Ghz }
}

pub fn convert_rssi_bucket(rssi: i8) -> metrics::ConnectivityWlanMetricDimensionRssiBucket {
    use metrics::ConnectivityWlanMetricDimensionRssiBucket::*;
    match rssi {
        -128..=-90 => From128To90,
        -89..=-86 => From89To86,
        -85..=-83 => From85To83,
        -82..=-80 => From82To80,
        -79..=-77 => From79To77,
        -76..=-74 => From76To74,
        -73..=-71 => From73To71,
        -70..=-66 => From70To66,
        -65..=-61 => From65To61,
        -60..=-51 => From60To51,
        -50..=-35 => From50To35,
        -34..=-28 => From34To28,
        -27..=-1 => From27To1,
        _ => _0,
    }
}

pub fn convert_snr_bucket(snr: i8) -> metrics::ConnectivityWlanMetricDimensionSnrBucket {
    use metrics::ConnectivityWlanMetricDimensionSnrBucket::*;
    match snr {
        1..=10 => From1To10,
        11..=15 => From11To15,
        16..=25 => From16To25,
        26..=40 => From26To40,
        41..=127 => MoreThan40,
        _ => _0,
    }
}

pub fn convert_roam_reason_dimension(
    reason: RoamReason,
) -> metrics::PolicyRoamConnectedDurationBeforeRoamAttemptMetricDimensionReason {
    use metrics::PolicyRoamConnectedDurationBeforeRoamAttemptMetricDimensionReason::*;
    match reason {
        RoamReason::RssiBelowThreshold => RssiBelowThreshold,
        RoamReason::SnrBelowThreshold => SnrBelowThreshold,
    }
}

pub fn get_ghz_band_transition(
    origin_channel: &Channel,
    target_channel: &Channel,
) -> metrics::ConnectivityWlanMetricDimensionGhzBandTransition {
    let origin_is_2g = origin_channel.is_2ghz();
    let origin_is_5g = origin_channel.is_5ghz();
    let target_is_2g = target_channel.is_2ghz();
    let target_is_5g = target_channel.is_5ghz();

    match (origin_is_2g, origin_is_5g, target_is_2g, target_is_5g) {
        (true, false, true, false) => {
            metrics::ConnectivityWlanMetricDimensionGhzBandTransition::From2gTo2g
        }
        (true, false, false, true) => {
            metrics::ConnectivityWlanMetricDimensionGhzBandTransition::From2gTo5g
        }
        (true, false, false, false) => {
            metrics::ConnectivityWlanMetricDimensionGhzBandTransition::From2gTo6g
        }
        (false, true, true, false) => {
            metrics::ConnectivityWlanMetricDimensionGhzBandTransition::From5gTo2g
        }
        (false, true, false, true) => {
            metrics::ConnectivityWlanMetricDimensionGhzBandTransition::From5gTo5g
        }
        (false, true, false, false) => {
            metrics::ConnectivityWlanMetricDimensionGhzBandTransition::From5gTo6g
        }
        (false, false, true, false) => {
            metrics::ConnectivityWlanMetricDimensionGhzBandTransition::From6gTo2g
        }
        (false, false, false, true) => {
            metrics::ConnectivityWlanMetricDimensionGhzBandTransition::From6gTo5g
        }
        (false, false, false, false) => {
            metrics::ConnectivityWlanMetricDimensionGhzBandTransition::From6gTo6g
        }
        _ => panic!("Invalid channel band combination"),
    }
}

// Convert an RSSI delta (i8) into the bucket index for an RSSI delta based histogram, where bucket
// indexes run from 0 to 128, covering delta values -128 to 127, with step size of 2.
//
// bucket_index = (delta + 129) / 2
pub fn calculate_rssi_delta_bucket(delta: i8) -> i64 {
    let num_buckets: i64 = metrics::POLICY_ROAM_TRANSITION_RSSI_DELTA_BY_ROAM_REASON_FLEETWIDE_HISTOGRAM_INT_BUCKETS_NUM_BUCKETS.into();
    let step_size: i64 = metrics::POLICY_ROAM_TRANSITION_RSSI_DELTA_BY_ROAM_REASON_FLEETWIDE_HISTOGRAM_INT_BUCKETS_STEP_SIZE.into();
    let idx = delta as i64 + num_buckets;

    // "Euclidean" division floors towards -infinity (rather than toward zero).
    // By subtracting 1 before dividing and adding 1 after, we achieve ceiling division
    // that works across positive and negative numbers.
    // TODO(https://github.com/rust-lang/rust/issues/88581): Replace with `{integer}::div_ceil()`
    // when `int_roundings` is available.
    (idx - 1).div_euclid(step_size) + 1
}

pub fn convert_disconnect_info(
    info: &crate::telemetry::DisconnectInfo,
) -> wlan_telemetry::DisconnectInfo {
    wlan_telemetry::DisconnectInfo {
        iface_id: info.iface_id,
        connected_duration: zx::BootDuration::from_nanos(info.connected_duration.into_nanos()),
        is_sme_reconnecting: info.is_sme_reconnecting,
        disconnect_source: info.disconnect_source,
        original_bss_desc: Box::new(info.ap_state.original().clone()),
        current_rssi_dbm: info.ap_state.tracked.signal.rssi_dbm,
        current_snr_db: info.ap_state.tracked.signal.snr_db,
        current_channel: info.ap_state.tracked.channel,
    }
}

pub fn convert_client_connections_toggle(
    event: &crate::telemetry::TelemetryEvent,
) -> Option<wlan_telemetry::ClientConnectionsToggleEvent> {
    match event {
        crate::telemetry::TelemetryEvent::StartClientConnectionsRequest => {
            Some(wlan_telemetry::ClientConnectionsToggleEvent::Enabled)
        }
        crate::telemetry::TelemetryEvent::StopClientConnectionsRequest => {
            Some(wlan_telemetry::ClientConnectionsToggleEvent::Disabled)
        }
        _ => None,
    }
}

pub fn convert_to_wlan_telemetry_event(
    event: &crate::telemetry::TelemetryEvent,
) -> Option<wlan_telemetry::TelemetryEvent> {
    match event {
        crate::telemetry::TelemetryEvent::ConnectResult { result, ap_state, .. } => {
            Some(wlan_telemetry::TelemetryEvent::ConnectResult {
                result: result.code,
                bss: Box::new(ap_state.original().clone()),
                is_credential_rejected: result.is_credential_rejected,
                is_owe_transition: ap_state.original().protection()
                    == BssProtection::OpenOweTransition,
            })
        }
        crate::telemetry::TelemetryEvent::Disconnected { info: Some(info), .. } => {
            Some(wlan_telemetry::TelemetryEvent::Disconnect { info: convert_disconnect_info(info) })
        }
        crate::telemetry::TelemetryEvent::StartClientConnectionsRequest
        | crate::telemetry::TelemetryEvent::StopClientConnectionsRequest => {
            convert_client_connections_toggle(event).map(|toggle| {
                wlan_telemetry::TelemetryEvent::ClientConnectionsToggle { event: toggle }
            })
        }
        crate::telemetry::TelemetryEvent::IfaceCreationResult(Err(())) => {
            Some(wlan_telemetry::TelemetryEvent::IfaceCreationFailure)
        }
        crate::telemetry::TelemetryEvent::IfaceDestructionResult(Err(())) => {
            Some(wlan_telemetry::TelemetryEvent::IfaceDestructionFailure)
        }
        crate::telemetry::TelemetryEvent::SmeTimeout { .. } => {
            Some(wlan_telemetry::TelemetryEvent::SmeTimeout)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wlan_common::random_bss_description;

    #[fuchsia::test]
    fn test_calculate_rssi_delta_bucket() {
        // Lowest bucket encompasses i8::MIN, so hitting underflow bucket (idx: 0)
        // is not possible.
        assert_eq!(calculate_rssi_delta_bucket(i8::MIN), 1);

        // Linear buckets for deltas, with step size of 2
        assert_eq!(calculate_rssi_delta_bucket(-127), 1);
        assert_eq!(calculate_rssi_delta_bucket(0), 65);
        assert_eq!(calculate_rssi_delta_bucket(1), 65);
        assert_eq!(calculate_rssi_delta_bucket(i8::MAX), 128);
    }

    #[fuchsia::test]
    fn test_convert_client_connections_toggle() {
        assert_eq!(
            convert_client_connections_toggle(
                &crate::telemetry::TelemetryEvent::StartClientConnectionsRequest
            ),
            Some(wlan_telemetry::ClientConnectionsToggleEvent::Enabled)
        );
        assert_eq!(
            convert_client_connections_toggle(
                &crate::telemetry::TelemetryEvent::StopClientConnectionsRequest
            ),
            Some(wlan_telemetry::ClientConnectionsToggleEvent::Disabled)
        );
        assert_eq!(
            convert_client_connections_toggle(
                &crate::telemetry::TelemetryEvent::ClearEstablishConnectionStartTime
            ),
            None
        );
    }

    #[fuchsia::test]
    fn test_convert_disconnect_info() {
        let bss = random_bss_description!(Wpa2);
        let mut ap_state: crate::client::types::ApState = bss.clone().into();
        ap_state.tracked.signal.rssi_dbm = -55;
        ap_state.tracked.signal.snr_db = 32;
        let info = crate::telemetry::DisconnectInfo {
            iface_id: 1,
            connected_duration: zx::MonotonicDuration::from_seconds(120),
            is_sme_reconnecting: true,
            disconnect_source: fidl_sme::DisconnectSource::User(
                fidl_sme::UserDisconnectReason::FidlConnectRequest,
            ),
            previous_connect_reason: crate::client::types::ConnectReason::IdleInterfaceAutoconnect,
            ap_state,
            signals: crate::util::historical_list::HistoricalList::new(8),
        };

        let converted = convert_disconnect_info(&info);
        assert_eq!(converted.iface_id, 1);
        assert_eq!(converted.connected_duration, zx::BootDuration::from_seconds(120));
        assert!(converted.is_sme_reconnecting);
        assert_eq!(
            converted.disconnect_source,
            fidl_sme::DisconnectSource::User(fidl_sme::UserDisconnectReason::FidlConnectRequest)
        );
        assert_eq!(converted.original_bss_desc.bssid, bss.bssid);
        assert_eq!(converted.current_rssi_dbm, -55);
        assert_eq!(converted.current_snr_db, 32);
        assert_eq!(converted.current_channel, bss.channel);
    }

    #[fuchsia::test]
    fn test_convert_to_wlan_telemetry_event() {
        let bss = random_bss_description!(Wpa2);
        let connect_result_event = crate::telemetry::TelemetryEvent::ConnectResult {
            iface_id: 0,
            policy_connect_reason: None,
            result: fidl_sme::ConnectResult {
                code: fidl_fuchsia_wlan_ieee80211::StatusCode::Success,
                is_credential_rejected: false,
                is_reconnect: false,
            },
            multiple_bss_candidates: false,
            ap_state: bss.clone().into(),
            network_is_likely_hidden: false,
        };

        let converted = convert_to_wlan_telemetry_event(&connect_result_event);
        match converted {
            Some(wlan_telemetry::TelemetryEvent::ConnectResult {
                result,
                bss: converted_bss,
                is_credential_rejected,
                ..
            }) => {
                assert_eq!(result, fidl_fuchsia_wlan_ieee80211::StatusCode::Success);
                assert_eq!(converted_bss.bssid, bss.bssid);
                assert!(!is_credential_rejected);
            }
            _ => panic!("Expected ConnectResult event"),
        }

        match convert_to_wlan_telemetry_event(&crate::telemetry::TelemetryEvent::SmeTimeout {
            source: crate::telemetry::TimeoutSource::Scan,
        }) {
            Some(wlan_telemetry::TelemetryEvent::SmeTimeout) => {}
            _ => panic!("Expected SmeTimeout event"),
        }

        let disconnect_info = crate::telemetry::DisconnectInfo {
            iface_id: 42,
            connected_duration: zx::MonotonicDuration::from_seconds(120),
            is_sme_reconnecting: true,
            disconnect_source: fidl_sme::DisconnectSource::User(
                fidl_sme::UserDisconnectReason::FidlConnectRequest,
            ),
            previous_connect_reason: crate::client::types::ConnectReason::IdleInterfaceAutoconnect,
            ap_state: bss.clone().into(),
            signals: crate::util::historical_list::HistoricalList::new(8),
        };

        let converted_disconnect =
            convert_to_wlan_telemetry_event(&crate::telemetry::TelemetryEvent::Disconnected {
                track_subsequent_downtime: true,
                info: Some(disconnect_info),
            });
        match converted_disconnect {
            Some(wlan_telemetry::TelemetryEvent::Disconnect { info }) => {
                assert_eq!(info.iface_id, 42);
            }
            _ => panic!("Expected Disconnect event"),
        }

        assert!(
            convert_to_wlan_telemetry_event(
                &crate::telemetry::TelemetryEvent::ClearEstablishConnectionStartTime
            )
            .is_none()
        );
    }
}
