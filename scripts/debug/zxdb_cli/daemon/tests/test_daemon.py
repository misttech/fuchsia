# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import unittest
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import CommandHandlerRegistry, Daemon
from shared.protocol import (
    BaseRequest,
    BreakRequest,
    GetStateResponse,
    Response,
)
from shared.protocol.attach import AttachRequest
from shared.protocol.continue_request import ContinueRequest
from shared.protocol.evaluate import EvaluateRequest, EvaluateResponse
from shared.protocol.get_state import GetStateRequest
from shared.protocol.pause import PauseRequest
from shared.protocol.threads import ThreadsRequest
from shared.protocol.variables import VariablesRequest


class TestCommandHandlerRegistry(unittest.IsolatedAsyncioTestCase):
    async def test_register_and_handle(self) -> None:
        registry = CommandHandlerRegistry()

        async def mock_handler(_req: BaseRequest) -> Response:
            return Response(success=True, body={"data": "handled"})

        registry.register("test_cmd", mock_handler)

        resp = await registry.handle(
            "test_cmd", BaseRequest(command="test_cmd")
        )

        self.assertTrue(resp.success)
        self.assertEqual(resp.body, {"data": "handled"})

    async def test_unknown_command(self) -> None:
        registry = CommandHandlerRegistry()
        resp = await registry.handle("unknown", BaseRequest(command="unknown"))

        self.assertFalse(resp.success)
        self.assertIsNotNone(resp.message)
        self.assertIn("Unknown command", resp.message or "")

    async def test_attach_registration(self) -> None:
        daemon = Daemon(port=15678)
        self.assertIn("attach", daemon.registry.handlers)

    async def test_handle_attach_success(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon.dap_client, "attach", new_callable=AsyncMock
        ) as mock_attach:
            mock_attach_resp = Mock()
            mock_attach_resp.dump_dap.return_value = {"success": True}
            mock_attach.return_value = mock_attach_resp

            req = AttachRequest(filter="my_process")
            resp = await daemon.registry.handle("attach", req)

            self.assertTrue(resp.success)
            self.assertEqual(resp.body, {"success": True})
            mock_attach.assert_called_once()

    async def test_handle_attach_failure(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon.dap_client, "attach", new_callable=AsyncMock
        ) as mock_attach:
            mock_attach.side_effect = Exception("Failed to attach")

            req = AttachRequest(filter="my_process")
            resp = await daemon.registry.handle("attach", req)

            self.assertFalse(resp.success)
            self.assertIn("Failed to attach", resp.message or "")

    async def test_handle_attach_not_connected(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = None

        req = AttachRequest(filter="my_process")
        resp = await daemon.registry.handle("attach", req)

        self.assertFalse(resp.success)
        self.assertIn("Not connected", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_continue(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_continue_resp = Mock()
        mock_continue_resp.dump_dap.return_value = {"success": True}
        mock_dap_client.continue_thread = AsyncMock(
            return_value=mock_continue_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle(
            "continue", ContinueRequest(thread_id=1)
        )

        self.assertTrue(resp.success)
        mock_dap_client.continue_thread.assert_called_once()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_pause_sync(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_dap_client.pause_thread = AsyncMock(return_value={"success": True})

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        async def trigger_stopped() -> None:
            await asyncio.sleep(0.05)
            daemon.event_waiter.notify_thread_stop(
                1,
                {
                    "type": "event",
                    "event": "stopped",
                    "body": {"reason": "pause"},
                },
            )

        loop = asyncio.get_event_loop()
        loop.create_task(trigger_stopped())

        resp = await daemon.registry.handle("pause", PauseRequest(thread_id=1))

        self.assertTrue(resp.success)
        mock_dap_client.pause_thread.assert_called_once()

    def test_threads_registration(self) -> None:
        daemon = Daemon(port=15678)
        self.assertIn("threads", daemon.registry.handlers)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_threads(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock()
        mock_body = Mock()
        mock_thread1 = Mock()
        mock_thread1.id = 1
        mock_thread1.name = "main"
        mock_thread2 = Mock()
        mock_thread2.id = 2
        mock_thread2.name = "worker"
        mock_body.threads = [mock_thread1, mock_thread2]
        mock_body.model_dump.return_value = {
            "threads": [
                {"id": 1, "name": "main"},
                {"id": 2, "name": "worker"},
            ]
        }
        mock_threads_resp.body = mock_body
        mock_dap_client.threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle("threads", ThreadsRequest())

        if not resp.success:
            print(f"Test failed with message: {resp.message}")
        self.assertTrue(resp.success)
        assert resp.body is not None

        # Double-compatibility check:
        # Pydantic v2 union coercion automatically parses the dictionary
        # returned by handle_threads (which matches GetStateResponse's fields)
        # into a typed GetStateResponse object at runtime.
        # We check the type to support both strongly-typed GetStateResponse
        # objects and raw dictionaries in mock testing.
        if isinstance(resp.body, GetStateResponse):
            threads = resp.body.threads
            self.assertEqual(len(threads), 2)
            self.assertEqual(threads[0].id, 1)
            self.assertEqual(threads[0].name, "main")
            self.assertEqual(threads[1].id, 2)
            self.assertEqual(threads[1].name, "worker")
        else:
            threads = resp.body["threads"]
            self.assertEqual(len(threads), 2)
            self.assertEqual(threads[0]["id"], 1)
            self.assertEqual(threads[0]["name"], "main")
            self.assertEqual(threads[1]["id"], 2)
            self.assertEqual(threads[1]["name"], "worker")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_get_state(self, mock_dap_client_class: Mock) -> None:
        """Verifies handle_get_state successfully queries threads and returns
        GetStateResponse.
        """
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock()
        mock_body = Mock()
        mock_thread1 = Mock()
        mock_thread1.id = 1
        mock_thread1.name = "main"
        mock_body.threads = [mock_thread1]
        mock_threads_resp.body = mock_body
        mock_dap_client.threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.active_processes = {1234: "test_process"}
        daemon.active_breakpoints = {"/path/to/file.rs": {24, 12}}

        resp = await daemon.registry.handle("get-state", GetStateRequest())

        self.assertTrue(resp.success)
        state_resp = resp.body
        assert isinstance(state_resp, GetStateResponse)
        self.assertEqual(len(state_resp.threads), 1)
        self.assertEqual(state_resp.threads[0].id, 1)
        self.assertEqual(state_resp.threads[0].name, "main")
        self.assertEqual(state_resp.processes, {1234: "test_process"})
        self.assertEqual(state_resp.breakpoints, {"/path/to/file.rs": [12, 24]})

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_get_state_defensive(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Verifies handle_get_state gracefully handles None threads response
        body.
        """
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock()
        mock_threads_resp.body = None  # Simulate missing DAP body
        mock_dap_client.threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.active_processes = {1234: "test_process"}

        resp = await daemon.registry.handle("get-state", GetStateRequest())

        self.assertTrue(resp.success)
        state_resp = resp.body
        assert isinstance(state_resp, GetStateResponse)
        self.assertEqual(
            len(state_resp.threads), 0
        )  # Successfully defaulted to empty list without crashing
        self.assertEqual(state_resp.processes, {1234: "test_process"})

    @patch("daemon.daemon.asyncio.start_unix_server")
    @patch("daemon.daemon.ZxdbDapClient")
    async def test_run_cleanup_detach_on_existing_session(
        self, mock_dap_client_class: Mock, mock_start_unix_server: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.zxdb_detach = AsyncMock()

        daemon = Daemon(port=15678)
        daemon.connect_to_existing = True
        daemon.zxdb_writer = Mock()  # Simulate active connection

        # Mock the unix server
        mock_server = AsyncMock()
        mock_server.close = Mock()  # close is synchronous
        mock_start_unix_server.return_value = mock_server

        # Start run() in a task
        run_task = asyncio.create_task(daemon.run())

        # Let it run and reach the wait
        await asyncio.sleep(0.05)

        # Trigger stop
        daemon.stop_event.set()

        # Wait for run to complete
        await run_task

        # Verify zxdb_detach was called with all=True
        mock_dap_client.zxdb_detach.assert_called_once()
        args, kwargs = mock_dap_client.zxdb_detach.call_args
        self.assertTrue(args[1].detach_all)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_variables_success(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        # Mock stack trace
        mock_stack_resp = Mock()
        mock_frame = Mock()
        mock_frame.id = 42
        mock_stack_resp.body.stack_frames = [mock_frame]
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        # Mock scopes
        mock_scopes_resp = Mock()
        mock_scope1 = Mock()
        mock_scope1.name = "Locals"
        mock_scope1.variables_reference = 100
        mock_scope2 = Mock()
        mock_scope2.name = "Arguments"
        mock_scope2.variables_reference = 101
        mock_scope3 = Mock()
        mock_scope3.name = "Globals"
        mock_scope3.variables_reference = 102
        mock_scopes_resp.body.scopes = [mock_scope1, mock_scope2, mock_scope3]
        mock_dap_client.scopes = AsyncMock(return_value=mock_scopes_resp)

        # Mock variables for Locals (100)
        mock_vars_resp1 = Mock()
        mock_var1 = Mock()
        mock_var1.name = "x"
        mock_var1.value = "1"
        mock_var1.type = "int"
        mock_vars_resp1.body.variables = [mock_var1]

        # Mock variables for Arguments (101)
        mock_vars_resp2 = Mock()
        mock_var2 = Mock()
        mock_var2.name = "y"
        mock_var2.value = "2"
        mock_var2.type = "int"
        mock_vars_resp2.body.variables = [mock_var2]

        # variables is called twice, once for 100, once for 101
        mock_dap_client.variables = AsyncMock(
            side_effect=[mock_vars_resp1, mock_vars_resp2]
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon, "ensure_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "variables", VariablesRequest(thread_id=1, frame_index=0)
            )

            self.assertTrue(resp.success)
            self.assertEqual(
                resp.body,
                {
                    "variables": [
                        {"name": "x", "value": "1", "type": "int"},
                        {"name": "y", "value": "2", "type": "int"},
                    ]
                },
            )
            mock_ensure_stopped.assert_called_once_with(1)
            mock_dap_client.stack_trace.assert_called_once()
            mock_dap_client.scopes.assert_called_once()
            self.assertEqual(mock_dap_client.variables.call_count, 2)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_variables_no_frames(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_stack_resp.body.stack_frames = []
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon, "ensure_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "variables", VariablesRequest(thread_id=1, frame_index=0)
            )

            self.assertFalse(resp.success)
            self.assertIn("No stack frames found", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_variables_frame_out_of_bounds(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_frame = Mock()
        mock_stack_resp.body.stack_frames = [mock_frame]
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon, "ensure_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "variables", VariablesRequest(thread_id=1, frame_index=5)
            )

            self.assertFalse(resp.success)
            self.assertIn("Frame index 5 out of range", resp.message or "")

    async def test_handle_break_success(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon.dap_client, "set_breakpoints", new_callable=AsyncMock
        ) as mock_set_breakpoints:
            mock_resp = Mock()
            mock_resp.success = True
            mock_resp.body.dump_dap.return_value = {
                "breakpoints": [{"id": 1, "verified": True, "line": 12}]
            }
            mock_set_breakpoints.return_value = mock_resp

            req = BreakRequest(file="/path/to/file.rs", line=12)
            resp = await daemon.registry.handle("break", req)

            self.assertTrue(resp.success)
            self.assertEqual(
                resp.body,
                {"breakpoints": [{"id": 1, "verified": True, "line": 12}]},
            )
            self.assertEqual(
                daemon.active_breakpoints["/path/to/file.rs"], {12}
            )
            mock_set_breakpoints.assert_called_once()

    async def test_handle_break_additive(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.active_breakpoints["/path/to/file.rs"] = {12}

        with patch.object(
            daemon.dap_client, "set_breakpoints", new_callable=AsyncMock
        ) as mock_set_breakpoints:
            mock_resp = Mock()
            mock_resp.success = True
            mock_resp.body.dump_dap.return_value = {
                "breakpoints": [
                    {"id": 1, "verified": True, "line": 12},
                    {"id": 2, "verified": True, "line": 24},
                ]
            }
            mock_set_breakpoints.return_value = mock_resp

            req = BreakRequest(file="/path/to/file.rs", line=24)
            resp = await daemon.registry.handle("break", req)

            self.assertTrue(resp.success)
            self.assertEqual(
                daemon.active_breakpoints["/path/to/file.rs"], {12, 24}
            )
            mock_set_breakpoints.assert_called_once()
            args, _ = mock_set_breakpoints.call_args
            dap_args = args[1]
            self.assertEqual(len(dap_args.breakpoints), 2)
            self.assertEqual(dap_args.breakpoints[0].line, 12)
            self.assertEqual(dap_args.breakpoints[1].line, 24)

    async def test_handle_break_delete(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.active_breakpoints["/path/to/file.rs"] = {12, 24}

        with patch.object(
            daemon.dap_client, "set_breakpoints", new_callable=AsyncMock
        ) as mock_set_breakpoints:
            mock_resp = Mock()
            mock_resp.success = True
            mock_resp.body.dump_dap.return_value = {
                "breakpoints": [{"id": 1, "verified": True, "line": 24}]
            }
            mock_set_breakpoints.return_value = mock_resp

            req = BreakRequest(file="/path/to/file.rs", line=12, delete=True)
            resp = await daemon.registry.handle("break", req)

            self.assertTrue(resp.success)
            self.assertEqual(
                daemon.active_breakpoints["/path/to/file.rs"], {24}
            )
            mock_set_breakpoints.assert_called_once()
            args, _ = mock_set_breakpoints.call_args
            dap_args = args[1]
            self.assertEqual(len(dap_args.breakpoints), 1)
            self.assertEqual(dap_args.breakpoints[0].line, 24)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_evaluate_success_no_children(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        # Mock stack trace
        mock_stack_resp = Mock()
        mock_frame = Mock()
        mock_frame.id = 42
        mock_stack_resp.body.stack_frames = [mock_frame]
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        # Mock evaluate
        mock_eval_resp = Mock()
        mock_eval_resp.success = True
        mock_eval_resp.body.result = "10"
        mock_eval_resp.body.type = "int"
        mock_eval_resp.body.variables_reference = 0
        mock_dap_client.evaluate = AsyncMock(return_value=mock_eval_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.stopped_threads = {1}

        resp = await daemon.registry.handle(
            "evaluate",
            EvaluateRequest(thread_id=1, frame_index=0, expression="10"),
        )

        self.assertTrue(resp.success)
        self.assertIsNotNone(resp.body)
        assert isinstance(resp.body, EvaluateResponse)
        self.assertEqual(resp.body.result, "10")
        self.assertEqual(resp.body.type, "int")
        mock_dap_client.stack_trace.assert_called_once()
        mock_dap_client.evaluate.assert_called_once()

    # TODO(https://fxbug.dev/529329366): Add test_handle_evaluate_success_with_children
    # once variablesReference expansion is supported in EvaluateResponse.

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_evaluate_failure(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        # Mock stack trace
        mock_stack_resp = Mock()
        mock_stack_resp.body.stack_frames = []
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.stopped_threads = {1}

        resp = await daemon.registry.handle(
            "evaluate",
            EvaluateRequest(thread_id=1, frame_index=0, expression="10"),
        )

        self.assertFalse(resp.success)
        self.assertIn("No stack frames found", resp.message or "")

    async def test_handle_evaluate_thread_not_stopped(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.stopped_threads = set()

        resp = await daemon.registry.handle(
            "evaluate",
            EvaluateRequest(thread_id=1, frame_index=0, expression="10"),
        )

        self.assertFalse(resp.success)
        self.assertIn("Thread not stopped", resp.message or "")


if __name__ == "__main__":
    unittest.main()
