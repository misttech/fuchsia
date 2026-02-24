# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for util."""

import subprocess
import unittest
from unittest.mock import MagicMock, patch

import util


class TestUtil(unittest.TestCase):
    """Tests for util."""

    @patch("subprocess.check_call")
    def test_check_gcert_status_success(
        self, mock_check_call: MagicMock
    ) -> None:
        """Test that check_gcert_status returns True on success."""
        self.assertTrue(util.check_gcert_status())
        mock_check_call.assert_called_with(
            ["gcertstatus", "-check_ssh=false", "-quiet"]
        )

    @patch(
        "subprocess.check_call",
        side_effect=subprocess.CalledProcessError(1, "cmd"),
    )
    def test_check_gcert_status_failure(
        self, mock_check_call: MagicMock
    ) -> None:
        """Test that check_gcert_status returns False on failure."""
        self.assertFalse(util.check_gcert_status())


if __name__ == "__main__":
    unittest.main()
