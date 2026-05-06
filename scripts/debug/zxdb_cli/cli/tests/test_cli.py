# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import Mock, patch

from cli.cli import main
from shared.protocol import AttachRequest, StopRequest


class TestCLI(unittest.IsolatedAsyncioTestCase):
    @patch("cli.cli.start_daemon")
    async def test_start_command(self, mock_start: Mock) -> None:
        mock_start.return_value = 0
        exit_code = await main(["start"])
        self.assertEqual(exit_code, 0)
        mock_start.assert_called_once()

    @patch("cli.cli.send_command")
    async def test_stop_command(self, mock_stop: Mock) -> None:
        mock_stop.return_value = 0
        exit_code = await main(["stop"])
        self.assertEqual(exit_code, 0)
        mock_stop.assert_called_once_with(StopRequest())

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
        from shared.protocol import ThreadsRequest

        mock_send.return_value = 0
        exit_code = await main(["threads"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(ThreadsRequest())

    @patch("cli.cli.send_command")
    async def test_json_option_mutual_exclusion(self, mock_send: Mock) -> None:
        exit_code = await main(["--json", '{"command": "stop"}', "stop"])
        self.assertEqual(exit_code, 1)
        mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_json_option_valid(self, mock_send: Mock) -> None:
        from shared.protocol import StopRequest

        mock_send.return_value = 0
        exit_code = await main(["--json", '{"command": "stop"}'])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(StopRequest())

    @patch("cli.cli.send_command")
    async def test_json_option_invalid(self, mock_send: Mock) -> None:
        exit_code = await main(["--json", '{"command": "invalid"}'])
        self.assertEqual(exit_code, 1)
        mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_json_option_continue(self, mock_send: Mock) -> None:
        from shared.protocol import ContinueRequest

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
        from shared.protocol import PauseRequest

        mock_send.return_value = 0
        exit_code = await main(
            ["--json", '{"command": "pause", "thread_id": 1}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(PauseRequest(thread_id=1))

    @patch("cli.cli.send_command")
    async def test_json_option_stack_trace(self, mock_send: Mock) -> None:
        from shared.protocol import StackTraceRequest

        mock_send.return_value = 0
        exit_code = await main(
            ["--json", '{"command": "stackTrace", "thread_id": 1}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(StackTraceRequest(thread_id=1))

    @patch("cli.cli.send_command")
    async def test_json_option_attach(self, mock_send: Mock) -> None:
        from shared.protocol import AttachRequest

        mock_send.return_value = 0
        exit_code = await main(
            ["--json", '{"command": "attach", "filter": "my_process"}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(AttachRequest(filter="my_process"))


if __name__ == "__main__":
    unittest.main()
