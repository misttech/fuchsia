# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for Mobly driver's __init__.py."""

import os
import signal
import subprocess
import sys
import unittest
from datetime import timedelta
from typing import Any
from unittest import mock

import mobly_driver
from parameterized import parameterized


class MoblyDriverLibTest(unittest.TestCase):
    """Mobly Driver lib tests"""

    def setUp(self) -> None:
        self.mock_tmp = mock.Mock()
        self.mock_process = mock.Mock()
        self.mock_driver = mock.Mock()
        self.mock_driver.generate_test_config.return_value = ""
        # Tests that mock out Popen must never walk the real /proc of the test
        # runner (and kill whatever children it may have). Functional tests
        # that need the real tree walk stop this patcher explicitly.
        self._list_processes_patcher = mock.patch(
            "mobly_driver._list_processes", return_value={}
        )
        self._list_processes_patcher.start()
        self.addCleanup(self._list_processes_patcher.stop)

    @mock.patch("builtins.print")
    @mock.patch("subprocess.Popen")
    def test_run_success(self, mock_popen: Any, *unused_args: Any) -> None:
        """Test case to ensure run succeeds"""
        self.mock_process.wait.return_value = 0
        mock_popen.return_value.__enter__.return_value = self.mock_process

        mobly_driver.run(self.mock_driver, "/py/path", "/test/path")

        self.mock_driver.generate_test_config.assert_called()
        self.mock_driver.teardown.assert_called()

    @parameterized.expand(
        [
            ["invalid_driver", None, "/py/path", "/test/path", 0],
            ["invalid_python_path", mock.Mock(), "", "/test/path", 0],
            ["invalid_test_path", mock.Mock(), "/py/path", "", 0],
            ["invalid_timeout", mock.Mock(), "/py/path", "/test/path", -1],
        ]
    )
    def test_run_invalid_argument_raises_exception(
        self,
        unused_name: str,
        driver: Any,
        python_path: str,
        test_path: str,
        timeout_sec: int,
    ) -> None:
        """Test case to ensure exception raised on invalid args"""
        with self.assertRaises(ValueError):
            mobly_driver.run(
                driver,
                python_path,
                test_path,
                timeout=timedelta(seconds=timeout_sec),
            )

    @mock.patch("builtins.print")
    @mock.patch("subprocess.Popen")
    def test_run_mobly_test_failure_raises_exception(
        self, mock_popen: Any, *unused_args: Any
    ) -> None:
        """Test case to ensure exception raised on test failure"""
        expected_return_code = 42
        self.mock_process.wait.return_value = expected_return_code
        mock_popen.return_value.__enter__.return_value = self.mock_process

        with self.assertRaises(mobly_driver.MoblyTestFailureException) as cm:
            mobly_driver.run(self.mock_driver, "/py/path", "/test/path")
        self.assertEqual(cm.exception.return_code, expected_return_code)

    @mock.patch("builtins.print")
    @mock.patch("subprocess.Popen")
    def test_run_mobly_test_timeout_exception(
        self, mock_popen: Any, *unused_args: Any
    ) -> None:
        """Test case to ensure exception raised on test timeout"""
        mock_popen.return_value.__enter__.return_value = self.mock_process
        self.mock_process.wait.side_effect = [
            subprocess.TimeoutExpired("", 0),  # Main test times out
            # No cleanup period
            subprocess.TimeoutExpired("", 0),  # Final warning 1 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 2 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 3 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 4 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 5 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 6 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 7 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 8 times out
            0,  # Kill wait succeeds
        ]

        with self.assertRaises(mobly_driver.MoblyTestTimeoutException):
            mobly_driver.run(self.mock_driver, "/py/path", "/test/path")
        self.mock_process.kill.assert_called()

    @mock.patch("builtins.print")
    @mock.patch("subprocess.Popen")
    def test_run_with_cleanup_period_sends_sigterm(
        self, mock_popen: Any, *unused_args: Any
    ) -> None:
        """Testcase to ensure SIGTERM is sent when main test times out with cleanup_period set"""
        mock_popen.return_value.__enter__.return_value = self.mock_process
        self.mock_process.wait.side_effect = [
            subprocess.TimeoutExpired("", 0),  # Main test times out
            0,  # Cleanup period succeeds
        ]

        with self.assertRaises(mobly_driver.MoblyTestTimeoutException):
            mobly_driver.run(
                self.mock_driver,
                "/py/path",
                "/test/path",
                timeout=timedelta(seconds=30),
                cleanup_period=timedelta(seconds=10),
            )

        self.mock_process.terminate.assert_called_once()
        self.mock_process.kill.assert_not_called()
        self.mock_driver.teardown.assert_called_once_with(
            timed_out=True,
            test_timeout=timedelta(0),
        )

    @mock.patch("builtins.print")
    @mock.patch("subprocess.Popen")
    def test_run_with_bo_cleanup_period_sends_sigterm_grace_period_succeeds(
        self, mock_popen: Any, *unused_args: Any
    ) -> None:
        """Testcase to ensure SIGTERM is sent when main test times out with cleanup_period set"""
        mock_popen.return_value.__enter__.return_value = self.mock_process
        self.mock_process.wait.side_effect = [
            subprocess.TimeoutExpired("", 0),  # Main test times out
            # No cleanup period
            0,  # Grace period succeeds
        ]

        with self.assertRaises(mobly_driver.MoblyTestTimeoutException):
            mobly_driver.run(
                self.mock_driver,
                "/py/path",
                "/test/path",
                timeout=timedelta(seconds=30),
            )

        self.mock_process.terminate.assert_called_once()
        self.mock_process.kill.assert_not_called()
        self.mock_driver.teardown.assert_called_once_with(
            timed_out=True,
            test_timeout=timedelta(0),
        )

    @mock.patch("builtins.print")
    @mock.patch("subprocess.Popen")
    def test_run_with_cleanup_period_sends_sigkill(
        self, mock_popen: Any, *unused_args: Any
    ) -> None:
        """Testcase to ensure SIGKILL is sent when cleanup period also times out"""
        mock_popen.return_value.__enter__.return_value = self.mock_process
        self.mock_process.wait.side_effect = [
            subprocess.TimeoutExpired("", 0),  # Main test times out
            subprocess.TimeoutExpired("", 0),  # Cleanup period times out
            subprocess.TimeoutExpired("", 0),  # Final warning 1 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 2 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 3 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 4 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 5 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 6 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 7 times out
            subprocess.TimeoutExpired("", 0),  # Final warning 8 times out
            0,  # Kill wait succeeds
        ]

        with self.assertRaises(mobly_driver.MoblyTestTimeoutException):
            mobly_driver.run(
                self.mock_driver,
                "/py/path",
                "/test/path",
                timeout=timedelta(seconds=30),
                cleanup_period=timedelta(seconds=10),
            )

        # It calls terminate for each timeout before the last one
        self.assertEqual(self.mock_process.terminate.call_count, 9)
        self.mock_process.kill.assert_called_once()

    @mock.patch("builtins.print")
    @mock.patch("subprocess.Popen")
    def test_interrupted_error_also_works(
        self, mock_popen: Any, *unused_args: Any
    ) -> None:
        """Testcase to ensure SIGKILL is sent when cleanup period also times out"""
        mock_popen.return_value.__enter__.return_value = self.mock_process
        self.mock_process.wait.side_effect = [
            InterruptedError,  # Main test interrupted
            InterruptedError,  # Cleanup period interrupted
            InterruptedError,  # Final warning 1 interrupted
            InterruptedError,  # Final warning 2 interrupted
            InterruptedError,  # Final warning 3 interrupted
            InterruptedError,  # Final warning 4 interrupted
            InterruptedError,  # Final warning 5 interrupted
            InterruptedError,  # Final warning 6 interrupted
            InterruptedError,  # Final warning 7 interrupted
            InterruptedError,  # Final warning 8 interrupted
            0,  # Kill wait succeeds
        ]

        with self.assertRaises(mobly_driver.MoblyTestTimeoutException):
            mobly_driver.run(
                self.mock_driver,
                "/py/path",
                "/test/path",
                timeout=timedelta(seconds=30),
                cleanup_period=timedelta(seconds=10),
            )

        # It calls terminate for each timeout before the last one
        self.assertEqual(self.mock_process.terminate.call_count, 9)
        self.mock_process.kill.assert_called_once()

    @mock.patch("builtins.print")
    @mock.patch("subprocess.Popen")
    def test_interrupted_error_grace_period_succeeds_marks_timed_out(
        self, mock_popen: Any, *unused_args: Any
    ) -> None:
        """Testcase to ensure teardown receives timed_out=True when interrupted by SIGTERM."""
        mock_popen.return_value.__enter__.return_value = self.mock_process
        self.mock_process.wait.side_effect = [
            InterruptedError,  # Main test interrupted by external SIGTERM
            0,  # Grace period succeeds
        ]

        with self.assertRaises(mobly_driver.MoblyTestTimeoutException):
            mobly_driver.run(
                self.mock_driver,
                "/py/path",
                "/test/path",
            )

        self.mock_process.terminate.assert_called_once()
        self.mock_process.kill.assert_not_called()
        self.mock_driver.teardown.assert_called_once_with(
            timed_out=True,
            test_timeout=None,
        )

    @mock.patch("builtins.print")
    @mock.patch("subprocess.Popen")
    def test_run_teardown_runs_despite_subprocess_error(
        self, mock_popen: Any, *unused_args: Any
    ) -> None:
        """Test case to ensure teardown always executes"""
        self.mock_process.wait.return_value = 1
        mock_popen.return_value.__enter__.return_value = self.mock_process

        with self.assertRaises(mobly_driver.MoblyTestFailureException):
            mobly_driver.run(self.mock_driver, "/py/path", "/test/path")
        self.mock_driver.teardown.assert_called()

    @parameterized.expand([[True], [False]])
    @mock.patch("builtins.print")
    @mock.patch("subprocess.Popen")
    @mock.patch("mobly_driver.NamedTemporaryFile")
    def test_run_passes_params_to_popen(
        self,
        verbose: bool,
        mock_tempfile: Any,
        mock_popen: Any,
        *unused_args: Any,
    ) -> None:
        """Test case to ensure correct params are passed to Popen"""
        tmp_path = "/tmp/path"
        py_path = "/py/path"
        test_path = "/test/path"
        self.mock_tmp.name = tmp_path
        self.mock_process.wait.return_value = 0
        mock_tempfile.return_value.__enter__.return_value = self.mock_tmp
        mock_popen.return_value.__enter__.return_value = self.mock_process

        mobly_driver.run(self.mock_driver, py_path, test_path, verbose=verbose)
        mock_popen.assert_called_once_with(
            [py_path, test_path, "-c", tmp_path] + (["-v"] if verbose else []),
            universal_newlines=mock.ANY,
            env=mock.ANY,
            preexec_fn=mock.ANY,
        )

    @parameterized.expand(
        [
            [True, ["/test/path", "-c", "/tmp/path"]],
            [False, ["/py/path", "/test/path", "-c", "/tmp/path"]],
        ]
    )
    @mock.patch("builtins.print")
    @mock.patch("subprocess.Popen")
    @mock.patch("mobly_driver.NamedTemporaryFile")
    def test_run_hermetic(
        self,
        hermetic: bool,
        expected_args: list[str],
        mock_tempfile: Any,
        mock_popen: Any,
        *unused_args: Any,
    ) -> None:
        """Test case to ensure correct params are passed to Popen"""
        self.mock_tmp.name = "/tmp/path"
        self.mock_process.wait.return_value = 0
        mock_tempfile.return_value.__enter__.return_value = self.mock_tmp
        mock_popen.return_value.__enter__.return_value = self.mock_process

        mobly_driver.run(
            self.mock_driver, "/py/path", "/test/path", hermetic=hermetic
        )
        mock_popen.assert_called_once_with(
            expected_args,
            universal_newlines=mock.ANY,
            env=mock.ANY,
            preexec_fn=mock.ANY,
        )

    @mock.patch.object(
        mobly_driver, "_LEFTOVER_PROCESS_GRACE_PERIOD", timedelta(seconds=0)
    )
    @mock.patch("builtins.print")
    @mock.patch("os.kill")
    @mock.patch("mobly_driver._list_processes")
    @mock.patch("subprocess.Popen")
    def test_run_kills_orphaned_descendants_when_test_killed_by_sigkill(
        self,
        mock_popen: Any,
        mock_list_processes: Any,
        mock_kill: Any,
        *unused_args: Any,
    ) -> None:
        """Test case to ensure leftover test descendants are killed even when proc.wait returns -9."""
        self.mock_process.pid = 4242
        self.mock_process.wait.return_value = -9
        mock_popen.return_value.__enter__.return_value = self.mock_process
        me = os.getpid()
        # In container environments (e.g. Swarming nsjail), `me` may be PID 1.
        # Ensure unrelated processes use a parent PID distinct from `me`.
        unrelated_ppid = me + 50000
        # Simulated /proc after the test (4242) died: its adb server (4300)
        # was reparented to us (the subreaper), the adb server has a live
        # child (4301) and a zombie child (4302). 9999 is unrelated.
        mock_list_processes.return_value = {
            me: (unrelated_ppid, "S", "python3"),
            4300: (me, "S", "adb"),
            4301: (4300, "S", "adb"),
            4302: (4300, "Z", "adb"),
            9999: (unrelated_ppid, "S", "bash"),
        }

        with self.assertRaises(mobly_driver.MoblyTestFailureException) as cm:
            mobly_driver.run(self.mock_driver, "/py/path", "/test/path")

        self.assertEqual(cm.exception.return_code, -9)
        # Deepest live descendant first; SIGTERM sent first, then escalated to SIGKILL.
        self.assertEqual(
            mock_kill.call_args_list,
            [
                mock.call(4301, signal.SIGTERM),
                mock.call(4300, signal.SIGTERM),
                mock.call(4301, signal.SIGKILL),
                mock.call(4300, signal.SIGKILL),
            ],
        )

    @mock.patch.object(
        mobly_driver, "_LEFTOVER_PROCESS_GRACE_PERIOD", timedelta(seconds=1)
    )
    @mock.patch("builtins.print")
    @mock.patch("os.kill")
    @mock.patch("mobly_driver._list_processes")
    @mock.patch("subprocess.Popen")
    def test_run_kills_orphaned_descendants_cleanly_on_sigterm(
        self,
        mock_popen: Any,
        mock_list_processes: Any,
        mock_kill: Any,
        *unused_args: Any,
    ) -> None:
        """Test case to ensure leftover test descendants exit cleanly on SIGTERM without SIGKILL."""
        self.mock_process.pid = 4242
        self.mock_process.wait.return_value = -9
        mock_popen.return_value.__enter__.return_value = self.mock_process
        me = os.getpid()
        # In container environments (e.g. Swarming nsjail), `me` may be PID 1.
        # Ensure unrelated processes use a parent PID distinct from `me`.
        unrelated_ppid = me + 50000
        mock_list_processes.return_value = {
            me: (unrelated_ppid, "S", "python3"),
            4300: (me, "S", "adb"),
            4301: (4300, "S", "adb"),
            9999: (unrelated_ppid, "S", "bash"),
        }

        # os.kill(pid, 0) raises ProcessLookupError, simulating processes exiting immediately on SIGTERM.
        def fake_kill(pid: int, sig: int) -> None:
            if sig == 0:
                raise ProcessLookupError

        mock_kill.side_effect = fake_kill

        with self.assertRaises(mobly_driver.MoblyTestFailureException):
            mobly_driver.run(self.mock_driver, "/py/path", "/test/path")

        # Deepest first; SIGTERM sent, checked via kill(pid, 0) which finds them gone, no SIGKILL needed.
        self.assertEqual(
            mock_kill.call_args_list,
            [
                mock.call(4301, signal.SIGTERM),
                mock.call(4300, signal.SIGTERM),
                mock.call(4301, 0),
                mock.call(4300, 0),
            ],
        )

    @mock.patch("builtins.print")
    @mock.patch("os.kill")
    @mock.patch("mobly_driver._list_processes", return_value={})
    @mock.patch("subprocess.Popen")
    def test_run_does_not_kill_anything_when_no_descendants_remain(
        self,
        mock_popen: Any,
        mock_list_processes: Any,
        mock_kill: Any,
        *unused_args: Any,
    ) -> None:
        """Test case to ensure a clean exit does not send any signals."""
        self.mock_process.pid = 4242
        self.mock_process.wait.return_value = 0
        mock_popen.return_value.__enter__.return_value = self.mock_process

        mobly_driver.run(self.mock_driver, "/py/path", "/test/path")

        mock_kill.assert_not_called()

    @mock.patch("mobly_driver._list_processes")
    def test_find_descendants_handles_cycles_safely(
        self, mock_list_processes: Any
    ) -> None:
        """Test case to ensure cyclic parent-child references do not cause infinite loops."""
        mock_list_processes.return_value = {
            200: (100, "S", "child1"),
            300: (200, "S", "child2"),
            100: (300, "S", "root"),
        }
        descendants = mobly_driver._find_descendants(100)
        self.assertIsInstance(descendants, list)

    @mock.patch("mobly_driver._list_processes")
    def test_find_descendants_skips_zombies(
        self, mock_list_processes: Any
    ) -> None:
        """Test case to ensure zombie descendants are skipped."""
        mock_list_processes.return_value = {
            100: (1, "S", "root"),
            200: (100, "Z", "zombie"),
            300: (100, "S", "alive"),
        }
        descendants = mobly_driver._find_descendants(100)
        self.assertEqual(descendants, [(300, "alive")])

    @mock.patch("mobly_driver._LIBC")
    def test_set_child_subreaper_calls_prctl(self, mock_libc: Any) -> None:
        """Test case to ensure prctl is called with PR_SET_CHILD_SUBREAPER on Linux."""
        if sys.platform != "linux":
            self.skipTest("Linux only")
        mock_libc.prctl.return_value = 0
        mobly_driver._set_child_subreaper()
        mock_libc.prctl.assert_called_once_with(36, 1)

    @mock.patch("mobly_driver._LIBC")
    def test_set_pdeathsig_calls_prctl(self, mock_libc: Any) -> None:
        """Test case to ensure prctl is called with PR_SET_PDEATHSIG on Linux."""
        if sys.platform != "linux":
            self.skipTest("Linux only")
        mobly_driver._set_pdeathsig()
        mock_libc.prctl.assert_called_once_with(1, signal.SIGKILL)
