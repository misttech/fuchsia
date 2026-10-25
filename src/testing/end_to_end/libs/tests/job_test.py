#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import subprocess
import unittest
from unittest import mock

from libs.proc import job
from libs.proc.runner import CalledProcessError


class FakePopen:
    """A fake version of the object returned from subprocess.Popen()."""

    def __init__(
        self,
        stdout: str | None = None,
        stderr: str | None = None,
        returncode: int = 0,
        will_timeout: bool = False,
    ) -> None:
        self.returncode = returncode
        self._stdout = bytes(stdout, "utf-8") if stdout is not None else bytes()
        self._stderr = bytes(stderr, "utf-8") if stderr is not None else bytes()
        self._will_timeout = will_timeout

    def communicate(
        self,
        stdin: bytes | None = None,
        timeout: float | None = None,
    ) -> tuple[bytes, bytes]:
        if self._will_timeout:
            raise subprocess.TimeoutExpired(
                "Timed out according to test logic",
                timeout or 0,
            )
        return self._stdout, self._stderr

    def poll(self) -> int:
        return self.returncode

    def kill(self) -> None:
        pass

    def wait(self) -> None:
        pass


class JobTestCases(unittest.TestCase):
    @mock.patch(
        "libs.proc.job.subprocess.Popen",
        return_value=FakePopen(stdout="TEST\n"),
    )
    def test_run_success(self, popen: mock.Mock) -> None:
        """Test running a simple shell command."""
        result = job.run("echo TEST")
        self.assertTrue(result.stdout.startswith(b"TEST"))

    @mock.patch(
        "libs.proc.job.subprocess.Popen",
        return_value=FakePopen(stderr="TEST\n"),
    )
    def test_run_stderr(self, popen: mock.Mock) -> None:
        """Test that we can read process stderr."""
        result = job.run("echo TEST 1>&2")
        self.assertEqual(len(result.stdout), 0)
        self.assertTrue(result.stderr.startswith(b"TEST"))
        self.assertFalse(result.stdout)

    @mock.patch(
        "libs.proc.job.subprocess.Popen",
        return_value=FakePopen(returncode=1),
    )
    def test_run_error(self, popen: mock.Mock) -> None:
        """Test that we raise on non-zero exit statuses."""
        self.assertRaises(CalledProcessError, job.run, "exit 1")

    @mock.patch(
        "libs.proc.job.subprocess.Popen",
        return_value=FakePopen(returncode=1),
    )
    def test_run_with_ignored_error(self, popen: mock.Mock) -> None:
        """Test that we can ignore exit status on request."""
        result = job.run("exit 1", ignore_status=True)
        self.assertEqual(result.returncode, 1)

    @mock.patch(
        "libs.proc.job.subprocess.Popen",
        return_value=FakePopen(will_timeout=True),
    )
    def test_run_timeout(self, popen: mock.Mock) -> None:
        """Test that we correctly implement command timeouts."""
        self.assertRaises(
            subprocess.TimeoutExpired, job.run, "sleep 5", timeout_sec=0.1
        )

    @mock.patch(
        "libs.proc.job.subprocess.Popen",
        return_value=FakePopen(stdout="TEST\n"),
    )
    def test_run_no_shell(self, popen: mock.Mock) -> None:
        """Test that we handle running without a wrapping shell."""
        result = job.run(["echo", "TEST"])
        self.assertTrue(result.stdout.startswith(b"TEST"))

    @mock.patch(
        "libs.proc.job.subprocess.Popen",
        return_value=FakePopen(stdout="TEST\n"),
    )
    def test_job_env(self, popen: mock.Mock) -> None:
        """Test that we can set environment variables correctly."""
        test_env = {"MYTESTVAR": "20"}
        job.run("printenv", env=test_env.copy())
        popen.assert_called_once()
        _, kwargs = popen.call_args
        self.assertTrue("env" in kwargs)
        self.assertEqual(kwargs["env"], test_env)


if __name__ == "__main__":
    unittest.main()
