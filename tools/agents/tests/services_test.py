# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for daemon service discovery and management."""

from __future__ import annotations

import io
import pathlib
import tempfile
import unittest
from unittest import mock

from agents.lib import services


class ServicesTest(unittest.TestCase):
    """Hermetic unit tests for daemon service discovery and restart."""

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

    def test_find_daemon_services(self) -> None:
        fuchsia_dir = self.mock_root
        public_cfg = fuchsia_dir / ".agents" / "config"
        public_cfg.mkdir(parents=True, exist_ok=True)
        (public_cfg / "services.txt").write_text(
            "service_a\nservice_b\n# comment\n", encoding="utf-8"
        )

        vendor_cfg = fuchsia_dir / "vendor" / "google" / ".agents" / "config"
        vendor_cfg.mkdir(parents=True, exist_ok=True)
        (vendor_cfg / "services.txt").write_text(
            "service_c\nservice_a\n", encoding="utf-8"
        )

        found = services.find_daemon_services(fuchsia_dir)
        self.assertEqual(found, ["service_a", "service_b", "service_c"])

    def test_restart_daemons_dry_run(self) -> None:
        with mock.patch("shutil.which", return_value="/bin/systemctl"):
            with mock.patch("subprocess.run") as mock_run:
                services.restart_daemons(["foo-service"], dry_run=True)
                mock_run.assert_not_called()
                self.assertIn(
                    "Would run: systemctl --user try-restart foo-service",
                    self.mock_stdout.getvalue(),
                )

    def test_restart_daemons_actual(self) -> None:
        with mock.patch("shutil.which", return_value="/bin/systemctl"):
            with mock.patch("subprocess.run") as mock_run:
                services.restart_daemons(["foo-service"], dry_run=False)
                mock_run.assert_called_once_with(
                    ["systemctl", "--user", "try-restart", "foo-service"],
                    capture_output=True,
                    check=False,
                )

    def test_restart_daemons_no_systemctl(self) -> None:
        with mock.patch("shutil.which", return_value=None):
            with mock.patch("subprocess.run") as mock_run:
                services.restart_daemons(["service_a"])
                mock_run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
