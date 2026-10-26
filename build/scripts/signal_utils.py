#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utilities for signal handling in build scripts."""

import argparse
import contextlib
import signal
import subprocess
import sys
import time
from collections.abc import Callable, Iterator, Sequence
from typing import Any

from build.scripts import managed_process


@contextlib.contextmanager
def scoped_signal_handlers(
    signals: Sequence[int],
    handler: Callable[[int, Any], None],
) -> Iterator[None]:
    """Context manager to install signal handlers for a scoped block and restore previous handlers on exit."""
    old_handlers: dict[int, Any] = {}
    for sig in signals:
        old_handlers[sig] = signal.signal(sig, handler)
    try:
        yield
    finally:
        for sig, old_h in old_handlers.items():
            signal.signal(sig, old_h)


class BuildInterruptedError(KeyboardInterrupt):
    """Raised when a build is interrupted, carrying the child's exit status."""

    def __init__(self, return_code: int, signum: int) -> None:
        super().__init__()
        self.return_code = return_code
        self.signum = signum


class SignalManagedProcess:
    """Encapsulates a process whose signals are managed and forwarded.

    This class handles the entire signal management lifecycle:
    1. Child setup (resetting signals and optionally isolating PGID).
    2. Process spawning.
    3. Signal relay loop (forwarding SIGINT, etc., to the child).
    """

    def __init__(
        self,
        command: Sequence[str],
        *,
        separate_pgrp: bool = True,
        verbose: bool = False,
        timeout: float | None = None,
        timeout_callback: (
            Callable[[managed_process.ManagedProcess, float], None] | None
        ) = None,
        initial_timeout: float | None = None,
        post_spawn_callback: (
            Callable[[managed_process.ManagedProcess], None] | None
        ) = None,
        **kwargs: Any,
    ) -> None:
        """Initializes the managed process.

        Args:
            command: The command to run as a sequence of strings.
            separate_pgrp: Whether to isolate the child in a new process group.
            verbose: Whether to log signal receipt and forwarding.
            timeout: Optional interval in seconds to invoke timeout_callback.
            timeout_callback: Callback invoked when timeout expires while waiting.
            initial_timeout: Optional initial timeout before the first timeout_callback.
                If no timeout is set, we only timeout once and then wait
                indefinitely.
            post_spawn_callback: Callback invoked immediately after spawn.
            **kwargs: Additional arguments passed to subprocess.Popen.
        """
        if "preexec_fn" in kwargs:
            raise ValueError(
                "preexec_fn is managed by SignalManagedProcess and cannot be overridden."
            )
        self._command = command
        self._separate_pgrp = separate_pgrp
        self._verbose = verbose
        self._timeout = timeout
        self._timeout_callback = timeout_callback
        self._initial_timeout = initial_timeout
        self._post_spawn_callback = post_spawn_callback
        self._popen_kwargs = kwargs

    def run(self) -> int:
        """Starts the process and waits for it while forwarding signals.

        Returns:
            The return code of the process.

        Raises:
            BuildInterruptedError: If a signal was received and forwarded.
        """
        managed_proc = managed_process.ManagedProcess.spawn(
            self._command,
            separate_pgrp=self._separate_pgrp,
            **self._popen_kwargs,
        )
        if self._post_spawn_callback:
            self._post_spawn_callback(managed_proc)
        return self._wait_and_forward_signals(managed_proc)

    def _wait_and_forward_signals(
        self, process: managed_process.ManagedProcess
    ) -> int:
        """Relays signals to the given process and waits for it to terminate.

        This function ensures that the parent remains alive to relay signals
        until the child has finished. This prevents orphaning and ensures that
        the entire process tree is signaled correctly.

        It handles SIGINT, SIGHUP, SIGTERM, and SIGQUIT.

        The forwarding policy is determined automatically:
        - If the process is a group leader (pgid == pid), signals are sent to the
          entire group (os.killpg).
        - Otherwise, signals are sent to the process PID only (os.kill).
        This ensures that isolated sub-trees (like build tools wrapped in shell
        scripts) are fully signaled, while preventing signal bombardment in
        linear chains.

        Args:
            process: The subprocess to forward signals to and wait for.

        Returns:
            The return code of the process, converted to a shell-style positive
            status (128 + signum) if terminated by a signal.

        Raises:
            BuildInterruptedError: If a signal was received and forwarded.
        """
        received_signum = 0

        def handle_signal(signum: int, frame: Any) -> None:
            sig_name = signal.Signals(signum).name
            target = (
                managed_process.ProcessTarget.GROUP
                if process.is_group_leader
                else managed_process.ProcessTarget.LEADER
            )

            if self._verbose:
                target_desc = (
                    f"PGID {process.pgid}"
                    if process.is_group_leader
                    else f"PID {process.pid}"
                )
                print(
                    f"[signal_utils] Received {sig_name}. Child PID: {process.pid}, PGID: {process.pgid}, IsLeader: {process.is_group_leader}",
                    file=sys.stderr,
                )
                print(
                    f"[signal_utils] Forwarding {sig_name} to {target_desc}...",
                    file=sys.stderr,
                )

            process.send_signal(signum, target)
            if self._verbose and process.poll() is not None:
                print(
                    f"[signal_utils] Received {sig_name}, but child process {process.pid} has already exited.",
                    file=sys.stderr,
                )

            nonlocal received_signum
            received_signum = signum

        # Signals to forward.
        signals_to_handle = [signal.SIGINT, signal.SIGHUP, signal.SIGTERM]
        if hasattr(signal, "SIGQUIT"):
            signals_to_handle.append(signal.SIGQUIT)

        try:
            with scoped_signal_handlers(signals_to_handle, handle_signal):
                # Block until the process exits.
                # We handle the return code conversion to shell status codes.
                first_timeout = (
                    self._initial_timeout
                    if self._initial_timeout is not None
                    else self._timeout
                )
                if first_timeout is None:
                    rc = process.wait()
                    return 128 - rc if rc < 0 else rc

                start_time = time.monotonic()
                next_timeout = start_time + first_timeout
                while True:
                    try:
                        wait_timeout = max(0.0, next_timeout - time.monotonic())
                        rc = process.wait(timeout=wait_timeout)
                        return 128 - rc if rc < 0 else rc
                    except subprocess.TimeoutExpired:
                        elapsed = time.monotonic() - start_time
                        if self._timeout_callback:
                            self._timeout_callback(process, elapsed)
                        if self._timeout is not None and self._timeout > 0:
                            next_timeout += self._timeout
                            if next_timeout <= time.monotonic():
                                # This happens if the timeout handler takes longer than the
                                # next timeout interval, in which case we just reset the
                                # timeout.
                                next_timeout = time.monotonic() + self._timeout
                        else:
                            next_timeout = float("inf")
        finally:
            if received_signum != 0:
                # We determine the final return code here for the exception.
                # If the process exited via our signal, its rc will reflect that.
                final_rc = process.returncode
                if final_rc < 0:
                    final_rc = 128 - final_rc
                raise BuildInterruptedError(final_rc, received_signum)


def run_command(
    command: Sequence[str],
    *,
    separate_pgrp: bool = True,
    verbose: bool = False,
    **kwargs: Any,
) -> int:
    """One-shot function to run a command with signal management."""
    return SignalManagedProcess(
        command, separate_pgrp=separate_pgrp, verbose=verbose, **kwargs
    ).run()


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Wraps a command and relays signals to it."
    )
    parser.add_argument(
        "--verbose", action="store_true", help="Log signal details."
    )
    parser.add_argument("command", nargs="+", help="The command to run.")
    args = parser.parse_args(argv)

    try:
        return run_command(args.command, verbose=args.verbose)
    except BuildInterruptedError as e:
        # Return the carry-over exit code
        return e.return_code
    except KeyboardInterrupt:
        # Standard fallback for SIGINT if raised elsewhere
        return 130


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
