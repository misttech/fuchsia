#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import io
import json
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

# Ensure fuchsia root is in sys.path
_FUCHSIA_ROOT = Path(__file__).resolve().parent.parent.parent
if str(_FUCHSIA_ROOT) not in sys.path:
    sys.path.insert(0, str(_FUCHSIA_ROOT))

from build.scripts import build_timeout, graceful_timeout, managed_process


class BuildTimeoutTest(unittest.TestCase):
    def test_default_build_timeout_schedule_valid(self) -> None:
        schedule = build_timeout.DEFAULT_BUILD_TIMEOUT_SCHEDULE
        self.assertGreater(len(schedule.steps), 0)
        self.assertIsNotNone(schedule.final)
        events = schedule.to_events(hard_timeout=3600.0)
        self.assertGreater(len(events), 0)

    def test_build_timeout_default_in_code_schedule(self) -> None:
        mock_proc = mock.Mock(spec=managed_process.ManagedProcess)
        mock_proc.poll.return_value = 0
        mock_proc.returncode = 0
        mock_proc.wait.return_value = 0

        with mock.patch.object(
            managed_process.ManagedProcess, "spawn", return_value=mock_proc
        ) as mock_spawn:
            rc = build_timeout.main(["--timeout", "90m", "--", "echo", "hi"])

        self.assertEqual(rc, 0)
        mock_spawn.assert_called_once_with(["echo", "hi"], separate_pgrp=True)

    def test_build_timeout_no_separate_pgrp(self) -> None:
        mock_proc = mock.Mock(spec=managed_process.ManagedProcess)
        mock_proc.poll.return_value = 0
        mock_proc.returncode = 0
        mock_proc.wait.return_value = 0

        with mock.patch.object(
            managed_process.ManagedProcess, "spawn", return_value=mock_proc
        ) as mock_spawn:
            rc = build_timeout.main(
                [
                    "--timeout",
                    "90m",
                    "--no-separate-pgrp",
                    "--",
                    "echo",
                    "hi",
                ]
            )

        self.assertEqual(rc, 0)
        mock_spawn.assert_called_once_with(["echo", "hi"], separate_pgrp=False)

    def test_build_timeout_grace_period_forwarded(self) -> None:
        mock_proc = mock.Mock(spec=managed_process.ManagedProcess)
        mock_proc.poll.return_value = 0
        mock_proc.returncode = 0
        mock_proc.wait.return_value = 0

        with mock.patch.object(
            managed_process.ManagedProcess, "spawn", return_value=mock_proc
        ):
            rc = build_timeout.main(
                [
                    "--timeout",
                    "90m",
                    "--grace-period",
                    "60s",
                    "--",
                    "echo",
                    "hi",
                ]
            )
        self.assertEqual(rc, 0)

    def test_build_timeout_expired_presents_diagnostics(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            tmp_path = Path(tmpdir)
            log_dir = tmp_path / "timeout_logs"
            log_dir.mkdir(parents=True, exist_ok=True)

            snapshot_data = {
                "pid": 1234,
                "ppid": 1,
                "comm": "ninja",
                "state": "S",
                "cmdline": ["ninja", "-C", "out"],
                "children": [
                    {
                        "pid": 5678,
                        "ppid": 1234,
                        "comm": "rustc",
                        "state": "R",
                        "cmdline": ["rustc", "main.rs"],
                        "children": [],
                    }
                ],
            }
            (log_dir / "pstree_stop.json").write_text(
                json.dumps(snapshot_data), encoding="utf-8"
            )

            mock_proc = mock.Mock(spec=managed_process.ManagedProcess)
            mock_proc.pid = 1234
            mock_proc.returncode = None
            mock_proc.poll.return_value = None
            mock_proc.wait.side_effect = [
                subprocess.TimeoutExpired(cmd="test", timeout=10.0),
                0,
                0,
            ]

            timeout_res = graceful_timeout.TimeoutResult(
                returncode=build_timeout.EXIT_TIMEOUT,
                timed_out=True,
            )

            with (
                mock.patch.object(
                    managed_process.ManagedProcess,
                    "spawn",
                    return_value=mock_proc,
                ),
                mock.patch.object(
                    graceful_timeout.GracefulTimeoutRunner,
                    "_resolve_returncode",
                    return_value=timeout_res,
                ),
            ):
                stderr_buf = io.StringIO()
                with contextlib.redirect_stderr(stderr_buf):
                    rc = build_timeout.main(
                        [
                            "--timeout",
                            "60s",
                            "--log-dir",
                            str(log_dir),
                            "--",
                            "ninja",
                        ]
                    )

            self.assertEqual(rc, build_timeout.EXIT_TIMEOUT)
            output = stderr_buf.getvalue()
            self.assertIn("BUILD TIMED OUT (configured timeout: 60s)", output)
            self.assertIn("=== Active Leaf Processes at Timeout [stop]", output)
            self.assertIn("5678 rustc", output)
            self.assertIn("--- Process Tree [stop] ---", output)
            self.assertIn("1234 ninja", output)

    def test_build_timeout_cancelled_banner_printed(self) -> None:
        mock_proc = mock.Mock(spec=managed_process.ManagedProcess)
        mock_proc.pid = 1234
        mock_proc.returncode = None
        mock_proc.poll.return_value = None

        cancelled_res = graceful_timeout.TimeoutResult(
            returncode=130,
            timed_out=False,
            interrupted_signal=signal.SIGINT,
        )

        with (
            mock.patch.object(
                managed_process.ManagedProcess,
                "spawn",
                return_value=mock_proc,
            ),
            mock.patch.object(
                graceful_timeout.GracefulTimeoutRunner,
                "_resolve_returncode",
                return_value=cancelled_res,
            ),
        ):
            stderr_buf = io.StringIO()
            with contextlib.redirect_stderr(stderr_buf):
                rc = build_timeout.main(
                    [
                        "--timeout",
                        "60s",
                        "--",
                        "ninja",
                    ]
                )

        self.assertEqual(rc, 130)
        output = stderr_buf.getvalue()
        self.assertIn("BUILD CANCELLED (interrupted by", output)

    def test_format_timeout_diagnostics(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            tmp_path = Path(tmpdir)
            log_dir = tmp_path / "timeout_logs"
            log_dir.mkdir(parents=True, exist_ok=True)

            snapshot_data = {
                "pid": 1234,
                "ppid": 1,
                "comm": "ninja",
                "state": "S",
                "cmdline": ["ninja", "-C", "out"],
                "children": [
                    {
                        "pid": 5678,
                        "ppid": 1234,
                        "comm": "rustc",
                        "state": "R",
                        "cmdline": ["rustc", "main.rs"],
                        "children": [],
                    }
                ],
            }
            (log_dir / "pstree_stop.json").write_text(
                json.dumps(snapshot_data), encoding="utf-8"
            )

            lines = list(build_timeout.format_timeout_diagnostics(log_dir))

            self.assertIn(
                f"Process tree diagnostics saved to: {log_dir}", lines
            )
            self.assertIn(
                "=== Active Leaf Processes at Timeout [stop] (1 active) ===",
                lines,
            )
            self.assertIn("--- Process Tree [stop] ---", lines)
            self.assertTrue(any("5678 rustc" in line for line in lines))

    def test_build_timeout_exclude_process_from_summary(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            tmp_path = Path(tmpdir)
            log_dir = tmp_path / "timeout_logs"
            log_dir.mkdir(parents=True, exist_ok=True)

            snapshot_data = {
                "pid": 1234,
                "ppid": 1,
                "comm": "ninja",
                "state": "S",
                "cmdline": ["ninja"],
                "children": [
                    {
                        "pid": 5678,
                        "ppid": 1234,
                        "comm": "fake_daemon",
                        "state": "S",
                        "cmdline": ["fake_daemon"],
                        "children": [],
                    },
                    {
                        "pid": 5679,
                        "ppid": 1234,
                        "comm": "rustc",
                        "state": "R",
                        "cmdline": ["rustc", "foo.rs"],
                        "children": [],
                    },
                ],
            }
            (log_dir / "pstree_stop.json").write_text(
                json.dumps(snapshot_data), encoding="utf-8"
            )

            lines = list(
                build_timeout.format_timeout_diagnostics(
                    log_dir,
                    filter_fn=lambda n: n.comm != "fake_daemon",
                )
            )
            summary_section = "\n".join(lines)
            self.assertIn(
                "Active Leaf Processes at Timeout [stop] (1 active)",
                summary_section,
            )
            self.assertIn("[CPU] rustc foo.rs (pid 5679)", summary_section)
            # fake_daemon should NOT be in the active leaf section
            # but MUST still be present in the process tree section
            self.assertIn("--- Process Tree [stop] ---", summary_section)
            self.assertIn("5678 fake_daemon", summary_section)

    def test_present_timeout_diagnostics_banner(self) -> None:
        result = graceful_timeout.TimeoutResult(returncode=124, timed_out=True)
        stderr_buf = io.StringIO()
        with contextlib.redirect_stderr(stderr_buf):
            build_timeout.present_timeout_diagnostics(
                None, result, timeout_str="45m"
            )
        output = stderr_buf.getvalue()
        self.assertIn(
            "[build_timeout.py] BUILD TIMED OUT (configured timeout: 45m)",
            output,
        )

    def test_load_timeout_snapshots_handles_corrupt_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            log_dir = Path(tmpdir)
            (log_dir / "pstree_bad_json.json").write_text(
                "invalid json", encoding="utf-8"
            )
            (log_dir / "pstree_bad_schema.json").write_text(
                json.dumps({"wrong": "schema"}), encoding="utf-8"
            )

            stderr_buf = io.StringIO()
            with contextlib.redirect_stderr(stderr_buf):
                snapshots = build_timeout.load_timeout_snapshots(log_dir)

            self.assertEqual(snapshots, {})
            output = stderr_buf.getvalue()
            self.assertIn("Failed to parse snapshot JSON", output)
            self.assertIn("Invalid process tree schema", output)

    def test_timeout_metadata_serialization_roundtrip(self) -> None:
        meta = build_timeout.TimeoutMetadata(
            log_dir="/path/to/logs",
            snapshots=["pstree_stop.json"],
            active_processes=["[CPU] rustc (pid 1)"],
            total_matching_processes=1,
            total_leaf_processes=1,
        )
        data = meta.to_dict()
        self.assertEqual(data["log_dir"], "/path/to/logs")
        self.assertEqual(data["snapshots"], ["pstree_stop.json"])
        self.assertEqual(data["active_processes"], ["[CPU] rustc (pid 1)"])
        self.assertEqual(data["total_matching_processes"], 1)
        self.assertEqual(data["total_leaf_processes"], 1)

        meta2 = build_timeout.TimeoutMetadata.from_dict(data)
        self.assertEqual(meta, meta2)

    def test_timeout_metadata_collect(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            timeout_dir = Path(tmpdir)
            snapshot_data = {
                "pid": 100,
                "ppid": 1,
                "comm": "ninja",
                "state": "S",
                "cmdline": ["ninja"],
                "children": [
                    {
                        "pid": 200,
                        "ppid": 100,
                        "comm": "rustc",
                        "state": "R",
                        "cmdline": ["rustc", "foo.rs"],
                        "children": [],
                    }
                ],
            }
            (timeout_dir / "pstree_stop.json").write_text(
                json.dumps(snapshot_data), encoding="utf-8"
            )

            meta = build_timeout.TimeoutMetadata.collect(timeout_dir)
            self.assertIsNotNone(meta)
            assert meta is not None
            self.assertEqual(meta.snapshots, ["pstree_stop.json"])
            self.assertEqual(meta.total_matching_processes, 1)
            self.assertEqual(meta.total_leaf_processes, 1)
            self.assertEqual(len(meta.active_processes), 1)
            self.assertIn(
                "[CPU] rustc foo.rs (pid 200)", meta.active_processes[0]
            )


if __name__ == "__main__":
    unittest.main()
