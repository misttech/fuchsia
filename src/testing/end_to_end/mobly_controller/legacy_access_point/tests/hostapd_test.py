#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import Mock

from honeydew.typing.custom_types import MacAddress
from legacy_access_point.ap_lib import hostapd
from libs.proc.job import Result

# MAC address that will be used in these tests.
STA_MAC = MacAddress("aa:bb:cc:dd:ee:ff")

# Abbreviated output of hostapd_cli STA commands, showing various AUTH/ASSOC/AUTHORIZED states.
STA_OUTPUT_WITHOUT_STA_AUTHENTICATED = """aa:bb:cc:dd:ee:ff
flags=[WMM][HT][VHT]"""

STA_OUTPUT_WITH_STA_AUTHENTICATED = """aa:bb:cc:dd:ee:ff
flags=[AUTH][WMM][HT][VHT]"""

STA_OUTPUT_WITH_STA_ASSOCIATED = """aa:bb:cc:dd:ee:ff
flags=[AUTH][ASSOC][WMM][HT][VHT]
aid=42"""

STA_OUTPUT_WITH_STA_AUTHORIZED = """aa:bb:cc:dd:ee:ff
flags=[AUTH][ASSOC][AUTHORIZED][WMM][HT][VHT]
aid=42"""

STA_OUTPUT_WITH_RATES = """aa:bb:cc:dd:ee:ff
flags=[AUTH][ASSOC][AUTHORIZED][WMM][HT][VHT]
aid=42
signal: -45 dBm
tx bitrate: 144.4 MBit/s
rx bitrate: 72.2 MBit/s"""


