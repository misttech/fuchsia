#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import itertools
import logging
import re
from dataclasses import dataclass
from typing import Literal

import fidl_fuchsia_wlan_internal as fidl_security
import fuchsia_wlan_base_test
import honeydew.affordances.connectivity.wlan.core as wlan_core
from honeydew.affordances.connectivity.wlan.utils.types import (
    KNOWN_COUNTRY_CODES,
)
from legacy_access_point.access_point import setup_ap
from legacy_access_point.ap_lib import hostapd_config, hostapd_constants
from legacy_access_point.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from legacy_access_point.ap_lib.hostapd_security import (
    SecurityMode as DeprecatedSecurityMode,
)
from mobly import asserts, signals, test_runner
from openwrt_access_point.lib import capabilities
from openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    BssChannel,
    BssSettings,
    CapabilitySelection,
    HtMode,
    RadioConfig,
    Security,
    SecurityOpen,
    SecurityWpa2,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)
from openwrt_access_point.lib.uci_radio_options import UciRadioOptions

logger = logging.getLogger(__name__)

FREQUENCY_24: str = "2.4GHz"
FREQUENCY_5: str = "5GHz"
CHANNEL_BANDWIDTH_20: str = "HT20"
CHANNEL_BANDWIDTH_40_LOWER: str = "HT40-"
CHANNEL_BANDWIDTH_40_UPPER: str = "HT40+"

# HT capabilities unsupported on OpenWrt One (MT7981B + MT7976C, HT capab 0x09ef):
# * Multi-stream RX STBC ([RX-STBC12], [RX-STBC123])
# * DSSS/CCK 40 MHz ([DSSS_CCK-40])
# * 40 MHz Intolerant ([40-INTOLERANT])
# * SM Power Save ([SMPS-STATIC])
OPENWRT_ONE_UNSUPPORTED_N_CAPS: frozenset[str] = frozenset(
    {
        capabilities.N_CAPABILITY_RX_STBC12,
        capabilities.N_CAPABILITY_RX_STBC123,
        capabilities.N_CAPABILITY_DSSS_CCK_40,
        capabilities.N_CAPABILITY_40_INTOLERANT,
        capabilities.N_CAPABILITY_SMPS_STATIC,
    }
)

# Capabilities and 2.4 GHz HT40 boundary channels unsupported on legacy Whirlwind APs (IPQ4019 2x2).
WHIRLWIND_UNSUPPORTED_N_CAPS: frozenset[str] = frozenset(
    {
        capabilities.N_CAPABILITY_GREENFIELD,
        capabilities.N_CAPABILITY_RX_STBC12,
        capabilities.N_CAPABILITY_RX_STBC123,
    }
)
WHIRLWIND_UNSUPPORTED_24_HT40_CHANNELS: frozenset[int] = frozenset({5, 9, 13})

TX_STBC = ["", capabilities.N_CAPABILITY_TX_STBC]
RX_STBC = [
    "",
    capabilities.N_CAPABILITY_RX_STBC1,
    capabilities.N_CAPABILITY_RX_STBC12,
    capabilities.N_CAPABILITY_RX_STBC123,
]

MAX_HT_CAPABS_40MHZ = [
    capabilities.N_CAPABILITY_LDPC,
    capabilities.N_CAPABILITY_SHORT_GI_20,
    capabilities.N_CAPABILITY_SHORT_GI_40,
    capabilities.N_CAPABILITY_TX_STBC,
    capabilities.N_CAPABILITY_RX_STBC1,
    capabilities.N_CAPABILITY_MAX_AMSDU_7935,
]


@dataclass
class TestParams:
    frequency: str
    chbw: str
    channel: int
    n_mode: hostapd_constants.Mode
    security: Security
    # TODO(http://b/290396383): Type AP capabilities as enums
    n_capabilities: list[str]


