# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for adb_server.py."""

import errno
import os
import signal
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from honeydew.transports.adb import adb_server
from honeydew.transports.adb import errors as adb_errors

_ADB_BINARY = "/custom/adb"
_SERIAL_ID = "12345678"
_OTHER_SERIAL_ID = "87654321"
_EPHEMERAL_PORT = 54321
_VENDOR_KEYS_PATH = "/path/to/vendor_keys"


# pylint: disable=protected-access
class AdbServerTests(unittest.TestCase):
    """Unit tests for AdbServer lifecycle, port allocation, and conflict detection."""

    def setUp(self) -> None:
        super().setUp()
        self._atexit_patcher = mock.patch("atexit.register", autospec=True)
        self.mock_atexit_register = self._atexit_patcher.start()

        self._temp_dir = tempfile.TemporaryDirectory()
        self.output_dir = Path(self._temp_dir.name)

        self.server = adb_server.AdbServer(
            adb_binary_path=_ADB_BINARY,
            serial_id=_SERIAL_ID,
            output_dir=self.output_dir,
        )

    def tearDown(self) -> None:
        self._temp_dir.cleanup()
        self._atexit_patcher.stop()
        super().tearDown()

    def test_init_registers_atexit_and_sets_defaults(self) -> None:
        """Test __init__ registers self.stop with atexit and initializes state."""
        # self.server is instantiated in setUp() while atexit.register is patched
        self.mock_atexit_register.assert_called_once_with(self.server.stop)
        self.assertEqual(self.server._adb_binary_path, _ADB_BINARY)
        self.assertEqual(self.server._serial_id, _SERIAL_ID)
        self.assertEqual(self.server._output_dir, self.output_dir)
        self.assertIsNone(self.server._vendor_keys_path)
        self.assertIsNone(self.server._process)
        self.assertIsNone(self.server._server_port)
        self.assertIsNone(self.server._server_thread)
        self.assertEqual(self.server._attempt_count, 0)
        self.assertEqual(self.server._server_id, 0)
        self.assertFalse(self.server._server_is_ready.is_set())
        self.assertFalse(self.server._server_stop_requested.is_set())

    def test_host_and_port(self) -> None:
        """Test host() and port() accessors."""
        self.assertEqual(self.server.host(), "127.0.0.1")
        # port() asserts if called before _server_port is assigned
        with self.assertRaises(AssertionError):
            self.server.port()

        self.server._server_port = _EPHEMERAL_PORT
        self.assertEqual(self.server.port(), _EPHEMERAL_PORT)

    @mock.patch("threading.Thread", autospec=True)
    def test_start_creates_daemon_thread_and_waits_for_ready(
        self, mock_thread_cls: mock.Mock
    ) -> None:
        """Test start() spawns a daemon thread and returns once ready flag is set."""
        mock_thread = mock.Mock()
        mock_thread.is_alive.return_value = True
        # Unblock start()'s readiness loop as soon as the thread starts
        mock_thread.start.side_effect = self.server._server_is_ready.set
        mock_thread_cls.return_value = mock_thread

        self.server.start()

        mock_thread_cls.assert_called_once_with(
            target=self.server._run_server,
            name=f"adb_server_thread_{_SERIAL_ID}",
            daemon=True,
        )
        mock_thread.start.assert_called_once_with()
        self.assertIs(self.server._server_thread, mock_thread)

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("threading.Thread", autospec=True)
    def test_start_polls_until_ready(
        self, mock_thread_cls: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test start() sleeps while waiting for ready flag to be set."""
        mock_thread = mock.Mock()
        mock_thread.is_alive.return_value = True
        mock_thread_cls.return_value = mock_thread

        # 1st False: initial check before spawning thread; 2nd False: 1st poll; 3rd True: 2nd poll
        with mock.patch.object(
            self.server._server_is_ready,
            "is_set",
            side_effect=[False, False, True],
        ):
            self.server.start()

        mock_sleep.assert_called_once_with(0.1)

    @mock.patch("threading.Thread", autospec=True)
    def test_start_server_thread_dies_before_ready_raises_error(
        self, mock_thread_cls: mock.Mock
    ) -> None:
        """Test start() raises AdbServerError if server thread dies before setting ready flag."""
        mock_thread = mock.Mock()
        mock_thread.is_alive.return_value = False
        mock_thread_cls.return_value = mock_thread

        with self.assertRaisesRegex(
            adb_errors.AdbServerError,
            "ADB server thread died before setting ready flag",
        ):
            self.server.start()

    @mock.patch("threading.Thread", autospec=True)
    def test_start_when_thread_already_alive_is_noop(
        self, mock_thread_cls: mock.Mock
    ) -> None:
        """Test start() does not spawn a new thread if one is already alive."""
        existing_thread = mock.Mock()
        existing_thread.is_alive.return_value = True
        self.server._server_thread = existing_thread

        self.server.start()

        mock_thread_cls.assert_not_called()

    @mock.patch("threading.Thread", autospec=True)
    def test_start_when_already_ready_is_noop(
        self, mock_thread_cls: mock.Mock
    ) -> None:
        """Test start() returns early if _server_is_ready is already set."""
        self.server._server_is_ready.set()

        self.server.start()

        mock_thread_cls.assert_not_called()

    @mock.patch("threading.Thread", autospec=True)
    def test_start_replaces_dead_thread_and_clears_stop_requested(
        self, mock_thread_cls: mock.Mock
    ) -> None:
        """Test start() cleans up a dead thread reference before starting a new one."""
        # Simulate a leftover dead thread and stale stop flag from a previous run
        dead_thread = mock.Mock()
        dead_thread.is_alive.return_value = False
        self.server._server_thread = dead_thread
        self.server._server_stop_requested.set()

        new_thread = mock.Mock()
        new_thread.is_alive.return_value = True
        new_thread.start.side_effect = self.server._server_is_ready.set
        mock_thread_cls.return_value = new_thread

        self.server.start()

        self.assertFalse(self.server._server_stop_requested.is_set())
        self.assertIs(self.server._server_thread, new_thread)

    def test_start_exceeds_max_restarts_raises_error(self) -> None:
        """Test start() raises AdbServerError when restart count reaches limit."""
        self.server._attempt_count = adb_server.AdbServer._MAX_SERVER_RESTARTS

        with self.assertRaisesRegex(
            adb_errors.AdbServerError,
            "Exceeded maximum number of ADB server restarts",
        ):
            self.server.start()

    def test_stop_when_already_stopped_is_noop(self) -> None:
        """Test stop() returns early when _server_thread is None."""
        self.server._server_thread = None
        self.server.stop()
        self.assertFalse(self.server._server_stop_requested.is_set())

    def test_stop_graceful_termination_sigterm(self) -> None:
        """Test stop() terminates the process gracefully with SIGTERM when it exits in time."""
        mock_proc = mock.Mock()
        mock_thread = mock.Mock()
        # Simulate thread exiting cleanly by the time join(timeout=5) finishes
        mock_thread.is_alive.return_value = False
        self.server._process = mock_proc
        self.server._server_thread = mock_thread

        self.server.stop()

        mock_proc.terminate.assert_called_once_with()
        mock_proc.wait.assert_called_once_with(timeout=3)
        mock_proc.kill.assert_not_called()
        mock_thread.join.assert_called_once_with(timeout=5)
        self.assertIsNone(self.server._server_thread)
        self.assertFalse(self.server._server_stop_requested.is_set())

    def test_stop_forceful_termination_sigkill_on_timeout(self) -> None:
        """Test stop() escalates from SIGTERM to SIGKILL if process.wait times out."""
        mock_proc = mock.Mock()
        # 1st wait(timeout=3) after SIGTERM times out; 2nd wait() after SIGKILL succeeds
        mock_proc.wait.side_effect = [
            subprocess.TimeoutExpired(cmd="adb", timeout=3),
            0,
        ]
        mock_thread = mock.Mock()
        mock_thread.is_alive.return_value = False
        self.server._process = mock_proc
        self.server._server_thread = mock_thread

        self.server.stop()

        mock_proc.terminate.assert_called_once_with()
        mock_proc.kill.assert_called_once_with()
        self.assertEqual(
            mock_proc.wait.call_args_list,
            [mock.call(timeout=3), mock.call()],
        )
        mock_thread.join.assert_called_once_with(timeout=5)
        self.assertIsNone(self.server._server_thread)

    def test_stop_keeps_thread_reference_if_still_alive_after_join(
        self,
    ) -> None:
        """Test stop() does not clear _server_thread if thread fails to exit after join."""
        mock_thread = mock.Mock()
        # Simulate a hung thread that remains alive even after join(timeout=5)
        mock_thread.is_alive.return_value = True
        self.server._server_thread = mock_thread

        self.server.stop()

        mock_thread.join.assert_called_once_with(timeout=5)
        self.assertIs(self.server._server_thread, mock_thread)
        self.assertTrue(self.server._server_stop_requested.is_set())

    def test_stop_reraises_unexpected_exception(self) -> None:
        """Test stop() logs and re-raises unexpected errors during termination."""
        mock_proc = mock.Mock()
        mock_proc.terminate.side_effect = RuntimeError("terminate failed")
        self.server._process = mock_proc
        self.server._server_thread = mock.Mock()

        with self.assertRaises(RuntimeError):
            self.server.stop()

    def test_restart_calls_stop_and_start(self) -> None:
        """Test restart() stops and then starts the server."""
        with (
            mock.patch.object(self.server, "stop") as mock_stop,
            mock.patch.object(self.server, "start") as mock_start,
        ):
            self.server.restart()
            mock_stop.assert_called_once_with()
            mock_start.assert_called_once_with()

    def test_reset_restart_count(self) -> None:
        """Test reset_restart_count() resets _attempt_count to 0 while preserving _server_id."""
        self.server._attempt_count = 2
        self.server._server_id = 5
        self.server.reset_restart_count()
        self.assertEqual(self.server._attempt_count, 0)
        self.assertEqual(self.server._server_id, 5)

    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_without_one_device(
        self, mock_run: mock.Mock, mock_kill: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers logs error for servers without --one-device."""
        # Both "fork-server" and "server nodaemon" without --one-device conflict with all devices
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-a", "adb"],
            returncode=0,
            stdout=(
                "1001 /usr/bin/adb -L tcp:5037 fork-server server --reply-fd 4\n"
                "1002 /custom/adb -P 5037 server nodaemon\n"
            ),
            stderr="",
        )
        # First two calls send SIGKILL; next two calls (signal 0) raise ProcessLookupError once dead
        mock_kill.side_effect = [
            None,
            None,
            ProcessLookupError,
            ProcessLookupError,
        ]

        with self.assertLogs(adb_server._LOGGER, level="ERROR") as cm:
            self.server._check_for_conflicting_adb_servers()

        self.assertEqual(len(cm.output), 2)
        self.assertIn("PID: 1001", cm.output[0])
        self.assertIn("PID: 1002", cm.output[1])
        self.assertEqual(
            mock_kill.call_args_list,
            [
                mock.call(1001, signal.SIGKILL),
                mock.call(1002, signal.SIGKILL),
                mock.call(1001, 0),
                mock.call(1002, 0),
            ],
        )

    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_matching_serial(
        self, mock_run: mock.Mock, mock_kill: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers logs error for server with matching serial."""
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-a", "adb"],
            returncode=0,
            stdout=(
                f"2001 /custom/adb -P 51000 --one-device {_SERIAL_ID} server nodaemon\n"
            ),
            stderr="",
        )
        # 1st call sends SIGKILL; 2nd call (signal 0) raises ProcessLookupError once process exits
        mock_kill.side_effect = [None, ProcessLookupError]

        with self.assertLogs(adb_server._LOGGER, level="ERROR") as cm:
            self.server._check_for_conflicting_adb_servers()

        self.assertEqual(len(cm.output), 1)
        self.assertIn("PID: 2001", cm.output[0])
        self.assertIn(_SERIAL_ID, cm.output[0])
        self.assertEqual(
            mock_kill.call_args_list,
            [
                mock.call(2001, signal.SIGKILL),
                mock.call(2001, 0),
            ],
        )

    @mock.patch("os.kill", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_different_serial_and_clients(
        self, mock_run: mock.Mock, mock_kill: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers ignores own_pids, different serials, and non-servers."""
        self.server._process = mock.Mock(pid=4001)
        # os.getpid() and 4001: own_pids (should be skipped even with conflicting args);
        # 3001: server for a different serial; 3002: adb client (not server); 3003: no command args
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-a", "adb"],
            returncode=0,
            stdout=(
                f"{os.getpid()} /custom/adb -P 5037 server nodaemon\n"
                f"4001 /custom/adb -P 51000 --one-device {_SERIAL_ID} server nodaemon\n"
                f"3001 /custom/adb -P 51001 --one-device {_OTHER_SERIAL_ID} server nodaemon\n"
                f"3002 /custom/adb -s {_SERIAL_ID} shell echo hi\n"
                "3003\n"
            ),
            stderr="",
        )

        with mock.patch.object(adb_server._LOGGER, "error") as mock_log_error:
            self.server._check_for_conflicting_adb_servers()
            mock_log_error.assert_not_called()
        mock_kill.assert_not_called()

    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_pgrep_no_matches(
        self, mock_run: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers returns quietly when pgrep exits with 1."""
        # Exit code 1 means no processes matched
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-a", "adb"],
            returncode=1,
            stdout="",
            stderr="",
        )
        with mock.patch.object(
            adb_server._LOGGER, "warning"
        ) as mock_log_warning:
            self.server._check_for_conflicting_adb_servers()
            mock_log_warning.assert_not_called()

    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_pgrep_error_code(
        self, mock_run: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers logs warning when pgrep exits with error code."""
        mock_run.return_value = subprocess.CompletedProcess(
            args=["pgrep", "-a", "adb"],
            returncode=2,
            stdout="",
            stderr="error",
        )
        with self.assertLogs(adb_server._LOGGER, level="WARNING") as cm:
            self.server._check_for_conflicting_adb_servers()
        self.assertTrue(
            any("pgrep failed with return code 2" in msg for msg in cm.output)
        )

    @mock.patch("subprocess.run", autospec=True)
    def test_check_for_conflicting_adb_servers_pgrep_exception(
        self, mock_run: mock.Mock
    ) -> None:
        """Test _check_for_conflicting_adb_servers logs warning if running pgrep raises OSError."""
        mock_run.side_effect = FileNotFoundError("pgrep not found")
        with self.assertLogs(adb_server._LOGGER, level="WARNING") as cm:
            self.server._check_for_conflicting_adb_servers()
        self.assertTrue(any("Failed to run pgrep" in msg for msg in cm.output))

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("subprocess.run", autospec=True)
    def test_kill_shared_server(
        self, mock_run: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Test _kill_shared_server runs adb kill-server and waits 5 seconds."""
        mock_run.return_value = subprocess.CompletedProcess(
            args=[_ADB_BINARY, "kill-server"],
            returncode=0,
            stdout="killed\n",
            stderr="warning\n",
        )

        self.server._kill_shared_server()

        mock_run.assert_called_once_with(
            [_ADB_BINARY, "kill-server"],
            capture_output=True,
            text=True,
        )
        mock_sleep.assert_called_once_with(5)

    @mock.patch("os.getppid", autospec=True)
    def test_run_server_retries_until_max_restarts(
        self, mock_getppid: mock.Mock
    ) -> None:
        """Test _run_server retries _run_one_server up to _MAX_SERVER_RESTARTS then raises RuntimeError."""
        # getppid() != 1 ensures _run_server does not exit early as an orphaned process
        mock_getppid.return_value = 1234

        with (
            mock.patch.object(
                self.server, "_run_one_server"
            ) as mock_run_one_server,
            mock.patch.object(
                self.server._server_stop_requested, "wait"
            ) as mock_wait,
        ):
            with self.assertRaisesRegex(
                RuntimeError,
                "ADB server failed to start multiple times. Giving up.",
            ):
                self.server._run_server()

            self.assertEqual(
                mock_run_one_server.call_args_list,
                [
                    mock.call("adb_server_1"),
                    mock.call("adb_server_2"),
                    mock.call("adb_server_3"),
                ],
            )
            self.assertEqual(
                self.server._attempt_count,
                adb_server.AdbServer._MAX_SERVER_RESTARTS,
            )
            self.assertEqual(
                self.server._server_id,
                adb_server.AdbServer._MAX_SERVER_RESTARTS,
            )
            # Waits 1s before the 2nd and 3rd attempts (not before the 1st)
            self.assertEqual(mock_wait.call_count, 2)

    @mock.patch("os.getppid", autospec=True)
    def test_run_server_exits_if_orphaned(
        self, mock_getppid: mock.Mock
    ) -> None:
        """Test _run_server exits loop immediately if parent process died (ppid == 1)."""
        mock_getppid.return_value = 1

        with mock.patch.object(
            self.server, "_run_one_server"
        ) as mock_run_one_server:
            self.server._run_server()
            mock_run_one_server.assert_not_called()

    @mock.patch("os.getppid", autospec=True)
    def test_run_server_reraises_exception_from_run_one_server(
        self, mock_getppid: mock.Mock
    ) -> None:
        """Test _run_server propagates exceptions raised by _run_one_server."""
        mock_getppid.return_value = 1234

        with mock.patch.object(
            self.server, "_run_one_server"
        ) as mock_run_one_server:
            mock_run_one_server.side_effect = adb_errors.AdbServerError(
                "bind failed"
            )
            with self.assertRaisesRegex(
                adb_errors.AdbServerError, "bind failed"
            ):
                self.server._run_server()

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("subprocess.Popen", autospec=True)
    @mock.patch("socket.socket", autospec=True)
    def test_run_one_server_ephemeral_port_allocation_and_poll_retry(
        self,
        mock_socket_cls: mock.Mock,
        mock_popen_cls: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test _run_one_server allocates an ephemeral port, retries connect_ex until open, and cleans up."""
        mock_sock = mock.MagicMock()
        mock_sock.__enter__.return_value = mock_sock
        mock_sock.getsockname.return_value = (mock.ANY, _EPHEMERAL_PORT)
        # First poll returns ECONNREFUSED, second poll succeeds (0)
        mock_sock.connect_ex.side_effect = [errno.ECONNREFUSED, 0]
        mock_socket_cls.return_value = mock_sock

        mock_proc = mock.Mock()
        mock_proc.returncode = 0
        # Process has already exited when finally block checks poll()
        mock_proc.poll.return_value = 0

        def wait_side_effect() -> int:
            # Verify server is marked ready while waiting on process
            self.assertTrue(self.server._server_is_ready.is_set())
            self.assertEqual(self.server._server_port, _EPHEMERAL_PORT)
            self.server._server_stop_requested.set()
            return 0

        mock_proc.wait.side_effect = wait_side_effect
        mock_popen_cls.return_value = mock_proc

        with (
            mock.patch.object(self.server, "_kill_shared_server"),
            mock.patch.object(
                self.server, "_check_for_conflicting_adb_servers"
            ),
        ):
            self.server._run_one_server("adb_server_1")

        mock_sock.bind.assert_called_once_with(("", 0))
        self.assertEqual(mock_sock.connect_ex.call_count, 2)
        mock_sleep.assert_called_once_with(0.1)

        mock_popen_cls.assert_called_once()
        popen_args, popen_kwargs = mock_popen_cls.call_args
        self.assertEqual(
            popen_args[0],
            [
                _ADB_BINARY,
                "-P",
                str(_EPHEMERAL_PORT),
                "--one-device",
                _SERIAL_ID,
                "server",
                "nodaemon",
            ],
        )
        self.assertEqual(popen_kwargs["env"]["ADB_TRACE"], "all")
        self.assertEqual(popen_kwargs["env"]["ADB_MDNS_OPENSCREEN"], "0")

        # Finally block resets _process and _server_port so subsequent runs pick a new port
        self.assertIsNone(self.server._process)
        self.assertIsNone(self.server._server_port)
        self.assertFalse(self.server._server_is_ready.is_set())

    @mock.patch("time.sleep", autospec=True)
    @mock.patch("time.time", autospec=True)
    @mock.patch("subprocess.Popen", autospec=True)
    @mock.patch("socket.socket", autospec=True)
    def test_run_one_server_port_collision_timeout_kills_leaked_process(
        self,
        mock_socket_cls: mock.Mock,
        mock_popen_cls: mock.Mock,
        mock_time: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Test _run_one_server raises AdbServerError on port bind timeout and kills leaked process."""
        mock_sock = mock.MagicMock()
        mock_sock.__enter__.return_value = mock_sock
        mock_sock.getsockname.return_value = (mock.ANY, _EPHEMERAL_PORT)
        mock_sock.connect_ex.return_value = errno.ECONNREFUSED
        mock_socket_cls.return_value = mock_sock

        # In the 5s port-polling loop, time.time() calls:
        # start_time=0.0, 1st poll=1.0 (<5s), 2nd check=6.0 (>=5s timeout)
        mock_time.side_effect = [0.0, 1.0, 6.0]

        mock_proc = mock.Mock()
        # poll() returning None means the spawned process is still running in the finally block
        mock_proc.poll.return_value = None
        # 1st wait(timeout=3) after SIGTERM times out; 2nd wait() after SIGKILL succeeds
        mock_proc.wait.side_effect = [
            subprocess.TimeoutExpired(cmd="adb", timeout=3),
            0,
        ]
        mock_popen_cls.return_value = mock_proc

        with (
            mock.patch.object(self.server, "_kill_shared_server"),
            mock.patch.object(
                self.server, "_check_for_conflicting_adb_servers"
            ),
        ):
            with self.assertRaisesRegex(
                adb_errors.AdbServerError,
                f"ADB server failed to bind to port {_EPHEMERAL_PORT} within 5 seconds",
            ):
                self.server._run_one_server("adb_server_1")

        mock_sleep.assert_called_once_with(0.1)

        # Verify leaked process was terminated and then forcefully killed on timeout
        mock_proc.terminate.assert_called_once_with()
        mock_proc.kill.assert_called_once_with()
        self.assertIsNone(self.server._process)
        self.assertIsNone(self.server._server_port)

    @mock.patch("subprocess.Popen", autospec=True)
    def test_run_one_server_aborts_if_stop_requested_before_popen(
        self, mock_popen_cls: mock.Mock
    ) -> None:
        """Test _run_one_server returns early if stop is requested before starting process."""
        self.server._server_stop_requested.set()

        with (
            mock.patch.object(self.server, "_kill_shared_server"),
            mock.patch.object(
                self.server, "_check_for_conflicting_adb_servers"
            ),
        ):
            self.server._run_one_server("adb_server_1")

        mock_popen_cls.assert_not_called()

    @mock.patch("subprocess.Popen", autospec=True)
    @mock.patch("socket.socket", autospec=True)
    def test_run_one_server_aborts_if_stop_requested_during_port_polling(
        self,
        mock_socket_cls: mock.Mock,
        mock_popen_cls: mock.Mock,
    ) -> None:
        """Test _run_one_server returns early if stop is requested while polling the port."""
        mock_sock = mock.MagicMock()
        mock_sock.__enter__.return_value = mock_sock
        mock_sock.getsockname.return_value = (mock.ANY, _EPHEMERAL_PORT)

        # Trigger stop request during the 1st port poll (after Popen has already started)
        def request_stop_on_connect(_: tuple[str, int]) -> int:
            self.server._server_stop_requested.set()
            return errno.ECONNREFUSED

        mock_sock.connect_ex.side_effect = request_stop_on_connect
        mock_socket_cls.return_value = mock_sock

        mock_proc = mock.Mock()
        mock_proc.poll.return_value = 0
        mock_popen_cls.return_value = mock_proc

        with (
            mock.patch.object(self.server, "_kill_shared_server"),
            mock.patch.object(
                self.server, "_check_for_conflicting_adb_servers"
            ),
        ):
            self.server._run_one_server("adb_server_1")

        self.assertFalse(self.server._server_is_ready.is_set())
        self.assertIsNone(self.server._process)
        self.assertIsNone(self.server._server_port)

    @mock.patch("subprocess.Popen", autospec=True)
    @mock.patch("socket.socket", autospec=True)
    def test_run_one_server_sets_vendor_keys_env(
        self,
        mock_socket_cls: mock.Mock,
        mock_popen_cls: mock.Mock,
    ) -> None:
        """Test _run_one_server sets ADB_VENDOR_KEYS when vendor_keys_path is provided."""
        mock_sock = mock.MagicMock()
        mock_sock.__enter__.return_value = mock_sock
        mock_sock.getsockname.return_value = (mock.ANY, _EPHEMERAL_PORT)
        mock_sock.connect_ex.return_value = 0
        mock_socket_cls.return_value = mock_sock

        mock_proc = mock.Mock()
        mock_proc.returncode = 0
        mock_proc.poll.return_value = 0
        mock_popen_cls.return_value = mock_proc

        server_with_keys = adb_server.AdbServer(
            adb_binary_path=_ADB_BINARY,
            serial_id=_SERIAL_ID,
            output_dir=self.output_dir,
            vendor_keys_path=_VENDOR_KEYS_PATH,
        )
        with (
            mock.patch.object(server_with_keys, "_kill_shared_server"),
            mock.patch.object(
                server_with_keys, "_check_for_conflicting_adb_servers"
            ),
        ):
            server_with_keys._run_one_server("adb_server_1")

        _, popen_kwargs = mock_popen_cls.call_args
        self.assertEqual(
            popen_kwargs["env"]["ADB_VENDOR_KEYS"], _VENDOR_KEYS_PATH
        )

    @mock.patch("subprocess.Popen", autospec=True)
    @mock.patch("socket.socket", autospec=True)
    def test_run_one_server_strips_inherited_vendor_keys_env(
        self,
        mock_socket_cls: mock.Mock,
        mock_popen_cls: mock.Mock,
    ) -> None:
        """Test _run_one_server strips inherited ADB_VENDOR_KEYS when vendor_keys_path is None."""
        mock_sock = mock.MagicMock()
        mock_sock.__enter__.return_value = mock_sock
        mock_sock.getsockname.return_value = (mock.ANY, _EPHEMERAL_PORT)
        mock_sock.connect_ex.return_value = 0
        mock_socket_cls.return_value = mock_sock

        mock_proc = mock.Mock()
        mock_proc.returncode = 0
        mock_proc.poll.return_value = 0
        mock_popen_cls.return_value = mock_proc

        with (
            mock.patch.dict(
                "os.environ", {"ADB_VENDOR_KEYS": _VENDOR_KEYS_PATH}
            ),
            mock.patch.object(self.server, "_kill_shared_server"),
            mock.patch.object(
                self.server, "_check_for_conflicting_adb_servers"
            ),
        ):
            self.server._run_one_server("adb_server_1")

        _, popen_kwargs = mock_popen_cls.call_args
        self.assertNotIn("ADB_VENDOR_KEYS", popen_kwargs["env"])


if __name__ == "__main__":
    unittest.main()
