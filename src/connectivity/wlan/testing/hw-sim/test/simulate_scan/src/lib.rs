// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_wlan_policy as fidl_policy;
use fidl_test_wlan_realm::WlanConfig;
use ieee80211::{Bssid, MacAddrBytes, Ssid};
use lazy_static::lazy_static;
use std::pin::pin;
use wlan_common::bss::Protection;
use wlan_common::channel::{Cbw, Channel};
use wlan_hw_sim::event::action;
use wlan_hw_sim::*;

lazy_static! {
    static ref BSS_FOO: Bssid = Bssid::from([0x62, 0x73, 0x73, 0x66, 0x6f, 0x6f]);
    static ref BSS_FOO_2: Bssid = Bssid::from([0x62, 0x73, 0x73, 0x66, 0x66, 0x66]);
    static ref BSS_BAR: Bssid = Bssid::from([0x62, 0x73, 0x73, 0x62, 0x61, 0x72]);
    static ref BSS_BAR_2: Bssid = Bssid::from([0x63, 0x74, 0x74, 0x63, 0x62, 0x73]);
    static ref BSS_BAZ: Bssid = Bssid::from([0x62, 0x73, 0x73, 0x62, 0x61, 0x7a]);
    static ref BSS_BAZ_2: Bssid = Bssid::from([0x60, 0x70, 0x70, 0x60, 0x60, 0x70]);
    static ref SSID_FOO: Ssid = Ssid::try_from("foo").unwrap();
    static ref SSID_BAR: Ssid = Ssid::try_from("bar").unwrap();
    static ref SSID_BAZ: Ssid = Ssid::try_from("baz").unwrap();
}

// TODO(https://fxbug.dev/42164611): This is a test of scan functionality from the perspective of wlancfg,
// but the results of wlanstack itself are untested.
/// Test scan is working by simulating some fake APs that sends out beacon frames on specific
/// channel and verify all beacon frames are correctly reported as valid networks.
#[fuchsia::test]
async fn simulate_scan() {
    let mut helper = test_utils::TestHelper::begin_test(
        default_wlantap_config_client(),
        WlanConfig { use_legacy_privacy: Some(false), ..Default::default() },
    )
    .await;
    let () = loop_until_iface_is_found(&mut helper).await;
    let phy = helper.proxy();

    // Create a client controller.
    let (client_controller, _update_stream) = init_client_controller(helper.test_ns_prefix()).await;

    let scan_result_list_fut = pin!(test_utils::policy_scan_for_networks(client_controller));
    let scan_result_list = helper
        .run_until_complete_or_timeout(
            *SCAN_RESPONSE_TEST_TIMEOUT,
            "receive a scan response",
            // Configure the scan event to return beacon frames corresponding to each `Beacon`
            // specified.
            event::on_scan(action::send_advertisements_and_scan_completion(
                &phy,
                [
                    Beacon {
                        channel: Channel::new(1, Cbw::Cbw20),
                        bssid: *BSS_FOO,
                        ssid: SSID_FOO.clone(),
                        protection: Protection::Wpa2Personal,
                        rssi_dbm: -60,
                    },
                    Beacon {
                        channel: Channel::new(2, Cbw::Cbw20),
                        bssid: *BSS_FOO_2,
                        ssid: SSID_FOO.clone(),
                        protection: Protection::Open,
                        rssi_dbm: -60,
                    },
                    Beacon {
                        channel: Channel::new(3, Cbw::Cbw20),
                        bssid: *BSS_BAR,
                        ssid: SSID_BAR.clone(),
                        protection: Protection::Wpa2Personal,
                        rssi_dbm: -60,
                    },
                    Beacon {
                        channel: Channel::new(4, Cbw::Cbw20),
                        bssid: *BSS_BAR_2,
                        ssid: SSID_BAR.clone(),
                        protection: Protection::Wpa2Personal,
                        rssi_dbm: -40,
                    },
                    Beacon {
                        channel: Channel::new(5, Cbw::Cbw20),
                        bssid: *BSS_BAZ,
                        ssid: SSID_BAZ.clone(),
                        protection: Protection::Open,
                        rssi_dbm: -60,
                    },
                    Beacon {
                        channel: Channel::new(6, Cbw::Cbw20),
                        bssid: *BSS_BAZ_2,
                        ssid: SSID_BAZ.clone(),
                        protection: Protection::Wpa2Personal,
                        rssi_dbm: -60,
                    },
                ],
            )),
            scan_result_list_fut,
        )
        .await;

    let expected_scan_result_list = test_utils::sort_policy_scan_result_list(vec![
        fidl_policy::ScanResult {
            id: Some(fidl_policy::NetworkIdentifier {
                ssid: SSID_FOO.to_vec(),
                type_: fidl_policy::SecurityType::Wpa2,
            }),
            entries: Some(vec![fidl_policy::Bss {
                bssid: Some(BSS_FOO.to_array()),
                rssi: Some(-60),
                frequency: Some(2412),
                ..Default::default()
            }]),
            compatibility: Some(fidl_policy::Compatibility::Supported),
            ..Default::default()
        },
        fidl_policy::ScanResult {
            id: Some(fidl_policy::NetworkIdentifier {
                ssid: SSID_FOO.to_vec(),
                type_: fidl_policy::SecurityType::None,
            }),
            entries: Some(vec![fidl_policy::Bss {
                bssid: Some(BSS_FOO_2.to_array()),
                rssi: Some(-60),
                frequency: Some(2417),
                ..Default::default()
            }]),
            compatibility: Some(fidl_policy::Compatibility::Supported),
            ..Default::default()
        },
        fidl_policy::ScanResult {
            id: Some(fidl_policy::NetworkIdentifier {
                ssid: SSID_BAR.to_vec(),
                type_: fidl_policy::SecurityType::Wpa2,
            }),
            entries: Some(vec![
                fidl_policy::Bss {
                    bssid: Some(BSS_BAR.to_array()),
                    rssi: Some(-60),
                    frequency: Some(2422),
                    ..Default::default()
                },
                fidl_policy::Bss {
                    bssid: Some(BSS_BAR_2.to_array()),
                    rssi: Some(-40),
                    frequency: Some(2427),
                    ..Default::default()
                },
            ]),
            compatibility: Some(fidl_policy::Compatibility::Supported),
            ..Default::default()
        },
        fidl_policy::ScanResult {
            id: Some(fidl_policy::NetworkIdentifier {
                ssid: SSID_BAZ.to_vec(),
                type_: fidl_policy::SecurityType::None,
            }),
            entries: Some(vec![fidl_policy::Bss {
                bssid: Some(BSS_BAZ.to_array()),
                rssi: Some(-60),
                frequency: Some(2432),
                ..Default::default()
            }]),
            compatibility: Some(fidl_policy::Compatibility::Supported),
            ..Default::default()
        },
        fidl_policy::ScanResult {
            id: Some(fidl_policy::NetworkIdentifier {
                ssid: SSID_BAZ.to_vec(),
                type_: fidl_policy::SecurityType::Wpa2,
            }),
            entries: Some(vec![fidl_policy::Bss {
                bssid: Some(BSS_BAZ_2.to_array()),
                rssi: Some(-60),
                frequency: Some(2437),
                ..Default::default()
            }]),
            compatibility: Some(fidl_policy::Compatibility::Supported),
            ..Default::default()
        },
    ]);

    // Compare one at a time for improved debuggability.
    assert_eq!(scan_result_list.len(), expected_scan_result_list.len());
    for i in 0..expected_scan_result_list.len() {
        assert_eq!(scan_result_list[i], expected_scan_result_list[i]);
    }
}
