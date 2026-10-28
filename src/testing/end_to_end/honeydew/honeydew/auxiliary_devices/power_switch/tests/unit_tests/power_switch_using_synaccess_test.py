# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for power_switch_using_synaccess.py."""

import unittest
from unittest import mock

from honeydew import errors
from honeydew.auxiliary_devices.power_switch import (
    power_switch,
    power_switch_using_synaccess,
)
from honeydew.utils import http_utils

_MOCK_PDU_HOST: str = "192.168.42.185"
_EXPECTED_AUTH_HEADERS: dict[str, str] = {
    "Authorization": "Basic YWRtaW46YWRtaW4=",
}


class PowerSwitchUsingSynaccessTests(unittest.TestCase):
    """Unit tests for power_switch_using_synaccess.py."""

    def setUp(self) -> None:
        super().setUp()
        self.pdu_obj: power_switch_using_synaccess.PowerSwitchUsingSynaccess = (
            power_switch_using_synaccess.PowerSwitchUsingSynaccess(
                host=_MOCK_PDU_HOST,
            )
        )

    def test_power_switch_using_synaccess_is_a_power_switch(self) -> None:
        """Test case to make sure PowerSwitchUsingSynaccess is a PowerSwitch."""
        self.assertIsInstance(self.pdu_obj, power_switch.PowerSwitch)

    def test_status_power_on_and_power_off(self) -> None:
        """Test case for status(), power_on(), and power_off() HTTP commands."""
        with mock.patch.object(
            http_utils,
            "send_http_request",
            side_effect=[
                "$A0,10",
                "$A0,10",
                "$A0",
                "$A0,11",
                "$A0",
                "$A0,01",
            ],
            autospec=True,
        ) as mock_send_http_request:
            self.assertEqual(self.pdu_obj.status(), {"1": False, "2": True})
            self.pdu_obj.power_on(outlet=1)
            self.pdu_obj.power_off(outlet=2)

            mock_send_http_request.assert_any_call(
                f"http://{_MOCK_PDU_HOST}/cmd.cgi?%24A5",
                headers=_EXPECTED_AUTH_HEADERS,
                parse_json=False,
            )
            mock_send_http_request.assert_any_call(
                f"http://{_MOCK_PDU_HOST}/cmd.cgi?%24A3+1+1",
                headers=_EXPECTED_AUTH_HEADERS,
                parse_json=False,
            )
            mock_send_http_request.assert_any_call(
                f"http://{_MOCK_PDU_HOST}/cmd.cgi?%24A3+2+0",
                headers=_EXPECTED_AUTH_HEADERS,
                parse_json=False,
            )

    def test_power_on_off_no_outlet_failure(self) -> None:
        """Test case to ensure failure when outlet is None."""
        with self.assertRaisesRegex(
            power_switch_using_synaccess.PowerSwitchSynaccessError,
            "Outlet number must be specified",
        ):
            self.pdu_obj.power_off(outlet=None)

        with self.assertRaisesRegex(
            power_switch_using_synaccess.PowerSwitchSynaccessError,
            "Outlet number must be specified",
        ):
            self.pdu_obj.power_on(outlet=None)

    def test_invalid_port_and_error_response(self) -> None:
        """Test case to ensure invalid ports and error responses raise PowerSwitchSynaccessError."""
        with self.assertRaises(
            power_switch_using_synaccess.PowerSwitchSynaccessError
        ):
            self.pdu_obj.power_on(outlet=0)

        with self.assertRaises(
            power_switch_using_synaccess.PowerSwitchSynaccessError
        ):
            self.pdu_obj.power_on(outlet=3)

        with mock.patch.object(
            http_utils,
            "send_http_request",
            return_value="$AF",
            autospec=True,
        ):
            with self.assertRaises(
                power_switch_using_synaccess.PowerSwitchSynaccessError
            ):
                self.pdu_obj.power_on(outlet=1)

        with mock.patch.object(
            http_utils,
            "send_http_request",
            side_effect=errors.HttpRequestError("HTTP request failed"),
            autospec=True,
        ):
            with self.assertRaises(
                power_switch_using_synaccess.PowerSwitchSynaccessError
            ):
                self.pdu_obj.power_on(outlet=1)


if __name__ == "__main__":
    unittest.main()
