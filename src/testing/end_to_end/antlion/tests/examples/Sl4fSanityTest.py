#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Test to verify SL4F is running on a Fuchsia device and can communicate with
antlion successfully.
"""

import logging

from antlion import base_test
from antlion.controllers import fuchsia_device
from antlion.controllers.fuchsia_device import FuchsiaDevice
from mobly import asserts, test_runner


class Sl4fSanityTest(base_test.AntlionBaseTest):
    def setup_class(self) -> None:
        self.log = logging.getLogger()
        self.fuchsia_devices: list[FuchsiaDevice] = self.register_controller(
            fuchsia_device
        )

        asserts.abort_class_if(
            len(self.fuchsia_devices) == 0,
            "Requires at least one Fuchsia device",
        )

    def test_example(self) -> None:
        for fuchsia_device in self.fuchsia_devices:
            res = fuchsia_device.honeydew_fd.netstack.list_interfaces()
            self.log.info(res)


if __name__ == "__main__":
    test_runner.main()
