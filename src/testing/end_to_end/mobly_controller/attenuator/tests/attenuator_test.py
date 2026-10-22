# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import unittest
from unittest import mock

import attenuator
from attenuator import (
    Attenuator,
    InvalidDataError,
    get_attenuators_for_device,
)
from attenuator.lib.minicircuits import http
from libs.types import ControllerConfig


class AttenuatorTest(unittest.TestCase):
    """Unit tests for the attenuator Mobly controller."""

    @mock.patch("urllib.request.urlopen")
    def test_create_and_destroy_minicircuits_http(
        self, mock_urlopen: mock.Mock
    ) -> None:
        mock_response = mock.MagicMock()
        mock_response.read.return_value = b"MN=RCDAT-6000-90\r\n"
        mock_urlopen.return_value = mock_response

        cfg: ControllerConfig = {
            "Model": "minicircuits",
            "InstrumentCount": 4,
            "Address": "192.168.42.200",
            "Protocol": "http",
            "Port": 80,
        }
        attenuators = attenuator.create([cfg])
        self.assertEqual(len(attenuators), 4)
        for i, attn in enumerate(attenuators):
            self.assertIsInstance(attn, Attenuator)
            self.assertEqual(attn.idx, i)
            self.assertEqual(attn.instrument.address, "192.168.42.200")
            self.assertEqual(attn.get_max_atten(), 90.0)

        info = attenuator.get_info(attenuators)
        self.assertEqual(
            info,
            [
                {"Address": "192.168.42.200", "Attenuator_Port": 0},
                {"Address": "192.168.42.200", "Attenuator_Port": 1},
                {"Address": "192.168.42.200", "Attenuator_Port": 2},
                {"Address": "192.168.42.200", "Attenuator_Port": 3},
            ],
        )

        attenuator.destroy(attenuators)

    @mock.patch("urllib.request.urlopen")
    def test_set_and_get_atten(self, mock_urlopen: mock.Mock) -> None:
        instrument = http.AttenuatorInstrument(num_atten=2)

        open_resp = mock.MagicMock()
        open_resp.read.return_value = b"MN=RCDAT-6000-90"
        set_resp = mock.MagicMock()
        set_resp.read.return_value = b"1"
        get_resp = mock.MagicMock()
        get_resp.read.return_value = b"15.5"

        mock_urlopen.side_effect = [open_resp, set_resp, get_resp]

        instrument.open("10.0.0.1", 80)
        attn = Attenuator(instrument, idx=1)
        attn.set_atten(15.5)
        self.assertEqual(attn.get_atten(), 15.5)

        mock_urlopen.assert_any_call(
            "http://10.0.0.1:80/CHAN:2:SETATT:15.5", timeout=2
        )
        mock_urlopen.assert_any_call(
            "http://10.0.0.1:80/CHAN:2:ATT?", timeout=2
        )

    @mock.patch("urllib.request.urlopen")
    def test_invalid_data_error_on_open(self, mock_urlopen: mock.Mock) -> None:
        instrument = http.AttenuatorInstrument(num_atten=1)
        bad_resp = mock.MagicMock()
        bad_resp.read.return_value = b"INVALID"
        mock_urlopen.return_value = bad_resp

        with self.assertRaises(InvalidDataError):
            instrument.open("10.0.0.1", 80)

    @mock.patch("urllib.request.urlopen")
    def test_get_attenuators_for_device(self, mock_urlopen: mock.Mock) -> None:
        open_resp = mock.MagicMock()
        open_resp.read.return_value = b"MN=RCDAT-6000-90"
        mock_urlopen.return_value = open_resp

        instrument = http.AttenuatorInstrument(num_atten=4)
        instrument.open("192.168.42.200", 80)
        attenuators = [Attenuator(instrument, idx=i) for i in range(4)]

        device_configs: list[ControllerConfig] = [
            {
                "Address": "192.168.42.200",
                "attenuator_ports_wifi_2g": [0, 2],
                "attenuator_ports_wifi_5g": [1, 3],
            }
        ]
        matched_2g = get_attenuators_for_device(
            device_configs, attenuators, "attenuator_ports_wifi_2g"
        )
        self.assertEqual([a.idx for a in matched_2g], [0, 2])


if __name__ == "__main__":
    unittest.main()
