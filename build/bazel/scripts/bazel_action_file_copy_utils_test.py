#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import tempfile
import unittest
from pathlib import Path

from build.bazel.scripts.bazel_action_file_copy_utils import (
    copy_directory_if_changed,
)


class CopyDirectoryIfChangedTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.root = Path(self._td.name)
        self.src_dir = self.root / "src"
        self.dst_dir = self.root / "dst"

        (self.src_dir / "sub").mkdir(parents=True)
        (self.src_dir / "tracked.txt").write_text("tracked v1")
        (self.src_dir / "sub" / "nested_tracked.txt").write_text("nested v1")
        (self.src_dir / "untracked.txt").write_text("untracked v1")

        self.tracked_files: list[str | os.PathLike[str]] = [
            "tracked.txt",
            "sub/nested_tracked.txt",
        ]

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_initial_copy_returns_true(self) -> None:
        self.assertFalse(self.dst_dir.exists())

        copied = copy_directory_if_changed(
            self.src_dir, self.dst_dir, self.tracked_files
        )

        self.assertTrue(copied)
        self.assertEqual(
            (self.dst_dir / "tracked.txt").read_text(), "tracked v1"
        )
        self.assertEqual(
            (self.dst_dir / "sub" / "nested_tracked.txt").read_text(),
            "nested v1",
        )
        self.assertEqual(
            (self.dst_dir / "untracked.txt").read_text(), "untracked v1"
        )

    def test_unchanged_tracked_files_returns_false(self) -> None:
        self.assertTrue(
            copy_directory_if_changed(
                self.src_dir, self.dst_dir, self.tracked_files
            )
        )

        # Modify an untracked file in src_dir and add a marker in dst_dir;
        # since tracked files have identical mtimes, copy should be skipped.
        (self.src_dir / "untracked.txt").write_text("untracked v2")
        (self.dst_dir / "marker.txt").write_text("keep me")

        copied = copy_directory_if_changed(
            self.src_dir, self.dst_dir, self.tracked_files
        )

        self.assertFalse(copied)
        self.assertEqual(
            (self.dst_dir / "untracked.txt").read_text(), "untracked v1"
        )
        self.assertTrue((self.dst_dir / "marker.txt").exists())

    def test_changed_tracked_file_mtime_returns_true(self) -> None:
        self.assertTrue(
            copy_directory_if_changed(
                self.src_dir, self.dst_dir, self.tracked_files
            )
        )

        (self.dst_dir / "stale.txt").write_text("stale")

        tracked_src = self.src_dir / "tracked.txt"
        old_mtime = os.path.getmtime(tracked_src)
        tracked_src.write_text("tracked v2")
        os.utime(tracked_src, (old_mtime + 10.0, old_mtime + 10.0))

        copied = copy_directory_if_changed(
            self.src_dir, self.dst_dir, self.tracked_files
        )

        self.assertTrue(copied)
        self.assertEqual(
            (self.dst_dir / "tracked.txt").read_text(), "tracked v2"
        )
        self.assertFalse((self.dst_dir / "stale.txt").exists())

        # Subsequent call with now-matching mtimes should return False.
        self.assertFalse(
            copy_directory_if_changed(
                self.src_dir, self.dst_dir, self.tracked_files
            )
        )

    def test_missing_tracked_file_in_dst_returns_true(self) -> None:
        self.assertTrue(
            copy_directory_if_changed(
                self.src_dir, self.dst_dir, self.tracked_files
            )
        )

        (self.dst_dir / "sub" / "nested_tracked.txt").unlink()

        copied = copy_directory_if_changed(
            self.src_dir, self.dst_dir, self.tracked_files
        )

        self.assertTrue(copied)
        self.assertEqual(
            (self.dst_dir / "sub" / "nested_tracked.txt").read_text(),
            "nested v1",
        )


if __name__ == "__main__":
    unittest.main()
