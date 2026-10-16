#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import signal
import subprocess
import sys
import unittest
from typing import Any
from unittest import mock

import managed_process


class PreexecSetupTest(unittest.TestCase):
    def test_preexec_setup_isolates_pgrp(self) -> None:
        with (
            mock.patch.object(signal, "signal") as mock_signal,
            mock.patch.object(os, "setpgrp") as mock_setpgrp,
        ):
            managed_process.preexec_setup(separate_pgrp=True)
            mock_setpgrp.assert_called_once()
            self.assertGreaterEqual(mock_signal.call_count, 3)

    def test_preexec_setup_skips_pgrp_when_false(self) -> None:
        with (
            mock.patch.object(signal, "signal") as mock_signal,
            mock.patch.object(os, "setpgrp") as mock_setpgrp,
        ):
            managed_process.preexec_setup(separate_pgrp=False)
            mock_setpgrp.assert_not_called()
            self.assertGreaterEqual(mock_signal.call_count, 3)


def make_fake_popen(
    pid: int = 1234, returncode: int | None = None
) -> mock.Mock:
    """Creates a mock subprocess.Popen instance."""
    fake = mock.Mock(spec=subprocess.Popen)
    fake.pid = pid
    fake.returncode = returncode
    return fake


def spawn_test_process(
    cmd: list[str] | None = None,
    separate_pgrp: bool = True,
    **kwargs: Any,
) -> managed_process.ManagedProcess:
    """Spawns a hermetic Python test process."""
    if cmd is None:
        cmd = [sys.executable, "-c", "pass"]
    return managed_process.ManagedProcess.spawn(
        cmd, separate_pgrp=separate_pgrp, **kwargs
    )


class ManagedProcessTest(unittest.TestCase):
    def test_spawn_and_properties(self) -> None:
        proc = spawn_test_process()
        self.assertGreater(proc.pid, 0)
        self.assertGreater(proc.pgid, 0)
        self.assertEqual(proc.pgid, proc.pid)
        self.assertTrue(proc.is_group_leader)
        self.assertEqual(proc.wait(), 0)
        self.assertEqual(proc.returncode, 0)
        self.assertIsNotNone(proc.process)

    def test_spawn_without_separate_pgrp(self) -> None:
        proc = spawn_test_process(separate_pgrp=False)
        self.assertGreater(proc.pid, 0)
        self.assertEqual(proc.pgid, os.getpgid(0))
        self.assertFalse(proc.is_group_leader)
        self.assertEqual(proc.wait(), 0)

    def test_spawn_passes_custom_kwargs(self) -> None:
        mock_proc = mock.Mock()
        mock_proc.pid = 999
        with mock.patch.object(
            subprocess, "Popen", return_value=mock_proc
        ) as mock_popen:
            proc = managed_process.ManagedProcess.spawn(
                ["echo", "hi"],
                stdout=subprocess.PIPE,
            )
            self.assertEqual(proc.pid, 999)
            mock_popen.assert_called_once()
            kwargs = mock_popen.call_args[1]
            self.assertEqual(kwargs["stdout"], subprocess.PIPE)
            self.assertIn("preexec_fn", kwargs)

    def test_pgid_fallback_on_lookup_error(self) -> None:
        fake_popen = make_fake_popen()
        with mock.patch.object(
            os, "getpgid", side_effect=ProcessLookupError
        ) as mock_getpgid:
            proc = managed_process.ManagedProcess(fake_popen)
            self.assertEqual(proc.pgid, 1234)
            self.assertTrue(proc.is_group_leader)
            mock_getpgid.assert_called_once_with(1234)

    def test_pgid_cached(self) -> None:
        fake_popen = make_fake_popen()
        with mock.patch.object(
            os, "getpgid", return_value=5678
        ) as mock_getpgid:
            proc = managed_process.ManagedProcess(fake_popen)
            mock_getpgid.assert_called_once_with(1234)
            self.assertEqual(proc.pgid, 5678)
            self.assertEqual(proc.pgid, 5678)
            self.assertFalse(proc.is_group_leader)
            self.assertEqual(mock_getpgid.call_count, 1)

    def test_send_signal_leader(self) -> None:
        proc = managed_process.ManagedProcess(make_fake_popen())
        with (
            mock.patch.object(os, "kill") as mock_kill,
            mock.patch.object(os, "killpg") as mock_killpg,
        ):
            proc.send_signal(
                signal.SIGINT, managed_process.ProcessTarget.LEADER
            )
            mock_kill.assert_called_once_with(1234, signal.SIGINT)
            mock_killpg.assert_not_called()

    def test_send_signal_group_when_group_leader(self) -> None:
        fake_popen = make_fake_popen(pid=1234)
        proc = managed_process.ManagedProcess(fake_popen, pgid=1234)
        self.assertTrue(proc.is_group_leader)
        with (
            mock.patch.object(os, "kill") as mock_kill,
            mock.patch.object(os, "killpg") as mock_killpg,
        ):
            proc.send_signal(
                signal.SIGKILL, managed_process.ProcessTarget.GROUP
            )
            mock_killpg.assert_called_once_with(1234, signal.SIGKILL)
            mock_kill.assert_not_called()

    def test_send_signal_group_falls_back_to_kill_when_not_group_leader(
        self,
    ) -> None:
        fake_popen = make_fake_popen(pid=1234)
        proc = managed_process.ManagedProcess(fake_popen, pgid=5678)
        self.assertFalse(proc.is_group_leader)
        with (
            mock.patch.object(os, "kill") as mock_kill,
            mock.patch.object(os, "killpg") as mock_killpg,
        ):
            proc.send_signal(
                signal.SIGKILL, managed_process.ProcessTarget.GROUP
            )
            mock_kill.assert_called_once_with(1234, signal.SIGKILL)
            mock_killpg.assert_not_called()

    def test_send_signal_handles_process_lookup_error(self) -> None:
        proc = managed_process.ManagedProcess(make_fake_popen())
        with mock.patch.object(os, "kill", side_effect=ProcessLookupError):
            # Should safely catch ProcessLookupError without raising
            proc.send_signal(
                signal.SIGINT, managed_process.ProcessTarget.LEADER
            )


if __name__ == "__main__":
    unittest.main()
