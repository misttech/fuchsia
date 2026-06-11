# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests for connecting to an access point.
"""
import logging

from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)

logger = logging.getLogger(__name__)

import fidl_fuchsia_wlan_common as fidl_common
import fidl_fuchsia_wlan_ieee80211 as fidl_ieee80211
import fidl_fuchsia_wlan_internal as fidl_security
import fidl_fuchsia_wlan_sme as fidl_sme
from antlion import utils
from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.ap_lib.hostapd_constants import (
    AP_DEFAULT_CHANNEL_2G,
    AP_SSID_LENGTH_2G,
)
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from common.utils.ies import Ie, read_ssid
from core_testing import base_test
from core_testing.handlers import ConnectTransactionEventHandler
from mobly import signals, test_runner
from mobly.asserts import assert_equal, assert_true, fail
from openwrt_access_point import OpenWrtAP
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    AccessPointConfig,
    BssSettings,
    RadioConfig,
    Security,
    SecurityOpen,
    SecurityWpa2,
)


class ConnectToApTest(base_test.ConnectionBaseTestClass):
    async def pre_run(self) -> None:
        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self.name_func,
            arg_sets=[
                (SecurityOpen(), None),
                (SecurityWpa2(), AccessPointConfig.random_string()),
            ],
        )

    def name_func(self, security: Security, password: str | None) -> str:
        return f"test_successfully_connect_to_ap_{security}"

    async def _test_logic(
        self, security: Security, password: str | None
    ) -> None:
        ssid = utils.rand_ascii_str(AP_SSID_LENGTH_2G)
        if isinstance(self.test_kit.access_point, OpenWrtAP):
            self.test_kit.access_point.configure_wifi(
                AccessPointConfig(
                    radios=[
                        RadioConfig(
                            channel=DEFAULT_2G_CHANNEL,
                            bss_settings=[
                                BssSettings(
                                    ssid=ssid,
                                    password=password,
                                    security=security,
                                )
                            ],
                        )
                    ]
                )
            )
            channel_number = DEFAULT_2G_CHANNEL.number
        elif isinstance(self.test_kit.access_point, AccessPoint):
            setup_ap(
                access_point=self.test_kit.access_point,
                profile_name="whirlwind",
                channel=AP_DEFAULT_CHANNEL_2G,
                ssid=ssid,
                security=DeprecatedSecurity(
                    security_mode=ConfigMapper.to_hostapd_security(security),
                    password=password,
                ),
            )
            channel_number = AP_DEFAULT_CHANNEL_2G

        scan_results = (
            (
                await self.test_kit.client_sme.scan_for_controller(
                    req=fidl_sme.ScanRequest(
                        passive=fidl_sme.PassiveScanRequest(
                            channels=[channel_number]
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
                logger.info(f"Scan result: {scan_result!r}")
                logger.info(
                    f"IEs: {Ie.read_ies(bytes(scan_result.bss_description.ies))!r}"
                )
                bss_description = scan_result.bss_description
                break
        assert bss_description is not None, f"Failed to find SSID: {ssid}"

        (
            proxy,
            server,
        ) = self.dut.fuchsia_controller.channel_create()
        async with ConnectTransactionEventHandler(
            proxy,
            server,
        ) as ctx:
            txn_queue = ctx.txn_queue
            server = ctx.server

            credentials = None
            protocol = fidl_security.Protocol.OPEN
            if isinstance(security, SecurityOpen):
                pass
            elif isinstance(security, SecurityWpa2):
                if password is None:
                    raise signals.TestError("Password is required for WPA2")
                protocol = fidl_security.Protocol.WPA2_PERSONAL
                credentials = fidl_security.Credentials(
                    wpa=fidl_security.WpaCredentials(
                        passphrase=list(password.encode("ascii"))
                    )
                )
            else:
                fail(f"Unsupported security mode: {security}")

            connect_request = fidl_sme.ConnectRequest(
                ssid=list(ssid.encode("ascii")),
                bss_description=bss_description,
                multiple_bss_candidates=False,
                authentication=fidl_security.Authentication(
                    protocol=protocol,
                    credentials=credentials,
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


if __name__ == "__main__":
    test_runner.main()
