# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import logging

import fidl_fuchsia_wlan_common as f_wlan_common
import fidl_fuchsia_wlan_ieee80211 as f_wlan_ieee80211
import fidl_fuchsia_wlan_internal as fidl_security
import fidl_fuchsia_wlan_sme as f_wlan_sme
import fuchsia_wlan_base_test
import honeydew.affordances.connectivity.wlan.core as wlan_core
from legacy_access_point.access_point import AccessPoint, setup_ap
from legacy_access_point.ap_lib.hostapd_constants import (
    AP_DEFAULT_CHANNEL_2G,
    AP_SSID_LENGTH_2G,
)
from legacy_access_point.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from mobly import asserts, signals, test_runner
from mobly.asserts import fail
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    AccessPointConfig,
    BssSettings,
    RadioConfig,
    Security,
    SecurityOpen,
    SecurityWpa2,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)

logger = logging.getLogger(__name__)


class AssociationRejectionTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """Tests that client interfaces properly handle AP association rejection.

    When an AP rejects association (e.g. max_num_sta=0), the WLAN core SME and
    driver must report connection failure and not enter a false connected state.
    """

    phy: wlan_core.Phy

    async def setup_class(self) -> None:
        await super().setup_class()
        self.phy = await self.dut.wlan_core.ensure_single_phy()

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.dut.wlan_core.destroy_all_ifaces()
        if self.access_point:
            self.access_point.stop_all_aps()

    async def teardown_test(self) -> None:
        if self.access_point:
            self.access_point.stop_all_aps()
        await super().teardown_test()

    async def pre_run(self) -> None:
        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self.name_func,
            arg_sets=[
                (SecurityOpen(), None),
                (SecurityWpa2(), AccessPointConfig.random_string(13)),
            ],
        )

    def name_func(self, security: Security, password: str | None) -> str:
        match security:
            case SecurityOpen():
                sec_name = "open"
            case SecurityWpa2():
                sec_name = "wpa2"
            case _:
                fail(f"Unrecognized security type: {security}")
        return f"test_reject_association_{sec_name}"

    async def _connect_and_get_result(
        self,
        iface: wlan_core.ClientIface,
        ssid: str,
        bss_desc: f_wlan_ieee80211.BssDescription,
        authentication: fidl_security.Authentication,
    ) -> f_wlan_sme.ConnectResult:
        client, server = iface._fc_transport.channel_create()
        connect_txn_client = f_wlan_sme.ConnectTransactionClient(client.take())
        req = f_wlan_sme.ConnectRequest(
            ssid=list(ssid.encode("utf-8")),
            bss_description=bss_desc,
            multiple_bss_candidates=False,
            authentication=authentication,
            deprecated_scan_type=f_wlan_common.ScanType.ACTIVE,
        )
        event_handler = wlan_core.ConnectTransactionEventHandler(
            connect_txn_client
        )
        event_handler_task = asyncio.create_task(event_handler.serve())
        try:
            iface.client_sme.connect(req=req, txn=server.take())
            on_connect_result_req = await asyncio.wait_for(
                event_handler.txn_queue.get(), timeout=30
            )
            assert isinstance(
                on_connect_result_req,
                f_wlan_sme.ConnectTransactionOnConnectResultRequest,
            )
            return on_connect_result_req.result
        finally:
            event_handler_task.cancel()
            try:
                await event_handler_task
            except asyncio.CancelledError:
                pass

    def _derive_authentication(
        self, security: Security, password: str | None
    ) -> fidl_security.Authentication:
        if isinstance(security, SecurityOpen):
            return fidl_security.Authentication(
                protocol=fidl_security.Protocol.OPEN,
                credentials=None,
            )
        elif isinstance(security, SecurityWpa2):
            if password is None:
                raise signals.TestError("Password is required for WPA2")
            return fidl_security.Authentication(
                protocol=fidl_security.Protocol.WPA2_PERSONAL,
                credentials=fidl_security.Credentials(
                    wpa=fidl_security.WpaCredentials(
                        passphrase=list(password.encode("utf-8"))
                    )
                ),
            )
        else:
            raise signals.TestError(f"Unsupported security mode: {security}")

    async def _test_logic(
        self, security: Security, password: str | None
    ) -> None:
        iface = await self.phy.create_client_iface()
        ssid = AccessPointConfig.random_string(AP_SSID_LENGTH_2G)
        if self.openwrt_ap:
            self.openwrt_ap.configure_wifi(
                AccessPointConfig(
                    radios=[
                        RadioConfig(
                            channel=DEFAULT_2G_CHANNEL,
                            bss_settings=[
                                BssSettings(
                                    ssid=ssid,
                                    password=password,
                                    security=security,
                                    custom_uci_options={"max_num_sta": 0},
                                )
                            ],
                        )
                    ]
                )
            )
        elif isinstance(self.access_point, AccessPoint):
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=AP_DEFAULT_CHANNEL_2G,
                ssid=ssid,
                security=DeprecatedSecurity(
                    security_mode=ConfigMapper.to_hostapd_security(security),
                    password=password,
                    wpa_strict_rekey=False,
                ),
                additional_ap_parameters={"max_num_sta": 0},
            )

        authentication = self._derive_authentication(security, password)

        logger.info(
            f"Scanning for {ssid} and connecting, expecting AP to reject association"
        )
        target_channel = (
            DEFAULT_2G_CHANNEL.number
            if self.openwrt_ap
            else AP_DEFAULT_CHANNEL_2G
        )
        scan_results = await iface.passive_scan(channels=[target_channel])
        asserts.assert_in(
            ssid,
            scan_results,
            f"Failed to find BSS with SSID {ssid} in scan results: {scan_results}",
        )
        target_bss_desc = scan_results[ssid][0]

        result = await self._connect_and_get_result(
            iface=iface,
            ssid=ssid,
            bss_desc=target_bss_desc,
            authentication=authentication,
        )

        logger.info(f"Received ConnectResult from SME: {result}")
        asserts.assert_in(
            result.code,
            [
                f_wlan_ieee80211.StatusCode.DENIED_NO_MORE_STAS,
                f_wlan_ieee80211.StatusCode.REFUSED_REASON_UNSPECIFIED,
                f_wlan_ieee80211.StatusCode.REFUSED_UNAUTHENTICATED_ACCESS_NOT_SUPPORTED,
            ],
            f"Expected rejection status code, got: {result.code}",
        )

        status = await iface.status()
        asserts.assert_false(
            bool(status.connected),
            f"Expected interface not to be connected, but got status: {status}",
        )


if __name__ == "__main__":
    test_runner.main()
