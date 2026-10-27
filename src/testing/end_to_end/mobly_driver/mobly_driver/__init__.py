# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly Driver module."""

import ctypes
import ctypes.util
import logging
import os
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import timedelta
from tempfile import NamedTemporaryFile
from typing import Any, NoReturn, Optional

import logging_utils
from libs.exception_utils import unroll_and_raise
from mobly_driver.api import api_infra
from mobly_driver.driver import base

_LOGGER = logging.getLogger(__name__)

_PR_SET_PDEATHSIG = 1
_PR_SET_CHILD_SUBREAPER = 36
_LEFTOVER_PROCESS_GRACE_PERIOD = timedelta(seconds=1)

_LIBC: ctypes.CDLL | None = None
if sys.platform == "linux":
    _LIBC = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)


def _set_pdeathsig() -> None:
    """Sets the parent-death signal of the calling process to SIGKILL on Linux.

    Used as a `preexec_fn` so that the Mobly test process is killed by the
    kernel if mobly_driver itself dies abruptly (e.g. SIGKILL from the test
    runner after its grace period), instead of being left behind as an orphan
    together with any host processes it spawned (e.g. an isolated adb server).

    Note that PR_SET_PDEATHSIG is bound to the *thread* that forked the child.
    This is safe here because `subprocess.Popen` is called from the main thread,
    which lives for the whole lifetime of mobly_driver.
    """
    if _LIBC:
        _LIBC.prctl(_PR_SET_PDEATHSIG, signal.SIGKILL)


def _set_child_subreaper() -> None:
    """Marks this process as a child subreaper on Linux.

    Without this, any process the Mobly test leaves behind (e.g. an isolated
    adb server) is reparented to init the moment the test exits, so it can no
    longer be found by walking the process tree below mobly_driver. With it,
    such orphans are reparented to mobly_driver instead and are killed by
    `_kill_process_tree(os.getpid())` once the test has finished.
    """
    if _LIBC and _LIBC.prctl(_PR_SET_CHILD_SUBREAPER, 1) != 0:
        errno = ctypes.get_errno()
        _LOGGER.warning(
            f"prctl(PR_SET_CHILD_SUBREAPER) failed with errno {errno}; "
            "orphaned test subprocesses may not be cleaned up."
        )


def _list_processes() -> dict[int, tuple[int, str, str]]:
    """Returns {pid: (ppid, state, comm)} for all processes in /proc."""
    procs: dict[int, tuple[int, str, str]] = {}
    try:
        entries = os.listdir("/proc")
    except OSError:
        return procs
    for entry in entries:
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat", "r", encoding="utf-8") as f:
                stat = f.read()
        except OSError:
            continue
        # Format: "<pid> (<comm>) <state> <ppid> ...". comm may contain spaces
        # or parentheses, so split after the *last* ')'.
        lparen = stat.find("(")
        rparen = stat.rfind(")")
        if lparen < 0 or rparen < lparen:
            continue
        comm = stat[lparen + 1 : rparen]
        fields = stat[rparen + 1 :].split()
        if len(fields) < 2:
            continue
        try:
            procs[int(entry)] = (int(fields[1]), fields[0], comm)
        except ValueError:
            continue
    return procs


def _find_descendants(root_pid: int) -> list[tuple[int, str]]:
    """Returns all live descendant (pid, comm) of `root_pid`, deepest last.

    Zombies are skipped: they are already dead and only waiting to be reaped.
    """
    procs = _list_processes()
    children: dict[int, list[int]] = {}
    for pid, (ppid, _, _) in procs.items():
        children.setdefault(ppid, []).append(pid)
    descendants: list[tuple[int, str]] = []
    queue = [root_pid]
    visited = {root_pid}
    while queue:
        pid = queue.pop(0)
        for child in children.get(pid, []):
            if child in visited:
                continue
            visited.add(child)
            proc_info = procs.get(child)
            if proc_info and proc_info[1] != "Z":
                descendants.append((child, proc_info[2]))
            queue.append(child)
    return descendants


