# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests for validating performance of firmware PM_MODEs.
"""
import logging

logger = logging.getLogger(__name__)


import time

import fidl_fuchsia_wlan_common as fidl_common
import fidl_fuchsia_wlan_common_security as fidl_security
import fidl_fuchsia_wlan_device_service as fidl_device_svc
import fidl_fuchsia_wlan_ieee80211 as fidl_ieee80211
import fidl_fuchsia_wlan_sme as fidl_sme
from antlion import utils
from antlion.controllers.access_point import setup_ap
from antlion.controllers.ap_lib.hostapd_constants import (
    AP_DEFAULT_CHANNEL_5G,
    AP_SSID_LENGTH_5G,
)
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from core_testing import base_test
from core_testing.handlers import ConnectTransactionEventHandler
from core_testing.ies import read_ssid
from fuchsia_controller_py.wrappers import asyncmethod
from mobly import test_runner
from mobly.asserts import assert_equal, assert_true, signals


class FirmwarePowerModesTest(base_test.ConnectionBaseTestClass):
    def pre_run(self) -> None:
        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self.name_func,
            arg_sets=[(mode,) for mode in fidl_common.PowerSaveType],
        )

    def name_func(self, mode: fidl_common.PowerSaveType) -> str:
        return f"test_pm_mode_{mode.name.replace('PS_MODE_', '').lower()}"

    @asyncmethod
    async def _test_logic(self, ps_mode: fidl_common.PowerSaveType) -> None:
        ssid = utils.rand_ascii_str(AP_SSID_LENGTH_5G)

        setup_ap(
            access_point=self.access_point(),
            profile_name="whirlwind",
            channel=AP_DEFAULT_CHANNEL_5G,
            ssid=ssid,
            security=Security(security_mode=SecurityMode.OPEN),
        )

        ps_resp = await self.device_monitor_proxy.set_power_save_mode(
            req=fidl_device_svc.SetPowerSaveModeRequest(
                phy_id=self.phy_id, ps_mode=ps_mode
            )
        )
        assert (
            ps_resp.status == 0
        ), f"SetPowerSaveMode failed with status {ps_resp.status}"

        scan_results = (
            (
                await self.client_sme_proxy.scan_for_controller(
                    req=fidl_sme.ScanRequest(
                        passive=fidl_sme.PassiveScanRequest()
                    )
                )
            )
            .unwrap()
            .scan_results
        )
        assert (
            scan_results is not None
        ), "ClientSme.ScanForController() response is missing scan_results"

        bss_description = None
        for scan_result in scan_results:
            assert (
                scan_result.bss_description is not None
            ), "ScanResult is missing bss_description"
            assert (
                scan_result.bss_description.ies is not None
            ), "ScanResult.BssDescription is missing ies"
            scanned_ssid = read_ssid(bytes(scan_result.bss_description.ies))
            if scanned_ssid == ssid:
                logger.info(f"Found SSID: {scanned_ssid}")
                bss_description = scan_result.bss_description
                break
        assert bss_description is not None, f"Failed to find SSID: {ssid}"

        with ConnectTransactionEventHandler() as ctx:
            txn_queue = ctx.txn_queue
            server = ctx.server

            connect_request = fidl_sme.ConnectRequest(
                ssid=list(ssid.encode("ascii")),
                bss_description=bss_description,
                multiple_bss_candidates=False,
                authentication=fidl_security.Authentication(
                    protocol=fidl_security.Protocol.OPEN,
                    credentials=None,
                ),
                deprecated_scan_type=fidl_common.ScanType.PASSIVE,
            )
            logger.info(f"ConnectRequest: {connect_request!r}")
            self.client_sme_proxy.connect(
                req=connect_request, txn=server.take()
            )

            next_txn = await txn_queue.get()
            assert_equal(
                next_txn,
                fidl_sme.ConnectTransactionOnConnectResultRequest(
                    result=fidl_sme.ConnectResult(
                        code=fidl_ieee80211.StatusCode.SUCCESS,
                        is_credential_rejected=False,
                        is_reconnect=False,
                    )
                ),
            )
            assert_true(
                txn_queue.empty(),
                "Unexpectedly received additional callback messages.",
            )

        # TODO(http://b/371574733#comment6): Calling honeydew
        # methods results in a RuntimeError because an event loop already
        # exists. Otherwise, instead of sleeping, this test would call
        # these methods to check for an IP address:
        #    self.fuchsia_device.update_wlan_interfaces()
        #    iface_name = self.fuchsia_device.wlan_client_test_interface_name
        #    assert iface_name is not None, "Failed to get WLAN interface name"
        #    self.fuchsia_device.wait_for_ipv4_addr(iface_name)
        # For now, wait for the DHCP server to assign the DUT an IP address.
        # This should take no more than 5 seconds, typically.
        time.sleep(10)

        ap_test_interface = self.access_point().wlan_5g
        ap_address = utils.get_addr(self.access_point().ssh, ap_test_interface)
        ping_result = self.fuchsia_device.ping(ap_address)
        if ping_result.success:
            logger.info("Ping was successful.")
        else:
            raise signals.TestFailure(f"Ping was unsuccessful: {ping_result}")


if __name__ == "__main__":
    test_runner.main()
