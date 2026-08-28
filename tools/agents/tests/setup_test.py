#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for the agents setup command handler."""

from __future__ import annotations

import argparse
import io
import json
import pathlib
import tempfile
import unittest
from unittest import mock

from agents.commands import setup
from agents.lib import permissions, state


class SetupCommandTest(unittest.TestCase):
    """Tests for setup command parser and execution."""

    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        self.mock_root = pathlib.Path(self.temp_dir.name)
        self.state_dir = self.mock_root / "state"
        self.backups_dir = self.state_dir / "backups"
        self.state_dir.mkdir(parents=True, exist_ok=True)
        self.backups_dir.mkdir(parents=True, exist_ok=True)

        self.stdout_patch = mock.patch("sys.stdout", new_callable=io.StringIO)
        self.mock_stdout = self.stdout_patch.start()
        self.addCleanup(self.stdout_patch.stop)

        self.stderr_patch = mock.patch("sys.stderr", new_callable=io.StringIO)
        self.mock_stderr = self.stderr_patch.start()
        self.addCleanup(self.stderr_patch.stop)

        self.fuchsia_dir = self.mock_root / "fuchsia"
        self.perm_dir = self.fuchsia_dir / ".agents" / "config" / "permissions"
        self.perm_dir.mkdir(parents=True, exist_ok=True)

        self.fuchsia_dir_patch = mock.patch.object(
            permissions, "find_fuchsia_dir", return_value=self.fuchsia_dir
        )
        self.fuchsia_dir_patch.start()
        self.addCleanup(self.fuchsia_dir_patch.stop)

        (self.perm_dir / "read_only.txt").write_text(
            "fx status\n", encoding="utf-8"
        )
        (self.perm_dir / "local_changes.txt").write_text(
            "git commit\n", encoding="utf-8"
        )
        (self.perm_dir / "never_allow.txt").write_text(
            "git clean\n", encoding="utf-8"
        )
        (self.perm_dir / "device_ops.txt").write_text(
            "fx ota\n", encoding="utf-8"
        )
        (self.perm_dir / "cache_destruction.txt").write_text(
            "fx clean\n", encoding="utf-8"
        )
        (self.perm_dir / "batch_execution.txt").write_text(
            "find\n", encoding="utf-8"
        )
        (self.perm_dir / "external_changes.txt").write_text(
            "git push\n", encoding="utf-8"
        )

    def test_add_arguments(self) -> None:
        """Verify CLI arguments are added correctly."""
        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)

        args = parser.parse_args(
            [
                "--profile",
                "read-only",
                "--allow",
                "fx build",
                "--deny",
                "git push --force",
                "--dry-run",
            ]
        )
        self.assertEqual(args.profile, "read-only")
        self.assertEqual(args.allow, ["fx build"])
        self.assertEqual(args.deny, ["git push --force"])
        self.assertTrue(args.dry_run)

    def test_run_dry_run(self) -> None:
        """Verify dry-run mode does not create or modify files."""
        config_path = self.mock_root / "config.json"
        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)
        args = parser.parse_args(
            [
                "--config",
                str(config_path),
                "--state-dir",
                str(self.state_dir),
                "--allow",
                "fx build",
                "--dry-run",
            ]
        )

        exit_code = setup.run(args)
        self.assertEqual(exit_code, 0)
        self.assertFalse(config_path.exists())
        self.assertIn("[DRY RUN]", self.mock_stdout.getvalue())

    def test_run_apply_grants(self) -> None:
        """Verify grants are applied and written to config."""
        config_path = self.mock_root / "config.json"
        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)
        args = parser.parse_args(
            [
                "--config",
                str(config_path),
                "--state-dir",
                str(self.state_dir),
                "--allow",
                "fx build",
                "--deny",
                "git push --force",
            ]
        )

        exit_code = setup.run(args)
        self.assertEqual(exit_code, 0)
        self.assertTrue(config_path.exists())

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
        """Verify setting profile populates category grants."""
        config_path = self.mock_root / "config.json"
        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)
        args = parser.parse_args(
            [
                "--config",
                str(config_path),
                "--state-dir",
                str(self.state_dir),
                "-p",
                "read-only",
            ]
        )

        exit_code = setup.run(args)
        self.assertEqual(exit_code, 0)

        with config_path.open("r", encoding="utf-8") as fh:
            data = json.load(fh)
        grants = data["userSettings"]["globalPermissionGrants"]
        self.assertTrue(
            any(
                g.startswith("command(regex:") and "fx" in g and "status" in g
                for g in grants["allow"]
            )
        )
        self.assertTrue(
            any(
                g.startswith("command(regex:") and "git" in g and "clean" in g
                for g in grants["deny"]
            )
        )

    def test_run_default_profile(self) -> None:
        """Verify running setup without explicit args defaults to local-changes profile."""
        config_path = self.mock_root / "config.json"
        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)
        args = parser.parse_args(
            ["--config", str(config_path), "--state-dir", str(self.state_dir)]
        )

        exit_code = setup.run(args)

        self.assertEqual(exit_code, 0)
        with config_path.open("r", encoding="utf-8") as fh:
            data = json.load(fh)
        grants = data["userSettings"]["globalPermissionGrants"]
        self.assertTrue(
            any(
                g.startswith("command(regex:") and "fx" in g and "status" in g
                for g in grants["allow"]
            )
        )
        self.assertTrue(
            any(
                g.startswith("command(regex:") and "git" in g and "commit" in g
                for g in grants["allow"]
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
                "--state-dir",
                str(self.state_dir),
                "--allow-list",
                str(list_file),
            ]
        )

        exit_code = setup.run(args)
        self.assertEqual(exit_code, 0)

        with config_path.open("r", encoding="utf-8") as fh:
            data = json.load(fh)
        grants = data["userSettings"]["globalPermissionGrants"]
        self.assertIn("command(my_custom_tool)", grants["allow"])

    def test_run_status(self) -> None:
        """Verify --status option prints status summary."""
        config_path = self.mock_root / "config.json"
        config_path.write_text('{"userSettings": {}}', encoding="utf-8")

        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)
        args = parser.parse_args(
            [
                "--status",
                "--config",
                str(config_path),
                "--state-dir",
                str(self.state_dir),
            ]
        )

        exit_code = setup.run(args)
        self.assertEqual(exit_code, 0)
        self.assertIn(
            "=== AI Coding Agent Configuration Status ===",
            self.mock_stdout.getvalue(),
        )

    def test_run_rollback(self) -> None:
        """Verify --rollback executes rollback."""
        config_path = self.mock_root / "config.json"
        config_path.write_text('{"v": 1}', encoding="utf-8")
        backup = state.create_backup(config_path, self.backups_dir)

        journal = state.StateJournal(
            active_profile="local-changes",
            history=[
                {
                    "timestamp": "t1",
                    "profile": "read-only",
                    "backup_file": None,
                },
                {
                    "timestamp": "t2",
                    "profile": "local-changes",
                    "backup_file": str(backup),
                },
            ],
            managed_grants={"allow": [], "deny": [], "ask": []},
        )
        state.save_state(journal, self.state_dir / "state.json")

        config_path.write_text('{"v": 2}', encoding="utf-8")

        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)
        args = parser.parse_args(
            [
                "--rollback",
                "--config",
                str(config_path),
                "--state-dir",
                str(self.state_dir),
            ]
        )

        exit_code = setup.run(args)
        self.assertEqual(exit_code, 0)
        with config_path.open("r", encoding="utf-8") as fh:
            data = json.load(fh)
        self.assertEqual(data, {"v": 1})

    def test_run_reset(self) -> None:
        """Verify --reset executes reset."""
        config_path = self.mock_root / "config.json"
        config_path.write_text(
            json.dumps(
                {
                    "userSettings": {
                        "globalPermissionGrants": {
                            "allow": [
                                "command(fx status)",
                                "command(custom_tool)",
                            ],
                            "deny": ["command(git clean)"],
                            "ask": [],
                        }
                    }
                }
            ),
            encoding="utf-8",
        )

        journal = state.StateJournal(
            active_profile="local-changes",
            history=[
                {
                    "timestamp": "t1",
                    "profile": "local-changes",
                    "backup_file": None,
                }
            ],
            managed_grants={
                "allow": ["command(fx status)"],
                "deny": ["command(git clean)"],
                "ask": [],
            },
        )
        state.save_state(journal, self.state_dir / "state.json")

        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)
        args = parser.parse_args(
            [
                "--reset",
                "--config",
                str(config_path),
                "--state-dir",
                str(self.state_dir),
            ]
        )

        exit_code = setup.run(args)
        self.assertEqual(exit_code, 0)

        with config_path.open("r", encoding="utf-8") as fh:
            data = json.load(fh)
        grants = data["userSettings"]["globalPermissionGrants"]
        self.assertEqual(grants["allow"], ["command(custom_tool)"])
        self.assertEqual(grants["deny"], [])
        self.assertIn(
            "Purged Fuchsia-managed rules", self.mock_stdout.getvalue()
        )

    def test_run_restarts_daemons_on_success(self) -> None:
        """Verify daemons are restarted on apply, rollback, and reset."""
        config_path = self.mock_root / "config.json"
        config_path.write_text("{}", encoding="utf-8")
        backup = state.create_backup(config_path, self.backups_dir)

        journal = state.StateJournal(
            active_profile="local-changes",
            history=[
                {
                    "timestamp": "t1",
                    "profile": "local-changes",
                    "backup_file": str(backup),
                }
            ],
            managed_grants={"allow": [], "deny": [], "ask": []},
        )
        state.save_state(journal, self.state_dir / "state.json")

        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)

        with mock.patch.object(setup, "_restart_daemons") as mock_restart:
            # Test apply
            args_apply = parser.parse_args(
                [
                    "--config",
                    str(config_path),
                    "--state-dir",
                    str(self.state_dir),
                    "-p",
                    "read-only",
                ]
            )
            setup.run(args_apply)
            mock_restart.assert_called_with(self.fuchsia_dir, False)
            mock_restart.reset_mock()

            # Test rollback
            args_rollback = parser.parse_args(
                [
                    "--rollback",
                    "--config",
                    str(config_path),
                    "--state-dir",
                    str(self.state_dir),
                ]
            )
            setup.run(args_rollback)
            mock_restart.assert_called_with(self.fuchsia_dir, False)
            mock_restart.reset_mock()

            # Test reset
            args_reset = parser.parse_args(
                [
                    "--reset",
                    "--config",
                    str(config_path),
                    "--state-dir",
                    str(self.state_dir),
                ]
            )
            setup.run(args_reset)
            mock_restart.assert_called_with(self.fuchsia_dir, False)

    def test_run_adhoc_grant_preserves_active_profile(self) -> None:
        """Verify adding an ad-hoc grant preserves the existing active profile."""
        config_path = self.mock_root / "config.json"
        parser = argparse.ArgumentParser()
        setup.add_arguments(parser)

        # 1. Apply read-only profile first
        args_init = parser.parse_args(
            [
                "--config",
                str(config_path),
                "--state-dir",
                str(self.state_dir),
                "-p",
                "read-only",
            ]
        )
        self.assertEqual(setup.run(args_init), 0)

        # Verify active profile is read-only
        j1 = state.load_state(self.state_dir / "state.json")
        self.assertEqual(j1.active_profile, "read-only")

        # 2. Add ad-hoc grant without specifying --profile
        args_adhoc = parser.parse_args(
            [
                "--config",
                str(config_path),
                "--state-dir",
                str(self.state_dir),
                "--allow",
                "fx custom_tool",
            ]
        )
        self.assertEqual(setup.run(args_adhoc), 0)

        # Verify active profile remains read-only
        j2 = state.load_state(self.state_dir / "state.json")
        self.assertEqual(j2.active_profile, "read-only")

        # Verify config contains both read-only rules and custom tool rule
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
                g.startswith("command(regex:") and "custom_tool" in g
                for g in grants["allow"]
            )
        )


if __name__ == "__main__":
    unittest.main()
