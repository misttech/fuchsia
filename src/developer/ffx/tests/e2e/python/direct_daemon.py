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
# All we are doing is ensuring that the commands don't exit with a non-
# zero return code.
# These tests use an isolate-dir, because we don't want to interact with
# the user's daemon, if any.
class FfxDirectDaemonTest(ffxtestcase.FfxTestCase):
    """FFX host tool E2E test for daemon subtools when in direct mode."""

    async def setup_class(self) -> None:
        await super().setup_class()
        self.isolate_dir = self.dut.ffx.config.isolate_dir.directory()

    async def teardown_class(self) -> None:
        # These tests will leave a daemon turned on, but that might effect other
        # tests that expect the daemon to be off.
        self.run_ffx(
            [
                "--isolate-dir",
                self.isolate_dir,
                "daemon",
                "stop",
            ]
        )
        await super().teardown_class()

    def _run_ffx_direct_isolated(self, cmd: list[str]) -> None:
        self.run_ffx(
            [
                "--isolate-dir",
                self.isolate_dir,
                "--direct",
                *cmd,
            ],
        )

    def test_direct_daemon_disconnect(self) -> None:
        """Test `ffx --direct daemon disconnect` does not raise an exception."""
        self._run_ffx_direct_isolated(
            [
                "daemon",
                "disconnect",
            ],
        )

    def test_direct_daemon_echo(self) -> None:
        """Test `ffx --direct daemon echo` does not raise an exception."""
        self._run_ffx_direct_isolated(
            [
                "daemon",
                "echo",
            ],
        )

    def test_direct_daemon_stop(self) -> None:
        """Test `ffx --direct daemon stop` does not raise an exception."""
        self._run_ffx_direct_isolated(
            [
                "daemon",
                "stop",
            ],
        )

    def test_direct_daemon_crash(self) -> None:
        """Test `ffx --direct daemon crash` does not raise an exception."""
        self._run_ffx_direct_isolated(
            [
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
