#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import signal
import subprocess
import unittest
from typing import Any
from unittest import mock

from build.scripts import managed_process, signal_utils


def make_mock_managed_process(
    pid: int = 1234,
    pgid: int | None = None,
    returncode: int | None = 0,
    is_group_leader: bool = True,
) -> mock.Mock:
    """Creates a mock ManagedProcess instance for testing."""
    proc = mock.Mock(spec=managed_process.ManagedProcess)
    proc.pid = pid
    proc.pgid = pgid if pgid is not None else pid
    proc.returncode = returncode
    proc.is_group_leader = is_group_leader
    proc.poll.return_value = None
    return proc


class SignalUtilsTest(unittest.TestCase):
    def _wait_and_forward_signals(
        self,
        process: managed_process.ManagedProcess,
        verbose: bool = False,
        timeout: float | None = None,
        timeout_callback: Any = None,
        initial_timeout: float | None = None,
    ) -> int:
        return signal_utils.SignalManagedProcess(
            [],
            verbose=verbose,
            timeout=timeout,
            timeout_callback=timeout_callback,
            initial_timeout=initial_timeout,
        )._wait_and_forward_signals(process)

    @mock.patch.object(signal, "signal")
    def test_wait_and_forward_signals_leader(
        self,
        mock_signal: mock.Mock,
    ) -> None:
        """Verify that signals are forwarded to the PGID if the child is a group leader."""
        mock_managed = make_mock_managed_process()

        def side_effect() -> int:
            for call in mock_signal.call_args_list:
                if call.args[0] == signal.SIGINT:
                    handler = call.args[1]
                    handler(signal.SIGINT, None)
            return 0

        mock_managed.wait.side_effect = side_effect

        with self.assertRaises(signal_utils.BuildInterruptedError) as cm:
            self._wait_and_forward_signals(mock_managed)

        self.assertEqual(cm.exception.return_code, 0)
        self.assertEqual(cm.exception.signum, signal.SIGINT)
        mock_managed.send_signal.assert_called_once_with(
            signal.SIGINT, managed_process.ProcessTarget.GROUP
        )

    @mock.patch.object(signal, "signal")
    def test_wait_and_forward_signals_follower(
        self,
        mock_signal: mock.Mock,
    ) -> None:
        """Verify that signals are forwarded to the PID only if the child is not a group leader."""
        mock_managed = make_mock_managed_process(
            pgid=1111, is_group_leader=False
        )

        def side_effect() -> int:
            for call in mock_signal.call_args_list:
                if call.args[0] == signal.SIGINT:
                    handler = call.args[1]
                    handler(signal.SIGINT, None)
            return 0

        mock_managed.wait.side_effect = side_effect

        with self.assertRaises(signal_utils.BuildInterruptedError) as cm:
            self._wait_and_forward_signals(mock_managed)

        self.assertEqual(cm.exception.return_code, 0)
        mock_managed.send_signal.assert_called_once_with(
            signal.SIGINT, managed_process.ProcessTarget.LEADER
        )

    @mock.patch.object(signal, "signal")
    def test_wait_and_forward_signals_failure_preserved(
        self,
        mock_signal: mock.Mock,
    ) -> None:
        """Verify that child failure code is preserved even when interrupted."""
        mock_managed = make_mock_managed_process(returncode=1)

        def side_effect() -> int:
            for call in mock_signal.call_args_list:
                if call.args[0] == signal.SIGTERM:
                    handler = call.args[1]
                    handler(signal.SIGTERM, None)
            return 1

        mock_managed.wait.side_effect = side_effect

        with self.assertRaises(signal_utils.BuildInterruptedError) as cm:
            self._wait_and_forward_signals(mock_managed)

        self.assertEqual(cm.exception.return_code, 1)
        self.assertEqual(cm.exception.signum, signal.SIGTERM)

    @mock.patch.object(signal, "signal")
    def test_wait_and_forward_signals_negative_rc_conversion(
        self,
        mock_signal: mock.Mock,
    ) -> None:
        """Verify that negative signal status is converted to shell-style (128+N)."""
        mock_managed = make_mock_managed_process(returncode=-15)

        def side_effect() -> int:
            for call in mock_signal.call_args_list:
                if call.args[0] == signal.SIGTERM:
                    handler = call.args[1]
                    handler(signal.SIGTERM, None)
            return -15

        mock_managed.wait.side_effect = side_effect

        with self.assertRaises(signal_utils.BuildInterruptedError) as cm:
            self._wait_and_forward_signals(mock_managed)

        self.assertEqual(cm.exception.return_code, 143)
        self.assertEqual(cm.exception.signum, signal.SIGTERM)

    @mock.patch.object(signal, "signal")
    def test_wait_and_forward_signals_multiple_succession(
        self,
        mock_signal: mock.Mock,
    ) -> None:
        """Verify that multiple different signals are all forwarded correctly."""
        mock_managed = make_mock_managed_process()

        def side_effect() -> int:
            handlers = {
                call.args[0]: call.args[1]
                for call in mock_signal.call_args_list
            }
            handlers[signal.SIGINT](signal.SIGINT, None)
            handlers[signal.SIGTERM](signal.SIGTERM, None)
            return 0

        mock_managed.wait.side_effect = side_effect

        with self.assertRaises(signal_utils.BuildInterruptedError) as cm:
            self._wait_and_forward_signals(mock_managed)

        self.assertEqual(cm.exception.signum, signal.SIGTERM)
        self.assertEqual(mock_managed.send_signal.call_count, 2)

    @mock.patch.object(
        signal_utils.SignalManagedProcess,
        "_wait_and_forward_signals",
        return_value=42,
    )
    @mock.patch.object(managed_process.ManagedProcess, "spawn")
    def test_signal_managed_process(
        self, mock_spawn: mock.Mock, mock_wait: mock.Mock
    ) -> None:
        """Verify SignalManagedProcess correctly orchestrates execution."""
        mock_proc = mock.Mock()
        mock_spawn.return_value = mock_proc

        managed = signal_utils.SignalManagedProcess(
            ["test", "cmd"], separate_pgrp=True, verbose=True, env={"A": "B"}
        )
        rc = managed.run()

        self.assertEqual(rc, 42)
        mock_spawn.assert_called_once_with(
            ["test", "cmd"], separate_pgrp=True, env={"A": "B"}
        )
        mock_wait.assert_called_once_with(mock_proc)

    @mock.patch.object(signal_utils, "SignalManagedProcess")
    def test_run_command(self, mock_managed_class: mock.Mock) -> None:
        """Verify run_command convenience function."""
        mock_instance = mock_managed_class.return_value
        mock_instance.run.return_value = 123

        rc = signal_utils.run_command(
            ["cmd"], separate_pgrp=False, verbose=True
        )

        self.assertEqual(rc, 123)
        mock_managed_class.assert_called_once_with(
            ["cmd"], separate_pgrp=False, verbose=True
        )
        mock_instance.run.assert_called_once()

    def test_wait_and_forward_signals_timeout(self) -> None:
        """Verify timeout_callback is invoked when wait times out."""
        mock_process = mock.Mock()
        call_count = 0

        def fake_wait(timeout: float | None = None) -> int:
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                raise subprocess.TimeoutExpired(cmd="test", timeout=0.01)
            return 0

        mock_process.wait.side_effect = fake_wait
        callback_called: list[float] = []

        rc = self._wait_and_forward_signals(
            mock_process,
            timeout=0.01,
            timeout_callback=lambda p, elapsed: callback_called.append(elapsed),
        )
        self.assertEqual(rc, 0)
        self.assertEqual(len(callback_called), 1)

    @mock.patch.object(
        signal_utils.SignalManagedProcess,
        "_wait_and_forward_signals",
        return_value=0,
    )
    @mock.patch.object(managed_process.ManagedProcess, "spawn")
    def test_signal_managed_process_with_timeout_and_post_spawn(
        self, mock_spawn: mock.Mock, mock_wait: mock.Mock
    ) -> None:
        """Verify SignalManagedProcess passes timeout, timeout_callback, and invokes post_spawn_callback."""
        mock_proc = mock.Mock()
        mock_spawn.return_value = mock_proc

        post_spawn_called = []
        timeout_cb = lambda p, e: None

        managed = signal_utils.SignalManagedProcess(
            ["cmd"],
            timeout=60.0,
            timeout_callback=timeout_cb,
            post_spawn_callback=lambda p: post_spawn_called.append(p),
        )
        rc = managed.run()

        self.assertEqual(rc, 0)
        self.assertEqual(post_spawn_called, [mock_proc])
        mock_wait.assert_called_once_with(mock_proc)

    def test_wait_and_forward_signals_initial_timeout(self) -> None:
        """Verify initial_timeout is used for the first tick."""
        mock_process = mock.Mock()
        call_count = 0

        def fake_wait(timeout: float | None = None) -> int:
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                # First wait should use initial_timeout (0.01)
                self.assertAlmostEqual(timeout or 0.0, 0.01, places=2)
                raise subprocess.TimeoutExpired(cmd="test", timeout=0.01)
            return 0

        mock_process.wait.side_effect = fake_wait
        callback_called: list[float] = []

        rc = self._wait_and_forward_signals(
            mock_process,
            timeout=60.0,
            initial_timeout=0.01,
            timeout_callback=lambda p, elapsed: callback_called.append(elapsed),
        )
        self.assertEqual(rc, 0)
        self.assertEqual(len(callback_called), 1)

    def test_wait_and_forward_signals_initial_timeout_larger_than_timeout(
        self,
    ) -> None:
        """Verify initial_timeout is not clamped when larger than timeout."""
        mock_process = mock.Mock()
        call_count = 0

        def fake_wait(timeout: float | None = None) -> int:
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                # First wait should use initial_timeout (0.05) rather than timeout (0.01)
                self.assertAlmostEqual(timeout or 0.0, 0.05, places=2)
                raise subprocess.TimeoutExpired(cmd="test", timeout=0.05)
            return 0

        mock_process.wait.side_effect = fake_wait
        callback_called: list[float] = []

        rc = self._wait_and_forward_signals(
            mock_process,
            timeout=0.01,
            initial_timeout=0.05,
            timeout_callback=lambda p, elapsed: callback_called.append(elapsed),
        )
        self.assertEqual(rc, 0)
        self.assertEqual(len(callback_called), 1)

    def test_wait_and_forward_signals_only_initial_timeout(self) -> None:
        """Verify that when only initial_timeout is provided, subsequent waits do not time out."""
        mock_process = mock.Mock()
        call_count = 0

        def fake_wait(timeout: float | None = None) -> int:
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                self.assertAlmostEqual(timeout or 0.0, 0.01, places=2)
                raise subprocess.TimeoutExpired(cmd="test", timeout=0.01)
            if call_count == 2:
                self.assertEqual(timeout, float("inf"))
            return 0

        mock_process.wait.side_effect = fake_wait
        callback_called: list[float] = []

        rc = self._wait_and_forward_signals(
            mock_process,
            initial_timeout=0.01,
            timeout_callback=lambda p, elapsed: callback_called.append(elapsed),
        )
        self.assertEqual(rc, 0)
        self.assertEqual(len(callback_called), 1)

    @mock.patch.object(
        signal_utils.SignalManagedProcess,
        "_wait_and_forward_signals",
        return_value=0,
    )
    @mock.patch.object(managed_process.ManagedProcess, "spawn")
    def test_signal_managed_process_with_initial_timeout(
        self, mock_spawn: mock.Mock, mock_wait: mock.Mock
    ) -> None:
        """Verify SignalManagedProcess passes initial_timeout to _wait_and_forward_signals."""
        mock_proc = mock.Mock()
        mock_spawn.return_value = mock_proc
        mock_wait.return_value = 0

        timeout_cb = lambda p, e: None

        managed = signal_utils.SignalManagedProcess(
            ["cmd"],
            timeout=60.0,
            initial_timeout=2.0,
            timeout_callback=timeout_cb,
        )
        rc = managed.run()

        self.assertEqual(rc, 0)
        mock_wait.assert_called_once_with(mock_proc)


class ScopedSignalHandlersTest(unittest.TestCase):
    def test_scoped_signal_handlers_restores_previous_handlers(self) -> None:
        handler_calls: list[int] = []

        def custom_handler(signum: int, frame: Any) -> None:
            handler_calls.append(signum)

        old_handler = signal.getsignal(signal.SIGINT)
        try:
            with signal_utils.scoped_signal_handlers(
                [signal.SIGINT], custom_handler
            ):
                current_handler = signal.getsignal(signal.SIGINT)
                self.assertEqual(current_handler, custom_handler)

            restored_handler = signal.getsignal(signal.SIGINT)
            self.assertEqual(restored_handler, old_handler)
        finally:
            signal.signal(signal.SIGINT, old_handler)


if __name__ == "__main__":
    unittest.main()