def _kill_process_tree(
    root_pid: int,
    include_root: bool = True,
    grace_period: Optional[timedelta] = None,
) -> None:
    """Terminates all descendants of `root_pid` (and optionally to it).

    Sends SIGTERM first to give leftover processes an opportunity to shut down
    gracefully. If any survive past `grace_period`, escalates to SIGKILL.
    Descendants are collected before any signal is sent and killed deepest
    first. Processes that have already exited are ignored.
    """
    if not isinstance(root_pid, int) or root_pid <= 0:
        return
    targets = list(reversed(_find_descendants(root_pid)))
    if include_root:
        targets.append((root_pid, "test"))
    if not targets:
        return

    effective_grace_period = (
        _LEFTOVER_PROCESS_GRACE_PERIOD if grace_period is None else grace_period
    )

    # First, send SIGTERM to allow graceful cleanup.
    survivors: list[tuple[int, str]] = []
    for pid, comm in targets:
        try:
            os.kill(pid, signal.SIGTERM)
            survivors.append((pid, comm))
        except ProcessLookupError:
            pass
        except OSError as e:
            _LOGGER.warning(
                f"Failed to send SIGTERM to leftover process {pid}: {e}"
            )

    # Poll for up to grace_period for processes to exit.
    if effective_grace_period > timedelta(0) and survivors:
        deadline = time.monotonic() + effective_grace_period.total_seconds()
        while survivors and time.monotonic() < deadline:
            # Reap any re-parented child zombies so os.kill(pid, 0) sees ESRCH.
            while True:
                try:
                    reaped_pid, _ = os.waitpid(-1, os.WNOHANG)
                    if reaped_pid <= 0:
                        break
                except (ChildProcessError, OSError):
                    break

            remaining: list[tuple[int, str]] = []
            for pid, comm in survivors:
                try:
                    os.kill(pid, 0)
                    remaining.append((pid, comm))
                except ProcessLookupError:
                    _LOGGER.warning(
                        f"Killed leftover process {pid} ({comm}) from the "
                        "Mobly test process tree."
                    )
                except OSError:
                    remaining.append((pid, comm))
            survivors = remaining
            if survivors:
                time.sleep(min(0.05, max(0.0, deadline - time.monotonic())))

    # Any remaining processes did not exit in time; send SIGKILL.
    for pid, comm in survivors:
        try:
            os.kill(pid, signal.SIGKILL)
            _LOGGER.warning(
                f"Killed leftover process {pid} ({comm}) from the Mobly test "
                "process tree."
            )
        except ProcessLookupError:
            pass
        except OSError as e:
            _LOGGER.warning(f"Failed to kill leftover process {pid}: {e}")


class MoblyTestTimeoutException(Exception):
    """Raised when the underlying Mobly test times out."""


class MoblyTestFailureException(Exception):
    """Raised when the underlying Mobly test returns a non-zero return code."""

    def __init__(self, return_code: int):
        self.return_code = return_code

    def __repr__(self) -> str:
        return f"Mobly test failed with return code {self.return_code}."


# The "final grace period" is how long we wait before killing the test in the
# case that the cleanup period times out. Once a test has received a SIGTERM,
# we give it several opportunities to exit gracefully. In particular, tests
# shouldn't do anything during this period other than persist information they
# have already collected and exit.
#
# TODO(https://fxbug.dev/486240505): When a device locks up, we experience
# hangs in each of the following places:
#   - the test itself
#   - teardown_test
#   - teardown_class
#   - cleanup
#
# The first SIGTERM aborts the test and begins the cleanup period. But if every
# one of these methods hangs we'll still need three more SIGTERMs to get out of
# lacewing code and back into Mobly, which can then persist the test results.
#
# So anyway, we give the test binary many opportunities to exit gracefully.
# It'd be preferable for the test itself to not hang so many times on a device
# that's clearly not responding. Once that's the case, we can make this tidier.
FINAL_GRACE_PERIOD_WARNINGS = 8
FINAL_GRACE_PERIOD_TIMEOUT = timedelta(seconds=5)


