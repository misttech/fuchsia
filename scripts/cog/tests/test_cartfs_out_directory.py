# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import tempfile
import unittest
from pathlib import Path
from unittest import mock

from cartfs_out_directory import CartfsOutDirectory


class TestCartfsOutDirectory(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp_dir = tempfile.TemporaryDirectory()
        self.cog_workspace_dir = Path(self.tmp_dir.name) / "cog"
        self.cartfs_workspace_dir = Path(self.tmp_dir.name) / "cartfs"
        self.cog_workspace_dir.mkdir()
        self.cartfs_workspace_dir.mkdir()

        self.manager = CartfsOutDirectory(
            cog_workspace_dir=self.cog_workspace_dir,
            cartfs_workspace_dir=self.cartfs_workspace_dir,
        )

    def tearDown(self) -> None:
        self.tmp_dir.cleanup()

    def test_properties(self) -> None:
        self.assertEqual(
            self.manager.cog_out_symlink, self.cog_workspace_dir / "out"
        )
        self.assertEqual(
            self.manager.cartfs_symlink_tree,
            self.cartfs_workspace_dir / "PROJECT_ROOT",
        )
        self.assertEqual(
            self.manager.cartfs_out_dir,
            self.cartfs_workspace_dir / "PROJECT_ROOT" / "out",
        )

    def test_is_installed_false_when_not_installed(self) -> None:
        self.assertFalse(self.manager.is_installed)

    def test_is_installed_false_when_out_is_dir(self) -> None:
        self.manager.cog_out_symlink.mkdir()
        self.assertFalse(self.manager.is_installed)

    def test_is_installed_false_when_out_is_file(self) -> None:
        self.manager.cog_out_symlink.touch()
        self.assertFalse(self.manager.is_installed)

    def test_is_installed_false_when_symlink_is_wrong(self) -> None:
        wrong_target = self.cartfs_workspace_dir / "wrong"
        wrong_target.mkdir(parents=True)
        self.manager.cog_out_symlink.symlink_to(wrong_target)
        self.assertFalse(self.manager.is_installed)

    def test_is_installed_false_when_symlink_is_broken(self) -> None:
        self.manager.cog_out_symlink.symlink_to(self.manager.cartfs_out_dir)
        self.assertFalse(self.manager.is_installed)

    def test_is_installed_true_when_correct(self) -> None:
        self.manager.cartfs_out_dir.mkdir(parents=True)
        self.manager.cog_out_symlink.symlink_to(self.manager.cartfs_out_dir)
        self.assertTrue(self.manager.is_installed)

    def test_update_asserts_if_not_installed(self) -> None:
        with self.assertRaises(AssertionError):
            self.manager.update()

    def _install_for_update_test(self) -> None:
        self.manager.cartfs_out_dir.mkdir(parents=True)
        self.manager.cog_out_symlink.symlink_to(self.manager.cartfs_out_dir)

        # Create some files in cog workspace
        (self.cog_workspace_dir / "src").mkdir()
        (self.cog_workspace_dir / "zircon").mkdir()
        (self.cog_workspace_dir / "BUILD.gn").touch()

    def test_update_creates_symlinks(self) -> None:
        self._install_for_update_test()
        self.manager.update()

        self.assertTrue((self.manager.cartfs_symlink_tree / "src").is_symlink())
        self.assertEqual(
            (self.manager.cartfs_symlink_tree / "src").resolve(),
            self.cog_workspace_dir / "src",
        )
        self.assertTrue(
            (self.manager.cartfs_symlink_tree / "zircon").is_symlink()
        )
        self.assertEqual(
            (self.manager.cartfs_symlink_tree / "zircon").resolve(),
            self.cog_workspace_dir / "zircon",
        )
        self.assertTrue(
            (self.manager.cartfs_symlink_tree / "BUILD.gn").is_symlink()
        )
        self.assertEqual(
            (self.manager.cartfs_symlink_tree / "BUILD.gn").resolve(),
            self.cog_workspace_dir / "BUILD.gn",
        )
        self.assertTrue((self.manager.cartfs_symlink_tree / "out").is_dir())
        self.assertFalse(
            (self.manager.cartfs_symlink_tree / "out").is_symlink()
        )

    def test_update_removes_stale_symlinks_and_dirs(self) -> None:
        self._install_for_update_test()
        stale_link = self.manager.cartfs_symlink_tree / "stale_link"
        stale_link.symlink_to(self.cog_workspace_dir / "src")
        stale_dir = self.manager.cartfs_symlink_tree / "stale_dir"
        stale_dir.mkdir()
        self.assertTrue(stale_link.exists())
        self.assertTrue(stale_dir.exists())

        self.manager.update()

        self.assertFalse(stale_link.exists())
        self.assertFalse(stale_dir.exists())

    def test_reinstall_from_scratch(self) -> None:
        (self.cog_workspace_dir / "src").mkdir()
        (self.cog_workspace_dir / "BUILD.gn").touch()

        self.manager.install()

        self.assertTrue(self.manager.is_installed)
        self.assertTrue(self.manager.cartfs_out_dir.is_dir())
        self.assertTrue((self.manager.cartfs_symlink_tree / "src").is_symlink())
        self.assertTrue(
            (self.manager.cartfs_symlink_tree / "BUILD.gn").is_symlink()
        )
        self.assertFalse(
            (self.manager.cartfs_symlink_tree / "out").is_symlink()
        )

    @mock.patch("cartfs_out_directory.uuid.uuid4")
    def test_reinstall_preserves_existing_cog_out_dir(
        self, mock_uuid4: mock.Mock
    ) -> None:
        mock_uuid4.return_value = "TEST_UUID"
        out_dir = self.cog_workspace_dir / "out"
        out_dir.mkdir()
        (out_dir / "some_file").touch()

        self.manager.install()

        self.assertTrue(self.manager.is_installed)
        self.assertTrue(out_dir.is_symlink())

        preserved_dir = self.cog_workspace_dir / "out-TEST_UUID"
        self.assertTrue(preserved_dir.is_dir())
        self.assertTrue((preserved_dir / "some_file").exists())

    def test_reinstall_with_existing_out_file(self) -> None:
        (self.cog_workspace_dir / "out").touch()
        self.manager.install()
        self.assertTrue(self.manager.is_installed)
        self.assertTrue((self.cog_workspace_dir / "out").is_symlink())

    def test_reinstall_with_existing_wrong_symlink(self) -> None:
        wrong_target = self.cartfs_workspace_dir / "wrong"
        wrong_target.mkdir(parents=True)
        self.manager.cog_out_symlink.symlink_to(wrong_target)
        self.manager.install()
        self.assertTrue(self.manager.is_installed)

    def test_reinstall_with_existing_cartfs_tree(self) -> None:
        self.manager.cartfs_symlink_tree.mkdir()
        (self.manager.cartfs_symlink_tree / "stale").touch()
        self.manager.install()
        self.assertTrue(self.manager.is_installed)
        self.assertFalse((self.manager.cartfs_symlink_tree / "stale").exists())

    def test_reinstall_preserves_existing_cartfs_out_dir(self) -> None:
        self.manager.cartfs_out_dir.mkdir(parents=True)
        (self.manager.cartfs_out_dir / "some_file").touch()

        self.manager.install()

        self.assertTrue(self.manager.is_installed)
        self.assertEqual(
            self.manager.cog_out_symlink.resolve(),
            self.manager.cartfs_out_dir.resolve(),
        )
        self.assertTrue((self.manager.cartfs_out_dir / "some_file").exists())

    @mock.patch.object(CartfsOutDirectory, "update")
    def test_apply_calls_update_if_installed(
        self, mock_update: mock.Mock
    ) -> None:
        with mock.patch.object(
            CartfsOutDirectory, "is_installed", new_callable=mock.PropertyMock
        ) as mock_is_installed:
            mock_is_installed.return_value = True
            self.manager.apply()
            mock_update.assert_called_once()

    @mock.patch.object(CartfsOutDirectory, "install")
    def test_apply_calls_reinstall_if_not_installed(
        self, mock_reinstall: mock.Mock
    ) -> None:
        with mock.patch.object(
            CartfsOutDirectory, "is_installed", new_callable=mock.PropertyMock
        ) as mock_is_installed:
            mock_is_installed.return_value = False
            self.manager.apply()
            mock_reinstall.assert_called_once()


if __name__ == "__main__":
    unittest.main()
