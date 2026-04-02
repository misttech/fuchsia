#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import signal
import subprocess
import unittest
from typing import Any
from unittest import mock

import signal_utils


class SignalUtilsTest(unittest.TestCase):
    @mock.patch("signal_utils.os.killpg")
    @mock.patch("signal_utils.os.getpgid", return_value=1234)
    @mock.patch("signal_utils.signal.signal")
    def test_forward_signals(
        self,
        mock_signal: mock.Mock,
        mock_getpgid: mock.Mock,
        mock_killpg: mock.Mock,
    ) -> None:
        mock_process = mock.Mock(spec=subprocess.Popen)
        mock_process.pid = 5678

        registered_handlers = {}

        def fake_signal(sig: int, handler: Any) -> int:
            registered_handlers[sig] = handler
            return signal.SIG_DFL

        mock_signal.side_effect = fake_signal

        with signal_utils.forward_signals(mock_process):
            # Check that signals were registered
            self.assertIn(signal.SIGINT, registered_handlers)
            self.assertIn(signal.SIGHUP, registered_handlers)
            self.assertIn(signal.SIGTERM, registered_handlers)

            # Trigger a handler
            handler = registered_handlers[signal.SIGINT]
            handler(signal.SIGINT, None)

            # Check forwarding
            mock_getpgid.assert_called_with(5678)
            mock_killpg.assert_called_once_with(1234, signal.SIGINT)

        # Check that original handlers were restored (mock_signal called with SIG_DFL)
        # Each signal is called twice: once to set, once to restore.
        expected_calls = []
        sigs = [signal.SIGINT, signal.SIGHUP, signal.SIGTERM]
        if hasattr(signal, "SIGQUIT"):
            sigs.append(signal.SIGQUIT)

        for sig in sigs:
            expected_calls.append(mock.call(sig, mock.ANY))  # Set
        for sig in sigs:
            expected_calls.append(mock.call(sig, signal.SIG_DFL))  # Restore

        mock_signal.assert_has_calls(expected_calls, any_order=True)

    @mock.patch("signal_utils.os.killpg")
    @mock.patch("signal_utils.os.getpgid", side_effect=ProcessLookupError)
    @mock.patch("signal_utils.signal.signal")
    def test_forward_signals_process_finished(
        self,
        mock_signal: mock.Mock,
        mock_getpgid: mock.Mock,
        mock_killpg: mock.Mock,
    ) -> None:
        mock_process = mock.Mock(spec=subprocess.Popen)
        mock_process.pid = 5678

        registered_handlers = {}

        def fake_signal(sig: int, handler: Any) -> int:
            registered_handlers[sig] = handler
            return signal.SIG_DFL

        mock_signal.side_effect = fake_signal

        with signal_utils.forward_signals(mock_process):
            handler = registered_handlers[signal.SIGINT]
            # Should not raise exception if process is gone
            handler(signal.SIGINT, None)

        mock_killpg.assert_not_called()


if __name__ == "__main__":
    unittest.main()
