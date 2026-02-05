# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""OpenWRT AP test for Lacewing."""

import logging

from fuchsia_base_test import fuchsia_base_test
from mobly import test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class OpenwrtApTest(fuchsia_base_test.FuchsiaBaseTest):
    def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        super().setup_class()
        self.log = logging.getLogger()

        if not self.openwrt_aps:
            _LOGGER.warning(
                "Skipping AP setup: No OpenWRT controller available."
            )
            return

        # TODO(b/461905545): generate ssid from random string util
        # self.ssid = rand_ascii_str(10)
        self.ssid = "test_ssid"
        self.openwrt_ap = self.openwrt_aps[0]
        self.openwrt_ap.setup_ap(ssid=self.ssid)

    def teardown_class(self) -> None:
        super().teardown_class()
        if self.openwrt_aps:
            self.openwrt_ap.close()
        else:
            _LOGGER.warning(
                "Skipping AP teardown: No OpenWRT controller available."
            )


if __name__ == "__main__":
    test_runner.main()
