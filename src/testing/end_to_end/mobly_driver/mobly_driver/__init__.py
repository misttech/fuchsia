# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly Driver module."""

import os
import signal
import subprocess
from dataclasses import dataclass
from datetime import timedelta
from tempfile import NamedTemporaryFile
from typing import Any, Optional

from mobly_driver.api import api_infra
from mobly_driver.driver import base


class MoblyTestTimeoutException(Exception):
    """Raised when the underlying Mobly test times out."""


class MoblyTestFailureException(Exception):
    """Raised when the underlying Mobly test returns a non-zero return code."""

    def __init__(self, return_code: int):
        self.return_code = return_code

    def __repr__(self) -> str:
        return f"Mobly test failed with return code {self.return_code}."


# The "final grace period" is how long we wait before killing the test in the
# case that the cleanup period times out. Once tests have received two
# SIGTERMs, they get this last chance to exit gracefully. In particular, tests
# shouldn't do anything during this period other than persist information they
# have already collected and exit.
FINAL_GRACE_PERIOD_TIMEOUT = timedelta(seconds=10)


def _execute_test(
    driver: base.BaseDriver,
    python_path: str,
    test_path: str,
    test_cases: Optional[list[str]] = None,
    timeout: Optional[timedelta] = None,
    cleanup_period: Optional[timedelta] = None,
    verbose: bool = False,
    hermetic: bool = False,
    list_mobly_tests: bool = False,
) -> int:
    """Executes a Mobly test with the specified Mobly Driver.

    Mobly test output is streamed to the console.

    Args:
      driver: The environment-specific Mobly driver to use for test execution.
      python_path: path to the Python runtime for to use.
      test_path: path to the Mobly test executable to run.
      test_cases: The set of cases to run. If None, all methods in test are run.
      timeout: Duration before a test is killed due to timeout.
        If set to None, timeout is not enforced.
      cleanup_period: If set, we send SIGTERM to the test this long before the
          test is killed due to timeout. Requires timeout to be set.
      verbose: Whether to enable verbose output from the mobly test.
      hermetic: Whether the mobly test is a self-contained executable.
      list_mobly_tests: Whether to list test cases instead of running them.

    Returns:
      The return code of the Mobly test.

    Raises:
      MoblyTestTimeoutException if Mobly test duration exceeds timeout.
    """
    test_env = os.environ.copy()
    # Set line-buffering for Mobly tests to flush output immediately.
    test_env["PYTHONUNBUFFERED"] = "1"

    with NamedTemporaryFile(mode="w") as tmp_config:
        config = driver.generate_test_config()
        print(api_infra.TESTPARSER_PREAMBLE)
        print(config)
        print("======================================")
        tmp_config.write(config)
        tmp_config.flush()

        cmd = [] if hermetic else [python_path]
        if list_mobly_tests:
            cmd += [test_path, "--list_tests"]
        else:
            cmd += [test_path, "-c", tmp_config.name]
            if test_cases:
                cmd += ["--test_case"] + test_cases
            if verbose:
                cmd.append("-v")

        cmd_str = " ".join(cmd)
        print(f'[Mobly Driver] - Executing Mobly test via cmd:\n"$ {cmd_str}"')

        with subprocess.Popen(
            cmd,
            universal_newlines=True,
            env=test_env,
        ) as proc:
            # Treat SIGTERM as SIGINT
            def sigterm_handler(signum: int, _: Any) -> None:
                raise InterruptedError(
                    f"[Mobly Driver] - Received signal: {signum}, interrupting the mobly test"
                )

            signal.signal(signal.SIGTERM, sigterm_handler)

            # The main test timeout is the total timeout minus the (optional)
            # cleanup period and the final grace period.
            if timeout is not None:
                main_test_timeout = (
                    timeout
                    - (cleanup_period or timedelta(0))
                    - FINAL_GRACE_PERIOD_TIMEOUT
                )
            else:
                main_test_timeout = None

            if main_test_timeout is not None:
                print(
                    f"[Mobly Driver] - Waiting {main_test_timeout} for test to complete."
                )
            else:
                print(
                    f"[Mobly Driver] - Waiting indefinitely for test to complete."
                )
            try:
                return proc.wait(
                    timeout=main_test_timeout.total_seconds()
                    if main_test_timeout is not None
                    else None
                )
            except subprocess.TimeoutExpired:
                print(
                    f"[Mobly Driver] - test timed out after {main_test_timeout}."
                )
            except InterruptedError:
                print(
                    "[Mobly Driver] - got SIGINT/SIGTERM while waiting for test to complete."
                )

            if cleanup_period is not None:
                print(
                    "[Mobly Driver] - Sending SIGTERM to begin cleanup period."
                )
                proc.terminate()
                try:
                    return proc.wait(timeout=cleanup_period.total_seconds())
                except subprocess.TimeoutExpired:
                    print(
                        f"[Mobly Driver] - cleanup period timed out after {cleanup_period}."
                    )
                except InterruptedError:
                    print(
                        "[Mobly Driver] - got SIGINT/SIGTERM during cleanup period."
                    )

            print(
                "[Mobly Driver] - Sending SIGTERM to begin final grace period."
            )
            proc.terminate()
            try:
                return proc.wait(
                    timeout=FINAL_GRACE_PERIOD_TIMEOUT.total_seconds()
                )
            except subprocess.TimeoutExpired:
                print(
                    f"[Mobly Driver] - final grace period timed out after {FINAL_GRACE_PERIOD_TIMEOUT}."
                )
            except InterruptedError:
                print(
                    "[Mobly Driver] - got SIGINT/SIGTERM during final grace period."
                )

            print("[Mobly Driver] - Sending SIGKILL")
            proc.kill()
            proc.wait()
            raise MoblyTestTimeoutException("Mobly test had to be killed.")


