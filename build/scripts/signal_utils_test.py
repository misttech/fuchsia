#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import signal
import subprocess
import unittest
from typing import Any
from unittest import mock

import signal_utils


class SignalUtilsTest(unittest.TestCase):
    @mock.patch.object(os, "kill")
    @mock.patch.object(os, "killpg")
    @mock.patch.object(os, "getpgid")
    @mock.patch.object(signal, "signal")
    def test_wait_and_forward_signals_leader(
        self,
        mock_signal: mock.Mock,
        mock_getpgid: mock.Mock,
        mock_killpg: mock.Mock,
        mock_kill: mock.Mock,
    ) -> None:
        """Verify that signals are forwarded to the PGID if the child is a group leader."""
        mock_process = mock.Mock(spec=subprocess.Popen)
        mock_process.pid = 5678
        # Simulate that the process is a group leader (PGID == PID).
        # This happens when using os.setpgrp in main_build.py.
        mock_getpgid.return_value = 5678

        registered_handlers = {}

        def fake_signal(sig: int, handler: Any) -> Any:
            registered_handlers[sig] = handler
            return signal.SIG_DFL

        mock_signal.side_effect = fake_signal

        def trigger_handler_during_wait() -> int:
            # This is called when wait_and_forward_signals calls process.wait().
            # We trigger our custom signal handler while it is waiting.
            handler = registered_handlers[signal.SIGINT]
            handler(signal.SIGINT, None)
            return 0

        mock_process.wait.side_effect = trigger_handler_during_wait

        # Since a SIGINT was received, the function should raise BuildInterruptedError
        # after the process has "finished".
        with self.assertRaises(signal_utils.BuildInterruptedError) as cm:
            signal_utils.wait_and_forward_signals(mock_process)
        # Child returned 0, so parent should upgrade to 130 (SIGINT).
        self.assertEqual(cm.exception.return_code, 130)
        self.assertEqual(cm.exception.signum, signal.SIGINT)

        # Because the child was a group leader, we must have called killpg
        # to ensure the entire sub-tree (including Bazel's children) is signaled.
        mock_killpg.assert_called_once_with(5678, signal.SIGINT)
        mock_kill.assert_not_called()

    @mock.patch.object(os, "kill")
    @mock.patch.object(os, "killpg")
    @mock.patch.object(os, "getpgid")
    @mock.patch.object(signal, "signal")
    def test_wait_and_forward_signals_follower(
        self,
        mock_signal: mock.Mock,
        mock_getpgid: mock.Mock,
        mock_killpg: mock.Mock,
        mock_kill: mock.Mock,
    ) -> None:
        """Verify that signals are forwarded to the PID only if the child is not a group leader."""
        mock_process = mock.Mock(spec=subprocess.Popen)
        mock_process.pid = 5678
        # Simulate that the process is NOT a group leader (PGID != PID).
        # This happens for simple linear wrapper chains without isolation.
        mock_getpgid.return_value = 1234

        registered_handlers = {}

        def fake_signal(sig: int, handler: Any) -> Any:
            registered_handlers[sig] = handler
            return signal.SIG_DFL

        mock_signal.side_effect = fake_signal

        def trigger_handler_during_wait() -> int:
            handler = registered_handlers[signal.SIGINT]
            handler(signal.SIGINT, None)
            return 0

        mock_process.wait.side_effect = trigger_handler_during_wait

        with self.assertRaises(signal_utils.BuildInterruptedError) as cm:
            signal_utils.wait_and_forward_signals(mock_process)
        # Child returned 0, so parent should upgrade to 130 (SIGINT).
        self.assertEqual(cm.exception.return_code, 130)
        self.assertEqual(cm.exception.signum, signal.SIGINT)

        # Because the child was just a follower, we must have called kill (PID)
        # to avoid "bombarding" the entire process group multiple times.
        mock_kill.assert_called_once_with(5678, signal.SIGINT)
        mock_killpg.assert_not_called()

    @mock.patch.object(os, "kill")
    @mock.patch.object(os, "killpg")
    @mock.patch.object(os, "getpgid")
    @mock.patch.object(signal, "signal")
    def test_wait_and_forward_signals_failure_preserved(
        self,
        mock_signal: mock.Mock,
        mock_getpgid: mock.Mock,
        mock_killpg: mock.Mock,
        mock_kill: mock.Mock,
    ) -> None:
        """Verify that child failure code is preserved even when interrupted."""
        mock_process = mock.Mock(spec=subprocess.Popen)
        mock_process.pid = 5678
        mock_getpgid.return_value = 5678  # leader

        registered_handlers = {}
        mock_signal.side_effect = (
            lambda s, h: registered_handlers.update({s: h}) or signal.SIG_DFL
        )

        def trigger_handler_during_wait() -> int:
            handler = registered_handlers[signal.SIGINT]
            handler(signal.SIGINT, None)
            # Child failed during interrupt.
            return 1

        mock_process.wait.side_effect = trigger_handler_during_wait

        with self.assertRaises(signal_utils.BuildInterruptedError) as cm:
            signal_utils.wait_and_forward_signals(mock_process)
        # Child returned 1, so parent should return 1.
        self.assertEqual(cm.exception.return_code, 1)
        self.assertEqual(cm.exception.signum, signal.SIGINT)

    @mock.patch.object(os, "kill")
    @mock.patch.object(os, "killpg")
    @mock.patch.object(os, "getpgid")
    @mock.patch.object(signal, "signal")
    def test_wait_and_forward_signals_negative_rc_conversion(
        self,
        mock_signal: mock.Mock,
        mock_getpgid: mock.Mock,
        mock_killpg: mock.Mock,
        mock_kill: mock.Mock,
    ) -> None:
        """Verify that negative signal status is converted to shell-style (128+N)."""
        mock_process = mock.Mock(spec=subprocess.Popen)
        mock_process.pid = 5678
        mock_getpgid.return_value = 5678  # leader

        registered_handlers = {}
        mock_signal.side_effect = (
            lambda s, h: registered_handlers.update({s: h}) or signal.SIG_DFL
        )

        def trigger_handler_during_wait() -> int:
            handler = registered_handlers[signal.SIGINT]
            handler(signal.SIGINT, None)
            # Child killed by SIGTERM (15). Subprocess returns -15.
            return -15

        mock_process.wait.side_effect = trigger_handler_during_wait

        with self.assertRaises(signal_utils.BuildInterruptedError) as cm:
            signal_utils.wait_and_forward_signals(mock_process)
        # 128 - (-15) = 143.
        self.assertEqual(cm.exception.return_code, 143)
        self.assertEqual(cm.exception.signum, signal.SIGINT)

    @mock.patch.object(os, "kill")
    @mock.patch.object(os, "killpg")
    @mock.patch.object(os, "getpgid")
    @mock.patch.object(signal, "signal")
    def test_wait_and_forward_signals_multiple_succession(
        self,
        mock_signal: mock.Mock,
        mock_getpgid: mock.Mock,
        mock_killpg: mock.Mock,
        mock_kill: mock.Mock,
    ) -> None:
        """Verify that multiple different signals are all forwarded correctly."""
        mock_process = mock.Mock(spec=subprocess.Popen)
        mock_process.pid = 5678
        mock_getpgid.return_value = 5678  # leader

        registered_handlers = {}
        mock_signal.side_effect = (
            lambda s, h: registered_handlers.update({s: h}) or signal.SIG_DFL
        )

        def trigger_handlers_during_wait() -> int:
            # Trigger SIGINT
            registered_handlers[signal.SIGINT](signal.SIGINT, None)
            # Trigger SIGTERM
            registered_handlers[signal.SIGTERM](signal.SIGTERM, None)
            # Child killed by SIGTERM (15)
            return -15

        mock_process.wait.side_effect = trigger_handlers_during_wait

        with self.assertRaises(signal_utils.BuildInterruptedError) as cm:
            signal_utils.wait_and_forward_signals(mock_process)

        # Both signals should have been forwarded to the process group.
        mock_killpg.assert_has_calls(
            [
                mock.call(5678, signal.SIGINT),
                mock.call(5678, signal.SIGTERM),
            ]
        )
        # Since child died of SIGTERM (-15), status should be 143.
        self.assertEqual(cm.exception.return_code, 143)
        # The last received signal was SIGTERM.
        self.assertEqual(cm.exception.signum, signal.SIGTERM)


if __name__ == "__main__":
    unittest.main()