class WlanPhyCompliance11NTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """Tests for validating 11n PHYs.

    For test matrix design and rationale, see http://go/wlan-test-optimization.

    Test Bed Requirement:
    * One Fuchsia device
    * One Access Point
    """

    phy: wlan_core.Phy

    async def setup_class(self) -> None:
        await super().setup_class()
        if not self.openwrt_ap and not self.access_point:
            raise signals.TestError("Requires at least one access point")
        if self.access_point:
            self.access_point.stop_all_aps()
        self.phy = await self.dut.wlan_core.ensure_single_phy()

        # 802.11n operates on both 2.4 GHz and 5 GHz. The PHY initializes in
        # the worldwide (WW) regulatory domain, which disables 5 GHz
        # transmission, so set country to US to enable the 5 GHz channels
        # used below.
        await self.phy.set_country(
            KNOWN_COUNTRY_CODES["UNITED_STATES_OF_AMERICA"]
        )

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.dut.wlan_core.destroy_all_ifaces()

    async def teardown_test(self) -> None:
        await self.dut.wlan_core.destroy_all_ifaces()
        if self.access_point:
            self.access_point.stop_all_aps()
        await super().teardown_test()

    async def pre_run(self) -> None:
        raw_test_args: list[tuple[TestParams]] = (
            self._generate_spatial_stbc_test_args()
            + self._generate_ht_capability_bit_sweep_test_args()
            + self._generate_bss_operational_and_channel_mode_test_args()
            + self._generate_composite_stress_test_args()
        )

        def generate_test_name(params: TestParams) -> str:
            ret = []
            mapped_caps = [
                ConfigMapper.to_hostapd_n_cap(c)
                for c in params.n_capabilities
                if c
            ]
            for cap in hostapd_constants.N_CAPABILITIES_MAPPING.keys():
                if cap in mapped_caps:
                    ret.append(
                        hostapd_constants.N_CAPABILITIES_MAPPING[cap]
                        .replace("[", "_")
                        .replace("]", "")
                    )
            # '+' is used by Mobile Harness as special character, don't use it in test names
            if params.chbw == CHANNEL_BANDWIDTH_40_LOWER:
                chbw = "HT40Lower"
            elif params.chbw == CHANNEL_BANDWIDTH_40_UPPER:
                chbw = "HT40Upper"
            else:
                chbw = params.chbw
            # Maintain legacy naming for BUILD.gn filters
            security_name = (
                "open" if params.security == SecurityOpen() else "wpa2"
            )
            return f"test_11n_{params.frequency}_{chbw}_ch{params.channel}_{security_name}_{params.n_mode}{''.join(ret)}"

        # Deduplicate via dict keys (unique test names overwrite duplicates);
        # avoids set() since TestParams contains an unhashable list.
        test_args = list(
            {generate_test_name(p): (p,) for (p,) in raw_test_args}.values()
        )

        self.generate_tests(
            test_logic=self.setup_and_connect,
            name_func=generate_test_name,
            arg_sets=test_args,
        )

    async def setup_and_connect(self, params: TestParams) -> None:
        """Start hostapd and associate the DUT.

        Args:
            params: Test parameters
        """
        self._skip_if_unsupported(params)

        ssid = AccessPointConfig.random_string(20)
        password: str | None = None
        channel = params.channel

        # Channels 9+13 (HT40+) and 13+9 (HT40-) in 2.4 GHz require a regulatory
        # domain that permits Channel 13 (e.g., Australia "AU").
        if params.frequency == FREQUENCY_24 and channel in (9, 13):
            ap_country = "AU"
            dut_country = KNOWN_COUNTRY_CODES["AUSTRALIA"]
        else:
            ap_country = "US"
            dut_country = KNOWN_COUNTRY_CODES["UNITED_STATES_OF_AMERICA"]

        await self.phy.set_country(dut_country)
        protocol = params.security.to_fidl_wlan_internal()
        if protocol != fidl_security.Protocol.OPEN:
            password = AccessPointConfig.random_string(20)

        if self.openwrt_ap:
            band = (
                Band.BAND_2G
                if params.frequency == FREQUENCY_24
                else Band.BAND_5G
            )
            bandwidth: Literal[20, 40] = 40 if "HT40" in params.chbw else 20

            extension_channel: Literal["+", "-", None] = None
            if params.chbw == CHANNEL_BANDWIDTH_40_UPPER:
                extension_channel = "+"
            elif params.chbw == CHANNEL_BANDWIDTH_40_LOWER:
                extension_channel = "-"

            n_caps = [cap for cap in params.n_capabilities if cap]

            custom_uci_options: UciRadioOptions = {}
            if params.n_mode == hostapd_constants.Mode.MODE_11N_PURE:
                custom_uci_options["require_mode"] = "n"
            if bandwidth == 40:
                # Disable 802.11n OBSS coexistence scanning so hostapd does not
                # fall back from 40 MHz to 20 MHz due to overlapping BSS in the lab.
                custom_uci_options["noscan"] = True

            config = AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=BssChannel(
                            band=band,
                            number=channel,
                            phy_mode=HtMode(
                                bw=bandwidth, extension=extension_channel
                            ),
                        ),
                        country=ap_country,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=params.security,
                                password=password,
                            )
                        ],
                        n_capabilities=CapabilitySelection.CUSTOM(n_caps),
                        custom_uci_options=custom_uci_options,
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
            self._verify_openwrt_ht_capabilities(
                band=band,
                chbw=params.chbw,
                n_mode=params.n_mode,
                n_capabilities=n_caps,
            )

            await self._connect_and_validate_channel(
                ssid,
                protocol,
                target_channel=channel,
                target_pwd=password,
            )
        elif self.access_point:
            security_profile = DeprecatedSecurity()
            n_capabilities = []
            for cap in params.n_capabilities:
                if not cap:
                    continue
                mapped_cap = ConfigMapper.to_hostapd_n_cap(cap)
                if (
                    mapped_cap
                    in hostapd_constants.N_CAPABILITIES_MAPPING.keys()
                ):
                    n_capabilities.append(mapped_cap)

            if params.chbw == CHANNEL_BANDWIDTH_40_UPPER:
                if not hostapd_config.ht40_plus_allowed(channel):
                    raise ValueError(f"Invalid HT40+ channel: {channel}")
                n_capabilities.append(hostapd_constants.N_CAPABILITY_HT40_PLUS)
            elif params.chbw == CHANNEL_BANDWIDTH_40_LOWER:
                if not hostapd_config.ht40_minus_allowed(channel):
                    raise ValueError(f"Invalid HT40- channel: {channel}")
                n_capabilities.append(hostapd_constants.N_CAPABILITY_HT40_MINUS)

            if params.security == SecurityWpa2():
                security_profile = DeprecatedSecurity(
                    security_mode=DeprecatedSecurityMode.WPA2,
                    password=password,
                    wpa_cipher="CCMP",
                    wpa2_cipher="CCMP",
                )

            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                mode=params.n_mode,
                channel=channel,
                n_capabilities=n_capabilities,
                ac_capabilities=[],
                force_wmm=True,
                ssid=ssid,
                security=security_profile,
            )
            await self._connect_and_validate_channel(
                ssid,
                protocol,
                target_channel=channel,
                target_pwd=password,
            )

    def _skip_if_unsupported(self, params: TestParams) -> None:
        """Skips the test case if the active AP does not support params."""
        if self.openwrt_ap:
            ap_name = "OpenWrt One AP"
            unsupported_caps = OPENWRT_ONE_UNSUPPORTED_N_CAPS
        elif self.access_point:
            asserts.skip_if(
                params.frequency == FREQUENCY_24
                and params.channel in WHIRLWIND_UNSUPPORTED_24_HT40_CHANNELS,
                f"Skipped because 2.4 GHz HT40 channel {params.channel} is not supported by Whirlwind AP",
            )
            ap_name = "Whirlwind AP"
            unsupported_caps = WHIRLWIND_UNSUPPORTED_N_CAPS
        else:
            return

        unsupported = [
            c for c in params.n_capabilities if c in unsupported_caps
        ]
        asserts.skip_if(
            bool(unsupported),
            f"Skipped because capabilities {unsupported} are not supported by {ap_name}",
        )

    def _verify_openwrt_ht_capabilities(
        self,
        band: Band,
        chbw: str,
        n_mode: hostapd_constants.Mode,
        n_capabilities: list[str],
    ) -> None:
        if not self.openwrt_ap:
            return
        phy = "phy0" if band == Band.BAND_2G else "phy1"
        conf_path = f"/var/run/hostapd-{phy}.conf"
        res = self.openwrt_ap.ssh.run(
            f"grep -E '^(ht_capab|require_ht)=' {conf_path} || true"
        )
        out_lines = res.stdout.decode("utf-8").strip().splitlines()
        ht_capab_line = next(
            (line for line in out_lines if line.startswith("ht_capab=")), ""
        )
        actual_tokens = set(re.findall(r"\[[^\]]+\]", ht_capab_line))

        expected_tokens: set[str] = set()
        if chbw in (CHANNEL_BANDWIDTH_40_UPPER, CHANNEL_BANDWIDTH_40_LOWER):
            expected_tokens.add(f"[{chbw}]")

        for cap in n_capabilities:
            if not cap:
                continue
            expected_tokens.add(f"[{cap}]")

        logger.info(
            "Verifying OpenWrt HT capabilities in %s: expected=%s, actual=%s (raw=%r)",
            conf_path,
            sorted(expected_tokens),
            sorted(actual_tokens),
            ht_capab_line,
        )
        asserts.assert_equal(
            actual_tokens,
            expected_tokens,
            f"Mismatch in {conf_path} ht_capab! "
            f"Missing: {sorted(expected_tokens - actual_tokens)}, "
            f"Unexpected: {sorted(actual_tokens - expected_tokens)}, "
            f"Raw line: {ht_capab_line}",
        )
        if n_mode == hostapd_constants.Mode.MODE_11N_PURE:
            asserts.assert_true(
                "require_ht=1" in out_lines,
                f"Expected require_ht=1 in {conf_path} for 11n-pure mode, got: {out_lines}",
            )
        logger.info(
            "Successfully verified OpenWrt HT capabilities in %s", conf_path
        )

    async def _connect_and_validate_channel(
        self,
        target_ssid: str,
        target_security: fidl_security.Protocol,
        target_channel: int,
        target_pwd: str | None = None,
    ) -> None:
        iface = await self.phy.create_client_iface()
        await iface.scan_and_connect(
            ssid=target_ssid,
            password=target_pwd,
            security=target_security,
            channels=[target_channel],
        )
        status = await iface.status()
        if status.connected is None:
            raise signals.TestFailure(
                f"Expected connected status, got: {status}"
            )
        got_channel = status.connected.primary.number
        asserts.assert_equal(
            got_channel,
            target_channel,
            f"Connected to wrong channel. Expected channel {target_channel}, "
            f"got {got_channel}.",
        )

    @staticmethod
    def _make_test_params(
        frequency: str,
        chbw: str,
        channel: int,
        n_capabilities: list[str],
        n_mode: hostapd_constants.Mode = hostapd_constants.Mode.MODE_11N_MIXED,
    ) -> tuple[TestParams]:
        n_caps: list[str] = []
        for cap in n_capabilities:
            if cap and cap not in n_caps:
                n_caps.append(cap)
        return (
            TestParams(
                frequency=frequency,
                chbw=chbw,
                channel=channel,
                n_mode=n_mode,
                security=SecurityWpa2(),
                n_capabilities=n_caps,
            ),
        )

    def _generate_spatial_stbc_test_args(self) -> list[tuple[TestParams]]:
        """Generates spatial and STBC coupled test args."""
        test_args: list[tuple[TestParams]] = []
        band_bw_channels = [
            (FREQUENCY_24, CHANNEL_BANDWIDTH_20, 1),
            (FREQUENCY_24, CHANNEL_BANDWIDTH_40_UPPER, 1),
            (FREQUENCY_5, CHANNEL_BANDWIDTH_20, 36),
            (FREQUENCY_5, CHANNEL_BANDWIDTH_40_UPPER, 36),
        ]
        for (freq, chbw, ch), tx_stbc, rx_stbc in itertools.product(
            band_bw_channels,
            TX_STBC,
            RX_STBC,
        ):
            test_args.append(
                self._make_test_params(freq, chbw, ch, [tx_stbc, rx_stbc])
            )
        return test_args

    def _generate_ht_capability_bit_sweep_test_args(
        self,
    ) -> list[tuple[TestParams]]:
        """Generates single-bit HT capability sweep test args."""
        return [
            # 2a. LDPC
            self._make_test_params(
                FREQUENCY_24,
                CHANNEL_BANDWIDTH_20,
                1,
                [capabilities.N_CAPABILITY_LDPC],
            ),
            # 2b. Short GI 20
            self._make_test_params(
                FREQUENCY_24,
                CHANNEL_BANDWIDTH_20,
                1,
                [capabilities.N_CAPABILITY_SHORT_GI_20],
            ),
            # 2c. Short GI 40
            self._make_test_params(
                FREQUENCY_5,
                CHANNEL_BANDWIDTH_40_UPPER,
                36,
                [capabilities.N_CAPABILITY_SHORT_GI_40],
            ),
            # 2d. Max A-MSDU 7935
            self._make_test_params(
                FREQUENCY_24,
                CHANNEL_BANDWIDTH_20,
                1,
                [capabilities.N_CAPABILITY_MAX_AMSDU_7935],
            ),
            # 2e. Greenfield
            self._make_test_params(
                FREQUENCY_24,
                CHANNEL_BANDWIDTH_20,
                1,
                [capabilities.N_CAPABILITY_GREENFIELD],
            ),
            # 2f. DSSS/CCK 40 MHz
            self._make_test_params(
                FREQUENCY_24,
                CHANNEL_BANDWIDTH_40_UPPER,
                1,
                [capabilities.N_CAPABILITY_DSSS_CCK_40],
            ),
            # 2g. 40 MHz Intolerant
            self._make_test_params(
                FREQUENCY_24,
                CHANNEL_BANDWIDTH_20,
                1,
                [capabilities.N_CAPABILITY_40_INTOLERANT],
            ),
            # 2h. SM Power Save (Static)
            self._make_test_params(
                FREQUENCY_24,
                CHANNEL_BANDWIDTH_20,
                1,
                [capabilities.N_CAPABILITY_SMPS_STATIC],
            ),
        ]

    def _generate_bss_operational_and_channel_mode_test_args(
        self,
    ) -> list[tuple[TestParams]]:
        """Generates Pure-N mode and 40 MHz boundary channel offset test args."""
        test_args: list[tuple[TestParams]] = [
            # 3a. Pure-N Mode (require_ht=1)
            self._make_test_params(
                FREQUENCY_24,
                CHANNEL_BANDWIDTH_20,
                1,
                [],
                n_mode=hostapd_constants.Mode.MODE_11N_PURE,
            )
        ]
        # 3b. Extension Offset (8 boundary channel bonding tests)
        boundary_channels = [
            (FREQUENCY_24, CHANNEL_BANDWIDTH_40_UPPER, 1),  # Ch 1 + 5
            (FREQUENCY_24, CHANNEL_BANDWIDTH_40_UPPER, 9),  # Ch 9 + 13 (AU)
            (FREQUENCY_24, CHANNEL_BANDWIDTH_40_LOWER, 5),  # Ch 5 + 1
            (FREQUENCY_24, CHANNEL_BANDWIDTH_40_LOWER, 13),  # Ch 13 + 9 (AU)
            (FREQUENCY_5, CHANNEL_BANDWIDTH_40_UPPER, 36),  # Ch 36 + 40
            (FREQUENCY_5, CHANNEL_BANDWIDTH_40_UPPER, 157),  # Ch 157 + 161
            (FREQUENCY_5, CHANNEL_BANDWIDTH_40_LOWER, 40),  # Ch 40 + 36
            (FREQUENCY_5, CHANNEL_BANDWIDTH_40_LOWER, 161),  # Ch 161 + 157
        ]
        for freq, chbw, ch in boundary_channels:
            test_args.append(self._make_test_params(freq, chbw, ch, []))
        return test_args

    def _generate_composite_stress_test_args(self) -> list[tuple[TestParams]]:
        """Generates composite max HT capability bitmask stress test args."""
        return [
            self._make_test_params(
                FREQUENCY_24,
                CHANNEL_BANDWIDTH_40_UPPER,
                1,
                MAX_HT_CAPABS_40MHZ,
            ),
            self._make_test_params(
                FREQUENCY_5,
                CHANNEL_BANDWIDTH_40_UPPER,
                36,
                MAX_HT_CAPABS_40MHZ,
            ),
        ]


if __name__ == "__main__":
    test_runner.main()
