#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for Serial transport."""

import logging
import time

from fuchsia_base_test import fuchsia_base_test
from mobly import asserts, test_runner

from honeydew.fuchsia_device import fuchsia_device

_LOGGER: logging.Logger = logging.getLogger(__name__)


class SerialTransportTests(fuchsia_base_test.FuchsiaBaseTest):
    """Serial transport tests"""

    def setup_class(self) -> None:
        super().setup_class()
        self.device: fuchsia_device.FuchsiaDevice = self.fuchsia_devices[0]

    def test_send(self) -> None:
        """Test case for Serial.send()"""
        for cmd in [
            "echo hi",
            "echo hello",
            "echo foo",
            "echo bar",
            "ls",
            "ls -l",
        ]:
            self.device.serial.send(
                cmd=cmd,
            )

    def test_read(self) -> None:
        """Test case for Serial.read()"""
        read_end_time = time.time() + 60
        test_string_found = False
        test_string = "serial_transport_test__test_read__string"
        self.device.serial.send(cmd=f"echo {test_string}")
        buffer = ""
        while time.time() < read_end_time:
            try:
                read_data = self.device.serial.read()
                buffer += read_data
                if test_string in buffer:
                    test_string_found = True
                    break
            except Exception:
                time.sleep(0.1)

        asserts.assert_true(
            test_string_found,
            f"Target string not found within 30 seconds.",
        )


if __name__ == "__main__":
    test_runner.main()
