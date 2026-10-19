# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""AndroidSession affordance implementation using ADB."""

import contextlib
import logging
import shlex
import time
from collections.abc import Callable
from datetime import timedelta

from honeydew import errors
from honeydew.affordances import affordance
from honeydew.affordances.android_session import errors as session_errors
from honeydew.affordances.android_session import types
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.adb import errors as adb_errors
from honeydew.utils import common

_LOGGER: logging.Logger = logging.getLogger(__name__)

_SYSTEM_SERVER: str = "system_server"
_FETCH_TIMEOUT: timedelta = timedelta(minutes=1)
_FETCH_RETRY_DELAY: timedelta = timedelta(seconds=1)
_MIN_ADB_TIMEOUT_SEC: float = 3.0


def get_process_id_of(
    adb: adb_transport.Adb,
    process_name: str,
    timeout: float | None = None,
) -> int | None:
    """Returns the PID of a running Android process via `adb shell pidof`.

    Args:
        adb: ADB transport used to query the device.
        process_name: Name of the Android process.
        timeout: Optional timeout in seconds passed to `adb.run()`.

    Returns:
        The PID of the process, or None if it is not running. If multiple
        processes match, only the first PID is returned.

    Raises:
        adb_errors.AdbConnectionError: If the ADB connection is unauthorized
            or disconnected.
        session_errors.AndroidSessionError: If the ADB command fails or
            `pidof` returns unexpected non-numeric output.
    """
    cmd: list[str] = ["shell", f"pidof {shlex.quote(process_name)} || true"]
    try:
        raw: str = (
            adb.run(cmd) if timeout is None else adb.run(cmd, timeout=timeout)
        )
    except adb_errors.AdbConnectionError:
        raise
    except adb_errors.AdbError as err:
        raise session_errors.AndroidSessionError(
            f"Failed to query PID of {process_name} via ADB: {err}"
        ) from err
    # Ignore transient ADB daemon banner lines (e.g. "* daemon started
    # successfully") that `adb.run()` merges from stderr into stdout.
    lines: list[str] = [
        line.strip()
        for line in raw.splitlines()
        if line.strip() and not line.strip().startswith("*")
    ]
    if not lines:
        return None
    # Return only the first PID if there are multiple matching processes.
    pid_str: str = lines[0].split()[0]
    if not pid_str.isdigit():
        raise session_errors.AndroidSessionError(
            f"Unexpected non-numeric PID {pid_str!r} returned by pidof for "
            f"{process_name}: {raw!r}"
        )
    return int(pid_str)


