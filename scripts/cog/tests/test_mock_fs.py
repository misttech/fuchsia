# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from mock_fs import FileSystemTestHelper, FSType


class TestFileSystemTestHelper(unittest.TestCase):
    def setUp(self) -> None:
        self.fs = FileSystemTestHelper()

    def tearDown(self) -> None:
        self.fs.cleanup()

    def test_fs_helper(self) -> None:
        self.fs.mkdir("test_dir", FSType.COG)
        self.assertEqual(
            self.fs.cog_dir / "test_dir",
            self.fs.full_path("test_dir", FSType.COG),
        )
        self.fs.write("test.txt", FSType.COG, "hello")
        self.assertEqual(self.fs.read("test.txt", FSType.COG), "hello")
        self.fs.symlink_from_cog_to_cartfs("cartfs_link")
        self.assertTrue((self.fs.cog_dir / "cartfs_link").is_symlink())
        self.fs.symlink_from_cartfs_to_cog("cog_link")
        self.assertTrue((self.fs.cartfs_dir / "cog_link").is_symlink())
        self.fs.delete("test.txt", FSType.COG)
        with self.assertRaises(FileNotFoundError):
            self.fs.read("test.txt", FSType.COG)


if __name__ == "__main__":
    unittest.main()
