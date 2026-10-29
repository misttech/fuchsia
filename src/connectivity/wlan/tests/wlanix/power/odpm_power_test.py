# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""E2E Power Measurement Tests.

**Prerequisites:**
1. The target device must have On-Device Power Monitor (ODPM) hardware. These tests will fail immediately if ODPM is unavailable.
2. The WLAN chip must be on its own isolated power rail. This ensures that ODPM can isolate and measure the actual WLAN chip's power usage without interference from other components.

This test suite measures power usage using ODPM during various WLAN use cases. The primary goal is to evaluate the power profile
of device states, including Wi-Fi chip off, idle disconnected, active connections
with different levels of traffic, suspend mode, scanning, and firmware power saving modes.
"""

import asyncio
import logging
from typing import Any, List

import fidl_fuchsia_wlan_wlanix as fidl_wlanix
import fuchsia_wlan_base_test
import wlanix_testing
from legacy_access_point.access_point import AccessPoint, setup_ap
from legacy_access_point.ap_lib.hostapd_security import (
    Security as DeprecatedSecurity,
)
from mobly import signals, test_runner
from mobly.asserts import abort_class, assert_equal, assert_less, assert_true
from openwrt_access_point import OpenWrtAP
from openwrt_access_point.lib.access_point_config import (
    AccessPointConfig,
    Band,
    BssChannel,
    BssSettings,
    HtMode,
    RadioConfig,
    SecurityWpa2,
)
from openwrt_access_point.lib.access_point_config_mapper import (
    AccessPointConfigMapper as ConfigMapper,
)
from wlan_test_utils import odpm_utils
from wlanix_testing import SupplicantStaIfaceCallbackServer

_LOGGER = logging.getLogger(__name__)

# ==============================================================================
# TEST CONFIGURATION & CONSTANTS
# ==============================================================================
# Power thresholds for various tested states
# TODO(b/576709859): These are currently overly safe thresholds to be adjusted in the future.
BUDGET_ACTIVE_IDLE_W = 50.0
BUDGET_SUSPEND_W = 50.0
BUDGET_WLAN_TX_W = 50.0

# Durations for test scenarios and power sampling (in seconds)
DURATION_ACTIVE_IDLE_SEC = 30
DURATION_SUSPEND_SEC = 45
DURATION_WLAN_TX_SEC = 30

# Advanced timings
# Time to wait for wlanix association to complete
DURATION_WLANIX_CONNECT_WAIT_SEC = 5

# ==============================================================================


class OdpmPowerTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """Test suite for validating power states via wlanix and ODPM."""

    wifi_chip_proxy: fidl_wlanix.WifiChipClient
    wlanix_proxy: fidl_wlanix.WlanixClient
    nl80211_proxy: fidl_wlanix.Nl80211Client
    iface_name: str
    wlanix: "wlanix_testing.Wlanix"
    chip: "wlanix_testing.WifiChip"
    iface: "wlanix_testing.StaIface"

    def _set_display_power(self, power_on: bool) -> None:
        """Helper to power the display panel on or off."""
        state = "on" if power_on else "off"
        try:
            self.dut.ffx.run_ssh_cmd(f"display-tweak panel --power {state}")
        except Exception as e:
            _LOGGER.warning(f"Failed to set display power {state}: {e}")

    async def setup_class(self) -> None:
        """Initialize the DUT, Wlanix, and Tracing/ODPM."""
        await super().setup_class()

        if not hasattr(self.dut, "tracing"):
            abort_class(
                "Tracing affordance is missing; required for ODPM power metrics."
            )
        self.wlanix = wlanix_testing.Wlanix(self.dut)
        self.chip = await self.wlanix.ensure_single_chip()

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.chip.destroy_all_sta_ifaces()
        self.iface = await self.chip.create_sta_iface()
        self.wlanix_proxy = self.wlanix.proxy
        self.wifi_chip_proxy = self.chip.proxy
        self.nl80211_proxy = self.iface.nl80211.proxy

    async def teardown_class(self) -> None:
        """Global cleanup to restore baseline state."""
        self._set_display_power(power_on=True)
        await super().teardown_class()

    async def teardown_test(self) -> None:
        """Ensure tracing is stopped if a test crashes."""
        if getattr(self, "dut", None) and getattr(self.dut, "tracing", None):
            if self.dut.tracing.is_active():
                await self.dut.tracing.stop()
        await super().teardown_test()

    def _assert_power_budget(
        self, metrics_results: List[Any], budget_w: float
    ) -> None:
        """Helper to extract mean power from ODPM results and assert against budget."""
        for r in metrics_results:
            if getattr(r, "label", None) == "Power_wlan":
                power_result = r
                break
        else:
            power_result = None
        if not power_result or not power_result.values:
            raise signals.TestFailure("No power data collected from ODPM.")

        samples = power_result.values

        avg_power = sum(samples) / len(samples)
        _LOGGER.info(f"Calculated Mean Power: {avg_power:.3f} W")
        assert_less(
            avg_power,
            budget_w,
            f"Mean power ({avg_power:.3f}W) exceeded budget ({budget_w}W)",
        )

    async def _connect_via_wlanix(
        self, ssid: str, password: str | None = None
    ) -> None:
        """Helper to establish a connection directly via wlanix (Supplicant API)."""
        _LOGGER.info(f"Connecting to {ssid} via wlanix Supplicant API...")

        client, server = self.dut.fuchsia_controller.channel_create()
        async with SupplicantStaIfaceCallbackServer(client, server) as ctx:
            state_change_queue = ctx.state_change_queue
            self.iface.supplicant_proxy.register_callback(  # type: ignore
                callback=ctx.callback_channel.take()
            )

            (proxy, server) = self.dut.fuchsia_controller.channel_create()
            self.iface.supplicant_proxy.add_network(network=server.take())  # type: ignore
            supplicant_sta_network_proxy = (
                fidl_wlanix.SupplicantStaNetworkClient(proxy)
            )

            supplicant_sta_network_proxy.set_ssid(
                ssid=list(ssid.encode("ascii"))
            )

            if password:
                supplicant_sta_network_proxy.set_psk_passphrase(
                    passphrase=list(password.encode("ascii"))
                )

            try:
                (await supplicant_sta_network_proxy.select()).unwrap()
            except AssertionError as e:
                raise signals.TestFailure(
                    f'Failed to connect to "{ssid}"'
                ) from e

            state_change = await state_change_queue.get()
            assert isinstance(
                state_change,
                fidl_wlanix.SupplicantStaIfaceCallbackOnStateChangedRequest,
            ), f"Expected OnStateChanged. Got {state_change!r}"
            assert_equal(
                state_change.new_state,
                fidl_wlanix.StaIfaceCallbackState.COMPLETED,
            )
            assert_true(
                state_change_queue.empty(),
                "Unexpectedly received additional callback messages.",
            )

        _LOGGER.info(f"Successfully connected to {ssid} via wlanix.")

    def _setup_ap_and_get_credentials(self) -> tuple[str, str]:
        """Configure the AP and return (ssid, password)."""
        ap = self.access_point
        ssid = AccessPointConfig.random_string(12)
        password = AccessPointConfig.random_string(15)
        security = SecurityWpa2()

        if isinstance(ap, OpenWrtAP):
            ap.configure_wifi(
                AccessPointConfig(
                    radios=[
                        RadioConfig(
                            channel=BssChannel(Band.BAND_5G, 36, HtMode(bw=20)),
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
        elif isinstance(ap, AccessPoint):
            setup_ap(
                access_point=ap,
                profile_name="whirlwind",
                channel=36,
                ssid=ssid,
                security=DeprecatedSecurity(
                    security_mode=ConfigMapper.to_hostapd_security(security),
                    password=password,
                ),
            )
        else:
            raise signals.TestSkip("Unknown AP type.")
        return ssid, password

    async def test_wifi_off_power(self) -> None:
        """Measure power impact when Wi-Fi is completely powered down.

        This tests the absolute minimum power state when the device is awake,
        but the WLAN chip is completely powered off via the Wlanix HAL.
        """
        self._set_display_power(power_on=False)

        # Power down the Wi-Fi chip completely
        try:
            (
                await self.wifi_chip_proxy.remove_sta_iface(
                    iface_name=self.iface_name
                )
            ).unwrap()
        except Exception:
            pass

        proxy, server = self.dut.fuchsia_controller.channel_create()
        self.wlanix_proxy.get_wifi(wifi=server.take())
        wifi_proxy = fidl_wlanix.WifiClient(proxy)
        (await wifi_proxy.stop()).unwrap()

        try:
            # Start tracing
            await odpm_utils.start_trace(self.dut.tracing)
            _LOGGER.info(
                "ODPM trace started, waiting for steady state power measurement..."
            )

            # Spend enough time in the state to get power ODPM data
            await asyncio.sleep(DURATION_ACTIVE_IDLE_SEC)

            # Stop ODPM trace and process metrics
            results = await odpm_utils.stop_trace_and_save_metrics(
                tracing=self.dut.tracing,
                test_case_path=self.log_path,
                test_name=self.current_test_info.name,
                rail=odpm_utils.ODPM_WLAN_RAIL,
            )

            self._assert_power_budget(results, BUDGET_ACTIVE_IDLE_W)
        finally:
            # Restart for teardown
            (await wifi_proxy.start()).unwrap()


if __name__ == "__main__":
    test_runner.main()
