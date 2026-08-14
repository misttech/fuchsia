# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import json
import unittest
from io import StringIO
from unittest.mock import AsyncMock, Mock, patch

from cli.cli import main
from cli.commands.break_cmd import resolve_path
from daemon_manager.manager import (
    DaemonAlreadyRunningError,
    DaemonConnectionError,
    DaemonCrashError,
    DaemonHandshakeError,
    DaemonStartupTimeoutError,
)
from shared.protocol.attach import AttachRequest
from shared.protocol.break_request import BreakRequest
from shared.protocol.continue_request import ContinueRequest
from shared.protocol.evaluate import EvaluateRequest
from shared.protocol.finish import FinishRequest
from shared.protocol.next_request import NextRequest
from shared.protocol.pause import PauseRequest
from shared.protocol.stack_trace import StackTraceRequest
from shared.protocol.step_in import StepInRequest
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
    async def test_finish_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["finish", "1", "--single-thread"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            FinishRequest(command="finish", thread_id=1, single_thread=True)
        )

    @patch("cli.cli.send_command")
    async def test_finish_command_aliases(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        for alias in ["step-out", "step_out", "stepout"]:
            mock_send.reset_mock()
            exit_code = await main([alias, "1"])
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(
                FinishRequest(command="finish", thread_id=1)
            )

    @patch("cli.cli.send_command")
    async def test_json_option_finish(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        # TODO(https://fxbug.dev/541037615): support aliases such as step_out and step-out in JSON mode.
        exit_code = await main(
            [
                "--json",
                '{"command": "finish", "thread_id": 1, "single_thread": true}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            FinishRequest(command="finish", thread_id=1, single_thread=True)
        )

    @patch("cli.cli.send_command")
    async def test_next_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["next", "1", "--single-thread", "--granularity", "line"]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            NextRequest(
                command="next",
                thread_id=1,
                single_thread=True,
                granularity="line",
            )
        )

    @patch("cli.cli.send_command")
    async def test_next_command_aliases(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        for alias in ["n", "step-over", "step_over", "stepover"]:
            mock_send.reset_mock()
            exit_code = await main([alias, "1"])
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(
                NextRequest(command="next", thread_id=1)
            )

    @patch("cli.cli.send_command")
    async def test_json_option_next(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "next", "thread_id": 1, "single_thread": true, "granularity": "line"}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            NextRequest(thread_id=1, single_thread=True, granularity="line")
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
    async def test_pause_command_thread_id(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["pause", "-t", "1"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(PauseRequest(thread_id=1))

        mock_send.reset_mock()
        exit_code = await main(["pause", "--thread-id", "1"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(PauseRequest(thread_id=1))

    @patch("cli.cli.send_command")
    async def test_pause_command_pid(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["pause", "-p", "12345"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(PauseRequest(pid=12345))

    @patch("cli.cli.send_command")
    async def test_pause_command_both_thread_id_and_pid_fails(
        self, mock_send: Mock
    ) -> None:
        with contextlib.redirect_stderr(StringIO()):
            with self.assertRaises(SystemExit):
                await main(["pause", "-t", "1", "-p", "12345"])
        mock_send.assert_not_called()

    @patch("cli.cli.send_command")
    async def test_json_option_stack_trace(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            ["--json", '{"command": "stackTrace", "thread_id": 1}']
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            StackTraceRequest(thread_id=1, raw=False)
        )

    @patch("cli.cli.send_command")
    async def test_stack_trace_command_raw_flag(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["stackTrace", "1", "-r"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            StackTraceRequest(thread_id=1, raw=True)
        )

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

    @patch("cli.commands.break_cmd.resolve_path")
    @patch("cli.cli.send_command")
    async def test_break_command(
        self, mock_send: Mock, mock_resolve: Mock
    ) -> None:
        mock_send.return_value = 0
        mock_resolve.return_value = "/path/to/fuchsia/src/main.rs"

        exit_code = await main(["break", "src/main.rs:12"])
        self.assertEqual(exit_code, 0)
        mock_resolve.assert_called_once_with("src/main.rs")
        mock_send.assert_called_once_with(
            BreakRequest(file="/path/to/fuchsia/src/main.rs", line=12)
        )

    @patch("cli.commands.break_cmd.resolve_path")
    @patch("cli.cli.send_command")
    async def test_break_command_aliases(
        self, mock_send: Mock, mock_resolve: Mock
    ) -> None:
        mock_send.return_value = 0
        mock_resolve.return_value = "/path/to/fuchsia/src/main.rs"

        for alias in [
            "breakpoint",
            "b",
            "setBreakpoints",
            "set-breakpoints",
            "set_breakpoints",
        ]:
            mock_send.reset_mock()
            mock_resolve.reset_mock()
            exit_code = await main([alias, "src/main.rs:12"])
            self.assertEqual(exit_code, 0, f"Failed for alias: {alias}")
            mock_resolve.assert_called_once_with("src/main.rs")
            mock_send.assert_called_once_with(
                BreakRequest(file="/path/to/fuchsia/src/main.rs", line=12)
            )

    @patch("cli.cli.send_command")
    async def test_json_option_break(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "break", "file": "/path/to/file.rs", "line": 12}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            BreakRequest(file="/path/to/file.rs", line=12)
        )

    @patch("cli.commands.break_cmd.resolve_path")
    async def test_break_command_invalid_format(
        self, mock_resolve: Mock
    ) -> None:
        # 1. No colon
        exit_code = await main(["break", "src/main.rs"])
        self.assertEqual(exit_code, 1)
        mock_resolve.assert_not_called()

        # 2. Non-integer line number
        exit_code = await main(["break", "src/main.rs:abc"])
        self.assertEqual(exit_code, 1)
        mock_resolve.assert_not_called()

        # 3. Negative line number
        exit_code = await main(["break", "src/main.rs:-5"])
        self.assertEqual(exit_code, 1)
        mock_resolve.assert_not_called()

        # 4. Could not resolve path
        mock_resolve.return_value = None
        exit_code = await main(["break", "src/missing.rs:10"])
        self.assertEqual(exit_code, 1)
        mock_resolve.assert_called_once_with("src/missing.rs")

    @patch("os.path.isfile")
    @patch.dict("os.environ", {"FUCHSIA_DIR": "/workspace/fuchsia"})
    def test_resolve_path_fuchsia_dir(self, mock_isfile: Mock) -> None:
        mock_isfile.side_effect = lambda p: p == "/workspace/fuchsia/src/foo.rs"
        res = resolve_path("src/foo.rs")
        self.assertEqual(res, "/workspace/fuchsia/src/foo.rs")

    @patch("os.path.isfile")
    @patch.dict("os.environ", {"FUCHSIA_DIR": "/workspace/fuchsia"})
    def test_resolve_path_directory(self, mock_isfile: Mock) -> None:
        mock_isfile.return_value = False
        res = resolve_path("src")
        self.assertIsNone(res)

    @patch("os.path.isfile")
    @patch.dict("os.environ", {"FUCHSIA_DIR": "/workspace/fuchsia"})
    def test_resolve_path_unresolved(self, mock_isfile: Mock) -> None:
        mock_isfile.return_value = False
        res = resolve_path("missing/foo.rs")
        self.assertIsNone(res)

    @patch("cli.commands.break_cmd.resolve_path")
    @patch("cli.cli.send_command")
    async def test_break_command_delete(
        self, mock_send: Mock, mock_resolve: Mock
    ) -> None:
        mock_send.return_value = 0
        mock_resolve.return_value = "/path/to/fuchsia/src/main.rs"

        exit_code = await main(["break", "-d", "src/main.rs:12"])
        self.assertEqual(exit_code, 0)
        mock_resolve.assert_called_once_with("src/main.rs")
        mock_send.assert_called_once_with(
            BreakRequest(
                file="/path/to/fuchsia/src/main.rs", line=12, delete=True
            )
        )

    @patch("cli.cli.send_command")
    async def test_evaluate_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["evaluate", "--thread-id", "1", "x", "+", "y"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            EvaluateRequest(
                thread_id=1,
                frame_index=0,
                expression="x + y",
                start=0,
                count=50,
            )
        )

    @patch("cli.cli.send_command")
    async def test_print_alias_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(["print", "--thread-id", "1", "x", "+", "y"])
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            EvaluateRequest(
                thread_id=1,
                frame_index=0,
                expression="x + y",
                start=0,
                count=50,
            )
        )

    @patch("cli.cli.send_command")
    async def test_evaluate_command_options(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "evaluate",
                "--thread-id",
                "1",
                "x",
                "--frame-index",
                "2",
                "--start",
                "5",
                "--count",
                "10",
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            EvaluateRequest(
                thread_id=1, frame_index=2, expression="x", start=5, count=10
            )
        )

    @patch("cli.cli.send_command")
    async def test_json_option_evaluate(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "evaluate", "thread_id": 1, "frame_index": 2, "expression": "x", "start": 5, "count": 10}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            EvaluateRequest(
                thread_id=1, frame_index=2, expression="x", start=5, count=10
            )
        )

    @patch("cli.cli.send_command")
    async def test_step_in_command(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "step-in",
                "1",
                "--single-thread",
                "--target-id",
                "0",
                "--granularity",
                "line",
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            StepInRequest(
                thread_id=1, single_thread=True, target_id=0, granularity="line"
            )
        )

    @patch("cli.cli.send_command")
    async def test_step_in_aliases(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        for alias in ["step-in", "step_in", "stepin", "s"]:
            mock_send.reset_mock()
            exit_code = await main([alias, "1"])
            self.assertEqual(exit_code, 0)
            mock_send.assert_called_once_with(StepInRequest(thread_id=1))

    @patch("cli.cli.send_command")
    async def test_json_option_step_in(self, mock_send: Mock) -> None:
        mock_send.return_value = 0
        exit_code = await main(
            [
                "--json",
                '{"command": "step_in", "thread_id": 1, "target_id": 0}',
            ]
        )
        self.assertEqual(exit_code, 0)
        mock_send.assert_called_once_with(
            StepInRequest(thread_id=1, target_id=0)
        )


if __name__ == "__main__":
    unittest.main()