def _get_main_test_timeout(
    timeout: Optional[timedelta],
    cleanup_period: Optional[timedelta],
) -> Optional[timedelta]:
    """Calculates the main test timeout before cleanup and grace periods."""
    if timeout is not None:
        return max(
            timedelta(0),
            timeout
            - (cleanup_period or timedelta(0))
            - FINAL_GRACE_PERIOD_TIMEOUT * FINAL_GRACE_PERIOD_WARNINGS,
        )
    return None


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
    if logging_utils.supports_color():
        test_env["FORCE_COLOR"] = "1"

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
        _LOGGER.info(f'Executing Mobly test via cmd:\n"$ {cmd_str}"')

        # The test deliberately stays in mobly_driver's own process group so
        # that the test runner (botanist) can still kill the whole group with
        # `kill(-pgid)` after its grace period. Orphan cleanup on our side is
        # done by becoming a child subreaper and killing everything still
        # below us in the process tree in the `finally` block, and
        # PR_SET_PDEATHSIG covers the case where mobly_driver is killed before
        # that block can run.
        if sys.platform == "linux":
            _set_child_subreaper()

        with subprocess.Popen(
            cmd,
            universal_newlines=True,
            env=test_env,
            preexec_fn=_set_pdeathsig if sys.platform == "linux" else None,
        ) as proc:
            # If we get SIGTERM or SIGINT (say, because the user hits CTRL+C),
            # we want to do the following:
            # - Handle it, because we don't want to quit and leave the test
            #   subprocess as an orphan. If we don't handle the signal, we'll
            #   quit by default.
            # - Send the signal to the test subprocess, so that it knows to
            #   wrap things up. Often the signal will be sent to the whole
            #   process group, so this is redundant, but not always.
            # - Advance the counter of the number of warnings we've sent.
            #
            # All that is handled in the InterruptedError exception handlers below.
            def sigterm_handler(signum: int, _: Any) -> None:
                raise InterruptedError(
                    f"[Mobly Driver] - Received signal: {signum}, interrupting the mobly test"
                )

            signal.signal(signal.SIGTERM, sigterm_handler)

            try:
                # The main test timeout is the total timeout minus the (optional)
                # cleanup period and the final grace period.
                main_test_timeout = _get_main_test_timeout(
                    timeout, cleanup_period
                )

                if main_test_timeout is not None:
                    _LOGGER.info(
                        f"Waiting {main_test_timeout} for test to complete."
                    )
                else:
                    _LOGGER.info("Waiting indefinitely for test to complete.")
                try:
                    return proc.wait(
                        timeout=main_test_timeout.total_seconds()
                        if main_test_timeout is not None
                        else None
                    )
                except subprocess.TimeoutExpired:
                    _LOGGER.warning(
                        f"test timed out after {main_test_timeout}."
                    )
                except InterruptedError:
                    _LOGGER.warning(
                        "got out-of-band SIGINT/SIGTERM while waiting for test to complete."
                    )

                if cleanup_period is not None:
                    _LOGGER.info("Sending SIGTERM to begin cleanup period.")
                    proc.terminate()
                    try:
                        proc.wait(timeout=cleanup_period.total_seconds())
                    except subprocess.TimeoutExpired:
                        _LOGGER.warning(
                            f"cleanup period timed out after {cleanup_period}."
                        )
                    except InterruptedError:
                        _LOGGER.warning(
                            "got out-of-band SIGINT/SIGTERM during cleanup period."
                        )
                    else:
                        raise MoblyTestTimeoutException(
                            "Mobly test timed out and exited during cleanup period."
                        )

                _LOGGER.info("Begin final grace period.")
                for i in range(FINAL_GRACE_PERIOD_WARNINGS):
                    _LOGGER.info(
                        f"Sending SIGTERM {i+1}/{FINAL_GRACE_PERIOD_WARNINGS}."
                    )
                    proc.terminate()
                    try:
                        proc.wait(
                            timeout=FINAL_GRACE_PERIOD_TIMEOUT.total_seconds()
                        )
                    except subprocess.TimeoutExpired:
                        _LOGGER.warning(
                            f"timed out after {FINAL_GRACE_PERIOD_TIMEOUT}."
                        )
                    except InterruptedError:
                        _LOGGER.warning(
                            "got out-of-band SIGINT/SIGTERM during final grace period."
                        )
                    else:
                        raise MoblyTestTimeoutException(
                            "Mobly test timed out and exited during final grace period."
                        )

                _LOGGER.info("Sending SIGKILL")
                proc.kill()
                proc.wait()
                raise MoblyTestTimeoutException("Mobly test had to be killed.")
            finally:
                # Whether the test exited normally, failed, timed out or was
                # killed (e.g. -9), make sure nothing it spawned outlives it.
                # Because we are a child subreaper, its orphans are now our
                # direct children, so walk the tree from ourselves.
                _kill_process_tree(os.getpid(), include_root=False)


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
    _LOGGER.info(f"Running [{driver.__class__.__name__}]")
    main_test_timeout = _get_main_test_timeout(timeout, cleanup_period)
    timed_out = False
    try:
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
        except MoblyTestTimeoutException:
            timed_out = True
            raise
        finally:
            driver.teardown(
                timed_out=timed_out,
                test_timeout=main_test_timeout if timed_out else None,
            )
    except BaseException as e:
        unroll_and_raise(e)
