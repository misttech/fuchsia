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

        exit_code = setup.run(args)
        self.assertEqual(exit_code, 0)

        with config_path.open("r", encoding="utf-8") as fh:
            data = json.load(fh)
        grants = data["userSettings"]["globalPermissionGrants"]
        self.assertEqual(grants["allow"], ["command(fx test)"])
        self.assertEqual(grants["deny"], ["command(rm -rf)"])
        self.assertEqual(grants["ask"], ["command(reboot)"])


if __name__ == "__main__":
    unittest.main()
