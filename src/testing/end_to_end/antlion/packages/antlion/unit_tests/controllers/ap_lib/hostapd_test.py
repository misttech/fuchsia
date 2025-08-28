#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import Mock

from antlion.controllers.ap_lib import hostapd
from antlion.libs.proc.job import Result

# MAC address that will be used in these tests.
STA_MAC = "aa:bb:cc:dd:ee:ff"

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


if __name__ == "__main__":
    unittest.main()
