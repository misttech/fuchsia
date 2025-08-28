#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import time

from antlion import base_test, controllers
from antlion.controllers.fuchsia_device import FuchsiaDevice
from mobly import asserts, signals, test_runner
from mobly.config_parser import TestRunConfig

# Time to wait until an interface is recreated after the softmac WLAN driver
# restarts.
DELAY_FOR_DRIVER_RESTART_SEC = 2.0


class WlanDriverRestartTest(base_test.AntlionBaseTest):
    def __init__(self, configs: TestRunConfig) -> None:
        super().__init__(configs)
        self.log = logging.getLogger()

    def setup_class(self) -> None:
        super().setup_class()

        fuchsia_devices: list[FuchsiaDevice] = self.register_controller(
            controllers.fuchsia_device
        )
        self.fuchsia_device = fuchsia_devices[0]

        # Skip this test suite if the device isn't running a softmac WLAN driver.
        driver_list = self.fuchsia_device.ffx.run(["driver", "list"])
        if "iwlwifi" not in driver_list:
            raise signals.TestSkip(
                "No intel WiFi driver found on this device, skipping test"
            )

    def test_driver_restart_recreates_interface(self) -> None:
        """Verify the WLAN interface gets recreated after its driver restarts."""
        # Store existing phy and interface identifiers.
        phys = self.fuchsia_device.honeydew_fd.wlan_core.get_phy_id_list()
        asserts.assert_equal(len(phys), 1, "Expected one phy_id")
        old_interfaces = (
            self.fuchsia_device.honeydew_fd.wlan_core.get_iface_id_list()
        )
        asserts.assert_not_equal(old_interfaces, [], "Iface not found.")

        # Restarting should replace the old interface with a new one.
        self.fuchsia_device.ffx.run(
            [
                "driver",
                "restart",
                "fuchsia-pkg://fuchsia.com/iwlwifi#meta/iwlwifi.cm",
            ]
        )

        # Check for new phy and interface identifiers.
        timeout = time.time() + DELAY_FOR_DRIVER_RESTART_SEC
        while time.time() < timeout:
            new_interfaces = (
                self.fuchsia_device.honeydew_fd.wlan_core.get_iface_id_list()
            )

            if new_interfaces == old_interfaces:
                # Interface has not been deleted yet. Keep waiting.
                time.sleep(0.1)
                continue
            if len(new_interfaces) == 0:
                # Interface has not come back up yet. Keep waiting.
                time.sleep(0.1)
                continue
            if len(new_interfaces) == 1:
                # New interface has been added! All done here
                break

            asserts.fail(
                "More interfaces exist than before! \n"
                f"Old: {old_interfaces}\n"
                f"New: {new_interfaces}"
            )
        else:
            asserts.fail(
                f"New interface not created within {DELAY_FOR_DRIVER_RESTART_SEC}s"
            )

        phys = self.fuchsia_device.honeydew_fd.wlan_core.get_phy_id_list()
        asserts.assert_equal(len(phys), 1, "Expected one phy_id")


if __name__ == "__main__":
    test_runner.main()
