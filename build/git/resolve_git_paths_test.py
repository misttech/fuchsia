# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for resolve_git_paths.py."""

import io
import json
import unittest
from contextlib import redirect_stdout
from unittest import mock

from build.git import resolve_git_paths


class ResolveGitPathsTest(unittest.TestCase):
    @mock.patch.object(resolve_git_paths, "get_git_path")
    @mock.patch.object(resolve_git_paths, "get_git_ref")
    def test_main(
        self, mock_get_git_ref: mock.Mock, mock_get_git_path: mock.Mock
    ) -> None:
        mock_get_git_path.side_effect = lambda path, name: path / f".git/{name}"
        mock_get_git_ref.side_effect = (
            lambda path: path / ".git/refs/heads/main"
        )

        buf = io.StringIO()
        with redirect_stdout(buf):
            ret = resolve_git_paths.main()
        self.assertEqual(ret, 0)
        output = json.loads(buf.getvalue())
        self.assertIn("fuchsia_head", output)
        self.assertIn("fuchsia_ref", output)
        self.assertIn("fuchsia_index", output)
        self.assertIn("mesa_head", output)


if __name__ == "__main__":
    unittest.main()
