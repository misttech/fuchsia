# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Test that the WifiChip can be restarted.
"""

import fuchsia_wlan_base_test
import wlanix_testing
from mobly import test_runner


class WifiChipRestartTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    wlanix: wlanix_testing.Wlanix
    chip: wlanix_testing.WifiChip

    async def setup_class(self) -> None:
        await super().setup_class()
        self.wlanix = wlanix_testing.Wlanix(self.dut)
        self.chip = await self.wlanix.ensure_single_chip()

    async def test_trigger_subsystem_restart(self) -> None:
        """Test calling TriggerSubsystemRestart API."""
        await self.chip.trigger_subsystem_restart_and_wait()


if __name__ == "__main__":
    test_runner.main()
