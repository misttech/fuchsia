# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for resolve_git_revisions.py."""

import io
import json
import unittest
from contextlib import redirect_stdout
from unittest import mock

from build.git import resolve_git_revisions


class ResolveGitRevisionsTest(unittest.TestCase):
    @mock.patch.object(resolve_git_revisions, "get_git_revision")
    def test_main(self, mock_get_git_revision: mock.Mock) -> None:
        mock_get_git_revision.side_effect = [
            "1111222233334444555566667777888899990000",
            "aaaabbbbccccddddeeeeffff0000111122223333",
        ]
        buf = io.StringIO()
        with redirect_stdout(buf):
            ret = resolve_git_revisions.main()
        self.assertEqual(ret, 0)
        output = json.loads(buf.getvalue())
        self.assertEqual(
            output["fuchsia_revision"],
            "1111222233334444555566667777888899990000",
        )
        self.assertEqual(output["mesa_revision"], "aaaabbbbcc")


if __name__ == "__main__":
    unittest.main()