class HostapdTest(unittest.TestCase):
    def test_sta_authenticated_true_for_authenticated_sta(self) -> None:
        hostapd_mock = hostapd.Hostapd(Mock(), "wlan0")
        hostapd_mock._run_hostapd_cli_cmd = Mock(  # type: ignore[method-assign]
            return_value=STA_OUTPUT_WITH_STA_AUTHENTICATED
        )
        self.assertTrue(hostapd_mock.sta_authenticated(STA_MAC))

    def test_sta_authenticated_false_for_unauthenticated_sta(self) -> None:
        hostapd_mock = hostapd.Hostapd(Mock(), "wlan0")
        hostapd_mock._run_hostapd_cli_cmd = Mock(  # type: ignore[method-assign]
            return_value=STA_OUTPUT_WITHOUT_STA_AUTHENTICATED
        )
        self.assertFalse(hostapd_mock.sta_authenticated(STA_MAC))

    def test_sta_associated_true_for_associated_sta(self) -> None:
        hostapd_mock = hostapd.Hostapd(Mock(), "wlan0")
        hostapd_mock._run_hostapd_cli_cmd = Mock(  # type: ignore[method-assign]
            return_value=STA_OUTPUT_WITH_STA_ASSOCIATED
        )
        self.assertTrue(hostapd_mock.sta_associated(STA_MAC))

    def test_sta_associated_false_for_unassociated_sta(self) -> None:
        hostapd_mock = hostapd.Hostapd(Mock(), "wlan0")
        # Uses the authenticated-only CLI output.
        hostapd_mock._run_hostapd_cli_cmd = Mock(  # type: ignore[method-assign]
            return_value=STA_OUTPUT_WITH_STA_AUTHENTICATED
        )
        self.assertFalse(hostapd_mock.sta_associated(STA_MAC))

    def test_sta_authorized_true_for_authorized_sta(self) -> None:
        hostapd_mock = hostapd.Hostapd(Mock(), "wlan0")
        hostapd_mock._run_hostapd_cli_cmd = Mock(  # type: ignore[method-assign]
            return_value=STA_OUTPUT_WITH_STA_AUTHORIZED
        )
        self.assertTrue(hostapd_mock.sta_authorized(STA_MAC))

    def test_sta_authorized_false_for_unauthorized_sta(self) -> None:
        hostapd_mock = hostapd.Hostapd(Mock(), "wlan0")
        # Uses the associated-only CLI output.
        hostapd_mock._run_hostapd_cli_cmd = Mock(  # type: ignore[method-assign]
            return_value=STA_OUTPUT_WITH_STA_ASSOCIATED
        )
        self.assertFalse(hostapd_mock.sta_authorized(STA_MAC))

    def test_get_sta_status_with_rates(self) -> None:
        hostapd_mock = hostapd.Hostapd(Mock(), "wlan0")
        hostapd_mock._run_hostapd_cli_cmd = Mock(  # type: ignore[method-assign]
            return_value=STA_OUTPUT_WITH_RATES
        )
        status = hostapd_mock.get_sta_status(STA_MAC)
        self.assertTrue(status.auth)
        self.assertTrue(status.assoc)
        self.assertTrue(status.authorized)
        self.assertEqual(status.rssi, -45)
        self.assertEqual(status.tx_rate_mbps, 144.4)
        self.assertEqual(status.rx_rate_mbps, 72.2)
        self.assertIsNone(status.snr)
        self.assertEqual(status.phy_mode, "vht")
        self.assertIsNone(status.nss)

    def test_get_sta_status_without_rates(self) -> None:
        hostapd_mock = hostapd.Hostapd(Mock(), "wlan0")
        hostapd_mock._run_hostapd_cli_cmd = Mock(  # type: ignore[method-assign]
            return_value=STA_OUTPUT_WITHOUT_STA_AUTHENTICATED
        )
        with self.assertRaises(hostapd.Error):
            hostapd_mock.get_sta_status(STA_MAC)

    def test_parse_station_output_mib_format(self) -> None:
        mib_output = (
            "flags=[AUTH][ASSOC][AUTHORIZED]\n"
            "signal=-55\n"
            "tx_rate_info=8667\n"
            "rx_rate_info=4333\n"
        )
        telemetry = hostapd.parse_station_output(mib_output)
        self.assertEqual(telemetry.rssi, -55)
        self.assertEqual(telemetry.tx_rate_mbps, 866.7)
        self.assertEqual(telemetry.rx_rate_mbps, 433.3)
        self.assertIsNone(telemetry.snr)
        self.assertIsNone(telemetry.phy_mode)
        self.assertIsNone(telemetry.nss)

    def test_parse_station_output_iw_format(self) -> None:
        iw_output = (
            "Station aa:bb:cc:dd:ee:ff (on wlan0)\n"
            "  signal:  -48 dBm\n"
            "  signal avg: -50 dBm\n"
            "  tx bitrate: 144.4 MBit/s\n"
            "  rx bitrate: 72.2 Mbps\n"
        )
        telemetry = hostapd.parse_station_output(iw_output)
        self.assertEqual(telemetry.rssi, -48)
        self.assertEqual(telemetry.tx_rate_mbps, 144.4)
        self.assertEqual(telemetry.rx_rate_mbps, 72.2)
        self.assertIsNone(telemetry.snr)
        self.assertIsNone(telemetry.phy_mode)
        self.assertIsNone(telemetry.nss)

    def test_parse_station_output_gbit_and_kbit(self) -> None:
        output = (
            "signal: -30 dBm\ntx bitrate: 1.2 GBit/s\nrx bitrate: 500 kbps\n"
        )
        telemetry = hostapd.parse_station_output(output)
        self.assertEqual(telemetry.rssi, -30)
        self.assertEqual(telemetry.tx_rate_mbps, 1200.0)
        self.assertEqual(telemetry.rx_rate_mbps, 0.5)
        self.assertIsNone(telemetry.snr)
        self.assertIsNone(telemetry.phy_mode)
        self.assertIsNone(telemetry.nss)

    def test_parse_station_output_result_object(self) -> None:
        res = Result(
            command=[],
            stdout=b"signal=-60\ntx_rate: 300.0 Mbps\nrx_rate: 150.0 Mbps\n",
            stderr=b"",
            exit_status=0,
        )
        telemetry = hostapd.parse_station_output(res)
        self.assertEqual(telemetry.rssi, -60)
        self.assertEqual(telemetry.tx_rate_mbps, 300.0)
        self.assertEqual(telemetry.rx_rate_mbps, 150.0)
        self.assertIsNone(telemetry.snr)
        self.assertIsNone(telemetry.phy_mode)
        self.assertIsNone(telemetry.nss)

    def test_parse_station_output_snr_phy_mode_nss(self) -> None:
        output = (
            "Station aa:bb:cc:dd:ee:ff (on wlan0)\n"
            "  signal: -45 dBm\n"
            "  noise: -95 dBm\n"
            "  tx bitrate: 1201.0 MBit/s HE-MCS 11 80MHz HE-NSS 2\n"
            "  rx bitrate: 864.7 MBit/s VHT-MCS 9 80MHz VHT-NSS 2\n"
        )
        telemetry = hostapd.parse_station_output(output)
        self.assertEqual(telemetry.rssi, -45)
        self.assertEqual(telemetry.tx_rate_mbps, 1201.0)
        self.assertEqual(telemetry.rx_rate_mbps, 864.7)
        self.assertEqual(telemetry.snr, 50)
        self.assertEqual(telemetry.phy_mode, "he")
        self.assertEqual(telemetry.nss, 2)

    def test_scan_for_errors_interface_not_started_raises_init_error(
        self,
    ) -> None:
        hostapd_mock = hostapd.Hostapd(Mock(), "wlan0")
        hostapd_mock.is_alive = Mock(return_value=True)  # type: ignore[method-assign]
        hostapd_mock._shell.search_file = Mock(  # type: ignore[method-assign]
            side_effect=[False, True]
        )
        with self.assertRaises(hostapd.InterfaceInitError) as context:
            hostapd_mock._scan_for_errors(False)
        self.assertIn("Interface wasn't started", str(context.exception))

    def test_start_retries_on_interface_init_error(self) -> None:
        mock_runner = Mock()
        hostapd_mock = hostapd.Hostapd(mock_runner, "wlan0")
        hostapd_mock.stop = Mock()  # type: ignore[method-assign]
        hostapd_mock._write_configs = Mock()  # type: ignore[method-assign]
        hostapd_mock._wait_for_process = Mock()  # type: ignore[method-assign]
        hostapd_mock._wait_for_interface = Mock(  # type: ignore[method-assign]
            side_effect=[
                hostapd.InterfaceInitError(
                    "Interface wasn't started", hostapd_mock
                ),
                None,
            ]
        )
        hostapd_mock.start(Mock())
        self.assertEqual(hostapd_mock._wait_for_interface.call_count, 2)


if __name__ == "__main__":
    unittest.main()