class AndroidSession(affordance.Affordance):
    """AndroidSession affordance implementation using ADB.

    Tracks the Android (Starnix) session so that unexpected Android restarts
    can be detected.
    """

    def __init__(self, device_name: str, adb: adb_transport.Adb) -> None:
        self._device_name: str = device_name
        self._adb: adb_transport.Adb = adb
        self._last_recorded_session: types.SessionId | None = None
        self._session_change_callbacks: list[Callable[[], None]] = []

        self.verify_supported()

    def verify_supported(self) -> None:
        """Verifies that AndroidSession affordance is supported by the device.

        Raises:
            NotSupportedError: If affordance is not supported.
        """
        self._adb.verify_supported()

    def fetch_current(self, wait: bool = True) -> types.SessionId:
        """Fetches the current Android session ID.

        Args:
            wait: If True, wait (up to 1 minute) until system_server has
                started. Otherwise, fail immediately if it is not running.

        Returns:
            The current Android session ID.

        Raises:
            AndroidSessionError: If the session ID could not be fetched.
        """
        if not wait:
            try:
                pid: int | None = get_process_id_of(self._adb, _SYSTEM_SERVER)
            except adb_errors.AdbError as err:
                raise session_errors.AndroidSessionError(
                    "Failed to fetch current Android session ID for "
                    f"{_SYSTEM_SERVER} on {self._device_name}: {err}"
                ) from err
            if pid is None:
                raise session_errors.AndroidSessionError(
                    "Failed to fetch current Android session ID for "
                    f"{_SYSTEM_SERVER} on {self._device_name}"
                )
            return types.SessionId(system_server_pid=pid)

        deadline: float = time.monotonic() + _FETCH_TIMEOUT.total_seconds()
        pid = None
        fatal_err: Exception | None = None
        last_err: Exception | None = None

        def _poll_system_server() -> bool:
            nonlocal pid, fatal_err, last_err
            timeout: float = max(
                deadline - time.monotonic(), _MIN_ADB_TIMEOUT_SEC
            )
            try:
                pid = get_process_id_of(
                    self._adb, _SYSTEM_SERVER, timeout=timeout
                )
            except (
                adb_errors.AdbUnauthorizedError,
                adb_errors.InitializationError,
            ) as err:
                fatal_err = err
                return True
            except (
                adb_errors.AdbError,
                session_errors.AndroidSessionError,
            ) as err:
                last_err = err
                _LOGGER.debug(
                    "Transient error while polling %s on %s, retrying: %s",
                    _SYSTEM_SERVER,
                    self._device_name,
                    err,
                )
                return False
            return pid is not None

        try:
            common.wait_for_state_sync(
                state_fn=_poll_system_server,
                expected_state=True,
                timeout=_FETCH_TIMEOUT.total_seconds(),
                wait_time=_FETCH_RETRY_DELAY.total_seconds(),
            )
        except errors.HoneydewTimeoutError as err:
            raise session_errors.AndroidSessionError(
                f"{_SYSTEM_SERVER} did not start on {self._device_name} "
                f"within {_FETCH_TIMEOUT}"
            ) from (last_err or err)

        if isinstance(fatal_err, session_errors.AndroidSessionError):
            raise fatal_err
        if fatal_err is not None:
            raise session_errors.AndroidSessionError(
                "Failed to fetch current Android session ID for "
                f"{_SYSTEM_SERVER} on {self._device_name}: {fatal_err}"
            ) from fatal_err
        assert pid is not None
        return types.SessionId(system_server_pid=pid)

    def register_session_change_callback(
        self, callback: Callable[[], None]
    ) -> None:
        """Registers a callback invoked whenever session_will_change() is called.

        Args:
            callback: Callback to invoke. Registering the same callback more
                than once has no effect.
        """
        if callback not in self._session_change_callbacks:
            self._session_change_callbacks.append(callback)

    def unregister_session_change_callback(
        self, callback: Callable[[], None]
    ) -> None:
        """Unregisters a previously registered session change callback.

        Args:
            callback: Callback to remove. Unregistering a callback that is not
                registered is a no-op.
        """
        if callback in self._session_change_callbacks:
            self._session_change_callbacks.remove(callback)

    def session_will_change(self) -> None:
        """Notifies that the Android session is expected to change.

        Clears the recorded session so that verify_same_session() does not fail
        on an expected restart (e.g. a reboot) and invokes all registered
        session change callbacks.
        """
        self._last_recorded_session = None
        # Use ExitStack so that every remaining callback still runs if an
        # earlier callback raises. ExitStack unwinds callbacks in LIFO order,
        # so registering reversed() executes them in FIFO registration order.
        with contextlib.ExitStack() as stack:
            for callback in reversed(self._session_change_callbacks):
                stack.callback(callback)

    def record_session(self, wait: bool = True) -> types.SessionId:
        """Records the current Android session ID.

        Args:
            wait: If True, wait until system_server has started.

        Returns:
            The recorded Android session ID.

        Raises:
            AndroidSessionError: If the session ID could not be fetched.
        """
        session_id: types.SessionId = self.fetch_current(wait)
        self._last_recorded_session = session_id
        return session_id

    def verify_same_session(self) -> None:
        """Verifies that Android has not restarted since record_session().

        This is a no-op if no session has been recorded, or if
        session_will_change() was called after the last record_session().

        Raises:
            AndroidSessionError: If the Android session has changed or if the
                session ID could not be fetched.
        """
        if self._last_recorded_session is None:
            return
        current: types.SessionId = self.fetch_current(wait=False)
        if self._last_recorded_session != current:
            raise session_errors.AndroidSessionError(
                f"Android session changed on {self._device_name}! Expected "
                f"{self._last_recorded_session}, got {current}"
            )
