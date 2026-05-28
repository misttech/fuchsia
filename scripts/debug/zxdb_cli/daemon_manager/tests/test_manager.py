# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import os
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, Mock, patch

from daemon_manager import (
    DaemonAlreadyRunningError,
    DaemonConnectionError,
    DaemonCrashError,
    DaemonHandshakeError,
    DaemonManager,
    DaemonStartupTimeoutError,
)
from shared.protocol import PROTOCOL_VERSION


def spawn_fake_daemon(write_fd: int) -> subprocess.Popen[bytes]:
    """Spawns a lightweight python fake process that simulates the daemon's startup behavior.

    This fake process will:
    1. Mark the write end of the pipe (write_fd) as inheritable.
    2. Write a single byte ("1") to the pipe to signal that it is ready.
    3. Sleep indefinitely (or for a long time) simulating a running daemon.

    Why:
      We spawn a real Python subprocess rather than using low-level mocks (like os.killpg,
      os.getpgid, or subprocess.Popen mock objects). This provides high-fidelity test coverage
      because:
      - It verifies the actual OS-level process group signal delivery (SIGTERM/SIGKILL to the PGID).
      - It avoids host process collisions (using real, active PIDs/PGIDs instead of fake integer mocks).
      - It accurately tests the interaction of raw file descriptors, pipes, and the asyncio event loop.

    Trade-offs:
      - Spawning a new Python interpreter process adds a tiny overhead of 10-20ms per test run.
      - This minor overhead is vastly outweighed by the massive gains in safety, realism, and
        prevention of fragile mock configurations.
    """
    # We want a portable python one-liner that can be executed on any host system.
    # The code must set write_fd as inheritable, write b"1" to it, and sleep.
    code = (
        f"import os, time; "
        f"os.set_inheritable({write_fd}, True); "
        f"os.write({write_fd}, b'1'); "
        f"time.sleep(100)"
    )

    # We set start_new_session=True so it gets its own PGID (critical for process group signaling).
    return subprocess.Popen(
        [sys.executable, "-c", code],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
        pass_fds=[write_fd],
    )


