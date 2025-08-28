#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from antlion.test_utils.abstract_devices.wlan_device import AssociationMode
from antlion.test_utils.wifi import base_test
from mobly import test_runner


class WlanInterfaceTest(base_test.WifiBaseTest):
    def setup_class(self) -> None:
        super().setup_class()
        self.dut = self.get_dut(AssociationMode.POLICY)

    def test_destroy_iface(self) -> None:
        """Test that we don't error out when destroying the WLAN interface.

        Steps:
        1. Find a wlan interface
        2. Destroy it

        Expected Result:
        Verify there are no errors in destroying the wlan interface.

        Returns:
          signals.TestPass if no errors
          signals.TestFailure if there are any errors during the test.

        TAGS: WLAN
        Priority: 1
        """
        wlan_interfaces = self.dut.get_wlan_interface_id_list()
        self.dut.destroy_wlan_interface(wlan_interfaces[0])


if __name__ == "__main__":
    test_runner.main()
