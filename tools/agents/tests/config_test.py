#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for config module, atomic writing, and backup creation."""

from __future__ import annotations

import io
import json
import os
import pathlib
import tempfile
import unittest
from unittest import mock

from agents.lib import config


class ConfigTest(unittest.TestCase):
    """Hermetic unit tests for config operations."""

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

    def test_get_default_config_path_fallback(self) -> None:
        """Verify default fallback to ~/.gemini/config/config.json when no env vars are set."""
        with mock.patch.dict(os.environ, {}, clear=True):
            with mock.patch.object(
                pathlib.Path, "home", return_value=pathlib.Path("/mock/home")
            ):
                expected = pathlib.Path("/mock/home/.gemini/config/config.json")
                self.assertEqual(config.get_default_config_path(), expected)

    def test_get_default_config_path_gemini_config_dir(self) -> None:
        """Verify GEMINI_CONFIG_DIR environment variable override."""
        with mock.patch.dict(
            os.environ, {"GEMINI_CONFIG_DIR": "/custom/config/dir"}, clear=True
        ):
            expected = pathlib.Path("/custom/config/dir/config.json")
            self.assertEqual(config.get_default_config_path(), expected)

    def test_get_default_config_path_gemini_home(self) -> None:
        """Verify GEMINI_HOME environment variable override."""
        with mock.patch.dict(
            os.environ, {"GEMINI_HOME": "/custom/gemini/home"}, clear=True
        ):
            expected = pathlib.Path("/custom/gemini/home/config/config.json")
            self.assertEqual(config.get_default_config_path(), expected)

    def test_get_default_config_path_precedence(self) -> None:
        """Verify GEMINI_CONFIG_DIR takes precedence over GEMINI_HOME."""
        with mock.patch.dict(
            os.environ,
            {
                "GEMINI_CONFIG_DIR": "/custom/config/dir",
                "GEMINI_HOME": "/custom/gemini/home",
            },
            clear=True,
        ):
            expected = pathlib.Path("/custom/config/dir/config.json")
            self.assertEqual(config.get_default_config_path(), expected)

    def test_load_config_nonexistent(self) -> None:
        """Verify load_config returns empty dict for nonexistent file."""
        nonexistent = self.mock_root / "does_not_exist.json"
        self.assertEqual(config.load_config(nonexistent), {})

    def test_load_config_valid(self) -> None:
        """Verify load_config correctly parses JSON object."""
        cfg_file = self.mock_root / "config.json"
        cfg_file.write_text('{"foo": "bar", "num": 42}', encoding="utf-8")
        self.assertEqual(
            config.load_config(cfg_file), {"foo": "bar", "num": 42}
        )

    def test_load_config_non_dict(self) -> None:
        """Verify load_config returns empty dict for JSON list or primitive."""
        cfg_file = self.mock_root / "config.json"
        cfg_file.write_text("[1, 2, 3]", encoding="utf-8")
        self.assertEqual(config.load_config(cfg_file), {})

    def test_load_config_invalid_json(self) -> None:
        """Verify load_config raises JSONDecodeError on syntax error."""
        cfg_file = self.mock_root / "config.json"
        cfg_file.write_text("{bad json", encoding="utf-8")
        with self.assertRaises(json.JSONDecodeError):
            config.load_config(cfg_file)

    def test_save_config_atomic(self) -> None:
        """Verify save_config_atomic writes valid JSON."""
        cfg_file = self.mock_root / "nested" / "config.json"
        data = {"userSettings": {"enabled": True}}
        config.save_config_atomic(cfg_file, data)
        self.assertTrue(cfg_file.is_file())
        with cfg_file.open("r", encoding="utf-8") as fh:
            loaded = json.load(fh)
        self.assertEqual(loaded, data)

    def test_merge_grants(self) -> None:
        """Verify grant deduplication and order preservation."""
        existing = ["grant1", "grant2"]
        to_add = ["grant2", "grant3", "grant1", "grant4"]
        updated, added = config.merge_grants(existing, to_add)
        self.assertEqual(updated, ["grant1", "grant2", "grant3", "grant4"])
        self.assertEqual(added, ["grant3", "grant4"])

    def test_apply_grants_new_file(self) -> None:
        """Verify apply_grants creates new config atomically."""
        cfg_file = self.mock_root / "config.json"
        success = config.apply_grants(
            config_path=cfg_file,
            allow=["command(fx build)"],
            deny=["command(fx clean)"],
            ask=["command(fx reboot)"],
            dry_run=False,
        )
        self.assertTrue(success)
        self.assertTrue(cfg_file.is_file())
        with cfg_file.open("r", encoding="utf-8") as fh:
            data = json.load(fh)
        grants = data["userSettings"]["globalPermissionGrants"]
        self.assertEqual(grants["allow"], ["command(fx build)"])
        self.assertEqual(grants["deny"], ["command(fx clean)"])
        self.assertEqual(grants["ask"], ["command(fx reboot)"])

    def test_apply_grants_existing_file_and_backup(self) -> None:
        """Verify preexisting config is backed up to .bak before modification."""
        cfg_file = self.mock_root / "config.json"
        initial_data = {
            "userSettings": {
                "customSetting": 123,
                "globalPermissionGrants": {
                    "allow": ["command(custom_cmd)"],
                    "deny": [],
                    "ask": [],
                },
            },
            "rootField": "foo",
        }
        original_json = json.dumps(initial_data, indent=2)
        cfg_file.write_text(original_json, encoding="utf-8")

        success = config.apply_grants(
            config_path=cfg_file,
            allow=["command(fx build)"],
            deny=["command(fx clean)"],
            ask=[],
            dry_run=False,
        )
        self.assertTrue(success)

        bak_file = cfg_file.with_name(f"{cfg_file.name}.bak")
        self.assertTrue(bak_file.is_file())
        self.assertEqual(bak_file.read_text(encoding="utf-8"), original_json)

        data = json.loads(cfg_file.read_text(encoding="utf-8"))
        self.assertEqual(data["rootField"], "foo")
        self.assertEqual(data["userSettings"]["customSetting"], 123)
        grants = data["userSettings"]["globalPermissionGrants"]
        self.assertEqual(
            grants["allow"], ["command(custom_cmd)", "command(fx build)"]
        )
        self.assertEqual(grants["deny"], ["command(fx clean)"])

    def test_apply_grants_dry_run(self) -> None:
        """Verify dry_run=True does not touch disk."""
        cfg_file = self.mock_root / "config.json"
        success = config.apply_grants(
            config_path=cfg_file,
            allow=["command(fx build)"],
            deny=[],
            ask=[],
            dry_run=True,
        )
        self.assertTrue(success)
        self.assertFalse(cfg_file.exists())

    def test_apply_grants_invalid_json(self) -> None:
        """Verify invalid JSON in existing config.json is gracefully handled."""
        cfg_file = self.mock_root / "config.json"
        cfg_file.write_text("{invalid_json", encoding="utf-8")

        success = config.apply_grants(
            config_path=cfg_file,
            allow=["command(fx build)"],
            deny=[],
            ask=[],
            dry_run=False,
        )
        self.assertFalse(success)
        self.assertIn("Error reading JSON from", self.mock_stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
