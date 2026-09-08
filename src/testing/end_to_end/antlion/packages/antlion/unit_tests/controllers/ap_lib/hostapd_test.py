#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import Mock

from antlion.controllers.ap_lib import hostapd
from antlion.libs.proc.job import Result
from honeydew.typing.custom_types import MacAddress

# MAC address that will be used in these tests.
STA_MAC = MacAddress("aa:bb:cc:dd:ee:ff")

# Abbreviated output of hostapd_cli STA commands, showing various AUTH/ASSOC/AUTHORIZED states.
STA_OUTPUT_WITHOUT_STA_AUTHENTICATED = b"""aa:bb:cc:dd:ee:ff
flags=[WMM][HT][VHT]"""

STA_OUTPUT_WITH_STA_AUTHENTICATED = b"""aa:bb:cc:dd:ee:ff
flags=[AUTH][WMM][HT][VHT]"""

STA_OUTPUT_WITH_STA_ASSOCIATED = b"""aa:bb:cc:dd:ee:ff
flags=[AUTH][ASSOC][WMM][HT][VHT]
aid=42"""

STA_OUTPUT_WITH_STA_AUTHORIZED = b"""aa:bb:cc:dd:ee:ff
flags=[AUTH][ASSOC][AUTHORIZED][WMM][HT][VHT]
aid=42"""

STA_OUTPUT_WITH_RATES = b"""aa:bb:cc:dd:ee:ff
flags=[AUTH][ASSOC][AUTHORIZED][WMM][HT][VHT]
aid=42
signal: -45 dBm
tx bitrate: 144.4 MBit/s
rx bitrate: 72.2 MBit/s"""


