# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""OpenWRT AP test for Lacewing."""

import logging

import fidl_fuchsia_wlan_common_security as f_wlan_common_security
from fuchsia_base_test import fuchsia_base_test
from honeydew.affordances.connectivity.wlan.utils.types import ClientStatusIdle
from honeydew.fuchsia_device import fuchsia_device
from mobly import asserts, signals, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class OpenwrtApTest(fuchsia_base_test.FuchsiaBaseTest):
    def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        super().setup_class()
        self.log = logging.getLogger()

        if not self.fuchsia_devices:
            raise signals.TestAbortClass(
                "At least one Fuchsia device is required"
            )
        if not self.openwrt_aps:
            raise signals.TestAbortClass(
                "At least one OpenWRT access point is required"
            )

        self.device: fuchsia_device.FuchsiaDevice = self.fuchsia_devices[0]

        # TODO(b/461905545): generate ssid from random string util
        # self.ssid = rand_ascii_str(10)
        self.ssid = "test_ssid"
        self.openwrt_ap = self.openwrt_aps[0]
        self.openwrt_ap.setup_ap(ssid=self.ssid)
        asserts.assert_true(
            self.openwrt_ap.verify_wifi_status(), "WiFi failed to start."
        )

    def test_scan_and_connect(self) -> None:
        """Test case for scanning and connecting to an OpenWRT AP."""
        if not self.openwrt_ap:
            raise signals.TestSkip("OpenWRT AP required for this test.")

        self.log.info("Starting scan for SSID: %s", self.ssid)
        bss_scan_response = self.device.wlan_core.scan_for_bss_info()
        bss_desc_for_ssid = bss_scan_response.get(self.ssid)

        if bss_desc_for_ssid and len(bss_desc_for_ssid) > 0:
            success = self.device.wlan_core.connect(
                ssid=self.ssid,
                bss_desc=bss_desc_for_ssid[0],
                authentication=f_wlan_common_security.Authentication(
                    f_wlan_common_security.Protocol.OPEN, None
                ),
            )
            asserts.assert_true(success, "Failed to connect.")
        else:
            asserts.fail(f"SSID {self.ssid} not found in bss descriptions.")

        self.device.wlan_core.disconnect()
        status = self.device.wlan_core.status()
        if status == ClientStatusIdle():
            return
        asserts.fail(
            f"Status did not return to idle after disconnect: {status}"
        )

    def teardown_class(self) -> None:
        super().teardown_class()
        if self.openwrt_ap:
            self.openwrt_ap.close()
        else:
            _LOGGER.warning(
                "Skipping AP teardown: No OpenWRT controller available."
            )


if __name__ == "__main__":
    test_runner.main()
