# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for setup_cog_workspace."""

import unittest
from unittest.mock import MagicMock, patch

import setup_cog_workspace
import workspace


class TestSetupCogWorkspace(unittest.TestCase):
    """Tests for setup_cog_workspace."""

    def test_main_no_gcert(self) -> None:
        """Test that main returns 1 when gcert check fails."""
        with patch.object(
            setup_cog_workspace,
            "_parse_args",
            return_value=MagicMock(
                verbose=0,
                snapshot=False,
                use_local_mock_cartfs=False,
                enable_status_updates=False,
                color=True,
            ),
        ), patch("util.check_gcert_status", return_value=False), patch(
            "setup_cog_workspace.logger.log_error"
        ) as mock_log_error:
            result = setup_cog_workspace.main()
            self.assertEqual(result, 1)
            self.assertEqual(mock_log_error.call_count, 2)

    def test_main_not_in_cog_workspace(self) -> None:
        """Test that main returns 1 when not in a cog workspace."""
        with patch.object(
            setup_cog_workspace,
            "_parse_args",
            return_value=MagicMock(
                verbose=0,
                snapshot=False,
                use_local_mock_cartfs=False,
                enable_status_updates=False,
                color=True,
            ),
        ), patch("util.check_gcert_status", return_value=True), patch(
            "workspace.Workspace.create",
            side_effect=workspace.NotInCogWorkspaceError(
                "mock cogd error: CWD is not in cog workspace"
            ),
        ), patch(
            "setup_cog_workspace.logger.log_error"
        ) as mock_log_error:
            result = setup_cog_workspace.main()
            self.assertEqual(result, 1)
            self.assertEqual(mock_log_error.call_count, 2)


if __name__ == "__main__":
    unittest.main()
