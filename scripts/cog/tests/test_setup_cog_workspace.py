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

    def test_try_create_workspace_success(self) -> None:
        """Test that try_create_workspace returns workspace instance."""
        mock_ws = MagicMock()
        with patch("workspace.Workspace.create", return_value=mock_ws):
            ws = setup_cog_workspace.try_create_workspace(False)
            self.assertEqual(ws, mock_ws)

    def test_try_create_workspace_not_in_cog(self) -> None:
        """Test that try_create_workspace returns None when not in cog."""
        with patch(
            "workspace.Workspace.create",
            side_effect=workspace.NotInCogWorkspaceError("error"),
        ), patch("setup_cog_workspace.logger.log_error") as mock_log_error:
            ws = setup_cog_workspace.try_create_workspace(False)
            self.assertIsNone(ws)
            self.assertEqual(mock_log_error.call_count, 2)

    def test_init_cartfs_snapshot(self) -> None:
        """Test that init_cartfs attempts snapshot."""
        mock_ws = MagicMock()
        mock_ws.cartfs_instance.use_local_mock_cartfs = False
        mock_ws.has_cartfs_dir = False
        setup_cog_workspace.init_cartfs(mock_ws, snapshot=True)
        mock_ws.init_cartfs_workspace_snapshot.assert_called_once()

    def test_init_cartfs_empty(self) -> None:
        """Test that init_cartfs creates empty workspace if not snapshot or failed."""
        mock_ws = MagicMock()
        mock_ws.cartfs_instance.use_local_mock_cartfs = False
        mock_ws.has_cartfs_dir = False
        setup_cog_workspace.init_cartfs(mock_ws, snapshot=False)
        mock_ws.init_cartfs_workspace_empty.assert_called_once()

    def test_main_success_needs_init(self) -> None:
        """Test that main returns 0 on success and calls init_cartfs."""
        mock_ws = MagicMock()
        mock_ws.is_checkout_uptodate.return_value = True
        mock_ws.has_cartfs_dir = False

        mock_lock = MagicMock()
        mock_ws.lock.return_value = mock_lock

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
            "setup_cog_workspace.try_create_workspace", return_value=mock_ws
        ), patch(
            "setup_cog_workspace.init_cartfs"
        ) as mock_init_cartfs:
            result = setup_cog_workspace.main()
            self.assertEqual(result, 0)
            mock_init_cartfs.assert_called_once_with(mock_ws, False)


if __name__ == "__main__":
    unittest.main()
