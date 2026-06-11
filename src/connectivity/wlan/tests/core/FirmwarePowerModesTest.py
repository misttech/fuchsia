# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests for validating performance of firmware PM_MODEs.
"""
import asyncio
import logging

logger = logging.getLogger(__name__)


import fidl_fuchsia_wlan_common as fidl_common
import fidl_fuchsia_wlan_device_service as fidl_device_svc
import fidl_fuchsia_wlan_ieee80211 as fidl_ieee80211
import fidl_fuchsia_wlan_internal as fidl_security
import fidl_fuchsia_wlan_sme as fidl_sme
from antlion import utils
from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.ap_lib.hostapd_constants import (
    AP_DEFAULT_CHANNEL_5G,
    AP_SSID_LENGTH_5G,
)
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from antlion.controllers.ap_lib.hostapd_security import (
    SecurityMode as DeprecatedSecurityMode,
)
from common.utils.ies import read_ssid
from core_testing import base_test
from core_testing.handlers import ConnectTransactionEventHandler
from mobly import signals, test_runner
from mobly.asserts import assert_equal, assert_true, fail
from openwrt_access_point import AddrType as OpenWrtAddrType
from openwrt_access_point import InterfaceName as OpenWrtInterfaceName
from openwrt_access_point import OpenWrtAP
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    AccessPointConfig,
    BssSettings,
    RadioConfig,
    SecurityOpen,
)


class FirmwarePowerModesTest(base_test.ConnectionBaseTestClass):
    async def pre_run(self) -> None:
        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self.name_func,
            arg_sets=[(mode,) for mode in fidl_common.PowerSaveType],
        )

    def name_func(self, ps_mode: fidl_common.PowerSaveType) -> str:
        return f"test_pm_mode_{ps_mode.name.replace('PS_MODE_', '').lower()}"

    async def _test_logic(self, ps_mode: fidl_common.PowerSaveType) -> None:
        ssid = utils.rand_ascii_str(AP_SSID_LENGTH_5G)
        if isinstance(self.test_kit.access_point, OpenWrtAP):
            self.test_kit.access_point.configure_wifi(
                AccessPointConfig(
                    radios=[
                        RadioConfig(
                            channel=DEFAULT_2G_CHANNEL,
                            bss_settings=[
                                BssSettings(
                                    ssid=ssid,
                                    security=SecurityOpen(),
                                )
                            ],
                        )
                    ]
                )
            )
        elif isinstance(self.test_kit.access_point, AccessPoint):
            setup_ap(
                access_point=self.test_kit.access_point,
                profile_name="whirlwind",
                channel=AP_DEFAULT_CHANNEL_5G,
                ssid=ssid,
                security=DeprecatedSecurity(
                    security_mode=DeprecatedSecurityMode.OPEN
                ),
            )
        else:
            raise signals.TestAbortClass(
                "No access point configured for this test."
            )

        ps_resp = await self.test_kit.device_monitor.set_power_save_mode(
            req=fidl_device_svc.SetPowerSaveModeRequest(
                phy_id=self.test_kit.phy_id,
                ps_mode=ps_mode,
            )
        )
        assert (
            ps_resp.status == 0
        ), f"SetPowerSaveMode failed with status {ps_resp.status}"

        scan_results = (
            (
                await self.test_kit.client_sme.scan_for_controller(
                    req=fidl_sme.ScanRequest(
                        passive=fidl_sme.PassiveScanRequest(
                            channels=[
                                DEFAULT_2G_CHANNEL.number,
                                AP_DEFAULT_CHANNEL_5G,
                            ]
                        )
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

        (
            proxy,
            server,
        ) = self.dut.fuchsia_controller.channel_create()
        async with ConnectTransactionEventHandler(proxy, server) as ctx:
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
            self.test_kit.client_sme.connect(
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
        #    self.dut.update_wlan_interfaces()
        #    iface_name = self.dut.wlan_client_test_interface_name
        #    assert iface_name is not None, "Failed to get WLAN interface name"
        #    self.dut.wait_for_ipv4_addr(iface_name)
        # For now, wait for the DHCP server to assign the DUT an IP address.
        # This should take no more than 5 seconds, typically.
        await asyncio.sleep(10)

        if isinstance(self.test_kit.access_point, OpenWrtAP):
            ap_address = self.test_kit.access_point.get_addr(
                interface=OpenWrtInterfaceName.lan,
                addr_type=OpenWrtAddrType.ipv4_private,
            )
        elif isinstance(self.test_kit.access_point, AccessPoint):
            ap_test_interface = self.test_kit.access_point.wlan_5g
            ap_address = utils.get_addr(
                self.test_kit.access_point.ssh, ap_test_interface
            )
        else:
            raise signals.TestAbortClass(
                "No access point configured for this test."
            )

        try:
            ping_result = await self.dut.netstack.ping(ap_address)
            logger.info(f"Ping succeeded: {ping_result.raw_output}")
        except Exception as e:
            logger.error(f"{e}")
            fail(f"Ping failed.")


if __name__ == "__main__":
    test_runner.main()
