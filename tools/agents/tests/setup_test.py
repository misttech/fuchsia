#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for setup subcommand CLI dispatching and ad-hoc permissions."""

from __future__ import annotations

import argparse
import io
import json
import pathlib
import tempfile
import unittest
from unittest import mock

from agents.commands import setup
from agents.lib import permissions


class SetupCommandTest(unittest.TestCase):
    """Hermetic unit tests for setup command argument parsing and execution."""

    def setUp(self) -> None:
        self.stdout_patch = mock.patch("sys.stdout", new_callable=io.StringIO)
        self.mock_stdout = self.stdout_patch.start()
        self.addCleanup(self.stdout_patch.stop)

        self.stderr_patch = mock.patch("sys.stderr", new_callable=io.StringIO)
        self.mock_stderr = self.stderr_patch.start()
        self.addCleanup(self.stderr_patch.stop)

        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        self.mock_root = pathlib.Path(self.temp_dir.name)

        self.fuchsia_dir = self.mock_root / "fuchsia"
        self.permissions_dir = (
            self.fuchsia_dir / ".agents" / "config" / "permissions"
        )
        self.permissions_dir.mkdir(parents=True, exist_ok=True)
        (self.permissions_dir / "read_only.txt").write_text(
            "git status\nfx status\n",
            encoding="utf-8",
        )
        (self.permissions_dir / "local_changes.txt").write_text(
            "fx format-code\ngit checkout\n",
            encoding="utf-8",
        )
        (self.permissions_dir / "external_changes.txt").write_text(
            "git push\n", encoding="utf-8"
        )
        (self.permissions_dir / "never_allow.txt").write_text(
            "git clean\n", encoding="utf-8"
        )
        (self.permissions_dir / "device_ops.txt").write_text(
            "fx ota\n", encoding="utf-8"
        )
        (self.permissions_dir / "cache_destruction.txt").write_text(
            "fx clean\n", encoding="utf-8"
        )
        (self.permissions_dir / "batch_execution.txt").write_text(
            "find\n", encoding="utf-8"
        )

    def test_run_with_custom_grants(self) -> None:
        """Verify run handler with ad-hoc grant arguments."""
        config_path = self.mock_root / "config.json"

        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)
        args = parser.parse_args(
            [
                "--config",
                str(config_path),
                "-a",
                "command(fx test)",
                "-d",
                "command(rm -rf)",
                "-k",
                "command(reboot)",
            ]
        )

        with mock.patch.object(
            permissions, "find_fuchsia_dir", return_value=self.fuchsia_dir
        ):
            exit_code = setup.run(args)
        self.assertEqual(exit_code, 0)

        with config_path.open("r", encoding="utf-8") as fh:
            data = json.load(fh)
        grants = data["userSettings"]["globalPermissionGrants"]
        self.assertEqual(grants["allow"], ["command(fx test)"])
        self.assertEqual(grants["deny"], ["command(rm -rf)"])
        self.assertEqual(grants["ask"], ["command(reboot)"])

    def test_run_with_command_expansion(self) -> None:
        """Verify unformatted commands are expanded during run."""
        config_path = self.mock_root / "config.json"

        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)
        args = parser.parse_args(
            [
                "--config",
                str(config_path),
                "-a",
                "fx build",
                "-d",
                "git push --force",
            ]
        )

        with mock.patch.object(
            permissions, "find_fuchsia_dir", return_value=self.fuchsia_dir
        ):
            exit_code = setup.run(args)
        self.assertEqual(exit_code, 0)

        with config_path.open("r", encoding="utf-8") as fh:
            data = json.load(fh)
        grants = data["userSettings"]["globalPermissionGrants"]
        self.assertTrue(
            any(
                g.startswith("command(regex:") and "fx" in g and "build" in g
                for g in grants["allow"]
            )
        )
        self.assertTrue(
            any(
                g.startswith("command(regex:") and "git" in g and "push" in g
                for g in grants["deny"]
            )
        )

    def test_run_with_profile(self) -> None:
        """Verify profile application applies manifest rules."""
        config_path = self.mock_root / "config.json"

        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)
        args = parser.parse_args(
            [
                "--config",
                str(config_path),
                "-p",
                "read-only",
            ]
        )

        with mock.patch.object(
            permissions, "find_fuchsia_dir", return_value=self.fuchsia_dir
        ):
            exit_code = setup.run(args)
        self.assertEqual(exit_code, 0)

        with config_path.open("r", encoding="utf-8") as fh:
            data = json.load(fh)
        grants = data["userSettings"]["globalPermissionGrants"]
        self.assertTrue(
            any(
                g.startswith("command(regex:") and "status" in g
                for g in grants["allow"]
            )
        )
        self.assertTrue(
            any(
                g.startswith("command(regex:") and "clean" in g
                for g in grants["deny"]
            )
        )

    def test_run_with_list_files(self) -> None:
        """Verify reading extra list files."""
        config_path = self.mock_root / "config.json"
        list_file = self.mock_root / "extra_allow.txt"
        list_file.write_text("my_custom_tool\n", encoding="utf-8")

        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)
        args = parser.parse_args(
            [
                "--config",
                str(config_path),
                "--allow-list",
                str(list_file),
            ]
        )

        with mock.patch.object(
            permissions, "find_fuchsia_dir", return_value=self.fuchsia_dir
        ):
            exit_code = setup.run(args)
        self.assertEqual(exit_code, 0)

        with config_path.open("r", encoding="utf-8") as fh:
            data = json.load(fh)
        grants = data["userSettings"]["globalPermissionGrants"]
        self.assertIn("command(my_custom_tool)", grants["allow"])


if __name__ == "__main__":
    unittest.main()