class HostapdTest(unittest.TestCase):
    def test_sta_authenticated_true_for_authenticated_sta(self):
        hostapd_mock = hostapd.Hostapd("mock_runner", "wlan0")
        hostapd_mock._run_hostapd_cli_cmd = Mock(
            return_value=Result(
                command=list(),
                stdout=STA_OUTPUT_WITH_STA_AUTHENTICATED,
                exit_status=0,
            )
        )
        self.assertTrue(hostapd_mock.sta_authenticated(STA_MAC))

    def test_sta_authenticated_false_for_unauthenticated_sta(self):
        hostapd_mock = hostapd.Hostapd("mock_runner", "wlan0")
        hostapd_mock._run_hostapd_cli_cmd = Mock(
            return_value=Result(
                command=list(),
                stdout=STA_OUTPUT_WITHOUT_STA_AUTHENTICATED,
                exit_status=0,
            )
        )
        self.assertFalse(hostapd_mock.sta_authenticated(STA_MAC))

    def test_sta_associated_true_for_associated_sta(self):
        hostapd_mock = hostapd.Hostapd("mock_runner", "wlan0")
        hostapd_mock._run_hostapd_cli_cmd = Mock(
            return_value=Result(
                command=list(),
                stdout=STA_OUTPUT_WITH_STA_ASSOCIATED,
                exit_status=0,
            )
        )
        self.assertTrue(hostapd_mock.sta_associated(STA_MAC))

    def test_sta_associated_false_for_unassociated_sta(self):
        hostapd_mock = hostapd.Hostapd("mock_runner", "wlan0")
        # Uses the authenticated-only CLI output.
        hostapd_mock._run_hostapd_cli_cmd = Mock(
            return_value=Result(
                command=list(),
                stdout=STA_OUTPUT_WITH_STA_AUTHENTICATED,
                exit_status=0,
            )
        )
        self.assertFalse(hostapd_mock.sta_associated(STA_MAC))

    def test_sta_authorized_true_for_authorized_sta(self):
        hostapd_mock = hostapd.Hostapd("mock_runner", "wlan0")
        hostapd_mock._run_hostapd_cli_cmd = Mock(
            return_value=Result(
                command=list(),
                stdout=STA_OUTPUT_WITH_STA_AUTHORIZED,
                exit_status=0,
            )
        )
        self.assertTrue(hostapd_mock.sta_authorized(STA_MAC))

    def test_sta_associated_false_for_unassociated_sta(self):
        hostapd_mock = hostapd.Hostapd("mock_runner", "wlan0")
        # Uses the associated-only CLI output.
        hostapd_mock._run_hostapd_cli_cmd = Mock(
            return_value=Result(
                command=list(),
                stdout=STA_OUTPUT_WITH_STA_ASSOCIATED,
                exit_status=0,
            )
        )
        self.assertFalse(hostapd_mock.sta_authorized(STA_MAC))

    def test_get_sta_status_with_rates(self):
        hostapd_mock = hostapd.Hostapd("mock_runner", "wlan0")
        hostapd_mock._run_hostapd_cli_cmd = Mock(
            return_value=Result(
                command=list(),
                stdout=STA_OUTPUT_WITH_RATES,
                exit_status=0,
            )
        )
        status = hostapd_mock.get_sta_status(STA_MAC)
        self.assertTrue(status.auth)
        self.assertTrue(status.assoc)
        self.assertTrue(status.authorized)
        self.assertEqual(status.rssi, -45)
        self.assertEqual(status.tx_rate_mbps, 144.4)
        self.assertEqual(status.rx_rate_mbps, 72.2)

    def test_get_sta_status_without_rates(self):
        hostapd_mock = hostapd.Hostapd("mock_runner", "wlan0")
        hostapd_mock._run_hostapd_cli_cmd = Mock(
            return_value=Result(
                command=list(),
                stdout=STA_OUTPUT_WITHOUT_STA_AUTHENTICATED,
                exit_status=0,
            )
        )
        with self.assertRaises(hostapd.Error):
            hostapd_mock.get_sta_status(STA_MAC)

    def test_parse_station_output_mib_format(self):
        mib_output = (
            "flags=[AUTH][ASSOC][AUTHORIZED]\n"
            "signal=-55\n"
            "tx_rate_info=8667\n"
            "rx_rate_info=4333\n"
        )
        rssi, tx_mbps, rx_mbps = hostapd.parse_station_output(mib_output)
        self.assertEqual(rssi, -55)
        self.assertEqual(tx_mbps, 866.7)
        self.assertEqual(rx_mbps, 433.3)

    def test_parse_station_output_iw_format(self):
        iw_output = (
            "Station aa:bb:cc:dd:ee:ff (on wlan0)\n"
            "  signal:  -48 dBm\n"
            "  signal avg: -50 dBm\n"
            "  tx bitrate: 144.4 MBit/s\n"
            "  rx bitrate: 72.2 Mbps\n"
        )
        rssi, tx_mbps, rx_mbps = hostapd.parse_station_output(iw_output)
        self.assertEqual(rssi, -48)
        self.assertEqual(tx_mbps, 144.4)
        self.assertEqual(rx_mbps, 72.2)

    def test_parse_station_output_gbit_and_kbit(self):
        output = (
            "signal: -30 dBm\ntx bitrate: 1.2 GBit/s\nrx bitrate: 500 kbps\n"
        )
        rssi, tx_mbps, rx_mbps = hostapd.parse_station_output(output)
        self.assertEqual(rssi, -30)
        self.assertEqual(tx_mbps, 1200.0)
        self.assertEqual(rx_mbps, 0.5)

    def test_parse_station_output_result_object(self):
        res = Result(
            command=[],
            stdout=b"signal=-60\ntx_rate: 300.0 Mbps\nrx_rate: 150.0 Mbps\n",
            exit_status=0,
        )
        rssi, tx_mbps, rx_mbps = hostapd.parse_station_output(res)
        self.assertEqual(rssi, -60)
        self.assertEqual(tx_mbps, 300.0)
        self.assertEqual(rx_mbps, 150.0)


if __name__ == "__main__":
    unittest.main()
