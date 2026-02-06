# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for setup_cog_workspace."""

import unittest
from unittest.mock import MagicMock, patch

import setup_cog_workspace


class TestSetupCogWorkspace(unittest.TestCase):
    """Tests for setup_cog_workspace."""

    def test_main_invalid_repo_root(self) -> None:
        """Test that main returns 1 when repo_root is invalid."""
        with patch.object(
            setup_cog_workspace,
            "_parse_args",
            return_value=MagicMock(
                repo_root="/invalid/path",
                verbose=0,
                disable_snapshot=False,
                use_local_mock_cartfs=False,
                enable_status_updates=False,
            ),
        ), patch("pathlib.Path.is_dir", return_value=False), patch(
            "setup_cog_workspace.logger.log_error"
        ) as mock_log_error:
            result = setup_cog_workspace.main()
            self.assertEqual(result, 1)
            mock_log_error.assert_called_once()


if __name__ == "__main__":
    unittest.main()