def run(
    driver: base.BaseDriver,
    python_path: str,
    test_path: str,
    test_cases: Optional[list[str]] = None,
    timeout: Optional[timedelta] = None,
    cleanup_period: Optional[timedelta] = None,
    verbose: bool = False,
    hermetic: bool = False,
    list_mobly_tests: bool = False,
) -> None:
    """Runs the Mobly Driver which handles the lifecycle of a Mobly test.

    This method manages the lifecycle of a Mobly test's execution.
    At a high level, run() creates a Mobly config, triggers a Mobly test with
    it, and performs any necessary clean up after test execution.

    Args:
      driver: The environment-specific Mobly driver to use for test execution.
      python_path: path to the Python runtime to use for test execution.
      test_path: path to the Mobly test executable to run.
      test_cases: The set of cases to run. If None, all methods in test are run.
      timeout: Duration before a test is killed due to timeout.
          If None, timeout is not enforced.
      cleanup_period: If set, we send SIGTERM to the test this long before the
          test is killed due to timeout. Requires timeout to be set.
      verbose: Whether to enable verbose output from the mobly test.
      hermetic: Whether the mobly test is a self-contained executable.
      list_mobly_tests: Whether to list test cases instead of running them.

    Raises:
      MoblyTestFailureException if the test returns a non-zero return code.
      MoblyTestTimeoutException if the test duration exceeds specified timeout.
      ValueError if any argument is invalid.
    """
    if not driver:
        raise ValueError("|driver| must not be None.")
    if not python_path:
        raise ValueError("|python_path| must not be empty.")
    if not test_path:
        raise ValueError("|test_path| must not be empty.")
    if timeout is not None and timeout < timedelta(0):
        raise ValueError("|timeout| must be None or a non-negative timedelta.")
    if cleanup_period is not None and cleanup_period < timedelta(0):
        raise ValueError(
            "|cleanup_period| must be None or a non-negative timedelta."
        )
    if cleanup_period is not None and timeout is None:
        raise ValueError("|cleanup_period| must be None if |timeout| is None.")
    print(f"Running [{driver.__class__.__name__}]")
    try:
        return_code = _execute_test(
            python_path=python_path,
            test_path=test_path,
            driver=driver,
            timeout=timeout,
            cleanup_period=cleanup_period,
            test_cases=test_cases,
            verbose=verbose,
            hermetic=hermetic,
            list_mobly_tests=list_mobly_tests,
        )
        if return_code != 0:
            # TODO(https://fxbug.dev/42070748) - differentiate between legitimate
            # test failures vs unexpected crashes.
            raise MoblyTestFailureException(return_code)
    finally:
        driver.teardown()
