#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utilities for signal handling in build scripts."""

import argparse
import os
import signal
import subprocess
import sys
from typing import Any


class BuildInterruptedError(KeyboardInterrupt):
    """Raised when a build is interrupted, carrying the child's exit status."""

    def __init__(self, return_code: int, signum: int) -> None:
        super().__init__()
        self.return_code = return_code
        self.signum = signum


def wait_and_forward_signals(
    process: subprocess.Popen[Any], verbose: bool = False
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
        verbose: If True, log signal receipt and forwarding to stderr.

    Returns:
        The return code of the process, converted to a shell-style positive
        status (128 + signum) if terminated by a signal.

    Raises:
        BuildInterruptedError: If a signal was received and forwarded.
    """
    received_signum = 0

    def handle_signal(signum: int, frame: Any) -> None:
        sig_name = signal.Signals(signum).name

        try:
            pgid = os.getpgid(process.pid)
            is_group_leader = pgid == process.pid

            if verbose:
                target_desc = (
                    f"PGID {pgid}" if is_group_leader else f"PID {process.pid}"
                )
                print(
                    f"[signal_utils] Received {sig_name}, forwarding to {target_desc}...",
                    file=sys.stderr,
                )

            if is_group_leader:
                os.killpg(pgid, signum)
            else:
                os.kill(process.pid, signum)
        except ProcessLookupError:
            # Process already finished.
            pass

        nonlocal received_signum
        received_signum = signum

    # Signals to forward.
    signals_to_forward = [signal.SIGINT, signal.SIGHUP, signal.SIGTERM]
    if hasattr(signal, "SIGQUIT"):
        signals_to_forward.append(signal.SIGQUIT)

    old_handlers: dict[int, Any] = {}
    for sig in signals_to_forward:
        old_handlers[sig] = signal.signal(sig, handle_signal)

    rc = 1
    try:
        # Wait for the process to finish.
        # Since we have custom handlers, SIGINT will not raise
        # KeyboardInterrupt during this call.
        rc = process.wait()
    finally:
        # Restore original signal handlers.
        for signum_restoring, handler in old_handlers.items():
            signal.signal(signum_restoring, handler)

        # If we received a signal, raise BuildInterruptedError now that we've
        # finished waiting and restored the handlers.
        if received_signum > 0:
            # If the child reported a failure status, preserve it.
            # If it reported success (graceful), override with 128 + signum
            # to ensure shell chains stop.
            # Convert signal-based exit (negative) to shell-style (128+N).
            exit_code = (
                rc
                if rc > 0
                else (128 - rc if rc < 0 else 128 + received_signum)
            )
            raise BuildInterruptedError(exit_code, received_signum)

    # Ensure we return a positive shell-style code even if no signal was
    # received by the wrapper (e.g. if the child was killed before the wrapper
    # could relay it).
    return rc if rc >= 0 else 128 - rc


def main(argv: list[str]) -> int:
    """Main function to use this script as a standalone signal wrapper.

    This script can even wrap around itself multiple times to demonstrate
    signal forwarding.
    """
    parser = argparse.ArgumentParser(
        description="Wraps a command and relays signals to it."
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Enable verbose signal logging",
    )
    parser.add_argument(
        "command",
        nargs=argparse.REMAINDER,
        help="The command to wrap (after --)",
    )

    # We manually handle the '--' separator because argparse's REMAINDER
    # is a bit greedy and doesn't strictly require it.
    if "--" not in argv:
        parser.print_help()
        return 1

    idx = argv.index("--")
    args = parser.parse_args(argv[:idx])
    cmd = argv[idx + 1 :]

    if not cmd:
        print("Error: No command specified after --")
        return 1

    if args.verbose:
        print(
            f"[signal_utils] Starting wrapper (PID: {os.getpid()})",
            file=sys.stderr,
        )

    # Start the child in a new process group to simulate isolation
    try:
        process = subprocess.Popen(cmd, preexec_fn=os.setpgrp)
    except Exception as e:
        print(f"Error: Failed to start command {' '.join(cmd)}: {e}")
        return 1

    try:
        return wait_and_forward_signals(process, verbose=args.verbose)
    except BuildInterruptedError as e:
        # Return the carry-over exit code
        return e.return_code
    except KeyboardInterrupt:
        # Standard fallback for SIGINT if raised elsewhere
        return 130
    finally:
        if args.verbose:
            print(
                f"[signal_utils] Wrapper exiting (PID: {os.getpid()})",
                file=sys.stderr,
            )


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
