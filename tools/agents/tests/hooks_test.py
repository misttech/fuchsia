#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for the agents hooks command handler."""

from __future__ import annotations

import argparse
import unittest
from unittest import mock

from agents.commands import hooks
from agents.lib import githooks, paths
from agents_testing.base import BaseTestCase


class HooksCommandTest(BaseTestCase):
    """Tests for hooks command parser and execution."""

    def setUp(self) -> None:
        super().setUp()
        self.mock_root = self.test_dir
        self.fuchsia_dir = self.mock_root / "fuchsia"
        self.fuchsia_dir.mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / ".git").mkdir(parents=True, exist_ok=True)

        self.patch_object(
            paths, "find_fuchsia_dir", return_value=self.fuchsia_dir
        )

    def test_register_subcommand(self) -> None:
        parser = argparse.ArgumentParser()
        subparsers = parser.add_subparsers(dest="subcommand")
        hooks.register_subcommand(subparsers)

        args_enable = parser.parse_args(["hooks", "enable", "--dry-run"])
        self.assertEqual(args_enable.subcommand, "hooks")
        self.assertEqual(args_enable.hooks_subcommand, "enable")
        self.assertTrue(args_enable.dry_run)
        self.assertEqual(args_enable.func, hooks.run_enable)

        args_disable = parser.parse_args(["hooks", "disable"])
        self.assertEqual(args_disable.subcommand, "hooks")
        self.assertEqual(args_disable.hooks_subcommand, "disable")
        self.assertFalse(args_disable.dry_run)
        self.assertEqual(args_disable.func, hooks.run_disable)

        args_status = parser.parse_args(["hooks", "status"])
        self.assertEqual(args_status.subcommand, "hooks")
        self.assertEqual(args_status.hooks_subcommand, "status")
        self.assertEqual(args_status.func, hooks.run_status)

    def test_run_enable_success(self) -> None:
        hook_path = (
            self.fuchsia_dir
            / ".git"
            / "hooks"
            / "pre-push.d"
            / "10-fuchsia-agent.sh"
        )
        res = githooks.HookOperationResult(modified=[hook_path])

        parser = argparse.ArgumentParser()
        subparsers = parser.add_subparsers(dest="subcommand")
        hooks.register_subcommand(subparsers)
        args = parser.parse_args(["hooks", "enable"])

        with mock.patch(
            "agents.lib.githooks.install_git_hooks", return_value=res
        ) as mock_install:
            ret = args.func(args)
            self.assertEqual(ret, 0)
            mock_install.assert_called_once_with(
                self.fuchsia_dir, dry_run=False
            )
            self.assertIn("Configured 1 Git hook across checkout.", self.stdout)

    def test_run_enable_dry_run(self) -> None:
        hook_path = (
            self.fuchsia_dir
            / ".git"
            / "hooks"
            / "pre-push.d"
            / "10-fuchsia-agent.sh"
        )
        res = githooks.HookOperationResult(modified=[hook_path])

        parser = argparse.ArgumentParser()
        subparsers = parser.add_subparsers(dest="subcommand")
        hooks.register_subcommand(subparsers)
        args = parser.parse_args(["hooks", "enable", "--dry-run"])

        with mock.patch(
            "agents.lib.githooks.install_git_hooks", return_value=res
        ) as mock_install:
            ret = args.func(args)
            self.assertEqual(ret, 0)
            mock_install.assert_called_once_with(self.fuchsia_dir, dry_run=True)
            self.assertIn(
                "[DRY RUN] Would configure 1 Git hook across checkout.",
                self.stdout,
            )

    def test_run_enable_failure(self) -> None:
        res = githooks.HookOperationResult(
            modified=[],
            failed=[(self.fuchsia_dir, "Permission denied")],
        )

        parser = argparse.ArgumentParser()
        subparsers = parser.add_subparsers(dest="subcommand")
        hooks.register_subcommand(subparsers)
        args = parser.parse_args(["hooks", "enable"])

        with mock.patch(
            "agents.lib.githooks.install_git_hooks", return_value=res
        ):
            ret = args.func(args)
            self.assertEqual(ret, 1)
            self.assertIn("Warning: Failed to configure Git hooks", self.stderr)

    def test_run_disable_success(self) -> None:
        hook_path = (
            self.fuchsia_dir
            / ".git"
            / "hooks"
            / "pre-push.d"
            / "10-fuchsia-agent.sh"
        )
        res = githooks.HookOperationResult(modified=[hook_path])

        parser = argparse.ArgumentParser()
        subparsers = parser.add_subparsers(dest="subcommand")
        hooks.register_subcommand(subparsers)
        args = parser.parse_args(["hooks", "disable"])

        with mock.patch(
            "agents.lib.githooks.uninstall_git_hooks", return_value=res
        ) as mock_uninstall:
            ret = args.func(args)
            self.assertEqual(ret, 0)
            mock_uninstall.assert_called_once_with(
                self.fuchsia_dir, dry_run=False
            )
            self.assertIn("Removed 1 Git hook across checkout.", self.stdout)

    def test_run_disable_dry_run(self) -> None:
        hook_path = (
            self.fuchsia_dir
            / ".git"
            / "hooks"
            / "pre-push.d"
            / "10-fuchsia-agent.sh"
        )
        res = githooks.HookOperationResult(modified=[hook_path])

        parser = argparse.ArgumentParser()
        subparsers = parser.add_subparsers(dest="subcommand")
        hooks.register_subcommand(subparsers)
        args = parser.parse_args(["hooks", "disable", "--dry-run"])

        with mock.patch(
            "agents.lib.githooks.uninstall_git_hooks", return_value=res
        ) as mock_uninstall:
            ret = args.func(args)
            self.assertEqual(ret, 0)
            mock_uninstall.assert_called_once_with(
                self.fuchsia_dir, dry_run=True
            )
            self.assertIn(
                "[DRY RUN] Would remove 1 Git hook across checkout.",
                self.stdout,
            )

    def test_run_disable_failure(self) -> None:
        res = githooks.HookOperationResult(
            modified=[],
            failed=[(self.fuchsia_dir, "File is locked")],
        )

        parser = argparse.ArgumentParser()
        subparsers = parser.add_subparsers(dest="subcommand")
        hooks.register_subcommand(subparsers)
        args = parser.parse_args(["hooks", "disable"])

        with mock.patch(
            "agents.lib.githooks.uninstall_git_hooks", return_value=res
        ):
            ret = args.func(args)
            self.assertEqual(ret, 1)
            self.assertIn("Warning: Failed to remove Git hooks", self.stderr)

    def test_run_status(self) -> None:
        res = githooks.HookStatusResult(total_repos=2, configured_repos=1)

        parser = argparse.ArgumentParser()
        subparsers = parser.add_subparsers(dest="subcommand")
        hooks.register_subcommand(subparsers)
        args = parser.parse_args(["hooks", "status"])

        with mock.patch(
            "agents.lib.githooks.get_git_hooks_status", return_value=res
        ) as mock_status:
            ret = args.func(args)
            self.assertEqual(ret, 0)
            mock_status.assert_called_once_with(self.fuchsia_dir)
            self.assertIn(
                "Git hooks: Configured across 1/2 repositories.", self.stdout
            )

    def test_hooks_no_subcommand_prints_help(self) -> None:
        parser = argparse.ArgumentParser()
        subparsers = parser.add_subparsers(dest="subcommand")
        hooks.register_subcommand(subparsers)
        args = parser.parse_args(["hooks"])
        ret = args.func(args)
        self.assertEqual(ret, 1)
        self.assertIn("usage:", self.stderr)


if __name__ == "__main__":
    unittest.main()
