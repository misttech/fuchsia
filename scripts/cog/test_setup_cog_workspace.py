# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for setup_cog_workspace."""

import unittest
from typing import Any
from unittest import mock

import setup_cog_workspace
from parameterized import parameterized


class TestFindWorkspaceName(unittest.TestCase):
    """Tests for the _find_workspace_name function."""

    @parameterized.expand(
        [
            (
                "direct_match",
                "/google/cog/cloud/testuser/myworkspace",
                "/google/cog/cloud/testuser/myworkspace",
            ),
            (
                "subdirectory_match",
                "/google/cog/cloud/testuser/myworkspace/some/subdir",
                "/google/cog/cloud/testuser/myworkspace",
            ),
            (
                "no_match_outside_cog",
                "/some/other/path/testuser/myworkspace",
                None,
            ),
            (
                "no_match_missing_cloud",
                "/google/cog/testuser/myworkspace",
                None,
            ),
            (
                "no_match_wrong_user",
                "/google/cog/cloud/anotheruser/myworkspace",
                None,
            ),
            ("no_match_root", "/", None),
        ]
    )
    @mock.patch.dict("os.environ", {"USER": "testuser"})
    @mock.patch("os.getcwd")
    def test_find_workspace_name(
        self, name: str, cwd: str, expected: str | None, mock_getcwd: Any
    ) -> None:
        mock_getcwd.return_value = cwd
        self.assertEqual(
            setup_cog_workspace.find_cog_workspace_directory(),
            expected,
            name,
        )

    @mock.patch.dict("os.environ", {"USER": "otheruser"})
    @mock.patch("os.getcwd")
    def test_no_user(self, mock_getcwd: Any) -> None:
        mock_getcwd.return_value = "/google/cog/cloud/testuser/myworkspace"
        self.assertIsNone(setup_cog_workspace.find_cog_workspace_directory())


if __name__ == "__main__":
    unittest.main()
