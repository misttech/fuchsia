#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Simple FFX host tool E2E test."""

import logging

import ffxtestcase
from mobly import test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


# These tests validate that "--direct" doesn't cause daemon commands to
# break. This is important when users set connectivity.direct to true;
# that should just mean that they want the daemon not be used when talking
# to the target, not that all daemon-related commands should fail.
# All we are doing is ensuring that the commands don't exist with a non-
# zero return code.
# These tests use `self.run_ffx()` unstead of `self.dut.ffx.run()` to
# ensure that we have specific control of exactly the arguments we need
# when invoking these commands.
class FfxDirectDaemonTest(ffxtestcase.FfxTestCase):
    """FFX host tool E2E test for daemon subtools when in direct mode."""

    def setup_class(self) -> None:
        # This just gets some things out of the way before we start turning
        # the daemon off and on again.
        super().setup_class()
        self.dut_ssh_address = self.dut.ffx.get_target_ssh_address()

    def test_direct_daemon_disconnect(self) -> None:
        """Test `ffx --direct daemon disconnect` does not raise an exception."""
        self.run_ffx(
            [
                "--direct",
                "daemon",
                "disconnect",
            ],
        )

    def test_direct_daemon_echo(self) -> None:
        """Test `ffx --direct daemon echo` does not raise an exception."""
        self.run_ffx(
            [
                "--direct",
                "daemon",
                "echo",
            ],
        )

    def test_direct_daemon_stop(self) -> None:
        """Test `ffx --direct daemon stop` does not raise an exception."""
        self.run_ffx(
            [
                "--direct",
                "daemon",
                "stop",
            ],
        )

    def test_direct_daemon_crash(self) -> None:
        """Test `ffx --direct daemon crash` does not raise an exception."""
        self.run_ffx(
            [
                "--direct",
                "daemon",
                "crash",
            ],
        )

    # We do not test `start` because it runs indefinitely. A test
    # is perhaps less important since we'll find out quickly if there
    # are problems here due to quick user reporting, since this command
    # is load-bearing.

    # We do not test `hang` because it runs indefinitely. A test
    # is perhaps less important this is an obscure command only used for
    # testing, not by users.

    # We do not test `log` or `socket` because they do not make a connection
    # to the daemon, so "--direct" has no effect.


if __name__ == "__main__":
    test_runner.main()
