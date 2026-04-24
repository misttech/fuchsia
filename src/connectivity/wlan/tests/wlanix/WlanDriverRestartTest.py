# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import logging

import fidl_fuchsia_wlan_common as fw_common
import fidl_fuchsia_wlan_sme as fw_sme
from core_testing import base_test
from mobly import test_runner
from mobly.asserts import assert_true

logger = logging.getLogger(__name__)


class WlanDriverRestartTest(base_test.CoreBaseTestClass):
    async def test_driver_host_restart(self) -> None:
        # TODO(b/494309251): If a driver debug side channel is created, use that to query out the
        # KOID and kill that instead.
        logger.info("Killing driver-host-#wlan...")
        self.dut.ffx.run_ssh_cmd("killall driver-host-#wlan")

        logger.info("Polling for PHY to be removed")
        phy_removal_timeout = 10
        poll_interval = 0.25

        for attempt in range(int(phy_removal_timeout / poll_interval)):
            response = await self.test_kit.device_monitor.list_phys()
            phy_list = response.phy_list
            logger.info(f"Attempt {attempt + 1}: Found PHY list: {phy_list}")

            if len(phy_list) == 0:
                logger.info("Successfully observed PHY removal")
                break

            await asyncio.sleep(poll_interval)
        else:
            assert_true(False, "Timed out waiting for PHYs to be removed")

        logger.info("Polling for PHY to return")
        phy_return_timeout = 30
        poll_interval = 1.0

        for attempt in range(int(phy_return_timeout / poll_interval)):
            response = await self.test_kit.device_monitor.list_phys()
            phy_list = response.phy_list
            logger.info(f"Attempt {attempt + 1}: Found PHY list: {phy_list}")

            if len(phy_list) > 0:
                logger.info(f"Successfully found restored PHY(s): {phy_list}")
                break

            await asyncio.sleep(poll_interval)
        else:
            assert_true(False, "Timed out waiting for PHYs to be restored")

        logger.info("Creating client interface...")
        create_iface_response = (
            await self.test_kit.device_monitor.create_iface(
                phy_id=phy_list[0],
                role=fw_common.WlanMacRole.CLIENT,
                sta_address=[0, 0, 0, 0, 0, 0],
            )
        ).unwrap()
        assert (
            create_iface_response.iface_id is not None
        ), "DeviceMonitor.CreateIface() response is missing an iface_id"
        iface_id = create_iface_response.iface_id

        logger.info(
            f"Created interface with ID {iface_id}, obtaining ClientSme..."
        )
        proxy, server = self.dut.fuchsia_controller.channel_create()
        (
            await self.test_kit.device_monitor.get_client_sme(
                iface_id=iface_id,
                sme_server=server.take(),
            )
        ).unwrap()
        client_sme = fw_sme.ClientSmeClient(proxy)

        logger.info("Issuing scan request...")
        # TODO(https://fxbug.dev/316037008): Get the list of supported channels when the call is supported.
        # Scan on all channels to check that a full scan succeeds after the driver restart.
        channels = [
            1,
            2,
            3,
            4,
            5,
            6,
            7,
            8,
            9,
            10,
            11,
            36,
            40,
            44,
            48,
            52,
            56,
            60,
            64,
            100,
            104,
            108,
            112,
            116,
            120,
            124,
            128,
            132,
            136,
            140,
            144,
            149,
            153,
            157,
            161,
            165,
        ]
        scan_req = fw_sme.ScanRequest(
            passive=fw_sme.PassiveScanRequest(channels)
        )
        scan_response = (
            await client_sme.scan_for_controller(req=scan_req)
        ).unwrap()


if __name__ == "__main__":
    test_runner.main()
