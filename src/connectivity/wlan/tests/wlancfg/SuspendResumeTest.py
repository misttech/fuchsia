# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
from datetime import timedelta

import fidl_fuchsia_wlan_policy as f_wlan_policy
import fuchsia_wlan_base_test
from honeydew.affordances.connectivity.netstack.types import PortClass
from honeydew.fuchsia_device.fuchsia_device import FuchsiaDevice
from honeydew.transports.ffx import types as ffx_types
from honeydew.utils import power
from honeydew.utils.deadline import Deadline
from legacy_access_point.access_point import setup_ap
from legacy_access_point.ap_lib import hostapd_constants
from legacy_access_point.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from mobly import asserts, signals, test_runner
from openwrt_access_point import AddrType as OpenWrtAddrType
from openwrt_access_point import InterfaceName as OpenWrtInterfaceName
from openwrt_access_point.lib.access_point_config import (
    DEFAULT_2G_CHANNEL,
    AccessPointConfig,
    Band,
    BssSettings,
    RadioConfig,
    SecurityWpa2,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)

logger = logging.getLogger(__name__)

SUSPEND_DURATION = timedelta(seconds=30)


class SuspendResumeConnectionTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """Tests that active WLAN connections survive system suspend/resume cycles.

    Testbed Requirements:
    * One Fuchsia device capable of USB suspend/resume
    * One Access Point (OpenWrt or legacy AP)
    """

    def _set_display_power(self, device: FuchsiaDevice, power_on: bool) -> None:
        """Power on/off the display panel."""
        state = "on" if power_on else "off"
        logger.info(
            "Setting display panel power to %s on %s...",
            state,
            device.device_name,
        )
        try:
            device.ffx.run_ssh_cmd(f"display-tweak panel --power {state}")
        except Exception as e:  # pylint: disable=broad-except
            logger.warning("Failed to power %s display panel: %s", state, e)

    def _set_battery_manager_running(
        self, device: FuchsiaDevice, running: bool
    ) -> None:
        """Start or stop `/core/battery_manager` around suspend.

        On workbench configurations with a battery, `/core/battery_manager`
        holds a `charging_block_suspension` wake lease that prevents SAG from
        entering suspend if charger/fuel-gauge disconnect updates are not yet
        propagated.
        """
        action = "start" if running else "stop"
        logger.info(
            "Running `ffx component %s /core/battery_manager` on %s...",
            action,
            device.device_name,
        )
        try:
            device.ffx.run(
                ["component", action, "/core/battery_manager"],
                machine=ffx_types.MachineFormat.RAW,
            )
        except Exception as e:  # pylint: disable=broad-except
            logger.warning("Failed to %s /core/battery_manager: %s", action, e)

    async def setup_class(self) -> None:
        await super().setup_class()

        if not self.openwrt_ap and not self.access_point:
            raise signals.TestAbortClass("Requires at least one access point.")

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

    async def test_suspend_resume(self) -> None:
        # Start AP
        ssid = AccessPointConfig.random_string(
            hostapd_constants.AP_SSID_LENGTH_2G
        )
        password = AccessPointConfig.random_string(
            hostapd_constants.AP_PASSPHRASE_LENGTH_2G
        )
        security = SecurityWpa2()
        security_type = security.to_fidl_wlan_policy()

        if self.openwrt_ap:
            config = AccessPointConfig(
                radios=[
                    RadioConfig.generate(
                        channel=DEFAULT_2G_CHANNEL,
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
            ap_address = self.openwrt_ap.get_addr(
                OpenWrtInterfaceName.lan,
                OpenWrtAddrType.ipv4_private,
            )
        elif self.access_point:
            hostapd_band = ConfigMapper.to_hostapd_band(Band.BAND_2G)
            hostapd_security = ConfigMapper.to_hostapd_security(security)
            self.access_point.stop_all_aps()
            setup_ap(
                access_point=self.access_point,
                profile_name="whirlwind",
                channel=hostapd_band.default_channel(),
                ssid=ssid,
                security=DeprecatedSecurity(
                    security_mode=hostapd_security,
                    password=password,
                ),
            )
            ap_address = self.access_point.get_addr(
                self.access_point.wlan_2g,
            )
        else:
            raise signals.TestAbortClass("Requires at least one access point.")
        logger.info(
            "Connecting DUT to AP (SSID: %s, Security: %s)...",
            ssid,
            security_type,
        )

        # Connect to AP
        await self.dut.wlan_policy.save_network(ssid, security_type, password)
        await self.dut.wlan_policy.connect(ssid, security_type)
        await self.dut.wlan_policy.wait_for_network_state(
            ssid, f_wlan_policy.ConnectionState.CONNECTED
        )
        logger.info("DUT successfully connected to %s.", ssid)

        # Ensure the update listener has no updates before the resume
        with asserts.assert_raises(TimeoutError):
            await self.dut.wlan_policy.get_update(
                timeout=timedelta(seconds=5).total_seconds()
            )

        # Suspend the device and verify resume occurred
        logger.info(
            "Suspending device for %s seconds...",
            int(SUSPEND_DURATION.total_seconds()),
        )
        # TODO(https://fxbug.dev/519249679): Find a better way to
        # separate out product-specific test logic.
        #
        # On workbench, the display panel must be manually powered off
        # before suspend. Otherwise, the device will not suspend.
        self._set_display_power(self.dut, power_on=False)
        self._set_battery_manager_running(self.dut, running=False)
        try:
            await power.suspend_resume(
                self.dut,
                deadline=Deadline.from_timeout(timedelta(minutes=2)),
                base_idle_duration=SUSPEND_DURATION,
            )
        except Exception as err:
            logger.error("Failed to execute suspend/resume: %s", err)
            raise
        finally:
            self._set_battery_manager_running(self.dut, running=True)
            # On workbench, the display panel must be manually powered on after
            # resume.
            self._set_display_power(self.dut, power_on=True)

        logger.info("Device resumed from suspend successfully.")

        # Confirm connection is still connected
        await self.dut.wlan_policy.wait_for_network_state(
            ssid, f_wlan_policy.ConnectionState.CONNECTED
        )
        logger.info(
            "Verified WLAN connection to %s remained connected across suspend/resume.",
            ssid,
        )

        wlan_interface = await self.dut.netstack.wait_for_interface(
            PortClass.WLAN_CLIENT
        )
        await self.dut.netstack.wait_for_ipv4_addr(wlan_interface.id_)
        logger.info("Attempting to ping %s...", ap_address)
        ping_result = await self.dut.netstack.ping(ap_address)
        asserts.assert_true(
            ping_result.any_pings_received,
            f"Failed to ping {ap_address}: {ping_result.raw_output}",
        )
        logger.info("Ping to %s succeeded.", ap_address)


if __name__ == "__main__":
    test_runner.main()