class TestDaemonManager(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.spawned_processes: list[subprocess.Popen[bytes]] = []

    def tearDown(self) -> None:
        patch.stopall()
        for proc in self.spawned_processes:
            if proc.poll() is None:
                try:
                    pgid = os.getpgid(proc.pid)
                    if pgid != os.getpgrp():
                        os.killpg(pgid, signal.SIGKILL)
                    else:
                        proc.kill()
                except Exception:
                    try:
                        proc.kill()
                    except Exception:
                        pass
                proc.wait()
        super().tearDown()

    def tracking_spawn(self, write_fd: int) -> subprocess.Popen[bytes]:
        """Spawns a fake daemon process and tracks it for cleanup."""
        proc = spawn_fake_daemon(write_fd)
        self.spawned_processes.append(proc)
        return proc

    @patch("daemon_manager.manager.UDS_PATH")
    @patch("asyncio.open_unix_connection")
    async def test_connect_to_running_daemon_success(
        self, mock_connect: Mock, mock_uds_path: Mock
    ) -> None:
        """Tests that connect_to_running_daemon succeeds silently when UDS responds success."""
        mock_uds_path.exists.return_value = True

        mock_reader = AsyncMock()
        mock_writer = MagicMock()
        mock_writer.drain = AsyncMock()
        mock_writer.wait_closed = AsyncMock()
        mock_connect.return_value = (mock_reader, mock_writer)

        mock_reader.readline.return_value = b'{"success": true}\n'

        manager = DaemonManager(socket_path=mock_uds_path, port=1234)
        await manager.connect_to_running_daemon()

        mock_connect.assert_called_once()
        mock_writer.write.assert_called_once()

    @patch("daemon_manager.manager.UDS_PATH")
    @patch("asyncio.open_unix_connection")
    async def test_connect_to_running_daemon_failure(
        self, mock_connect: Mock, mock_uds_path: Mock
    ) -> None:
        """Tests that connect_to_running_daemon raises DaemonConnectionError when daemon returns success=False."""
        mock_uds_path.exists.return_value = True

        mock_reader = AsyncMock()
        mock_writer = MagicMock()
        mock_writer.drain = AsyncMock()
        mock_writer.wait_closed = AsyncMock()
        mock_connect.return_value = (mock_reader, mock_writer)

        mock_reader.readline.return_value = (
            b'{"success": false, "message": "Connect failed"}\n'
        )

        manager = DaemonManager(socket_path=mock_uds_path, port=1234)
        with self.assertRaises(DaemonConnectionError) as ctx:
            await manager.connect_to_running_daemon()
        self.assertIn("Connect failed", str(ctx.exception))

    @patch("daemon_manager.manager.UDS_PATH")
    @patch("asyncio.open_unix_connection")
    async def test_connect_to_running_daemon_connection_error(
        self, mock_connect: Mock, mock_uds_path: Mock
    ) -> None:
        """Tests that connect_to_running_daemon raises DaemonConnectionError on connection failure."""
        mock_uds_path.exists.return_value = True
        mock_connect.side_effect = ConnectionRefusedError("Connection refused")

        manager = DaemonManager(socket_path=mock_uds_path, port=1234)
        with self.assertRaises(DaemonConnectionError) as ctx:
            await manager.connect_to_running_daemon()
        self.assertIn("Connection refused", str(ctx.exception))

    @patch("daemon_manager.manager.UDS_PATH")
    @patch("daemon_manager.manager.DaemonManager.do_handshake_and_connect")
    @patch("daemon_manager.manager.DaemonManager.connect_to_running_daemon")
    async def test_start_daemon_already_running_connect_success(
        self,
        mock_connect_helper: Mock,
        mock_handshake: Mock,
        mock_uds_path: Mock,
    ) -> None:
        """Tests that start returns None when connect_to_existing is True and helper succeeds."""
        mock_uds_path.exists.return_value = True
        mock_handshake.return_value = True  # Active daemon
        mock_connect_helper.return_value = None  # Success

        manager = DaemonManager(
            socket_path=mock_uds_path, port=1234, connect_to_existing=True
        )
        res = await manager.start()
        self.assertIsNone(res)
        mock_connect_helper.assert_called_once()

    @patch("daemon_manager.manager.UDS_PATH")
    @patch("daemon_manager.manager.DaemonManager.do_handshake_and_connect")
    @patch("daemon_manager.manager.DaemonManager.connect_to_running_daemon")
    async def test_start_daemon_already_running_connect_failure(
        self,
        mock_connect_helper: Mock,
        mock_handshake: Mock,
        mock_uds_path: Mock,
    ) -> None:
        """Tests that start propagates DaemonConnectionError."""
        mock_uds_path.exists.return_value = True
        mock_handshake.return_value = True
        mock_connect_helper.side_effect = DaemonConnectionError(
            "Failed to connect"
        )

        manager = DaemonManager(
            socket_path=mock_uds_path, port=1234, connect_to_existing=True
        )
        with self.assertRaises(DaemonConnectionError):
            await manager.start()

    @patch("daemon_manager.manager.UDS_PATH")
    @patch("daemon_manager.manager.DaemonManager.do_handshake_and_connect")
    async def test_start_daemon_already_running_no_connect(
        self, mock_handshake: Mock, mock_uds_path: Mock
    ) -> None:
        """Tests that start raises DaemonAlreadyRunningError when connect_to_existing is False."""
        mock_uds_path.exists.return_value = True
        mock_handshake.return_value = True

        manager = DaemonManager(
            socket_path=mock_uds_path, port=1234, connect_to_existing=False
        )
        with self.assertRaises(DaemonAlreadyRunningError):
            await manager.start()

    @patch("daemon_manager.manager.UDS_PATH")
    @patch("daemon_manager.manager.DaemonManager.do_handshake_and_connect")
    @patch("daemon_manager.manager.DaemonManager._spawn_daemon_process")
    async def test_start_daemon_stale_socket_cleanup(
        self,
        mock_spawn: Mock,
        mock_handshake: Mock,
        mock_uds_path: Mock,
    ) -> None:
        """Tests that start unlinks UDS socket when handshake returns None (stale socket)."""
        mock_uds_path.exists.return_value = True
        mock_handshake.return_value = None  # Stale socket

        # Mock spawn to stop execution after cleanup
        mock_spawn.side_effect = RuntimeError("Stop execution")

        manager = DaemonManager(socket_path=mock_uds_path, port=1234)
        with self.assertRaises(RuntimeError) as ctx:
            await manager.start()
        self.assertEqual(str(ctx.exception), "Stop execution")

        mock_uds_path.unlink.assert_called_once_with(missing_ok=True)

    @patch("daemon_manager.manager.FxCmd")
    @patch("os.set_inheritable")
    @patch("subprocess.Popen")
    def test_spawn_daemon_process(
        self,
        mock_popen: Mock,
        mock_set_inheritable: Mock,
        mock_fx_cmd_class: Mock,
    ) -> None:
        """Tests that _spawn_daemon_process spawns Popen process group with correct pass_fds."""
        mock_fx_cmd = mock_fx_cmd_class.return_value
        mock_fx_cmd.command_line.return_value = [
            "fx",
            "zxdb-daemon",
            "--port",
            "1234",
            "--ready-fd=5",
        ]

        mock_proc = Mock()
        mock_popen.return_value = mock_proc

        manager = DaemonManager(port=1234)
        proc = manager._spawn_daemon_process(5)
        self.assertEqual(proc, mock_proc)

        mock_fx_cmd.command_line.assert_called_once_with(
            "zxdb-daemon", "--port", "1234", "--ready-fd=5"
        )
        mock_set_inheritable.assert_called_once_with(5, True)
        mock_popen.assert_called_once_with(
            ["fx", "zxdb-daemon", "--port", "1234", "--ready-fd=5"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
            pass_fds=[5],
        )

    async def test_wait_for_ready_signal_success(self) -> None:
        """Tests _wait_for_ready_signal completes successfully when sync byte is written."""
        r, w = os.pipe()
        try:
            manager = DaemonManager()
            task = asyncio.create_task(manager._wait_for_ready_signal(r))
            os.write(w, b"1")
            await task
        finally:
            os.close(w)
            try:
                os.close(r)
            except OSError:
                pass

    async def test_wait_for_ready_signal_timeout(self) -> None:
        """Tests _wait_for_ready_signal raises DaemonStartupTimeoutError on timeout."""
        r, w = os.pipe()
        try:
            manager = DaemonManager(startup_timeout=0.01)
            with self.assertRaises(DaemonStartupTimeoutError):
                await manager._wait_for_ready_signal(r)
        finally:
            os.close(w)
            try:
                os.close(r)
            except OSError:
                pass

    async def test_wait_for_ready_signal_premature_eof(self) -> None:
        """Tests _wait_for_ready_signal raises DaemonCrashError when pipe is closed prematurely (EOF)."""
        r, w = os.pipe()
        try:
            manager = DaemonManager()
            task = asyncio.create_task(manager._wait_for_ready_signal(r))

            await asyncio.sleep(0)

            os.close(w)
            w = -1

            with self.assertRaises(DaemonCrashError) as ctx:
                await task
            self.assertIn("Daemon exited prematurely", str(ctx.exception))
        finally:
            if w != -1:
                os.close(w)
            try:
                os.close(r)
            except OSError:
                pass

    @patch("daemon_manager.manager.UDS_PATH")
    @patch("asyncio.open_unix_connection")
    @patch("daemon_manager.manager.DaemonManager.do_handshake_and_connect")
    @patch("daemon_manager.manager.DaemonManager._wait_for_ready_signal")
    async def test_start_new_daemon_success(
        self,
        mock_wait_ready: Mock,
        mock_handshake: Mock,
        mock_open_connection: Mock,
        mock_uds_path: Mock,
    ) -> None:
        """Tests starting a new daemon process successfully."""
        mock_uds_path.exists.return_value = False

        mock_proc = MagicMock(spec=subprocess.Popen)
        mock_proc.poll.return_value = None

        mock_spawn_fn = Mock(return_value=mock_proc)

        mock_reader = AsyncMock()
        mock_writer = MagicMock()
        mock_writer.drain = AsyncMock()
        mock_writer.wait_closed = AsyncMock()
        mock_open_connection.return_value = (mock_reader, mock_writer)

        mock_handshake.return_value = True
        mock_reader.readline.return_value = b'{"success": true}\n'

        manager = DaemonManager(
            socket_path=mock_uds_path,
            port=1234,
            spawn_fn=mock_spawn_fn,
        )
        proc = await manager.start()

        self.assertEqual(proc, mock_proc)
        mock_spawn_fn.assert_called_once()
        mock_wait_ready.assert_called_once()
        mock_handshake.assert_called_once()
        mock_open_connection.assert_called_once_with(mock_uds_path)
        mock_writer.write.assert_called_once()

    async def test_start_new_daemon_handshake_failure(self) -> None:
        """Tests startup failure because handshake fails after signaling ready."""
        with tempfile.TemporaryDirectory() as temp_dir:
            fake_socket = (
                Path(temp_dir) / "non_existent_handshake_failure_test.sock"
            )
            manager = DaemonManager(
                socket_path=fake_socket,
                port=1234,
                spawn_fn=self.tracking_spawn,
            )

            # Startup naturally raises DaemonHandshakeError since connecting to fake_socket fails.
            with self.assertRaises(DaemonHandshakeError) as ctx:
                await manager.start()

            self.assertIn(
                "Daemon started but failed to respond to handshake in time.",
                str(ctx.exception),
            )

            # Verify that the fake process is terminated and cleaned up cleanly.
            self.assertEqual(len(self.spawned_processes), 1)
            spawned_proc = self.spawned_processes[0]
            self.assertIsNotNone(
                spawned_proc.poll()
            )  # It should have exited (terminated)

    @patch("asyncio.open_unix_connection")
    async def test_start_new_daemon_session_startup_timeout(
        self,
        mock_open_connection: Mock,
    ) -> None:
        """Tests startup failure when connecting/establishing session times out."""
        mock_reader = AsyncMock()
        mock_writer = MagicMock()
        mock_writer.drain = AsyncMock()
        mock_writer.wait_closed = AsyncMock()
        mock_reader.readline.return_value = f'{{"success": true, "body": {{"protocol_version": {PROTOCOL_VERSION}}}}}\n'.encode(
            "utf-8"
        )

        # Mock the first connection (handshake) to succeed, and the second
        # connection (session start request) to timeout.
        mock_open_connection.side_effect = [
            (mock_reader, mock_writer),
            asyncio.TimeoutError("Connection timeout"),
        ]

        with tempfile.TemporaryDirectory() as temp_dir:
            fake_socket = Path(temp_dir) / "non_existent_timeout_test.sock"
            manager = DaemonManager(
                socket_path=fake_socket,
                port=1234,
                spawn_fn=self.tracking_spawn,
            )

            with self.assertRaises(DaemonConnectionError):
                await manager.start()

            # Verify that the fake process is terminated and cleaned up cleanly.
            self.assertEqual(len(self.spawned_processes), 1)
            spawned_proc = self.spawned_processes[0]
            self.assertIsNotNone(spawned_proc.poll())


if __name__ == "__main__":
    unittest.main()
