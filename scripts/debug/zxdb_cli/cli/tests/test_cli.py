# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import unittest
from io import StringIO
from unittest.mock import AsyncMock, Mock, patch

from cli.cli import main
from daemon_manager.manager import (
    DaemonAlreadyRunningError,
    DaemonConnectionError,
    DaemonCrashError,
    DaemonHandshakeError,
    DaemonStartupTimeoutError,
)
from shared.protocol.attach import AttachRequest
from shared.protocol.continue_request import ContinueRequest
from shared.protocol.pause import PauseRequest
from shared.protocol.stack_trace import StackTraceRequest
from shared.protocol.stop import StopRequest
from shared.protocol.threads import ThreadsRequest
from shared.protocol.variables import VariablesRequest


class TestCLI(unittest.IsolatedAsyncioTestCase):
    @patch("cli.commands.start.start_daemon")
    async def test_start_command(self, mock_start: Mock) -> None:
        mock_start.return_value = 0
        exit_code = await main(["start"])
        self.assertEqual(exit_code, 0)
        mock_start.assert_called_once()

    @patch("cli.commands.stop.stop_daemon")
    async def test_stop_command(self, mock_stop: Mock) -> None:
        mock_stop.return_value = 0
        exit_code = await main(["stop"])
        self.assertEqual(exit_code, 0)
        mock_stop.assert_called_once()

    @patch("cli.cli.send_command")
    async def test_attach_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["attach", "my_process"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(AttachRequest(filter="my_process"))

    @patch("cli.cli.make_request")
    @patch("cli.cli.send_command")
    async def test_attach_command_receives_args(
        self, mock_send: Mock, mock_make: Mock
    ) -> None:
        mock_send.return_value = 0
        mock_make.return_value = StopRequest()  # dummy
        await main(["attach", "my_process"])

        mock_make.assert_called_once()
        args = mock_make.call_args[0][0]  # first argument
        self.assertIn("filter", args)
        self.assertEqual(args["filter"], "my_process")

    @patch("cli.cli.send_command")
    async def test_threads_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["threads"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(ThreadsRequest())

    @patch("cli.cli.send_command")
    async def test_json_option_mutual_exclusion(self, mock_send: Mock) -> None:
        exit_code = await main(["--json", '{"command": "stop"}', "stop"])
        self.assertEqual(exit_code, 1)
        mock_send.assert_not_called()

    @patch("cli.commands.stop.stop_daemon")
    @patch("cli.cli.send_command")
    async def test_json_option_valid(
        self, mock_send: Mock, mock_stop: Mock
    ) -> None:
        mock_stop.return_value = 0
        exit_code = await main(["--json", '{"command": "stop"}'])
        self.assertEqual(exit_code, 0)
        mock_stop.assert_called_once()
        mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_json_option_invalid(self, mock_send: Mock) -> None:
        exit_code = await main(["--json", '{"command": "invalid"}'])
        self.assertEqual(exit_code, 1)
        mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_json_option_continue(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "continue", "thread_id": 1, "single_thread": true}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            ContinueRequest(thread_id=1, single_thread=True)
        )

    @patch("cli.cli.send_command")
    async def test_json_option_pause(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["--json", '{"command": "pause", "thread_id": 1}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(PauseRequest(thread_id=1))

    @patch("cli.cli.send_command")
    async def test_json_option_stack_trace(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["--json", '{"command": "stackTrace", "thread_id": 1}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(StackTraceRequest(thread_id=1))

    @patch("cli.cli.send_command")
    async def test_json_option_attach(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["--json", '{"command": "attach", "filter": "my_process"}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(AttachRequest(filter="my_process"))

    @patch("cli.commands.start.DaemonManager")
    async def test_start_command_errors_formatting(
        self, mock_manager_class: Mock
    ) -> None:
        mock_manager = mock_manager_class.return_value

        exceptions_to_test = [
            DaemonAlreadyRunningError("Daemon socket already exists"),
            DaemonConnectionError("Connection failed"),
            DaemonCrashError("Daemon exited prematurely"),
            DaemonHandshakeError("Protocol version mismatch"),
            DaemonStartupTimeoutError("Startup timed out"),
        ]

        for exc in exceptions_to_test:
            mock_manager.start = AsyncMock(side_effect=exc)
            stderr = StringIO()
            with patch("sys.stderr", stderr):
                exit_code = await main(["start"])

            self.assertEqual(exit_code, 1)
            output = json.loads(stderr.getvalue())
            self.assertFalse(output["success"])
            self.assertEqual(output["message"], str(exc))

    @patch("cli.commands.start.DaemonManager")
    async def test_start_command_generic_exception_formatting(
        self, mock_manager_class: Mock
    ) -> None:
        mock_manager = mock_manager_class.return_value
        mock_manager.start = AsyncMock(
            side_effect=RuntimeError("Unexpected error")
        )

        stderr = StringIO()
        with patch("sys.stderr", stderr):
            exit_code = await main(["start"])

        self.assertEqual(exit_code, 1)
        output = json.loads(stderr.getvalue())
        self.assertFalse(output["success"])
        self.assertIn(
            "Failed to start daemon: Unexpected error", output["message"]
        )

    @patch("cli.cli.send_command")
    async def test_variables_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["variables", "1", "--frame-index", "2"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            VariablesRequest(thread_id=1, frame_index=2)
        )

    @patch("cli.cli.send_command")
    async def test_variables_command_default_frame_index(
        self, mock_send: Mock
    ) -> None:
        mock_send.return_value = 0
        exit_code = await main(["variables", "1"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            VariablesRequest(thread_id=1, frame_index=0)
        )

    @patch("cli.cli.send_command")
    async def test_locals_alias_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["locals", "1", "--frame-index", "2"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            VariablesRequest(thread_id=1, frame_index=2)
        )

    @patch("cli.cli.send_command")
    async def test_json_option_variables(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "variables", "thread_id": 1, "frame_index": 2}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            VariablesRequest(thread_id=1, frame_index=2)
        )


if __name__ == "__main__":
    unittest.main()
