# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import logging
import random
import time
from dataclasses import dataclass
from typing import Literal

import fuchsia_wlan_base_test
from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from antlion.controllers.ap_lib.hostapd_security import SecurityMode
from honeydew.affordances.connectivity.wlan.utils.types import (
    CountryCode,
    SecurityType,
)
from mobly import signals, test_runner
from mobly.records import TestResultRecord
from openwrt_access_point import OpenWrtAP
from openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    BssChannel,
    BssSettings,
    CapabilitySelection,
    LegacyMode,
    RadioConfig,
    SecurityWep,
    SecurityWpa,
    SecurityWpa2,
    SecurityWpa2Wpa3Mixed,
    SecurityWpa3,
    SecurityWpaWpa2Mixed,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper,
)

AP_11ABG_PROFILE_NAME = "whirlwind_11ag_legacy"
SSID_LENGTH_DEFAULT = 15
MAX_WPA_PASSWORD_LENGTH = 63
WPA_HEX_PSK_LENGTH = 64


@dataclass
class WepTestParams:
    security: SecurityWep
    key_length: int
    hex_key: bool
    band: Band


@dataclass
class WpaTestParams:
    security: SecurityWpa | SecurityWpa2 | SecurityWpaWpa2Mixed
    band: Band


@dataclass
class Wpa3TestParams:
    security: SecurityWpa2Wpa3Mixed | SecurityWpa3
    band: Band


@dataclass
class BoundaryTestParams:
    security: SecurityWpa | SecurityWpa2 | SecurityWpa3
    password_type: Literal["max_length_password", "max_length_psk"]
    band: Band


@dataclass
class Utf8TestParams:
    security: SecurityWpa2
    char_class: str
    password: str
    band: Band


def _get_band_name(band: Band) -> str:
    """Returns the name representation of the given band (e.g. '2g', '5g')."""
    if band == Band.BAND_2G:
        return "2g"
    elif band == Band.BAND_5G:
        return "5g"
    else:
        raise ValueError(f"Unknown band: {band}")


class SecurityTypeTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """Tests for validating various security types on Fuchsia WLAN.

    Test Bed Requirement:
    * One Fuchsia device (DUT)
    * One Access Point (OpenWrt or legacy Whirlwind)
    """

    access_point: AccessPoint | None = None
    openwrt_ap: OpenWrtAP | None = None

    async def pre_run(self) -> None:
        self.log = logging.getLogger(self.__class__.__name__)
        self.seed = int(self.user_params.get("seed", time.time_ns()))
        self.log.info(f"Deterministic seed: {self.seed}")
        self.rng = random.Random(self.seed)

        wep_args: list[tuple[WepTestParams]] = []
        auth_modes: list[Literal["open", "shared"]] = ["open", "shared"]
        for auth_mode in auth_modes:
            for key_length, hex_key in [
                (5, False),
                (13, False),
                (10, True),
                (26, True),
            ]:
                wep_args.append(
                    (
                        WepTestParams(
                            security=SecurityWep(auth_mode=auth_mode),
                            key_length=key_length,
                            hex_key=hex_key,
                            band=self.rng.choice([Band.BAND_2G, Band.BAND_5G]),
                        ),
                    )
                )

        def generate_wep_test_name(params: WepTestParams) -> str:
            key_type = "hex" if params.hex_key else "chars"
            band_name = _get_band_name(params.band)
            name = f"test_associate_{band_name}_{params.security.uci_encryption}_{params.key_length}_{key_type}"
            self.log.info(f"Generated test case: {name}")
            return name

        self.generate_tests(
            test_logic=self._run_wep_test,
            name_func=generate_wep_test_name,
            arg_sets=wep_args,
        )

        wpa_args: list[tuple[WpaTestParams]] = []
        securities: list[
            type[SecurityWpa] | type[SecurityWpa2] | type[SecurityWpaWpa2Mixed]
        ] = [
            SecurityWpa,
            SecurityWpa2,
            SecurityWpaWpa2Mixed,
        ]
        ciphers: list[Literal["ccmp", "tkip", "ccmp+tkip"]] = [
            "ccmp",
            "tkip",
            "ccmp+tkip",
        ]
        for security in securities:
            for cipher in ciphers:
                wpa_args.append(
                    (
                        WpaTestParams(
                            security=security(cipher=cipher),
                            band=self.rng.choice([Band.BAND_2G, Band.BAND_5G]),
                        ),
                    )
                )

        def generate_wpa_test_name(params: WpaTestParams) -> str:
            band_name = _get_band_name(params.band)
            name = (
                f"test_associate_{band_name}_{params.security.uci_encryption}"
            )
            self.log.info(f"Generated test case: {name}")
            return name

        self.generate_tests(
            test_logic=self._run_wpa_test,
            name_func=generate_wpa_test_name,
            arg_sets=wpa_args,
        )

        wpa3_args: list[tuple[Wpa3TestParams]] = []
        wpa3_securities: list[
            type[SecurityWpa2Wpa3Mixed] | type[SecurityWpa3]
        ] = [
            SecurityWpa2Wpa3Mixed,
            SecurityWpa3,
        ]
        wpa3_ciphers: list[Literal["ccmp", "ccmp+tkip"]] = ["ccmp", "ccmp+tkip"]

        for security_cls in wpa3_securities:
            for cipher in wpa3_ciphers:
                wpa3_args.append(
                    (
                        Wpa3TestParams(
                            security=security_cls(cipher=cipher),
                            band=self.rng.choice([Band.BAND_2G, Band.BAND_5G]),
                        ),
                    )
                )

        def generate_wpa3_test_name(params: Wpa3TestParams) -> str:
            band_name = _get_band_name(params.band)
            name = (
                f"test_associate_{band_name}_{params.security.uci_encryption}"
            )
            self.log.info(f"Generated test case: {name}")
            return name

        self.generate_tests(
            test_logic=self._run_wpa3_test,
            name_func=generate_wpa3_test_name,
            arg_sets=wpa3_args,
        )

        boundary_args: list[tuple[BoundaryTestParams]] = []
        boundary_securities: list[SecurityWpa | SecurityWpa2 | SecurityWpa3] = [
            SecurityWpa(cipher="ccmp"),
            SecurityWpa2(cipher="ccmp"),
            SecurityWpa3(cipher="ccmp"),
        ]
        for boundary_security in boundary_securities:
            # All pure modes support ASCII password boundary validation
            boundary_args.append(
                (
                    BoundaryTestParams(
                        security=boundary_security,
                        password_type="max_length_password",
                        band=self.rng.choice([Band.BAND_2G, Band.BAND_5G]),
                    ),
                )
            )
            # Only WPA and WPA2 support raw HEX PSKs. WPA3-SAE only supports ASCII passphrases.
            if isinstance(boundary_security, (SecurityWpa, SecurityWpa2)):
                boundary_args.append(
                    (
                        BoundaryTestParams(
                            security=boundary_security,
                            password_type="max_length_psk",
                            band=self.rng.choice([Band.BAND_2G, Band.BAND_5G]),
                        ),
                    )
                )

        def generate_boundary_test_name(params: BoundaryTestParams) -> str:
            band_name = _get_band_name(params.band)
            name = f"test_associate_{band_name}_{params.password_type}_{params.security.uci_encryption}"
            self.log.info(f"Generated test case: {name}")
            return name

        self.generate_tests(
            test_logic=self._run_boundary_test,
            name_func=generate_boundary_test_name,
            arg_sets=boundary_args,
        )

        utf8_args: list[tuple[Utf8TestParams]] = []
        # Rather than testing individual human languages (which share identical byte-width classes),
        # we test representative samples of the three multi-byte UTF-8 width classes.
        # This ensures full Unicode encoding and PBKDF2 hashing coverage while minimizing test overhead.
        utf8_passwords = {
            # 2-byte UTF-8 (Latin Extended / Diacritics, covering European languages)
            "latin_2byte": "du Feÿ Château caffè",
            # 3-byte UTF-8 (CJK Ideographs / Basic Multilingual Plane)
            "cjk_3byte": "そっくりね。无线测试",
            # 4-byte UTF-8 (Emojis & Mathematical Symbols / Supplementary Multilingual Plane)
            "emoji_4byte": "𝔊𝔬𝔬𝔤𝔩𝔢_🚀🔒",
        }
        for char_class, pwd in utf8_passwords.items():
            utf8_args.append(
                (
                    Utf8TestParams(
                        security=SecurityWpa2(cipher="ccmp"),
                        char_class=char_class,
                        password=pwd,
                        band=self.rng.choice([Band.BAND_2G, Band.BAND_5G]),
                    ),
                )
            )

        def generate_utf8_test_name(params: Utf8TestParams) -> str:
            band_name = _get_band_name(params.band)
            name = f"test_associate_{band_name}_utf8_{params.char_class}_{params.security.uci_encryption}"
            self.log.info(f"Generated test case: {name}")
            return name

        self.generate_tests(
            test_logic=self._run_utf8_test,
            name_func=generate_utf8_test_name,
            arg_sets=utf8_args,
        )

    async def setup_class(self) -> None:
        await super().setup_class()
        self.log = logging.getLogger(self.__class__.__name__)

        if not self.openwrt_ap and not self.access_point:
            raise signals.TestAbortClass("Requires at least one access point")

        if self.access_point:
            self.access_point.stop_all_aps()

        await self.dut.wlan_policy.set_country_code(
            CountryCode.UNITED_STATES_OF_AMERICA
        )

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.dut.wlan_policy.ensure_clean_state()
        self.log.info(
            f"Running test case {self.current_test_info.name} via seed {self.seed}"
        )

    async def teardown_test(self) -> None:
        await self.dut.wlan_policy.ensure_clean_state()
        if self.access_point:
            self.access_point.stop_all_aps()
        await super().teardown_test()

    async def on_fail(self, record: TestResultRecord) -> None:
        await super().on_fail(record)
        if self.access_point:
            self.access_point.stop_all_aps()

    async def _run_wep_test(self, params: WepTestParams) -> None:
        """Helper to run a WEP test case with static band selection."""

        security = params.security
        key_length = params.key_length
        hex_key = params.hex_key
        band = params.band

        if hex_key:
            password = AccessPointConfig.random_hex_string(key_length).lower()
        else:
            password = AccessPointConfig.random_string(key_length)

        ssid = AccessPointConfig.random_string(SSID_LENGTH_DEFAULT)

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=BssChannel(
                            band=band,
                            number=band.default_channel,
                            phy_mode=LegacyMode(),
                        ),
                        n_capabilities=CapabilitySelection.DISABLED(),
                        ac_capabilities=CapabilitySelection.DISABLED(),
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=security,
                                password=password,
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
            band.default_channel
            legacy_security = DeprecatedSecurity(
                security_mode=SecurityMode.WEP,
                password=password,
            )
            setup_ap(
                access_point=self.access_point,
                profile_name=AP_11ABG_PROFILE_NAME,
                channel=band.default_channel,
                ssid=ssid,
                security=legacy_security,
                force_wmm=False,
                additional_ap_parameters=hostapd_constants.WEP_AUTH[
                    security.auth_mode
                ],
            )

        await self.dut.wlan_policy.save_network(
            ssid,
            SecurityType.WEP,
            target_pwd=password,
        )
        await self.dut.wlan_policy.connect(
            ssid,
            SecurityType.WEP,
        )

    async def _run_wpa_test(self, params: WpaTestParams) -> None:
        """Helper to run a WPA/WPA2 test case with static band selection."""
        band = params.band
        password = AccessPointConfig.random_string(length=10)
        ssid = AccessPointConfig.random_string(SSID_LENGTH_DEFAULT)

        security = params.security

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=band.default_bss_channel,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=security,
                                password=password,
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
            assert security.cipher is not None
            legacy_security = DeprecatedSecurity(
                security_mode=AccessPointConfigMapper.to_hostapd_security(
                    security
                ),
                password=password,
                wpa_cipher=AccessPointConfigMapper.to_hostapd_cipher(
                    security.cipher
                ),
                wpa2_cipher=AccessPointConfigMapper.to_hostapd_cipher(
                    security.cipher
                ),
            )

            setup_ap(
                access_point=self.access_point,
                profile_name=AP_11ABG_PROFILE_NAME,
                channel=band.default_channel,
                ssid=ssid,
                security=legacy_security,
                force_wmm=False,
            )

        await self.dut.wlan_policy.save_network(
            ssid,
            SecurityType.from_fidl(params.security.to_fidl_wlan_policy()),
            target_pwd=password,
        )
        await self.dut.wlan_policy.connect(
            ssid,
            SecurityType.from_fidl(params.security.to_fidl_wlan_policy()),
        )

    async def _run_wpa3_test(self, params: Wpa3TestParams) -> None:
        """Helper to run a WPA3 / Transition mode test case with static band selection."""
        band = params.band
        password = AccessPointConfig.random_string(length=10)
        ssid = AccessPointConfig.random_string(SSID_LENGTH_DEFAULT)

        security = params.security

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=band.default_bss_channel,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=security,
                                password=password,
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
            legacy_security_mode = AccessPointConfigMapper.to_hostapd_security(
                security
            )

            assert security.cipher is not None
            legacy_security = DeprecatedSecurity(
                security_mode=legacy_security_mode,
                password=password,
                wpa_cipher=AccessPointConfigMapper.to_hostapd_cipher(
                    security.cipher
                ),
                wpa2_cipher=AccessPointConfigMapper.to_hostapd_cipher(
                    security.cipher
                ),
            )

            setup_ap(
                access_point=self.access_point,
                profile_name=AP_11ABG_PROFILE_NAME,
                channel=band.default_channel,
                ssid=ssid,
                security=legacy_security,
                pmf_support=security.pmf_support,
                force_wmm=False,
            )

        await self.dut.wlan_policy.save_network(
            ssid,
            SecurityType.from_fidl(security.to_fidl_wlan_policy()),
            target_pwd=password,
        )
        await self.dut.wlan_policy.connect(
            ssid,
            SecurityType.from_fidl(security.to_fidl_wlan_policy()),
        )

    async def _run_boundary_test(self, params: BoundaryTestParams) -> None:
        """Helper to run a boundary test case (WPA/WPA2/WPA3) with static band selection."""
        band = params.band
        if params.password_type == "max_length_password":
            password = AccessPointConfig.random_string(MAX_WPA_PASSWORD_LENGTH)
        elif params.password_type == "max_length_psk":
            password = AccessPointConfig.random_hex_string(
                WPA_HEX_PSK_LENGTH
            ).lower()
        else:
            raise ValueError(f"Unknown password type: {params.password_type}")

        ssid = AccessPointConfig.random_string(SSID_LENGTH_DEFAULT)

        security = params.security

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=band.default_bss_channel,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=security,
                                password=password,
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
            legacy_security_mode = AccessPointConfigMapper.to_hostapd_security(
                security
            )

            assert security.cipher is not None
            legacy_security = DeprecatedSecurity(
                security_mode=legacy_security_mode,
                password=password,
                wpa_cipher=AccessPointConfigMapper.to_hostapd_cipher(
                    security.cipher
                ),
                wpa2_cipher=AccessPointConfigMapper.to_hostapd_cipher(
                    security.cipher
                ),
            )

            setup_ap(
                access_point=self.access_point,
                profile_name=AP_11ABG_PROFILE_NAME,
                channel=band.default_channel,
                ssid=ssid,
                security=legacy_security,
                pmf_support=security.pmf_support,
                force_wmm=False,
            )

        await self.dut.wlan_policy.save_network(
            ssid,
            SecurityType.from_fidl(security.to_fidl_wlan_policy()),
            target_pwd=password,
        )
        await self.dut.wlan_policy.connect(
            ssid,
            SecurityType.from_fidl(security.to_fidl_wlan_policy()),
        )

    async def _run_utf8_test(self, params: Utf8TestParams) -> None:
        """Helper to run multi-byte UTF-8 width class test cases (2-byte, 3-byte, 4-byte) across bands."""
        band = params.band
        password = params.password
        ssid = AccessPointConfig.random_string(SSID_LENGTH_DEFAULT)

        security = params.security

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig(
                        channel=band.default_bss_channel,
                        bss_settings=[
                            BssSettings(
                                ssid=ssid,
                                security=security,
                                password=password,
                            )
                        ],
                    )
                ]
            )
            self.openwrt_ap.configure_wifi(config)
        elif self.access_point:
            legacy_security_mode = AccessPointConfigMapper.to_hostapd_security(
                security
            )

            assert security.cipher is not None
            legacy_security = DeprecatedSecurity(
                security_mode=legacy_security_mode,
                password=password,
                wpa_cipher=AccessPointConfigMapper.to_hostapd_cipher(
                    security.cipher
                ),
                wpa2_cipher=AccessPointConfigMapper.to_hostapd_cipher(
                    security.cipher
                ),
            )

            setup_ap(
                access_point=self.access_point,
                profile_name=AP_11ABG_PROFILE_NAME,
                channel=band.default_channel,
                ssid=ssid,
                security=legacy_security,
                pmf_support=security.pmf_support,
                force_wmm=False,
            )

        await self.dut.wlan_policy.save_network(
            ssid,
            SecurityType.from_fidl(security.to_fidl_wlan_policy()),
            target_pwd=password,
        )
        await self.dut.wlan_policy.connect(
            ssid,
            SecurityType.from_fidl(security.to_fidl_wlan_policy()),
        )


if __name__ == "__main__":
    test_runner.main()
