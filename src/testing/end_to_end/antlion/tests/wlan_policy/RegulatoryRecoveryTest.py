#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from antlion.controllers.fuchsia_device import FuchsiaDevice
from antlion.controllers.fuchsia_lib.lib_controllers.wlan_policy_controller import (
    WlanPolicyControllerError,
)
from antlion.test_utils.wifi import base_test
from honeydew.affordances.connectivity.wlan.utils.types import (
    ConnectivityMode,
    CountryCode,
    OperatingBand,
    SecurityType,
    WlanClientState,
)
from mobly import asserts, signals, test_runner


class RegulatoryRecoveryTest(base_test.WifiBaseTest):
    """Tests the policy layer's response to setting country code.

    Test Bed Requirements:
    * One Fuchsia device that is capable of operating as a WLAN client and AP.

    Example Config:
    "regulatory_recovery_test_params": {
        "country_code": "US"
    }

    If no configuration information is provided, the test will default to
    toggling between WW and US.
    """

    def setup_class(self) -> None:
        super().setup_class()
        if len(self.fuchsia_devices) < 1:
            raise EnvironmentError("No Fuchsia devices found.")

        self.config_test_params = self.user_params.get(
            "regulatory_recovery_test_params", {}
        )
        self.country_code = self.config_test_params.get("country_code", "US")
        self.negative_test = self.config_test_params.get("negative_test", False)

        for fd in self.fuchsia_devices:
            fd.configure_wlan(association_mechanism="policy")

    def teardown_class(self) -> None:
        if not self.negative_test:
            for fd in self.fuchsia_devices:
                fd.wlan_controller.set_country_code(self.country_code)

        super().teardown_class()

    def setup_test(self) -> None:
        """Set PHYs to world-wide mode and disable AP and client connections."""
        for fd in self.fuchsia_devices:
            fd.wlan_controller.set_country_code(CountryCode.WORLDWIDE)
            fd.honeydew_fd.wlan_policy_ap.stop_all()

    def _set_country_code_check(self, fd: FuchsiaDevice) -> None:
        """Set the country code and check if successful.

        Args:
            fd: Fuchsia device to set country code on.

        Raises:
            EnvironmentError on failure to set country code or success setting country
                code when it should be a failure case.
        """
        try:
            fd.wlan_controller.set_country_code(self.country_code)
        except EnvironmentError as e:
            if self.negative_test:
                # In the negative case, setting the country code for an
                # invalid country should fail.
                pass
            else:
                # If this is not a negative test case, re-raise the
                # exception.
                raise e
        else:
            # The negative test case should have failed to set the country
            # code and the positive test case should succeed.
            if self.negative_test:
                raise EnvironmentError(
                    "Setting invalid country code succeeded."
                )
            else:
                pass

    def test_interfaces_not_recreated_when_initially_disabled(self) -> None:
        """Test after applying new region no new interfaces are automatically recreated.

        We start with client connections and access points disabled. There should be no
        state change after applying a new regulatory region.

        Raises:
            TestFailure if client or AP are in unexpected state.
        """
        for fd in self.fuchsia_devices:
            fd.honeydew_fd.wlan_policy.stop_client_connections()
            fd.wlan_policy_controller.wait_for_client_state(
                WlanClientState.CONNECTIONS_DISABLED
            )

            self._set_country_code_check(fd)

            # Verify that the client is still stopped.
            try:
                fd.wlan_policy_controller.wait_for_client_state(
                    WlanClientState.CONNECTIONS_DISABLED
                )
            except WlanPolicyControllerError:
                raise signals.TestFailure(
                    "Client policy layer is in unexpected state"
                )

            # Verify that the AP is still stopped.
            fd.honeydew_fd.wlan_policy_ap.set_new_update_listener()
            ap_updates = fd.honeydew_fd.wlan_policy_ap.get_update()
            if ap_updates:
                raise signals.TestFailure(
                    f"AP in unexpected state: {ap_updates}"
                )

    def test_interfaces_recreated_when_initially_enabled(self) -> None:
        """Test after applying new region interfaces are automatically recreated.

        After enabling client connections and access points we check that all interfaces
        are recreated.

        Raises:
            TestFailure if client or AP are in unexpected state.
        """
        test_ssid = "test_ssid"
        security_type = SecurityType.NONE
        for fd in self.fuchsia_devices:
            # Start client connections and start an AP before setting the country code.
            fd.honeydew_fd.wlan_policy.start_client_connections()
            fd.wlan_policy_controller.wait_for_client_state(
                WlanClientState.CONNECTIONS_ENABLED
            )
            fd.honeydew_fd.wlan_policy_ap.start(
                test_ssid,
                security_type,
                None,
                ConnectivityMode.LOCAL_ONLY,
                OperatingBand.ANY,
            )

            # Set the country code.
            self._set_country_code_check(fd)

            # Verify that the client connections are enabled.
            try:
                fd.wlan_policy_controller.wait_for_client_state(
                    WlanClientState.CONNECTIONS_ENABLED
                )
            except WlanPolicyControllerError:
                raise signals.TestFailure(
                    "Client policy layer is in unexpected state"
                )

            # Verify that the AP is brought up again.
            fd.honeydew_fd.wlan_policy_ap.set_new_update_listener()
            ap_updates = fd.honeydew_fd.wlan_policy_ap.get_update()
            if len(ap_updates) != 1:
                raise signals.TestFailure(f"No APs are running: {ap_updates}")
            else:
                asserts.assert_equal(
                    ap_updates[0].id_.ssid, test_ssid, "Wrong ssid", ap_updates
                )
                asserts.assert_equal(
                    ap_updates[0].id_.security_type,
                    security_type,
                    "Wrong security type",
                    ap_updates,
                )


if __name__ == "__main__":
    test_runner.main()
