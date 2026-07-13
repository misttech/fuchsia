# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging

logger = logging.getLogger(__name__)

import fidl_fuchsia_location_namedplace as f_location_namedplace
import fidl_fuchsia_wlan_policy as f_wlan_policy
import fuchsia_wlan_base_test
from honeydew.affordances.connectivity.wlan.utils import errors as wlan_errors
from honeydew.affordances.connectivity.wlan.utils.types import (
    CountryCode,
    OperatingBand,
)
from honeydew.typing.custom_types import FidlEndpoint
from mobly import asserts, signals, test_runner


class RegulatoryRecoveryTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    async def setup_class(self) -> None:
        await super().setup_class()

        regulatory_region_watcher = (
            f_location_namedplace.RegulatoryRegionWatcherClient(
                self.dut.fuchsia_controller.connect_device_proxy(
                    FidlEndpoint(
                        "core/regulatory_region",
                        "fuchsia.location.namedplace.RegulatoryRegionWatcher",
                    )
                )
            )
        )
        get_region_update_response = (
            await regulatory_region_watcher.get_region_update()
        )

        # If no region was set before this test runs, then the result could be None.
        # In that case, the only reasonable choice is to set the region to worldwide.
        if get_region_update_response.new_region is None:
            await self.dut.wlan_policy.set_country_code(CountryCode.WORLDWIDE)
            self.before_test_country_code = CountryCode.WORLDWIDE
        else:
            self.before_test_country_code = CountryCode(
                get_region_update_response.new_region
            )
        logger.info(
            f"Country code before tests is {self.before_test_country_code}."
        )

        await self.dut.wlan_policy.start_client_connections()
        self.device_supports_ap = True
        try:
            await self.dut.wlan_policy_ap.start(
                "test_ssid",
                f_wlan_policy.SecurityType.NONE,
                None,
                f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
                OperatingBand.ANY,
            )
            await self.dut.wlan_policy_ap.stop_all()
        except wlan_errors.HoneydewWlanError:
            logger.info(
                "Detected this device does not support an access point interface."
            )
            self.device_supports_ap = False
        else:
            logger.info(
                "Detected this device supports an access point interface."
            )
            self.device_supports_ap = True

    async def teardown_class(self) -> None:
        logger.info(
            f"Finishing test suite by setting country code back to {self.before_test_country_code}..."
        )
        await self.dut.wlan_policy.set_country_code(
            self.before_test_country_code
        )
        await super().teardown_class()

    async def test_interfaces_not_recreated_when_initially_disabled(
        self,
    ) -> None:
        """Test no interfaces created after applying a new country code."""

        # With the country code set to US, destroy all interfaces
        await self.dut.wlan_policy.set_country_code(CountryCode("US"))
        await self.dut.wlan_policy.stop_client_connections(
            wait_for_confirmation=True
        )
        if self.device_supports_ap:
            await self.dut.wlan_policy_ap.stop_all()

        # Change the country code while all interfaces are destroyed
        await self.dut.wlan_policy.set_country_code(CountryCode("AU"))

        # Verify changing the country code does not create interfaces
        await self.dut.wlan_policy.wait_for_client_state(
            f_wlan_policy.WlanClientState.CONNECTIONS_DISABLED
        )

        if self.device_supports_ap:
            await self.dut.wlan_policy_ap.set_new_update_listener()
            ap_updates = await self.dut.wlan_policy_ap.get_update()
            if ap_updates:
                raise signals.TestFailure(
                    f"AP in unexpected state: {ap_updates}"
                )

    async def test_interfaces_recreated_when_initially_enabled(self) -> None:
        """Test client and AP interfaces are automatically recreated after applying a new country code."""

        # With the country code set to US, create interfaces.
        await self.dut.wlan_policy.set_country_code(CountryCode("US"))
        await self.dut.wlan_policy.start_client_connections()
        await self.dut.wlan_policy.wait_for_client_state(
            f_wlan_policy.WlanClientState.CONNECTIONS_ENABLED
        )
        if self.device_supports_ap:
            await self.dut.wlan_policy_ap.start(
                "test_ssid",
                f_wlan_policy.SecurityType.NONE,
                None,
                f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
                OperatingBand.ANY,
            )

        # Change the country code while interfaces are up.
        await self.dut.wlan_policy.set_country_code(CountryCode("AU"))

        # Verify changing the country code cycles the client back to enabled.
        await self.dut.wlan_policy.wait_for_client_state(
            f_wlan_policy.WlanClientState.CONNECTIONS_ENABLED
        )

        # Don't reset the update listener so that this verifies
        # changing the country code recreates the interfaces.
        if self.device_supports_ap:
            await self.dut.wlan_policy_ap.set_new_update_listener()
            ap_updates = await self.dut.wlan_policy_ap.get_update()
            if len(ap_updates) != 1:
                raise signals.TestFailure(f"No APs are running: {ap_updates}")
            asserts.assert_equal(
                ap_updates[0].id_.ssid, "test_ssid", "Wrong ssid", ap_updates
            )
            asserts.assert_equal(
                ap_updates[0].id_.security_type,
                f_wlan_policy.SecurityType.NONE,
                "Wrong security type",
                ap_updates,
            )


if __name__ == "__main__":
    test_runner.main()
