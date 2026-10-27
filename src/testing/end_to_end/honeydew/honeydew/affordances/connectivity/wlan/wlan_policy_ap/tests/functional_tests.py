# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for WLAN policy access point affordance."""

import random
import string

import fidl_fuchsia_wlan_policy as f_wlan_policy
import fuchsia_wlan_base_test
from honeydew.affordances.connectivity.netstack.types import PortClass
from mobly import asserts, test_runner


def random_str(
    size: int = 6, chars: str = string.ascii_lowercase + string.digits
) -> str:
    """Generate a random string.

    Args:
        size: Length of output string
        chars: Characters to use

    Returns:
        A random string of length size using the characters in chars.
    """
    return "".join(random.choice(chars) for _ in range(size))


class WlanPolicyApTests(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """WlanPolicyAp affordance tests"""

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()

        await self.wait_for_interface(self.dut.netstack, PortClass.WLAN_CLIENT)

    async def teardown_test(self) -> None:
        # Don't allow access points to leak into other tests.
        await self.dut.wlan_policy_ap.stop_all()
        await super().teardown_test()

    async def test_ap_methods(self) -> None:
        """Verify WLAN policy access point methods."""
        await self.dut.wlan_policy_ap.stop_all()
        await self.dut.wlan_policy_ap.set_new_update_listener()
        asserts.assert_equal(
            await self.dut.wlan_policy_ap.get_update(),
            [],
        )

        test_ssid = random_str()
        await self.dut.wlan_policy_ap.start(
            test_ssid,
            f_wlan_policy.SecurityType.NONE,
            None,
            f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
            f_wlan_policy.OperatingBand.ONLY_2_4_GHZ,
        )
        asserts.assert_equal(
            await self.dut.wlan_policy_ap.get_update(),
            [
                f_wlan_policy.AccessPointState(
                    state=f_wlan_policy.OperatingState.STARTING,
                    mode=f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
                    band=f_wlan_policy.OperatingBand.ONLY_2_4_GHZ,
                    frequency=None,
                    clients=None,
                    id_=f_wlan_policy.NetworkIdentifier(
                        ssid=list(test_ssid.encode("utf-8")),
                        type_=f_wlan_policy.SecurityType.NONE,
                    ),
                )
            ],
        )
        asserts.assert_equal(
            await self.dut.wlan_policy_ap.get_update(),
            [
                f_wlan_policy.AccessPointState(
                    state=f_wlan_policy.OperatingState.ACTIVE,
                    mode=f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
                    band=f_wlan_policy.OperatingBand.ONLY_2_4_GHZ,
                    frequency=None,
                    clients=None,
                    id_=f_wlan_policy.NetworkIdentifier(
                        ssid=list(test_ssid.encode("utf-8")),
                        type_=f_wlan_policy.SecurityType.NONE,
                    ),
                )
            ],
        )
        got_states = await self.dut.wlan_policy_ap.get_update()
        asserts.assert_greater_equal(got_states[0].frequency, 2412)  # channel 1
        asserts.assert_less_equal(got_states[0].frequency, 2484)  # channel 14
        asserts.assert_equal(
            got_states,
            [
                f_wlan_policy.AccessPointState(
                    state=f_wlan_policy.OperatingState.ACTIVE,
                    mode=f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
                    band=f_wlan_policy.OperatingBand.ONLY_2_4_GHZ,
                    frequency=got_states[0].frequency,
                    clients=f_wlan_policy.ConnectedClientInformation(count=0),
                    id_=f_wlan_policy.NetworkIdentifier(
                        ssid=list(test_ssid.encode("utf-8")),
                        type_=f_wlan_policy.SecurityType.NONE,
                    ),
                )
            ],
        )

        await self.dut.wlan_policy_ap.set_new_update_listener()
        got_states = await self.dut.wlan_policy_ap.get_update()
        asserts.assert_is_not_none(got_states[0].frequency)
        asserts.assert_equal(
            got_states,
            [
                f_wlan_policy.AccessPointState(
                    state=f_wlan_policy.OperatingState.ACTIVE,
                    mode=f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
                    band=f_wlan_policy.OperatingBand.ONLY_2_4_GHZ,
                    frequency=got_states[0].frequency,
                    clients=f_wlan_policy.ConnectedClientInformation(count=0),
                    id_=f_wlan_policy.NetworkIdentifier(
                        ssid=list(test_ssid.encode("utf-8")),
                        type_=f_wlan_policy.SecurityType.NONE,
                    ),
                )
            ],
        )

        await self.dut.wlan_policy_ap.stop(
            test_ssid, f_wlan_policy.SecurityType.NONE, None
        )
        asserts.assert_equal(
            await self.dut.wlan_policy_ap.get_update(),
            [],
        )


if __name__ == "__main__":
    test_runner.main()
