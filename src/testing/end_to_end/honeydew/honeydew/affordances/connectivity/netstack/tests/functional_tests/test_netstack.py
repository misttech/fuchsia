# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for netstack affordance."""

import logging

import fuchsia_base_test
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class NetstackTests(fuchsia_base_test.FuchsiaBaseTest):
    """Netstack affordance tests"""

    async def test_list_interfaces(self) -> None:
        """Verify list_interfaces() works on device."""
        interfaces = await self.dut.netstack.list_interfaces()
        asserts.assert_greater(len(interfaces), 0)


if __name__ == "__main__":
    test_runner.main()
