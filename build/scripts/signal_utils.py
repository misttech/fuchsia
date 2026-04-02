# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utilities for signal handling in build scripts."""

import contextlib
import os
import signal
import subprocess
from typing import Any, Generator


@contextlib.contextmanager
def forward_signals(
    process: subprocess.Popen[Any],
) -> Generator[None, None, None]:
    """Context manager to forward signals to a subprocess's process group.

    This context manager intercepts SIGINT, SIGHUP, SIGTERM, and SIGQUIT
    sent to the current process and forwards them to the process group
    of the given subprocess. This is useful when the subprocess is started
    in its own process group (e.g., using preexec_fn=os.setpgrp) to ensure
    the entire process tree of the child is signaled correctly while the
    parent process remains alive to wait for completion.

    This can forward multiple signals (not just the first one)
    while the subprocess is running.

    Usage Example:
        process = subprocess.Popen(
            ["sleep", "10"],
            preexec_fn=os.setpgrp
        )
        with forward_signals(process):
            # The parent script will now forward signals like Ctrl+C
            # to the 'sleep' process group.
            rc = process.wait()

    Args:
        process: The subprocess to forward signals to.
    """

    def handle_signal(signum: int, frame: Any) -> None:
        try:
            # Forward the signal to the child's process group.
            pgid = os.getpgid(process.pid)
            os.killpg(pgid, signum)
        except ProcessLookupError:
            # Process already finished.
            pass

    # Signals to forward.
    signals_to_forward = [signal.SIGINT, signal.SIGHUP, signal.SIGTERM]
    if hasattr(signal, "SIGQUIT"):
        signals_to_forward.append(signal.SIGQUIT)

    old_handlers = {}
    for sig in signals_to_forward:
        old_handlers[sig] = signal.signal(sig, handle_signal)

    try:
        yield
    finally:
        # Restore original signal handlers.
        for sig, handler in old_handlers.items():
            signal.signal(sig, handler)
