# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for android_session.py."""

import unittest
from typing import Any
from unittest import mock

from honeydew.affordances.android_session import (
    android_session,
    errors,
    types,
)
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.adb import errors as adb_errors

_DEVICE_NAME: str = "fuchsia-emulator"


class GetProcessIdOfTests(unittest.TestCase):
    """Unit tests for android_session.get_process_id_of()."""

    def setUp(self) -> None:
        super().setUp()
        self.mock_adb: Any = mock.create_autospec(
            adb_transport.Adb, instance=True
        )

    def test_returns_first_pid(self) -> None:
        """Verifies the first PID is returned when multiple processes match."""
        self.mock_adb.run.return_value = "1234 5678\n"
        self.assertEqual(
            android_session.get_process_id_of(self.mock_adb, "system_server"),
            1234,
        )
        self.mock_adb.run.assert_called_once_with(
            ["shell", "pidof system_server || true"]
        )

    def test_quotes_process_name(self) -> None:
        """Verifies process_name with special characters is shell-quoted."""
        self.mock_adb.run.return_value = "1234\n"
        self.assertEqual(
            android_session.get_process_id_of(
                self.mock_adb, "my process; echo"
            ),
            1234,
        )
        self.mock_adb.run.assert_called_once_with(
            ["shell", "pidof 'my process; echo' || true"]
        )

    def test_ignores_adb_daemon_banner_lines(self) -> None:
        """Verifies ADB daemon banner lines starting with '*' are ignored."""
        self.mock_adb.run.return_value = (
            "* daemon not running; starting now at tcp:5037\n"
            "* daemon started successfully\n"
            "1234\n"
        )
        self.assertEqual(
            android_session.get_process_id_of(self.mock_adb, "system_server"),
            1234,
        )

    def test_passes_timeout_to_adb_run(self) -> None:
        """Verifies an explicit timeout is forwarded to adb.run()."""
        self.mock_adb.run.return_value = "1234\n"
        self.assertEqual(
            android_session.get_process_id_of(
                self.mock_adb, "system_server", timeout=15.0
            ),
            1234,
        )
        self.mock_adb.run.assert_called_once_with(
            ["shell", "pidof system_server || true"], timeout=15.0
        )

    def test_returns_none_when_not_running(self) -> None:
        """Verifies None is returned when pidof prints nothing."""
        self.mock_adb.run.return_value = "\n"
        self.assertIsNone(
            android_session.get_process_id_of(self.mock_adb, "system_server")
        )

    def test_raises_android_session_error_on_adb_command_error(self) -> None:
        """Verifies AdbCommandError is wrapped in AndroidSessionError."""
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError("failed")
        with self.assertRaisesRegex(
            errors.AndroidSessionError, "Failed to query PID of system_server"
        ):
            android_session.get_process_id_of(self.mock_adb, "system_server")

    def test_propagates_adb_unauthorized_and_connection_errors(self) -> None:
        """Verifies AdbUnauthorizedError and AdbConnectionError propagate."""
        self.mock_adb.run.side_effect = adb_errors.AdbUnauthorizedError(
            "unauthorized"
        )
        with self.assertRaisesRegex(
            adb_errors.AdbUnauthorizedError, "unauthorized"
        ):
            android_session.get_process_id_of(self.mock_adb, "system_server")

        self.mock_adb.run.side_effect = adb_errors.AdbConnectionError(
            "disconnected"
        )
        with self.assertRaisesRegex(
            adb_errors.AdbConnectionError, "disconnected"
        ):
            android_session.get_process_id_of(self.mock_adb, "system_server")

    def test_non_digit_raises_android_session_error(self) -> None:
        """Verifies a non-numeric PID raises AndroidSessionError immediately."""
        self.mock_adb.run.return_value = "not_a_pid\n"
        with self.assertRaisesRegex(
            errors.AndroidSessionError,
            r"Unexpected non-numeric PID 'not_a_pid'.*'not_a_pid\\n'",
        ):
            android_session.get_process_id_of(self.mock_adb, "system_server")

    def test_wraps_adb_timeout_error(self) -> None:
        """Verifies AdbTimeoutError is wrapped in AndroidSessionError."""
        self.mock_adb.run.side_effect = adb_errors.AdbTimeoutError(
            "adb timed out"
        )
        with self.assertRaisesRegex(
            errors.AndroidSessionError, "adb timed out"
        ):
            android_session.get_process_id_of(self.mock_adb, "system_server")


