#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Simple FFX host tool E2E test."""

import logging
import subprocess
from typing import Tuple

from fuchsia_base_test import fuchsia_base_test
from honeydew.fuchsia_device import fuchsia_device

_LOGGER: logging.Logger = logging.getLogger(__name__)


class SpawnedFfx:
    """A running ffx process."""

    def __init__(self, proc: subprocess.Popen[bytes], cmd: list[str]):
        self.proc = proc
        self.cmd = cmd

    def wait(self) -> str:
        """Wait for the ffx process to terminate. Raise an exception if it failed."""
        stdout, stderr = self.proc.communicate()
        if self.proc.returncode != 0:
            _LOGGER.warning("FFX cmd failed. Stderr: %s", stderr.decode())
            raise subprocess.CalledProcessError(
                self.proc.returncode, cmd=self.cmd, output=stdout, stderr=stderr
            )
        return stdout.decode()


class FfxTestCase(fuchsia_base_test.FuchsiaBaseTest):
    """FFX host tool E2E test For Strict."""

    def setup_class(self) -> None:
        """setup_class is called once before running the testsuite."""
        super().setup_class()
        self.dut: fuchsia_device.FuchsiaDevice = self.fuchsia_devices[0]

    def teardown_test(self) -> None:
        # Until total daemonless functionality is implemented, we must ensure
        # the daemon is running before test teardown, because Lacewing expects
        # the daemon to be running for teardown operations. Previously the
        # daemon was automatically restarted by `ffx target wait`, but this is
        # now no longer the case because the command does not use the daemon
        # anymore.
        self.dut.ffx.run(["daemon", "start", "--background"])
        super().teardown_test()

    def spawn_ffx(self, args: list[str]) -> SpawnedFfx:
        """Run ffx in the specific way we need, not the standard Honeydew way.
        Don't wait for the command to return."""
        config = self.dut.ffx.config
        cmd = [config.binary_path]
        if "--strict" not in args:
            cmd += [
                "--isolate-dir",
                config.isolate_dir.directory(),
            ]
        cmd += config.get_config_args()
        cmd += args
        _LOGGER.info("Running FFX cmd: %s", cmd)
        proc = subprocess.Popen(
            cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
        return SpawnedFfx(proc, cmd)

    def run_ffx(self, args: list[str]) -> str:
        """Run ffx in the specific way we need, not the standard Honeydew way"""
        config = self.dut.ffx.config
        cmd = [config.binary_path]
        if "--strict" not in args:
            cmd += [
                "--isolate-dir",
                config.isolate_dir.directory(),
            ]
        cmd += config.get_config_args()
        cmd += args
        _LOGGER.info("Running FFX cmd: %s", cmd)
        proc = subprocess.run(cmd, capture_output=True)
        try:
            proc.check_returncode()
            return proc.stdout.decode()
        except subprocess.CalledProcessError:
            _LOGGER.warning("FFX cmd failed. Stderr: %s", proc.stderr.decode())
            raise

    def run_ffx_unchecked(self, args: list[str]) -> Tuple[int, str, str]:
        """Run ffx in the specific way we need, not the standard Honeydew way.
        Also does not check for errors"""
        config = self.dut.ffx.config
        cmd = [config.binary_path]
        if "--strict" not in args:
            cmd += [
                "--isolate-dir",
                config.isolate_dir.directory(),
            ]
        cmd += config.get_config_args()
        cmd += args
        _LOGGER.info("Running FFX cmd: %s", cmd)
        proc = subprocess.run(cmd, capture_output=True)
        return (proc.returncode, proc.stdout.decode(), proc.stderr.decode())
