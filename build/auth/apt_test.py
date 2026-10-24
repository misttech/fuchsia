# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for apt.py."""

# Add our active parent directory to sys.path to enable flat, direct imports of sister modules
import pathlib
import sys

_SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(_SCRIPT_DIR))

import shutil
import subprocess
import unittest
from unittest import mock

import apt


class AptTest(unittest.TestCase):
    """Unit tests for apt.py module-scoped functions."""

    def setUp(self) -> None:
        # Clear the module's functools cache to prevent cross-contamination between test cases
        apt.is_available.cache_clear()

    @mock.patch.object(shutil, "which")
    def test_is_available_when_apt_exists(self, mock_which: mock.Mock) -> None:
        mock_which.side_effect = lambda cmd: f"/usr/bin/{cmd}"
        self.assertTrue(apt.is_available())

    @mock.patch.object(shutil, "which")
    def test_is_available_when_apt_missing(self, mock_which: mock.Mock) -> None:
        # Simulate 'apt-cache' being missing from the host
        mock_which.side_effect = (
            lambda cmd: None if cmd == "apt-cache" else f"/usr/bin/{cmd}"
        )
        self.assertFalse(apt.is_available())

    @mock.patch.object(shutil, "which", return_value="/usr/bin/apt")
    @mock.patch.object(subprocess, "run")
    def test_package_exists_true(
        self, mock_run: mock.Mock, mock_which: mock.Mock
    ) -> None:
        mock_proc = mock.Mock()
        mock_proc.returncode = 0
        mock_run.return_value = mock_proc

        self.assertTrue(apt.exists("some-package"))

    @mock.patch.object(shutil, "which", return_value="/usr/bin/apt")
    @mock.patch.object(subprocess, "run")
    def test_package_exists_false(
        self, mock_run: mock.Mock, mock_which: mock.Mock
    ) -> None:
        mock_proc = mock.Mock()
        mock_proc.returncode = 1
        mock_run.return_value = mock_proc

        self.assertFalse(apt.exists("some-package"))

    @mock.patch.object(shutil, "which", return_value="/usr/bin/apt")
    @mock.patch.object(subprocess, "run")
    def test_package_exists_os_error_returns_false(
        self, mock_run: mock.Mock, mock_which: mock.Mock
    ) -> None:
        mock_run.side_effect = OSError("Database locked")
        self.assertFalse(apt.exists("some-package"))

    @mock.patch.object(shutil, "which", return_value="/usr/bin/apt")
    def test_malformed_package_name_raises_value_error(
        self, mock_which: mock.Mock
    ) -> None:
        with self.assertRaises(ValueError) as ctx:
            apt.exists("invalid/package")
        self.assertIn("Malformed package name", str(ctx.exception))
        self.assertIn("apt.py", str(ctx.exception))

        with self.assertRaises(ValueError) as ctx:
            apt.install("invalid package")
        self.assertIn("Malformed package name", str(ctx.exception))
        self.assertIn("apt.py", str(ctx.exception))

    @mock.patch.object(shutil, "which", return_value="/usr/bin/apt")
    @mock.patch.object(sys.stdin, "isatty", return_value=True)
    @mock.patch.object(subprocess, "run")
    @mock.patch("builtins.input", return_value="y")
    def test_install_package_accepts(
        self,
        mock_input: mock.Mock,
        mock_run: mock.Mock,
        mock_isatty: mock.Mock,
        mock_which: mock.Mock,
    ) -> None:
        apt.install("some-package", interactive=True)
        mock_run.assert_called_once_with(
            ["sudo", "apt", "install", "-y", "some-package"],
            check=True,
        )

    @mock.patch.object(shutil, "which", return_value="/usr/bin/apt")
    @mock.patch.object(sys.stdin, "isatty", return_value=True)
    @mock.patch.object(subprocess, "run")
    @mock.patch("builtins.input", return_value="n")
    def test_install_package_declines(
        self,
        mock_input: mock.Mock,
        mock_run: mock.Mock,
        mock_isatty: mock.Mock,
        mock_which: mock.Mock,
    ) -> None:
        with self.assertRaises(RuntimeError) as ctx:
            apt.install("some-package", interactive=True)
        self.assertIn("installation declined.", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
