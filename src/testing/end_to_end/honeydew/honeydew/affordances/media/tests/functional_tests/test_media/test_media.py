# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for Media affordance."""

import logging

from fuchsia_base_test import fuchsia_base_test
from mobly import test_runner

from honeydew.affordances.media import media

_LOGGER: logging.Logger = logging.getLogger(__name__)


class MediaAffordanceTests(fuchsia_base_test.FuchsiaBaseTest):
    """Media affordance tests"""

    def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        super().setup_class()
        self.device = self.fuchsia_devices[0]

    def test_get_active_session_status(self) -> None:
        """Test case for Media.get_active_session_status()"""
        status = self.device.media.get_active_session_status()
        if status is not None:
            assert isinstance(status, media.PlayerState)
        # Verify that a subsequent call does not hang.
        status = self.device.media.get_active_session_status()
        if status is not None:
            assert isinstance(status, media.PlayerState)


if __name__ == "__main__":
    test_runner.main()
