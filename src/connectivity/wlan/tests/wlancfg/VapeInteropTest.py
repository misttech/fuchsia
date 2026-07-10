#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import fidl_fuchsia_wlan_policy as f_wlan_policy
import fuchsia_wlan_base_test
from antlion.controllers.access_point import AccessPoint, setup_ap
from antlion.controllers.ap_lib import hostapd_constants
from antlion.controllers.ap_lib.hostapd_security import Security, SecurityMode
from honeydew.affordances.connectivity.wlan.utils.types import CountryCode
from mobly import signals, test_runner
from openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    BssChannel,
    HtMode,
    LegacyMode,
    SecurityOpen,
    SecurityWpa2,
    VhtMode,
)
from openwrt_access_point.lib.profiles import (
    actiontec,
    asus,
    belkin,
    linksys,
    netgear,
    securifi,
    tplink,
)


class VapeInteropTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """Tests interoperability with mock third party AP profiles.

    Test Bed Requirement:
    * One Android or Fuchsia Device
    * One Whirlwind Access Point
    """

    access_point: AccessPoint

    async def setup_class(self) -> None:
        await super().setup_class()

        if not self.openwrt_ap and not self.access_point:
            raise signals.TestAbortClass("Requires at least one access point")

        await self.dut.wlan_policy.set_country_code(
            CountryCode.UNITED_STATES_OF_AMERICA
        )

        # Same for both 2g and 5g
        self.ssid = AccessPointConfig.random_string(
            hostapd_constants.AP_SSID_LENGTH_2G
        )
        self.password = AccessPointConfig.random_string(
            hostapd_constants.AP_PASSPHRASE_LENGTH_2G
        )
        self.security_profile_wpa2 = Security(
            security_mode=SecurityMode.WPA2,
            password=self.password,
            wpa2_cipher=hostapd_constants.WPA2_DEFAULT_CIPER,
        )

        if self.access_point:
            self.access_point.stop_all_aps()

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.dut.wlan_policy.ensure_clean_state()

    async def teardown_test(self) -> None:
        await self.dut.wlan_policy.ensure_clean_state()
        if self.access_point:
            self.access_point.stop_all_aps()
        await super().teardown_test()

    async def test_associate_actiontec_pk5000_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = actiontec.actiontec_pk5000(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    LegacyMode(),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="actiontec_pk5000",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_actiontec_pk5000_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = actiontec.actiontec_pk5000(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    LegacyMode(),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="actiontec_pk5000",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_actiontec_mi424wr_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = actiontec.actiontec_mi424wr(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="actiontec_mi424wr",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_actiontec_mi424wr_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = actiontec.actiontec_mi424wr(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="actiontec_mi424wr",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_asus_rtac66u_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtac66u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtac66u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_asus_rtac66u_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtac66u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtac66u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_asus_rtac66u_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtac66u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtac66u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_asus_rtac66u_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtac66u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtac66u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_asus_rtac86u_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtac86u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtac86u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_asus_rtac86u_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtac86u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtac86u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_asus_rtac86u_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtac86u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtac86u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_asus_rtac86u_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtac86u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtac86u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_asus_rtac5300_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtac5300(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtac5300",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_asus_rtac5300_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtac5300(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtac5300",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_asus_rtac5300_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtac5300(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtac5300",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_asus_rtac5300_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtac5300(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtac5300",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_asus_rtn56u_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtn56u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtn56u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_asus_rtn56u_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtn56u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtn56u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_asus_rtn56u_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtn56u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtn56u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_asus_rtn56u_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtn56u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtn56u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_asus_rtn66u_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtn66u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtn66u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_asus_rtn66u_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtn66u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtn66u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_asus_rtn66u_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtn66u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtn66u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_asus_rtn66u_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = asus.asus_rtn66u(
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="asus_rtn66u",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_belkin_f9k1001v5_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = belkin.belkin_f9k1001v5(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="belkin_f9k1001v5",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_belkin_f9k1001v5_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = belkin.belkin_f9k1001v5(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="belkin_f9k1001v5",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_linksys_ea4500_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = linksys.linksys_ea4500(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=40, extension="+"),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="linksys_ea4500",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_linksys_ea4500_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = linksys.linksys_ea4500(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=40, extension="+"),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="linksys_ea4500",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_linksys_ea4500_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = linksys.linksys_ea4500(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="linksys_ea4500",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_linksys_ea4500_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = linksys.linksys_ea4500(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="linksys_ea4500",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_linksys_ea9500_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = linksys.linksys_ea9500(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    LegacyMode(),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="linksys_ea9500",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_linksys_ea9500_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = linksys.linksys_ea9500(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    LegacyMode(),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="linksys_ea9500",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_linksys_ea9500_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = linksys.linksys_ea9500(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    LegacyMode(),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="linksys_ea9500",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_linksys_ea9500_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = linksys.linksys_ea9500(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    LegacyMode(),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="linksys_ea9500",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_linksys_wrt1900acv2_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = linksys.linksys_wrt1900acv2(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="linksys_wrt1900acv2",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_linksys_wrt1900acv2_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = linksys.linksys_wrt1900acv2(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="linksys_wrt1900acv2",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_linksys_wrt1900acv2_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = linksys.linksys_wrt1900acv2(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    VhtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="linksys_wrt1900acv2",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_linksys_wrt1900acv2_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = linksys.linksys_wrt1900acv2(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    VhtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="linksys_wrt1900acv2",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_netgear_r7000_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = netgear.netgear_r7000(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="netgear_r7000",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_netgear_r7000_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = netgear.netgear_r7000(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="netgear_r7000",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_netgear_r7000_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = netgear.netgear_r7000(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    VhtMode(bw=80),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="netgear_r7000",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_netgear_r7000_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = netgear.netgear_r7000(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    VhtMode(bw=80),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="netgear_r7000",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_netgear_wndr3400_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = netgear.netgear_wndr3400(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="netgear_wndr3400",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_netgear_wndr3400_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = netgear.netgear_wndr3400(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="netgear_wndr3400",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_netgear_wndr3400_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = netgear.netgear_wndr3400(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    HtMode(bw=40, extension="+"),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="netgear_wndr3400",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_netgear_wndr3400_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = netgear.netgear_wndr3400(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    HtMode(bw=40, extension="+"),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )
            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="netgear_wndr3400",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_securifi_almond_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = securifi.securifi_almond(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="securifi_almond",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_securifi_almond_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = securifi.securifi_almond(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="securifi_almond",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_tplink_archerc5_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_archerc5(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_archerc5",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_tplink_archerc5_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_archerc5(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_archerc5",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_tplink_archerc5_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_archerc5(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    VhtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_archerc5",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_tplink_archerc5_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_archerc5(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    VhtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_archerc5",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_tplink_archerc7_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_archerc7(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_archerc7",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_tplink_archerc7_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_archerc7(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_archerc7",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_tplink_archerc7_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_archerc7(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    VhtMode(bw=80),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_archerc7",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_tplink_archerc7_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_archerc7(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    VhtMode(bw=80),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_archerc7",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )

        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_tplink_c1200_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_c1200(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_c1200",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_tplink_c1200_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_c1200(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_c1200",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_tplink_c1200_5ghz_open(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_c1200(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    VhtMode(bw=80),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_c1200",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_tplink_c1200_5ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_c1200(
                channel=BssChannel(
                    Band.BAND_5G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                    VhtMode(bw=80),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_c1200",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_5G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )

    async def test_associate_tplink_tlwr940n_24ghz_open(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_tlwr940n(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityOpen(),
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_tlwr940n",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )
        await self.dut.wlan_policy.connect(
            self.ssid, f_wlan_policy.SecurityType.NONE
        )

    async def test_associate_tplink_tlwr940n_24ghz_wpa2(self) -> None:
        if self.openwrt_ap:
            config = tplink.tplink_tlwr940n(
                channel=BssChannel(
                    Band.BAND_2G,
                    hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                    HtMode(bw=20),
                ),
                ssid=self.ssid,
                security=SecurityWpa2(),
                password=self.password,
            )

            self.openwrt_ap.configure_wifi(config)
        else:
            setup_ap(
                access_point=self.access_point,
                profile_name="tplink_tlwr940n",
                channel=hostapd_constants.AP_DEFAULT_CHANNEL_2G,
                ssid=self.ssid,
                security=self.security_profile_wpa2,
            )
        await self.dut.wlan_policy.save_network(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
            target_pwd=self.password,
        )
        await self.dut.wlan_policy.connect(
            self.ssid,
            f_wlan_policy.SecurityType.WPA2,
        )


if __name__ == "__main__":
    test_runner.main()