class AndroidSessionTests(unittest.TestCase):
    """Unit tests for AndroidSession."""

    def setUp(self) -> None:
        super().setUp()
        self.mock_adb: Any = mock.create_autospec(
            adb_transport.Adb, instance=True
        )
        self.session = android_session.AndroidSession(
            device_name=_DEVICE_NAME, adb=self.mock_adb
        )
        sleep_patcher = mock.patch(
            "honeydew.affordances.android_session.android_session."
            "time.sleep",
            autospec=True,
        )
        self.mock_sleep = sleep_patcher.start()
        self.addCleanup(sleep_patcher.stop)

    def test_verify_supported_called_in_init(self) -> None:
        """Verifies that __init__ calls verify_supported() and delegates to adb."""
        self.mock_adb.verify_supported.assert_called_once_with()
        with mock.patch.object(
            android_session.AndroidSession,
            "verify_supported",
            autospec=True,
        ) as mock_verify_supported:
            android_session.AndroidSession(
                device_name=_DEVICE_NAME, adb=self.mock_adb
            )
        mock_verify_supported.assert_called_once()

    def test_fetch_current_immediate(self) -> None:
        """Verifies fetch_current() returns the system_server PID."""
        self.mock_adb.run.return_value = "1234\n"
        self.assertEqual(
            self.session.fetch_current(wait=False),
            types.SessionId(system_server_pid=1234),
        )
        self.mock_adb.run.assert_called_once_with(
            ["shell", "pidof system_server || true"]
        )
        self.mock_sleep.assert_not_called()

    def test_fetch_current_immediate_missing_raises(self) -> None:
        """Verifies fetch_current() fails fast if not waiting for processes."""
        self.mock_adb.run.return_value = ""
        with self.assertRaisesRegex(
            errors.AndroidSessionError,
            "Failed to fetch current Android session ID for system_server",
        ):
            self.session.fetch_current(wait=False)
        self.mock_sleep.assert_not_called()

    def test_fetch_current_fails_fast_on_persistent_adb_error(self) -> None:
        """Verifies fetch_current(wait=True) aborts immediately on permanent
        ADB errors (e.g. AdbUnauthorizedError)."""
        self.mock_adb.run.side_effect = adb_errors.AdbUnauthorizedError(
            "device unauthorized"
        )
        with self.assertRaisesRegex(
            errors.AndroidSessionError, "device unauthorized"
        ):
            self.session.fetch_current(wait=True)
        self.mock_adb.run.assert_called_once()
        self.mock_sleep.assert_not_called()

    def test_fetch_current_retries_transient_adb_error(self) -> None:
        """Verifies fetch_current(wait=True) retries transient ADB errors."""
        self.mock_adb.run.side_effect = [
            adb_errors.AdbCommandError("device offline"),
            "5678\n",
        ]
        self.assertEqual(
            self.session.fetch_current(wait=True),
            types.SessionId(system_server_pid=5678),
        )
        self.assertEqual(self.mock_adb.run.call_count, 2)
        self.mock_sleep.assert_called_once_with(1.0)

    def test_fetch_current_with_retry(self) -> None:
        """Verifies fetch_current() polls until system_server starts."""
        self.mock_adb.run.side_effect = ["", "5678\n"]
        self.assertEqual(
            self.session.fetch_current(wait=True),
            types.SessionId(system_server_pid=5678),
        )
        self.mock_sleep.assert_called_once_with(1.0)

    @mock.patch(
        "honeydew.utils.common.time.time",
        autospec=True,
    )
    @mock.patch(
        "honeydew.affordances.android_session.android_session."
        "time.monotonic",
        autospec=True,
    )
    def test_fetch_current_times_out(
        self, mock_monotonic: mock.Mock, mock_time: mock.Mock
    ) -> None:
        """Verifies fetch_current() raises after the timeout."""
        mock_monotonic.return_value = 0.0
        mock_time.side_effect = [0.0, 0.0, 61.0]
        self.mock_adb.run.return_value = ""
        with self.assertRaisesRegex(
            errors.AndroidSessionError, "did not start"
        ):
            self.session.fetch_current(wait=True)
        self.assertEqual(self.mock_sleep.call_count, 1)

    def test_record_and_verify_same_session_success(self) -> None:
        """Verifies verify_same_session() passes when the PID is unchanged."""
        self.mock_adb.run.return_value = "4321\n"
        recorded = self.session.record_session(wait=False)
        self.assertEqual(recorded, types.SessionId(system_server_pid=4321))
        self.session.verify_same_session()

    def test_verify_same_session_without_record_is_noop(self) -> None:
        """Verifies verify_same_session() is a no-op if nothing was recorded."""
        self.session.verify_same_session()
        self.mock_adb.run.assert_not_called()

    def test_verify_same_session_raises_on_change(self) -> None:
        """Verifies verify_same_session() raises when the PID changes."""
        self.mock_adb.run.side_effect = ["1111\n", "2222\n"]
        self.session.record_session(wait=False)
        with self.assertRaises(errors.AndroidSessionError):
            self.session.verify_same_session()

    def test_session_will_change_suppresses_verification_and_calls_callbacks(
        self,
    ) -> None:
        """Verifies session_will_change() clears the session and runs callbacks."""
        self.mock_adb.run.side_effect = ["1111\n", "2222\n"]
        cb = mock.Mock()
        self.session.register_session_change_callback(cb)
        self.session.register_session_change_callback(cb)
        self.session.record_session(wait=False)
        self.session.session_will_change()
        cb.assert_called_once()
        self.session.verify_same_session()

    def test_unregister_session_change_callback(self) -> None:
        """Verifies unregister_session_change_callback() removes callbacks."""
        cb = mock.Mock()
        self.session.register_session_change_callback(cb)
        self.session.unregister_session_change_callback(cb)
        # Unregistering again is a no-op.
        self.session.unregister_session_change_callback(cb)
        self.session.session_will_change()
        cb.assert_not_called()

    def test_session_will_change_runs_all_callbacks_on_error(self) -> None:
        """Verifies all callbacks run in order even if an earlier one raises."""
        order: list[str] = []

        def cb1() -> None:
            order.append("cb1")
            raise RuntimeError("cb1 failed")

        def cb2() -> None:
            order.append("cb2")

        self.session.register_session_change_callback(cb1)
        self.session.register_session_change_callback(cb2)
        with self.assertRaisesRegex(RuntimeError, "cb1 failed"):
            self.session.session_will_change()
        self.assertEqual(order, ["cb1", "cb2"])


if __name__ == "__main__":
    unittest.main()
