# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Tests fulfillment of roam requests from the SME FIDL roam API.
"""
import logging

logger = logging.getLogger(__name__)

import asyncio
import time
from dataclasses import dataclass

import fidl_fuchsia_wlan_common as fidl_common
import fidl_fuchsia_wlan_ieee80211 as fidl_ieee80211
import fidl_fuchsia_wlan_internal as fidl_security
import fidl_fuchsia_wlan_sme as fidl_sme
from antlion import utils
from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from common.utils.ies import read_ssid
from core_testing import base_test
from core_testing.handlers import ConnectTransactionEventHandler
from honeydew.affordances.connectivity.wlan.utils.types import MacAddress
from mobly import signals, test_runner
from mobly.asserts import (
    abort_class_if,
    assert_equal,
    assert_not_equal,
    assert_true,
    fail,
)
from openwrt_access_point import OpenWrtAP, StationStatus
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    DEFAULT_5G_CHANNEL,
    AccessPointConfig,
    Band,
    BssSettings,
    RadioConfig,
    Security,
    SecurityOpen,
    SecurityWep,
    SecurityWpa,
    SecurityWpa2,
    SecurityWpa2Wpa3Mixed,
    SecurityWpa3,
    SecurityWpaWpa2Mixed,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)

# Allows test to raise an error if the permutation logic is changed accidentally.
# 18 cases expect roam to succeed (9 2.4GHz to 5GHz, 9 5GHz to 2.4GHz)
# 44 cases with incompatible origin and target security expect roam to fail (2.4GHz to 5GHz only)
NUM_EXPECTED_TEST_CASE_PERMUTATIONS: int = 62

CONNECT_WAIT_TIME_SECONDS: int = 5
ROAM_RESULT_WAIT_TIME_SECONDS: int = 3
NEXT_TXN_WAIT_TIME_SECONDS: int = 1
TEST_WEP_PASSWORD_LITERAL = "1234567891234"


@dataclass
class TestParams:
    dut_security_mode: Security
    origin_security_mode: Security
    origin_band: hostapd_constants.BandType
    target_security_mode: Security
    target_band: hostapd_constants.BandType
    should_roam_succeed: bool


@dataclass
class RoamTestParameters:
    ssid: str
    origin_password: str | None
    target_password: str | None


_DUT_SECURITY_MODES: frozenset[Security] = frozenset(
    [
        SecurityOpen(),
        SecurityWep(),
        SecurityWpa(),
        SecurityWpa2(),
        SecurityWpa3(),
    ]
)

_AP_SECURITY_MODES: frozenset[Security] = _DUT_SECURITY_MODES | frozenset(
    [
        SecurityWpaWpa2Mixed(),
        SecurityWpa2Wpa3Mixed(),
    ]
)

_DUT_SECURITY_MODE_TO_COMPATIBLE_AP_MODES: dict[
    Security, frozenset[Security]
] = {
    SecurityOpen(): frozenset([SecurityOpen()]),
    SecurityWep(): frozenset([SecurityWep()]),
    SecurityWpa(): frozenset([SecurityWpa(), SecurityWpaWpa2Mixed()]),
    SecurityWpa2(): frozenset(
        [
            SecurityWpa2(),
            SecurityWpaWpa2Mixed(),
            SecurityWpa2Wpa3Mixed(),
        ]
    ),
    SecurityWpa3(): frozenset([SecurityWpa3(), SecurityWpa2Wpa3Mixed()]),
}


class RoamRequestTest(base_test.ConnectionBaseTestClass):
    """Tests fulfillment of roam requests from the SME FIDL roam API.

    Testbed Requirements:
    * One Fuchsia DUT
    * One AP

    Currently, this test only supports inter-band (2.4GHz/5GHz) roaming, as it is designed for
    standardized WLAN testbeds with a single AP. Multi-AP support is needed for intra-band
    roaming tests.
    """

    async def pre_run(self) -> None:
        """
        Generates test permutations.

        - For each compatible DUT and AP security mode pair:
            - A 2.4GHz to 5GHz test case with same origin and target AP mode
            - A 5GHz to 2.4GHz test case with same origin and target AP mode
            - For each security type that is incompatible with the AP mode:
                - A 2.4 GHz to 5GHz test case with origin AP mode and target incompatible mode,
                where the roam is expected to fail
        """
        test_args: list[tuple[TestParams]] = []
        for (
            dut_mode,
            compatible_ap_modes,
        ) in _DUT_SECURITY_MODE_TO_COMPATIBLE_AP_MODES.items():
            for ap_mode in compatible_ap_modes:
                # 2.4GHz to 5GHz
                test_args.append(
                    (
                        TestParams(
                            dut_security_mode=dut_mode,
                            origin_security_mode=ap_mode,
                            origin_band=hostapd_constants.BandType.BAND_2G,
                            target_security_mode=ap_mode,
                            target_band=hostapd_constants.BandType.BAND_5G,
                            should_roam_succeed=True,
                        ),
                    ),
                )
                # 5GHz to 2.4GHz
                test_args.append(
                    (
                        TestParams(
                            dut_security_mode=dut_mode,
                            origin_security_mode=ap_mode,
                            origin_band=hostapd_constants.BandType.BAND_5G,
                            target_security_mode=ap_mode,
                            target_band=hostapd_constants.BandType.BAND_2G,
                            should_roam_succeed=True,
                        ),
                    ),
                )
                incompatible_modes = _AP_SECURITY_MODES - compatible_ap_modes
                for incompatible_mode in incompatible_modes:
                    test_args.append(
                        (
                            TestParams(
                                dut_security_mode=dut_mode,
                                origin_security_mode=ap_mode,
                                origin_band=hostapd_constants.BandType.BAND_2G,
                                target_security_mode=incompatible_mode,
                                target_band=hostapd_constants.BandType.BAND_5G,
                                should_roam_succeed=False,
                            ),
                        ),
                    )
        if len(test_args) != NUM_EXPECTED_TEST_CASE_PERMUTATIONS:
            raise signals.TestError(
                f"Generated unexpected number of test permutations. Expected {NUM_EXPECTED_TEST_CASE_PERMUTATIONS}, got {len(test_args)}."
            )
        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self.name_func,
            arg_sets=test_args,
        )

    def skip_if_wep_not_supported(self, test_params: TestParams) -> None:
        # TODO(b/490162087): Remove this skip once OpenWrt supports WEP security
        if isinstance(self.test_kit.access_point, OpenWrtAP) and (
            isinstance(test_params.dut_security_mode, SecurityWep)
            or isinstance(test_params.origin_security_mode, SecurityWep)
            or isinstance(test_params.target_security_mode, SecurityWep)
        ):
            raise signals.TestSkip("OpenWrt does not support WEP security")

    def name_func(
        self,
        test_params: TestParams,
    ) -> str:
        expected_result: str = (
            "should_succeed"
            if test_params.should_roam_succeed
            else "should_fail"
        )
        dut_security_mode: str = test_params.dut_security_mode.uci_encryption
        origin_security_mode: str = (
            test_params.origin_security_mode.uci_encryption
        )
        target_security_mode: str = (
            test_params.target_security_mode.uci_encryption
        )
        return f"test_roam_request_{dut_security_mode}_dut_from_{origin_security_mode}_{test_params.origin_band.name}_to_{target_security_mode}_{test_params.target_band.name}_{expected_result}"

    def get_single_sta_status(self, mac: str, band: Band) -> StationStatus:
        """Gets station status and asserts there is only one interface."""
        assert isinstance(
            self.test_kit.access_point, OpenWrtAP
        ), "Expected OpenWrtAP"
        sta_dict = self.test_kit.access_point.get_sta_status(mac, band)
        assert (
            len(sta_dict) == 1
        ), f"Expected station on exactly one interface, but found: {list(sta_dict.keys())}"
        return list(sta_dict.values())[0]

    async def setup_aps(self, test_params: TestParams) -> RoamTestParameters:
        ssid = utils.rand_ascii_str(hostapd_constants.AP_SSID_LENGTH_2G)
        origin_password = None
        target_password = None
        if not isinstance(test_params.origin_security_mode, SecurityOpen):
            # Length 13, so it can be used for WEP or WPA
            origin_password = utils.rand_ascii_str(13)
            target_password = origin_password
        elif not isinstance(test_params.target_security_mode, SecurityOpen):
            # If the origin is open but the target is not, generate password for target.
            target_password = utils.rand_ascii_str(13)

        # Ensure the bands are a 2.4GHz and 5GHz pair. This test uses a single AP, and therefore
        # does not support the the same origin and target band.
        expected_bands = {
            hostapd_constants.BandType.BAND_2G,
            hostapd_constants.BandType.BAND_5G,
        }
        actual_bands = {test_params.origin_band, test_params.target_band}
        abort_class_if(
            actual_bands != expected_bands,
            f"Test expects one 2.4GHz AP and one 5GHz AP. Got origin: {test_params.origin_band}, target {test_params.target_band}",
        )

        if not self.test_kit.access_point:
            raise signals.TestAbortClass(
                "No access point configured for this test."
            )

        # Setup 2.4GHz AP
        if isinstance(self.test_kit.access_point, AccessPoint):
            origin_security_mode = ConfigMapper.to_hostapd_security(
                test_params.origin_security_mode
            )
            target_security_mode = ConfigMapper.to_hostapd_security(
                test_params.target_security_mode
            )

            origin_security_config = DeprecatedSecurity(
                origin_security_mode, password=origin_password
            )
            target_security_config = DeprecatedSecurity(
                target_security_mode, password=target_password
            )

            if test_params.origin_band == hostapd_constants.BandType.BAND_2G:
                deprecated_security_2g = origin_security_config
                deprecated_security_5g = target_security_config
            else:
                deprecated_security_2g = target_security_config
                deprecated_security_5g = origin_security_config

            setup_ap(
                access_point=self.test_kit.access_point,
                profile_name="whirlwind",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=ssid,
                security=deprecated_security_2g,
            )

            # Setup 5GHz AP
            setup_ap(
                access_point=self.test_kit.access_point,
                profile_name="whirlwind",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=ssid,
                security=deprecated_security_5g,
            )
        elif isinstance(self.test_kit.access_point, OpenWrtAP):
            if test_params.origin_band == hostapd_constants.BandType.BAND_2G:
                security_2g = test_params.origin_security_mode
                security_5g = test_params.target_security_mode
                password_2g = origin_password
                password_5g = target_password
            else:
                security_2g = test_params.target_security_mode
                security_5g = test_params.origin_security_mode
                password_2g = target_password
                password_5g = origin_password

            self.test_kit.access_point.configure_wifi(
                AccessPointConfig(
                    radios=[
                        RadioConfig(
                            channel=DEFAULT_2G_CHANNEL,
                            bss_settings=[
                                BssSettings(
                                    ssid=ssid,
                                    password=password_2g,
                                    security=security_2g,
                                )
                            ],
                        ),
                        RadioConfig(
                            channel=DEFAULT_5G_CHANNEL,
                            bss_settings=[
                                BssSettings(
                                    ssid=ssid,
                                    password=password_5g,
                                    security=security_5g,
                                )
                            ],
                        ),
                    ]
                )
            )
        return RoamTestParameters(ssid, origin_password, target_password)

    async def _test_logic(
        self,
        test_params: TestParams,
    ) -> None:
        if not self.test_kit.access_point:
            raise signals.TestAbortClass(
                "No access point configured for this test."
            )
        self.skip_if_wep_not_supported(test_params)
        # Setup APs using test params
        roam_params = await self.setup_aps(test_params)
        ssid = roam_params.ssid
        origin_password = roam_params.origin_password
        roam_params.target_password

        origin_band = (
            Band.BAND_2G
            if test_params.origin_band == hostapd_constants.BandType.BAND_2G
            else Band.BAND_5G
        )
        target_band = (
            Band.BAND_5G
            if test_params.origin_band == hostapd_constants.BandType.BAND_2G
            else Band.BAND_2G
        )

        # Passive scan on the channels used in this test, which are the default channels for 2.4GHz and 5GHz APs.
        if isinstance(self.test_kit.access_point, OpenWrtAP):
            channels = [DEFAULT_2G_CHANNEL.number, DEFAULT_5G_CHANNEL.number]
        else:
            channels = [
                hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                hostapd_constants.AP_DEFAULT_CHANNEL_5G,
            ]

        scan_results = (
            (
                await self.test_kit.client_sme.scan_for_controller(
                    req=fidl_sme.ScanRequest(
                        passive=fidl_sme.PassiveScanRequest(channels=channels)
                    )
                )
            )
            .unwrap()
            .scan_results
        )
        if scan_results is None:
            raise signals.TestError(
                "ClientSme.ScanForController() response is missing scan_results"
            )

        # Parse out scanned BSSs from the test network
        bss_desc_2g = None
        bss_desc_5g = None
        for scan_result in scan_results:
            assert (
                scan_result.bss_description is not None
            ), "ScanResult is missing bss_description"
            assert (
                scan_result.bss_description.ies is not None
            ), "ScanResult.BssDescription is missing ies"
            scanned_ssid = read_ssid(bytes(scan_result.bss_description.ies))
            if scanned_ssid == ssid:
                channel = scan_result.bss_description.channel.primary
                if channel in hostapd_constants.US_CHANNELS_2G:
                    bss_desc_2g = scan_result.bss_description
                elif channel in hostapd_constants.US_CHANNELS_5G:
                    bss_desc_5g = scan_result.bss_description
                else:
                    raise signals.TestError(
                        f"BSS for test network SSID '{ssid}' found on unexpected channel: {channel}"
                    )

        # Verify there are two BSSs seen for the test network
        if bss_desc_2g is None:
            raise signals.TestError(
                f"Failed to see 2.4GHz BSS for SSID '{ssid}' in scan results"
            )
        if bss_desc_5g is None:
            raise signals.TestError(
                f"Failed to see 5GHz BSS for SSID '{ssid}' in scan results"
            )

        if test_params.origin_band == hostapd_constants.BandType.BAND_2G:
            origin_bss_desc = bss_desc_2g
            target_bss_desc = bss_desc_5g
        else:
            origin_bss_desc = bss_desc_5g
            target_bss_desc = bss_desc_2g

        (
            proxy,
            server,
        ) = self.dut.fuchsia_controller.channel_create()
        async with ConnectTransactionEventHandler(proxy, server) as ctx:
            txn_queue = ctx.txn_queue
            server = ctx.server

            match test_params.origin_security_mode:
                case SecurityOpen():
                    protocol = fidl_security.Protocol.OPEN
                    credentials = None
                case SecurityWep():
                    protocol = fidl_security.Protocol.WEP
                    credentials = fidl_security.Credentials(
                        wep=fidl_security.WepCredentials(
                            TEST_WEP_PASSWORD_LITERAL.encode("ascii")
                        )
                    )
                case SecurityWpa():
                    protocol = fidl_security.Protocol.WPA1
                    if origin_password is None:
                        raise signals.TestError("Password is required for WPA.")
                    credentials = fidl_security.Credentials(
                        wpa=fidl_security.WpaCredentials(
                            passphrase=list(origin_password.encode("ascii"))
                        )
                    )
                case SecurityWpa2() | SecurityWpaWpa2Mixed():
                    protocol = fidl_security.Protocol.WPA2_PERSONAL
                    if origin_password is None:
                        raise signals.TestError(
                            "Password is required for WPA2/WPA_WPA2."
                        )
                    credentials = fidl_security.Credentials(
                        wpa=fidl_security.WpaCredentials(
                            passphrase=list(origin_password.encode("ascii"))
                        )
                    )
                case SecurityWpa3() | SecurityWpa2Wpa3Mixed():
                    protocol = fidl_security.Protocol.WPA3_PERSONAL
                    if origin_password is None:
                        raise signals.TestError(
                            "Password is required for WPA3/WPA2_WPA3."
                        )
                    credentials = fidl_security.Credentials(
                        wpa=fidl_security.WpaCredentials(
                            passphrase=list(origin_password.encode("ascii"))
                        )
                    )
                case _:
                    raise signals.TestError(
                        f"Unsupported security mode for origin AP: {test_params.origin_security_mode}"
                    )

            # Send connect request for origin BSS
            connect_request = fidl_sme.ConnectRequest(
                ssid=list(ssid.encode("ascii")),
                bss_description=origin_bss_desc,
                multiple_bss_candidates=True,
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

            # Verify a successful connect result is received
            try:
                next_txn = await asyncio.wait_for(
                    txn_queue.get(), timeout=CONNECT_WAIT_TIME_SECONDS
                )
            except TimeoutError:
                raise signals.TestError(
                    f"Timed out after {CONNECT_WAIT_TIME_SECONDS} seconds awaiting a connect result"
                )

            if next_txn != fidl_sme.ConnectTransactionOnConnectResultRequest(
                result=fidl_sme.ConnectResult(
                    code=fidl_ieee80211.StatusCode.SUCCESS,
                    is_credential_rejected=False,
                    is_reconnect=False,
                )
            ):
                raise signals.TestError(f"Failed to connect to initial AP.")

            logger.info(f"Connect result: success.")
            if not txn_queue.empty():
                raise signals.TestError(
                    "Unexpectedly received additional callback messages after connect result."
                )

            # Verify that DUT is actually associated (as seen from AP).
            target_iface = None
            if isinstance(self.test_kit.access_point, AccessPoint):
                client_mac = await self._get_client_mac()
                if (
                    test_params.origin_band
                    == hostapd_constants.BandType.BAND_2G
                ):
                    origin_iface = self.test_kit.access_point.wlan_2g
                    target_iface = self.test_kit.access_point.wlan_5g
                else:
                    origin_iface = self.test_kit.access_point.wlan_5g
                    target_iface = self.test_kit.access_point.wlan_2g

                if not self.test_kit.access_point.sta_authenticated(
                    origin_iface, client_mac
                ):
                    raise signals.TestError(
                        f"DUT is not authenticated on the {test_params.origin_band} band"
                    )

                if not self.test_kit.access_point.sta_associated(
                    origin_iface, client_mac
                ):
                    raise signals.TestError(
                        f"DUT is not associated on the {test_params.origin_band} band"
                    )

                if not self.test_kit.access_point.sta_authorized(
                    origin_iface, client_mac
                ):
                    raise signals.TestError(
                        f"DUT is not authorized on the {test_params.origin_band} band"
                    )
            elif isinstance(self.test_kit.access_point, OpenWrtAP):
                client_mac = await self._get_client_mac()

                sta_status = self.get_single_sta_status(
                    client_mac, band=origin_band
                )
                if not sta_status.auth:
                    raise signals.TestError(
                        f"DUT is not authenticated on the {test_params.origin_band} band"
                    )
                if not sta_status.assoc:
                    raise signals.TestError(
                        f"DUT is not associated on the {test_params.origin_band} band"
                    )
                if not sta_status.authorized:
                    raise signals.TestError(
                        f"DUT is not authorized on the {test_params.origin_band} band"
                    )
            else:
                raise signals.TestError(
                    "No access point configured for this test."
                )

            # Send a roam request for target BSS. From this point, failed assert calls are relevant
            # to the roam attempt.

            # Add a delay to let the connection stabalized, and avoid sending the roam request too
            # quickly after the initial connection (b/484056019).
            await asyncio.sleep(10)
            roam_request = fidl_sme.RoamRequest(bss_description=target_bss_desc)
            logger.info(f"RoamRequest: {roam_request!r}")
            self.test_kit.client_sme.roam(req=roam_request)

            # Verify a successful roam result is received. Filter out any signal reports. Waits up
            # to NEXT_TXN_WAIT_TIME_SECONDS for the next txn, and up to
            # ROAM_RESULT_WAIT_TIME_SECONDS for a roam result.
            start_time = time.time()
            while time.time() < start_time + ROAM_RESULT_WAIT_TIME_SECONDS:
                # Wait for the next txn. If next txn is:
                # - OnRoamResultRequest: verify roam result
                # - OnSignalReportRequest: ignore, and continue waiting (up to ROAM_RESULT_WAIT_TIME_SECONDS)
                # - None | something else: fail and exit
                next_txn = await asyncio.wait_for(
                    txn_queue.get(), timeout=NEXT_TXN_WAIT_TIME_SECONDS
                )
                if next_txn is None:
                    fail(
                        f"Failed to receive the next transaction connection (OnRoamResultRequest or otherwise) within {NEXT_TXN_WAIT_TIME_SECONDS} seconds."
                    )
                match next_txn:
                    case txn if isinstance(
                        txn, fidl_sme.ConnectTransactionOnSignalReportRequest
                    ):
                        # Ignore any signal reports
                        logger.info(f"Ignoring signal report: {txn}")
                        continue
                    case txn if isinstance(
                        txn, fidl_sme.ConnectTransactionOnRoamResultRequest
                    ):
                        if test_params.should_roam_succeed:
                            # Verify roam result
                            logger.info(
                                f"ConnectTransactionOnRoamResultRequest received: {next_txn}"
                            )

                            assert_equal(
                                next_txn.result.status_code,
                                fidl_ieee80211.StatusCode.SUCCESS,
                                "Roam status code is not SUCCESS",
                            )
                            assert_equal(
                                next_txn.result.bssid,
                                target_bss_desc.bssid,
                                "Roamed to wrong BSSID",
                            )
                            # Verify DUT is connected to the AP using the target interface
                            if (
                                isinstance(
                                    self.test_kit.access_point, AccessPoint
                                )
                                and target_iface
                            ):
                                assert_true(
                                    self.test_kit.access_point.sta_authenticated(
                                        target_iface, client_mac
                                    ),
                                    f"DUT is not authenticated on the {test_params.target_band} band",
                                )
                                assert_true(
                                    self.test_kit.access_point.sta_associated(
                                        target_iface, client_mac
                                    ),
                                    f"DUT is not associated on the {test_params.target_band} band",
                                )
                                assert_true(
                                    self.test_kit.access_point.sta_authorized(
                                        target_iface, client_mac
                                    ),
                                    f"DUT is not 802.1X authorized on the {test_params.target_band} band",
                                )
                            elif isinstance(
                                self.test_kit.access_point, OpenWrtAP
                            ):
                                status = self.get_single_sta_status(
                                    client_mac, band=target_band
                                )
                                assert_true(
                                    status.auth,
                                    f"DUT is not authenticated on the {test_params.target_band} band",
                                )
                                assert_true(
                                    status.assoc,
                                    f"DUT is not associated on the {test_params.target_band} band",
                                )
                                assert_true(
                                    status.authorized,
                                    f"DUT is not authorized on the {test_params.target_band} band",
                                )
                        else:
                            assert_not_equal(
                                txn.result.status_code,
                                fidl_ieee80211.StatusCode.SUCCESS,
                            )
                            # If the original association was maintained, the disconnect info should be None, and vice versa.
                            assert_equal(
                                txn.result.original_association_maintained,
                                txn.result.disconnect_info is None,
                            )
                        break
                    case _:
                        fail(
                            f"Unexpected transaction received while waiting for roam result: {next_txn}"
                        )
            else:
                fail(
                    f"Never received a roam result for target BSSID {target_bss_desc.bssid} within the {ROAM_RESULT_WAIT_TIME_SECONDS} second timeout period."
                )

    async def _get_client_mac(self) -> str:
        """Get the MAC address of the DUT client interface.

        Returns:
            str, MAC address of the DUT client interface.
        Raises:
            RuntimeError if there is no DUT client interface or if the DUT interface query fails.
        """
        try:
            query_iface_response = (
                await self.test_kit.device_monitor.query_iface(
                    iface_id=self.test_kit.iface_id
                )
            ).unwrap()
        except Exception as e:
            raise RuntimeError(f"DeviceMonitor.QueryIface() error: {e}") from e
        mac_addr = MacAddress.from_bytes(
            bytes(query_iface_response.resp.sta_addr)
        )
        return str(mac_addr)


if __name__ == "__main__":
    test_runner.main()
