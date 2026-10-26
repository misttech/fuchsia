# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import unittest
from typing import Any
from unittest.mock import AsyncMock, Mock, patch

from daemon.daemon import CommandHandlerRegistry, Daemon
from daemon.handlers import start
from daemon.state import Process, Thread
from pydap.client import DapError
from pydap.dap_types import Breakpoint, Source, StackFrame
from pydap.models import (
    DataBreakpointInfoResponse,
    DataBreakpointInfoResponseBody,
    PauseArguments,
)
from pydap.models import Response as DapResponse
from pydap.models import (
    SetDataBreakpointsResponse,
    SetDataBreakpointsResponseBody,
    StackTraceResponse,
    StackTraceResponseBody,
)
from shared.protocol import (
    BaseRequest,
    BreakRequest,
    GetStateResponse,
    Response,
    WatchpointInfo,
    WatchRequest,
)
from shared.protocol.aspace import AspaceRequest, AspaceResponse
from shared.protocol.async_backtrace import (
    AsyncBacktraceRequest,
    AsyncBacktraceResponse,
)
from shared.protocol.attach import AttachRequest
from shared.protocol.console import ConsoleRequest, ConsoleResponse
from shared.protocol.continue_request import ContinueRequest
from shared.protocol.evaluate import EvaluateRequest, EvaluateResponse
from shared.protocol.finish import FinishRequest
from shared.protocol.get_state import GetStateRequest
from shared.protocol.handles import HandlesRequest, HandlesResponse
from shared.protocol.mem_read import MemReadRequest, MemReadResponse
from shared.protocol.modules import ModulesRequest, ModulesResponse
from shared.protocol.next_request import NextRequest
from shared.protocol.stack_trace import (
    ProcessStackTraceResponse,
    StackTraceRequest,
    ThreadStackTraceResponse,
)
from shared.protocol.start import StartRequest
from shared.protocol.step_in import StepInRequest
from shared.protocol.threads import ThreadsRequest
from shared.protocol.variables import VariablesRequest
from zxdb_dap import (
    AsyncTaskNode,
    ZxdbConsoleCommandArguments,
    ZxdbDapClient,
    ZxdbPauseArguments,
    ZxdbStackFrame,
    ZxdbStackTraceArguments,
)


class TestCommandHandlerRegistry(unittest.IsolatedAsyncioTestCase):
    async def test_register_and_handle(self) -> None:
        registry = CommandHandlerRegistry()

        async def mock_handler(_req: BaseRequest[Any]) -> Response[Any]:
            return Response(success=True, body={"data": "handled"})

        registry.register("test_cmd", mock_handler)

        resp = await registry.handle(
            "test_cmd", BaseRequest[Any](command="test_cmd")
        )

        self.assertTrue(resp.success)
        self.assertEqual(resp.body, {"data": "handled"})

    async def test_unknown_command(self) -> None:
        registry = CommandHandlerRegistry()
        resp = await registry.handle(
            "unknown", BaseRequest[Any](command="unknown")
        )

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
            attach_args = mock_attach.call_args[0][0]
            self.assertEqual(
                attach_args.extra_fields, {"process": "my_process"}
            )

    async def test_handle_attach_with_flags(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon.dap_client, "attach", new_callable=AsyncMock
        ) as mock_attach:
            mock_attach_resp = Mock()
            mock_attach_resp.dump_dap.return_value = {"success": True}
            mock_attach.return_value = mock_attach_resp

            req = AttachRequest(
                filter="driver_host",
                weak=True,
                exact=True,
                recursive=True,
                job_only=True,
            )
            resp = await daemon.registry.handle("attach", req)

            self.assertTrue(resp.success)
            mock_attach.assert_called_once()
            attach_args = mock_attach.call_args[0][0]
            self.assertEqual(
                attach_args.extra_fields,
                {
                    "process": "driver_host",
                    "weak": True,
                    "exact": True,
                    "recursive": True,
                    "job_only": True,
                },
            )

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
    async def test_request_waits_for_startup_lock_instead_of_failing(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Verifies requests arriving while start is executing wait on _command_lock.

        Otherwise, it would show "Not connected to zxdb DAP server" because of an uninitialized zxdb_writer.
        """
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = None  # DAP server not yet connected

        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.attach = AsyncMock(
            return_value=Mock(dump_dap=lambda: {"success": True})
        )

        # 1. Simulate 'start' acquiring the command lock during initialization
        await daemon._command_lock.acquire()

        # 2. Background task simulates 'start' completing DAP connection after 50ms
        async def complete_dap_startup_after_delay() -> None:
            await asyncio.sleep(0.05)
            daemon.zxdb_writer = Mock()
            daemon._command_lock.release()

        startup_task = asyncio.create_task(complete_dap_startup_after_delay())

        # 3. Dispatch an operational command while startup is still locked.
        # It should block for 50ms, acquire the lock after startup completes, and succeed.
        req = AttachRequest(filter="my_process")
        async with daemon._command_lock:
            resp = await daemon.registry.handle("attach", req)

        self.assertTrue(resp.success)
        mock_dap_client.attach.assert_called_once()
        await startup_task

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_commands_execute_in_strict_fifo_order(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Verifies multiple commands arriving concurrently execute in FIFO order without interleaving."""
        execution_log: list[str] = []

        async def delayed_attach_side_effect(*args: Any, **kwargs: Any) -> Mock:
            execution_log.append("attach_start")
            await asyncio.sleep(0.05)
            execution_log.append("attach_end")
            return Mock(dump_dap=lambda: {"success": True})

        async def continue_side_effect(*args: Any, **kwargs: Any) -> Mock:
            execution_log.append("continue_start")
            await asyncio.sleep(0.01)
            execution_log.append("continue_end")
            return Mock(dump_dap=lambda: {"success": True})

        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.attach = AsyncMock(
            side_effect=delayed_attach_side_effect
        )
        mock_dap_client.continue_thread = AsyncMock(
            side_effect=continue_side_effect
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        async def dispatch(cmd: str, req: Any) -> Response[Any]:
            async with daemon._command_lock:
                return await daemon.registry.handle(cmd, req)

        # Launch attach first (takes 50ms)
        task1 = asyncio.create_task(
            dispatch("attach", AttachRequest(filter="proc1"))
        )
        # Ensure task1 acquires the lock first
        await asyncio.sleep(0.005)
        # Launch continue second (takes 10ms); should wait for attach to finish
        task2 = asyncio.create_task(
            dispatch("continue", ContinueRequest(thread_id=1))
        )

        resp1, resp2 = await asyncio.gather(task1, task2)

        self.assertTrue(resp1.success)
        self.assertTrue(resp2.success)
        self.assertEqual(
            execution_log,
            ["attach_start", "attach_end", "continue_start", "continue_end"],
        )

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_continue(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_continue_resp = Mock()
        mock_continue_resp.body = Mock()
        mock_continue_resp.body.all_threads_continued = True
        mock_continue_resp.dump_dap.return_value = {"allThreadsContinued": True}
        mock_dap_client.continue_thread = AsyncMock(
            return_value=mock_continue_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread = daemon.get_or_create_thread(1, process_id=1234)
        thread.is_stopped = True

        resp = await daemon.registry.handle(
            "continue", ContinueRequest(thread_id=1)
        )

        self.assertTrue(resp.success)
        mock_dap_client.continue_thread.assert_called_once()
        self.assertFalse(thread.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_continue_single_thread(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_continue_resp = Mock()
        mock_continue_resp.body = Mock()
        mock_continue_resp.body.all_threads_continued = False
        mock_continue_resp.dump_dap.return_value = {
            "allThreadsContinued": False
        }
        mock_dap_client.continue_thread = AsyncMock(
            return_value=mock_continue_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "continue", ContinueRequest(thread_id=1, single_thread=True)
        )

        self.assertTrue(resp.success)
        mock_dap_client.continue_thread.assert_called_once()
        self.assertFalse(thread1.is_stopped)
        self.assertTrue(thread2.is_stopped)

    def test_update_resumed_threads_single_thread_none_raises(self) -> None:
        daemon = Daemon(port=15678)
        with self.assertRaises(ValueError) as ctx:
            daemon.update_resumed_threads(None, single_thread=True)
        self.assertIn(
            "Single thread resumed without providing thread_id!",
            str(ctx.exception),
        )

    def test_process_resume(self) -> None:
        daemon = Daemon(port=15678)
        proc = daemon.get_or_create_process(1234)
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True
        self.assertTrue(proc.all_threads_stopped)

        proc.resume()
        self.assertFalse(thread1.is_stopped)
        self.assertFalse(thread2.is_stopped)
        self.assertFalse(proc.all_threads_stopped)

    def test_thread_resume(self) -> None:
        daemon = Daemon(port=15678)
        thread = daemon.get_or_create_thread(1, process_id=1234)
        thread.is_stopped = True

        thread.resume()
        self.assertFalse(thread.is_stopped)

    def test_thread_resume_without_process(self) -> None:
        daemon = Daemon(port=15678)
        thread = daemon.get_or_create_thread(1)
        thread.is_stopped = True

        thread.resume()
        self.assertFalse(thread.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_finish(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_finish_resp = Mock()
        mock_finish_resp.dump_dap.return_value = {"success": True}
        mock_dap_client.step_out = AsyncMock(return_value=mock_finish_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "finish",
            FinishRequest(command="finish", thread_id=1),
        )
        self.assertTrue(resp.success, resp.message)
        mock_dap_client.step_out.assert_called_once()
        args = mock_dap_client.step_out.call_args[0][0]
        self.assertEqual(args.thread_id, 1)
        self.assertTrue(args.single_thread)
        self.assertFalse(thread1.is_stopped)
        self.assertTrue(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_finish_all_threads(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_finish_resp = Mock()
        mock_finish_resp.dump_dap.return_value = {"success": True}
        mock_dap_client.step_out = AsyncMock(return_value=mock_finish_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "finish",
            FinishRequest(command="finish", thread_id=1, single_thread=False),
        )
        self.assertTrue(resp.success, resp.message)
        mock_dap_client.step_out.assert_called_once()
        args = mock_dap_client.step_out.call_args[0][0]
        self.assertEqual(args.thread_id, 1)
        self.assertFalse(args.single_thread)
        self.assertFalse(thread1.is_stopped)
        self.assertFalse(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_next(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_next_resp = Mock()
        mock_next_resp.success = True
        mock_next_resp.dump_dap.return_value = {"success": True}
        mock_dap_client.next = AsyncMock(return_value=mock_next_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "next",
            NextRequest(
                command="next",
                thread_id=1,
                granularity="line",
            ),
        )
        self.assertTrue(resp.success, resp.message)
        mock_dap_client.next.assert_called_once()
        args = mock_dap_client.next.call_args[0][0]
        self.assertEqual(args.thread_id, 1)
        self.assertTrue(args.single_thread)
        self.assertFalse(thread1.is_stopped)
        self.assertTrue(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_next_all_threads(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_next_resp = Mock()
        mock_next_resp.success = True
        mock_next_resp.dump_dap.return_value = {"success": True}
        mock_dap_client.next = AsyncMock(return_value=mock_next_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "next",
            NextRequest(
                command="next",
                thread_id=1,
                single_thread=False,
            ),
        )
        self.assertTrue(resp.success, resp.message)
        mock_dap_client.next.assert_called_once()
        args = mock_dap_client.next.call_args[0][0]
        self.assertEqual(args.thread_id, 1)
        self.assertFalse(args.single_thread)
        self.assertFalse(thread1.is_stopped)
        self.assertFalse(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_next_dap_error(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.next = AsyncMock(
            side_effect=DapError("Thread not stopped")
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle(
            "next", NextRequest(command="next", thread_id=1)
        )
        self.assertFalse(resp.success)
        self.assertIn("Thread not stopped", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_step_in(self, mock_dap_client_class: Mock) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_response = Mock()
        mock_dap_response.success = True
        mock_dap_response.dump_dap.return_value = {"success": True}
        mock_dap_client.step_in = AsyncMock(return_value=mock_dap_response)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "step-in",
            StepInRequest(command="step-in", thread_id=1),
        )

        self.assertTrue(resp.success)
        mock_dap_client.step_in.assert_called_once()
        args = mock_dap_client.step_in.call_args[0][0]
        self.assertEqual(args.thread_id, 1)
        self.assertTrue(args.single_thread)
        self.assertFalse(thread1.is_stopped)
        self.assertTrue(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_step_in_all_threads(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_response = Mock()
        mock_dap_response.success = True
        mock_dap_response.dump_dap.return_value = {"success": True}
        mock_dap_client.step_in = AsyncMock(return_value=mock_dap_response)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread1 = daemon.get_or_create_thread(1, process_id=1234)
        thread2 = daemon.get_or_create_thread(2, process_id=1234)
        thread1.is_stopped = True
        thread2.is_stopped = True

        resp = await daemon.registry.handle(
            "step-in",
            StepInRequest(command="step-in", thread_id=1, single_thread=False),
        )

        self.assertTrue(resp.success)
        mock_dap_client.step_in.assert_called_once()
        args = mock_dap_client.step_in.call_args[0][0]
        self.assertEqual(args.thread_id, 1)
        self.assertFalse(args.single_thread)
        self.assertFalse(thread1.is_stopped)
        self.assertFalse(thread2.is_stopped)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_step_in_dap_error(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.step_in = AsyncMock(
            side_effect=DapError("Thread not stopped")
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle(
            "step-in", StepInRequest(command="step-in", thread_id=1)
        )

        self.assertFalse(resp.success)
        self.assertIn("Thread not stopped", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_ensure_process_stopped_already_stopped(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.get_or_create_process(12345)
        daemon.get_or_create_thread(1, process_id=12345).is_stopped = True

        await daemon.ensure_process_stopped(12345)
        mock_dap_client.zxdb_pause_process.assert_not_called()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_ensure_process_stopped_failure(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_dap_client.zxdb_pause_process = AsyncMock(
            return_value=DapResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=False,
                message="Process not found",
            )
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with self.assertRaises(Exception) as ctx:
            await daemon.ensure_process_stopped(12345)
        self.assertIn("Process not found", str(ctx.exception))

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_ensure_process_stopped_when_not_stopped(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        proc = daemon.get_or_create_process(12345)
        thread = daemon.get_or_create_thread(1, process_id=12345)
        self.assertFalse(proc.all_threads_stopped)

        async def mock_pause_side_effect(
            args: ZxdbPauseArguments,
        ) -> DapResponse:
            assert args.process_id is not None
            thread.is_stopped = True
            daemon.event_waiter.notify_process_stop(
                args.process_id,
                {
                    "type": "event",
                    "event": "processStopped",
                    "body": {"processId": args.process_id, "name": "p1"},
                },
            )
            return DapResponse(
                seq=1, type="response", request_seq=1, success=True
            )

        mock_dap_client.zxdb_pause_process = AsyncMock(
            side_effect=mock_pause_side_effect
        )

        await daemon.ensure_process_stopped(12345)
        mock_dap_client.zxdb_pause_process.assert_called_once_with(
            ZxdbPauseArguments(process_id=12345)
        )
        self.assertTrue(proc.all_threads_stopped)

    async def test_ensure_process_stopped_not_connected(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = None
        with self.assertRaises(Exception) as ctx:
            await daemon.ensure_process_stopped(12345)
        self.assertIn("Not connected to zxdb DAP server", str(ctx.exception))

    @patch("daemon.daemon.ZxdbDapClient")
    @patch("daemon.daemon.asyncio.wait_for", side_effect=asyncio.TimeoutError)
    async def test_ensure_process_stopped_timeout(
        self, mock_wait_for: Mock, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_dap_client.zxdb_pause_process = AsyncMock(
            return_value=DapResponse(
                seq=1, type="response", request_seq=1, success=True
            )
        )
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with self.assertRaises(Exception) as ctx:
            await daemon.ensure_process_stopped(12345)
        self.assertIn(
            "Timed out waiting for process 12345 to stop", str(ctx.exception)
        )

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_ensure_stopped_already_stopped(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.get_or_create_thread(1).is_stopped = True

        await daemon.ensure_stopped(1)
        mock_dap_client.pause_thread.assert_not_called()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_ensure_stopped_when_not_stopped(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread = daemon.get_or_create_thread(1)
        self.assertFalse(thread.is_stopped)

        async def mock_pause_thread_side_effect(
            args: PauseArguments,
        ) -> dict[str, Any]:
            assert args.thread_id is not None
            thread.is_stopped = True
            daemon.event_waiter.notify_thread_stop(
                args.thread_id,
                {
                    "type": "event",
                    "event": "stopped",
                    "body": {"reason": "pause", "threadId": args.thread_id},
                },
            )
            return {"success": True}

        mock_dap_client.pause_thread = AsyncMock(
            side_effect=mock_pause_thread_side_effect
        )

        await daemon.ensure_stopped(1)
        mock_dap_client.pause_thread.assert_called_once_with(
            PauseArguments(threadId=1)
        )
        self.assertTrue(thread.is_stopped)

    async def test_ensure_stopped_not_connected(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = None
        with self.assertRaises(Exception) as ctx:
            await daemon.ensure_stopped(1)
        self.assertIn("Not connected to zxdb DAP server", str(ctx.exception))

    @patch("daemon.daemon.ZxdbDapClient")
    @patch("daemon.daemon.asyncio.wait_for", side_effect=asyncio.TimeoutError)
    async def test_ensure_stopped_timeout(
        self, mock_wait_for: Mock, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.pause_thread = AsyncMock(return_value={"success": True})
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with self.assertRaises(Exception) as ctx:
            await daemon.ensure_stopped(1)
        self.assertIn(
            "Timed out waiting for thread 1 to stop", str(ctx.exception)
        )

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
        mock_thread1.process_id = 1234
        mock_thread2 = Mock()
        mock_thread2.id = 2
        mock_thread2.name = "worker"
        mock_thread2.process_id = 1234
        mock_body.threads = [mock_thread1, mock_thread2]
        mock_body.model_dump.return_value = {
            "threads": [
                {"id": 1, "name": "main", "processId": 1234},
                {"id": 2, "name": "worker", "processId": 1234},
            ]
        }
        mock_threads_resp.body = mock_body
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle("threads", ThreadsRequest())

        if not resp.success:
            print(f"Test failed with message: {resp.message}")
        self.assertTrue(resp.success)
        assert resp.body is not None

        threads = resp.body["threads"]
        self.assertEqual(len(threads), 2)
        self.assertEqual(threads[0]["id"], 1)
        self.assertEqual(threads[0]["name"], "main")
        self.assertEqual(threads[1]["id"], 2)
        self.assertEqual(threads[1]["name"], "worker")

        assert daemon.threads[1].process is not None
        assert daemon.threads[2].process is not None
        self.assertEqual(daemon.threads[1].process.id, 1234)
        self.assertEqual(daemon.threads[2].process.id, 1234)

    async def test_update_thread_cache_evicts_stale_threads(self) -> None:
        daemon = Daemon(port=15678)
        daemon.get_or_create_thread(1, process_id=1234)
        daemon.get_or_create_thread(2, process_id=1234)
        daemon.get_or_create_thread(3, process_id=5678)

        mock_thread1 = Mock()
        mock_thread1.id = 1
        mock_thread1.process_id = 1234

        daemon.update_thread_cache([mock_thread1])

        self.assertEqual(list(daemon.threads.keys()), [1])
        assert daemon.threads[1].process is not None
        self.assertEqual(daemon.threads[1].process.id, 1234)

    def test_thread_and_process_equality(self) -> None:
        p1 = Process(id=123, name="p1")
        t1 = Thread(id=1, name="t1", is_stopped=False, process=p1)
        p1.threads[1] = t1

        p2 = Process(id=123, name="p2")
        t2 = Thread(id=1, name="t2", is_stopped=True, process=p2)
        p2.threads[1] = t2

        # Verify comparisons succeed without recursion error and match on ID only.
        self.assertEqual(t1, t2)
        self.assertEqual(p1, p2)

        t3 = Thread(id=2, process=p1)
        p3 = Process(id=456)
        self.assertNotEqual(t1, t3)
        self.assertNotEqual(p1, p3)

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
        mock_thread1.process_id = 1234
        mock_body.threads = [mock_thread1]
        mock_threads_resp.body = mock_body
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1234, name="test_process")
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

        assert daemon.threads[1].process is not None
        self.assertEqual(daemon.threads[1].process.id, 1234)

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
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1234, name="test_process")

        resp = await daemon.registry.handle("get-state", GetStateRequest())

        self.assertTrue(resp.success)
        state_resp = resp.body
        assert isinstance(state_resp, GetStateResponse)
        self.assertEqual(
            len(state_resp.threads), 0
        )  # Successfully defaulted to empty list without crashing
        self.assertEqual(state_resp.processes, {1234: "test_process"})

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_get_state_empty_threads_prunes_cache(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Verifies handle_get_state prunes cached threads when DAP returns an empty list."""
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock()
        mock_body = Mock()
        mock_body.threads = []
        mock_threads_resp.body = mock_body
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1, process_id=1234)
        daemon.get_or_create_process(1234, name="test_process")

        resp = await daemon.registry.handle("get-state", GetStateRequest())

        self.assertTrue(resp.success)
        state_resp = resp.body
        assert isinstance(state_resp, GetStateResponse)
        self.assertEqual(len(state_resp.threads), 0)
        self.assertEqual(daemon.threads, {})

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_threads_empty_prunes_cache(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Verifies handle(threads) prunes cached threads when DAP returns an empty list."""
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock()
        mock_body = Mock()
        mock_body.threads = []
        mock_body.model_dump.return_value = {"threads": []}
        mock_threads_resp.body = mock_body
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1, process_id=1234)

        resp = await daemon.registry.handle("threads", ThreadsRequest())

        self.assertTrue(resp.success)
        self.assertEqual(daemon.threads, {})

    @patch("daemon.daemon.UDS_PATH")
    @patch("daemon.daemon.asyncio.start_unix_server")
    @patch("daemon.daemon.ZxdbDapClient")
    async def test_run_cleanup_detach_on_existing_session(
        self,
        mock_dap_client_class: Mock,
        mock_start_unix_server: Mock,
        mock_uds_path: Mock,
    ) -> None:
        mock_uds_path.exists.return_value = False
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.zxdb_detach = AsyncMock()
        mock_dap_client.close = AsyncMock()

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
        self.assertTrue(args[0].detach_all)

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
            dap_args = args[0]
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
            dap_args = args[0]
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
        daemon.get_or_create_thread(1).is_stopped = True

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
        daemon.get_or_create_thread(1).is_stopped = True

        resp = await daemon.registry.handle(
            "evaluate",
            EvaluateRequest(thread_id=1, frame_index=0, expression="10"),
        )

        self.assertFalse(resp.success)
        self.assertIn("No stack frames found", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_evaluate_thread_not_stopped(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1).is_stopped = False

        def on_pause(*args: Any, **kwargs: Any) -> dict[str, Any]:
            daemon.event_waiter.notify_thread_stop(
                1,
                {
                    "type": "event",
                    "event": "stopped",
                    "body": {"reason": "pause"},
                },
            )
            return {"success": True}

        mock_dap_client.pause_thread = AsyncMock(side_effect=on_pause)

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

        resp = await daemon.registry.handle(
            "evaluate",
            EvaluateRequest(thread_id=1, frame_index=0, expression="10"),
        )

        self.assertTrue(resp.success)
        self.assertIsNotNone(resp.body)
        assert isinstance(resp.body, EvaluateResponse)
        self.assertEqual(resp.body.result, "10")
        self.assertEqual(resp.body.type, "int")
        mock_dap_client.pause_thread.assert_called_once()
        mock_dap_client.stack_trace.assert_called_once()
        mock_dap_client.evaluate.assert_called_once()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_console_without_thread_id(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_cmd_resp = Mock()
        mock_cmd_resp.success = True
        mock_cmd_resp.body.output = "Process 1 symbol status\n"
        mock_dap_client.zxdb_console_command = AsyncMock(
            return_value=mock_cmd_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle(
            "console",
            ConsoleRequest(input="sym-stat"),
        )

        self.assertTrue(resp.success)
        self.assertIsNotNone(resp.body)
        assert isinstance(resp.body, ConsoleResponse)
        self.assertEqual(resp.body.output, "Process 1 symbol status\n")
        mock_dap_client.stack_trace.assert_not_called()
        mock_dap_client.zxdb_console_command.assert_called_once_with(
            ZxdbConsoleCommandArguments(command="sym-stat", frame_id=None)
        )

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_console_with_thread_and_frame(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_frame_0 = Mock()
        mock_frame_0.id = 41
        mock_frame_1 = Mock()
        mock_frame_1.id = 42
        mock_stack_resp.body.stack_frames = [mock_frame_0, mock_frame_1]
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        mock_cmd_resp = Mock()
        mock_cmd_resp.success = True
        mock_cmd_resp.body.output = "rax = 0x1\n"
        mock_dap_client.zxdb_console_command = AsyncMock(
            return_value=mock_cmd_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1).is_stopped = True

        resp = await daemon.registry.handle(
            "console",
            ConsoleRequest(input="regs", thread_id=1, frame_index=1),
        )

        self.assertTrue(resp.success)
        self.assertIsNotNone(resp.body)
        assert isinstance(resp.body, ConsoleResponse)
        self.assertEqual(resp.body.output, "rax = 0x1\n")
        mock_dap_client.stack_trace.assert_called_once()
        mock_dap_client.zxdb_console_command.assert_called_once_with(
            ZxdbConsoleCommandArguments(command="regs", frame_id=42)
        )

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_console_frame_out_of_range(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_frame_0 = Mock()
        mock_frame_0.id = 41
        mock_stack_resp.body.stack_frames = [mock_frame_0]
        mock_dap_client.stack_trace = AsyncMock(return_value=mock_stack_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1).is_stopped = True

        resp = await daemon.registry.handle(
            "console",
            ConsoleRequest(input="regs", thread_id=1, frame_index=5),
        )

        self.assertFalse(resp.success)
        self.assertIn("out of range", resp.message or "")
        mock_dap_client.zxdb_console_command.assert_not_called()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_console_frame_index_without_thread_id(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle(
            "console",
            ConsoleRequest(input="regs", thread_id=None, frame_index=2),
        )

        self.assertFalse(resp.success)
        self.assertIn("thread_id is required", resp.message or "")
        mock_dap_client.zxdb_console_command.assert_not_called()

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_console_dap_error(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_dap_client.zxdb_console_command = AsyncMock(
            side_effect=DapError("not a valid verb")
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        resp = await daemon.registry.handle(
            "console",
            ConsoleRequest(input="bad_cmd"),
        )

        self.assertFalse(resp.success)
        self.assertIn("not a valid verb", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_elides_subtle_frames(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_stack_resp.body.stack_frames = [
            ZxdbStackFrame(
                id=1,
                name="f1",
                line=1,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
            ZxdbStackFrame(
                id=2,
                name="f2",
                line=2,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
        ]
        mock_stack_resp.body.total_frames = 2
        mock_dap_client.zxdb_stack_trace = AsyncMock(
            return_value=mock_stack_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon, "ensure_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(thread_id=1, raw=False)
            )

            self.assertTrue(resp.success)
            self.assertIsNotNone(resp.body)
            assert isinstance(resp.body, ThreadStackTraceResponse)
            self.assertEqual(resp.body.thread_id, 1)
            frames = resp.body.stack_frames
            self.assertEqual(len(frames), 1)
            self.assertIn("0…1 «Rust panic»", frames[0].name)
            self.assertEqual(frames[0].frame_index, 0)

            # Raw flag returns uncollapsing frames
            resp_raw = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(thread_id=1, raw=True)
            )
            self.assertTrue(resp_raw.success)
            assert isinstance(resp_raw.body, ThreadStackTraceResponse)
            self.assertEqual(len(resp_raw.body.stack_frames), 2)
            self.assertEqual(resp_raw.body.stack_frames[0].frame_index, 0)
            self.assertEqual(resp_raw.body.stack_frames[1].frame_index, 1)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_subtle_non_subtle_subtle(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_stack_resp.body.stack_frames = [
            ZxdbStackFrame(
                id=1,
                name="f1",
                line=1,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
            ZxdbStackFrame(
                id=2,
                name="f2",
                line=2,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
            ZxdbStackFrame(
                id=3,
                name="f3",
                line=3,
                column=1,
                presentation_hint="normal",
                source=None,
            ),
            ZxdbStackFrame(
                id=4,
                name="f4",
                line=4,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="C++ stdlib"),
            ),
            ZxdbStackFrame(
                id=5,
                name="f5",
                line=5,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="C++ stdlib"),
            ),
        ]
        mock_stack_resp.body.total_frames = 5
        mock_dap_client.zxdb_stack_trace = AsyncMock(
            return_value=mock_stack_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(daemon, "ensure_stopped", new_callable=AsyncMock):
            resp = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(thread_id=1, raw=False)
            )

            self.assertTrue(resp.success)
            self.assertIsNotNone(resp.body)
            assert isinstance(resp.body, ThreadStackTraceResponse)
            frames = resp.body.stack_frames
            self.assertEqual(len(frames), 3)
            self.assertIn("0…1 «Rust panic»", frames[0].name)
            self.assertEqual(frames[0].frame_index, 0)
            self.assertEqual(frames[1].name, "f3")
            self.assertEqual(frames[1].frame_index, 2)
            self.assertIn("3…4 «C++ stdlib»", frames[2].name)
            self.assertEqual(frames[2].frame_index, 3)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_non_subtle_subtle_non_subtle(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_stack_resp.body.stack_frames = [
            ZxdbStackFrame(
                id=1,
                name="f1",
                line=1,
                column=1,
                presentation_hint="normal",
                source=None,
            ),
            ZxdbStackFrame(
                id=2,
                name="f2",
                line=2,
                column=1,
                presentation_hint="normal",
                source=None,
            ),
            ZxdbStackFrame(
                id=3,
                name="f3",
                line=3,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
            ZxdbStackFrame(
                id=4,
                name="f4",
                line=4,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
            ZxdbStackFrame(
                id=5,
                name="f5",
                line=5,
                column=1,
                presentation_hint="normal",
                source=None,
            ),
        ]
        mock_stack_resp.body.total_frames = 5
        mock_dap_client.zxdb_stack_trace = AsyncMock(
            return_value=mock_stack_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(daemon, "ensure_stopped", new_callable=AsyncMock):
            resp = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(thread_id=1, raw=False)
            )

            self.assertTrue(resp.success)
            self.assertIsNotNone(resp.body)
            assert isinstance(resp.body, ThreadStackTraceResponse)
            frames = resp.body.stack_frames
            self.assertEqual(len(frames), 4)
            self.assertEqual(frames[0].name, "f1")
            self.assertEqual(frames[0].frame_index, 0)
            self.assertEqual(frames[1].name, "f2")
            self.assertEqual(frames[1].frame_index, 1)
            self.assertIn("2…3 «Rust panic»", frames[2].name)
            self.assertEqual(frames[2].frame_index, 2)
            self.assertEqual(frames[3].name, "f5")
            self.assertEqual(frames[3].frame_index, 4)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_unwinder_and_trust(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp = Mock()
        mock_stack_resp.body.stack_frames = [
            ZxdbStackFrame(
                id=1,
                name="f1",
                line=10,
                column=1,
                presentation_hint="normal",
                source=None,
                trust="FP",
            ),
        ]
        mock_stack_resp.body.total_frames = 1
        mock_dap_client.zxdb_stack_trace = AsyncMock(
            return_value=mock_stack_resp
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(daemon, "ensure_stopped", new_callable=AsyncMock):
            resp = await daemon.registry.handle(
                "stack-trace",
                StackTraceRequest(
                    thread_id=1,
                    force_remote_unwind=True,
                    unwinder="fp",
                ),
            )

            self.assertTrue(resp.success)
            mock_dap_client.zxdb_stack_trace.assert_called_once_with(
                ZxdbStackTraceArguments(
                    thread_id=1,
                    remote_unwind=True,
                    unwinder="fp",
                )
            )
            assert isinstance(resp.body, ThreadStackTraceResponse)
            self.assertEqual(len(resp.body.stack_frames), 1)
            self.assertEqual(resp.body.stack_frames[0].trust, "FP")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_exception(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        # Mock _send_request returning invalid response structure that fails model_validate
        mock_dap_client._send_request = AsyncMock(
            return_value={"invalid": "response_structure"}
        )
        # Real zxdb_stack_trace calls _send_request and model_validate
        mock_dap_client.zxdb_stack_trace = (
            lambda args: ZxdbDapClient.zxdb_stack_trace(mock_dap_client, args)
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(daemon, "ensure_stopped", new_callable=AsyncMock):
            resp = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(thread_id=1)
            )

            self.assertFalse(resp.success)
            self.assertIsNotNone(resp.message)
            self.assertIn("Failed to get stack trace", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_pid_success(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp_1 = Mock()
        mock_stack_resp_1.body.stack_frames = [
            ZxdbStackFrame(
                id=1,
                name="f1",
                line=1,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
            ZxdbStackFrame(
                id=2,
                name="f2",
                line=2,
                column=1,
                presentation_hint="subtle",
                source=Source(origin="Rust panic"),
            ),
        ]
        mock_stack_resp_1.body.total_frames = 2

        mock_stack_resp_2 = Mock()
        mock_stack_resp_2.body.stack_frames = [
            ZxdbStackFrame(
                id=3,
                name="worker_func",
                line=3,
                column=1,
                presentation_hint="normal",
                source=None,
            ),
        ]
        mock_stack_resp_2.body.total_frames = 1

        mock_dap_client.zxdb_stack_trace = AsyncMock(
            side_effect=[mock_stack_resp_1, mock_stack_resp_2]
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(12345, name="test_proc")
        daemon.get_or_create_thread(
            101, name="initial-thread", process_id=12345
        ).is_stopped = True
        daemon.get_or_create_thread(
            102, name="worker-thread", process_id=12345
        ).is_stopped = True

        resp = await daemon.registry.handle(
            "stack-trace", StackTraceRequest(pid=12345, raw=False)
        )

        self.assertTrue(resp.success)
        self.assertIsNotNone(resp.body)

        assert isinstance(resp.body, ProcessStackTraceResponse)
        self.assertEqual(resp.body.process_id, 12345)
        threads = resp.body.stacks
        self.assertEqual(len(threads), 2)
        self.assertEqual(threads[0].thread_id, 101)
        self.assertEqual(len(threads[0].stack_frames), 1)
        self.assertIn("0…1 «Rust panic»", threads[0].stack_frames[0].name)
        self.assertEqual(threads[0].stack_frames[0].frame_index, 0)
        self.assertEqual(threads[1].thread_id, 102)
        self.assertEqual(len(threads[1].stack_frames), 1)
        self.assertEqual(threads[1].stack_frames[0].name, "worker_func")
        self.assertEqual(threads[1].stack_frames[0].frame_index, 0)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_pid_not_found(
        self, mock_dap_client_class: Mock
    ) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(pid=99999, raw=False)
            )
            self.assertFalse(resp.success)
            self.assertIn(
                "No threads found for process 99999", resp.message or ""
            )
            mock_ensure_stopped.assert_called_once_with(99999)

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_pid_no_threads(
        self, mock_dap_client_class: Mock
    ) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(12345, name="test_proc")

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ):
            resp = await daemon.registry.handle(
                "stack-trace", StackTraceRequest(pid=12345)
            )
            self.assertFalse(resp.success)
            self.assertIn(
                "No threads found for process 12345", resp.message or ""
            )

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_stack_trace_pid_partial_thread_failure(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value

        mock_stack_resp_1 = Mock()
        mock_stack_resp_1.body.stack_frames = [
            ZxdbStackFrame(
                id=1,
                name="func_ok",
                line=1,
                column=1,
                presentation_hint="normal",
                source=None,
            ),
        ]
        mock_stack_resp_1.body.total_frames = 1

        mock_dap_client.zxdb_stack_trace = AsyncMock(
            side_effect=[mock_stack_resp_1, Exception("Thread exited")]
        )

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(12345, name="test_proc")
        daemon.get_or_create_thread(
            101, name="thread-ok", process_id=12345
        ).is_stopped = True
        daemon.get_or_create_thread(
            102, name="thread-exited", process_id=12345
        ).is_stopped = True

        resp = await daemon.registry.handle(
            "stack-trace", StackTraceRequest(pid=12345, raw=False)
        )

        self.assertTrue(resp.success)
        self.assertIsNotNone(resp.body)

        assert isinstance(resp.body, ProcessStackTraceResponse)
        self.assertEqual(len(resp.body.stacks), 2)
        self.assertEqual(resp.body.stacks[0].thread_id, 101)
        self.assertEqual(len(resp.body.stacks[0].stack_frames), 1)
        self.assertEqual(resp.body.stacks[1].thread_id, 102)
        self.assertEqual(len(resp.body.stacks[1].stack_frames), 0)
        self.assertEqual(resp.body.stacks[1].total_frames, 0)

    def test_async_backtrace_registration(self) -> None:
        daemon = Daemon(port=15678)
        self.assertIn("async-backtrace", daemon.registry.handlers)

    async def test_handle_async_backtrace_not_connected(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = None
        resp = await daemon.registry.handle(
            "async-backtrace", AsyncBacktraceRequest()
        )
        self.assertFalse(resp.success)
        self.assertIn("Not connected to zxdb DAP server", resp.message or "")

    async def test_handle_async_backtrace_no_processes(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        resp = await daemon.registry.handle(
            "async-backtrace", AsyncBacktraceRequest()
        )
        self.assertFalse(resp.success)
        self.assertIn("No active processes found", resp.message or "")

    async def test_handle_async_backtrace_ambiguous_processes(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        daemon.get_or_create_process(1002, name="proc2")
        resp = await daemon.registry.handle(
            "async-backtrace", AsyncBacktraceRequest()
        )
        self.assertFalse(resp.success)
        self.assertIn(
            "Multiple processes active; please specify a PID with --pid / -p",
            resp.message or "",
        )

    async def test_handle_async_backtrace_pid_not_found(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        resp = await daemon.registry.handle(
            "async-backtrace", AsyncBacktraceRequest(pid=9999)
        )
        self.assertFalse(resp.success)
        self.assertIn("Process 9999 not found", resp.message or "")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_async_backtrace_no_threads(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        mock_threads_resp = Mock(body=Mock(threads=[]))
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ):
            resp = await daemon.registry.handle(
                "async-backtrace", AsyncBacktraceRequest(pid=1001)
            )
            self.assertTrue(resp.success)
            assert isinstance(resp.body, AsyncBacktraceResponse)
            self.assertEqual(resp.body.process_id, 1001)
            self.assertEqual(resp.body.tasks, [])

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_async_backtrace_syncs_threads_when_empty(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")

        mock_thread = Mock()
        mock_thread.id = 3001
        mock_thread.name = "initial-thread"
        mock_thread.process_id = 1001

        mock_threads_resp = Mock()
        mock_threads_resp.body = Mock()
        mock_threads_resp.body.threads = [mock_thread]
        mock_dap_client.zxdb_threads = AsyncMock(return_value=mock_threads_resp)

        mock_abt_resp = Mock()
        mock_abt_resp.success = True
        mock_abt_resp.body = Mock()
        mock_abt_resp.body.tasks = [
            AsyncTaskNode(
                id="t_synced",
                name="synced_task",
                file="main.rs",
                line=10,
                children=[],
            )
        ]
        mock_dap_client.zxdb_async_backtrace = AsyncMock(
            return_value=mock_abt_resp
        )

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "async-backtrace", AsyncBacktraceRequest(pid=1001)
            )
            self.assertTrue(resp.success)
            mock_ensure_stopped.assert_called_once_with(1001)
            mock_dap_client.zxdb_threads.assert_called_once()
            mock_dap_client.zxdb_async_backtrace.assert_called_once()
            self.assertIsNotNone(resp.body)
            assert isinstance(resp.body, AsyncBacktraceResponse)
            self.assertEqual(resp.body.process_id, 1001)
            self.assertEqual(len(resp.body.tasks), 1)
            self.assertEqual(resp.body.tasks[0].id, "t_synced")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_async_backtrace_success_explicit_pid(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        daemon.get_or_create_thread(2001, name="thread1", process_id=1001)

        mock_abt_resp = Mock()
        mock_abt_resp.success = True
        mock_abt_resp.body = Mock()
        mock_abt_resp.body.tasks = [
            AsyncTaskNode(
                id="t1",
                name="root_task",
                file="main.rs",
                line=42,
                children=[],
            )
        ]
        mock_dap_client.zxdb_async_backtrace = AsyncMock(
            return_value=mock_abt_resp
        )

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ) as mock_ensure_stopped:
            resp = await daemon.registry.handle(
                "async-backtrace", AsyncBacktraceRequest(pid=1001)
            )
            self.assertTrue(resp.success)
            mock_ensure_stopped.assert_called_once_with(1001)
            mock_dap_client.zxdb_async_backtrace.assert_called_once()
            self.assertIsNotNone(resp.body)
            assert isinstance(resp.body, AsyncBacktraceResponse)
            self.assertEqual(resp.body.process_id, 1001)
            self.assertEqual(len(resp.body.tasks), 1)
            self.assertEqual(resp.body.tasks[0].id, "t1")
            self.assertEqual(resp.body.tasks[0].name, "root_task")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_async_backtrace_success_default_single_process(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        daemon.get_or_create_thread(2001, name="thread1", process_id=1001)

        mock_abt_resp = Mock()
        mock_abt_resp.success = True
        mock_abt_resp.body = Mock()
        mock_abt_resp.body.tasks = []
        mock_dap_client.zxdb_async_backtrace = AsyncMock(
            return_value=mock_abt_resp
        )

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ):
            resp = await daemon.registry.handle(
                "async-backtrace", AsyncBacktraceRequest()
            )
            self.assertTrue(resp.success)
            assert isinstance(resp.body, AsyncBacktraceResponse)
            self.assertEqual(resp.body.process_id, 1001)
            self.assertEqual(resp.body.tasks, [])

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_async_backtrace_returns_first_successful_thread(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        daemon.get_or_create_thread(2001, name="thread1", process_id=1001)
        daemon.get_or_create_thread(2002, name="thread2", process_id=1001)

        mock_resp_1 = Mock()
        mock_resp_1.success = True
        mock_resp_1.body = Mock()
        mock_resp_1.body.tasks = [
            AsyncTaskNode(
                id="t1",
                name="parent_task",
                file="foo.rs",
                line=10,
                children=[
                    AsyncTaskNode(
                        id="t2",
                        name="child_task",
                        file="bar.rs",
                        line=20,
                        children=[],
                    )
                ],
            )
        ]

        mock_dap_client.zxdb_async_backtrace = AsyncMock(
            return_value=mock_resp_1
        )

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ):
            resp = await daemon.registry.handle(
                "async-backtrace", AsyncBacktraceRequest(pid=1001)
            )
            self.assertTrue(resp.success)
            assert isinstance(resp.body, AsyncBacktraceResponse)
            self.assertEqual(mock_dap_client.zxdb_async_backtrace.call_count, 1)
            self.assertEqual(len(resp.body.tasks), 1)
            parent = resp.body.tasks[0]
            self.assertIsInstance(parent, AsyncTaskNode)
            self.assertEqual(parent.name, "parent_task")
            self.assertEqual(len(parent.children), 1)
            child = parent.children[0]
            self.assertIsInstance(child, AsyncTaskNode)
            self.assertEqual(child.name, "child_task")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_async_backtrace_fallback_to_second_thread_when_first_empty(
        self, mock_dap_client_class: Mock
    ) -> None:
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(1001, name="proc1")
        daemon.get_or_create_thread(2001, name="thread1", process_id=1001)
        daemon.get_or_create_thread(2002, name="thread2", process_id=1001)

        mock_resp_1 = Mock()
        mock_resp_1.success = True
        mock_resp_1.body = Mock()
        mock_resp_1.body.tasks = []

        mock_resp_2 = Mock()
        mock_resp_2.success = True
        mock_resp_2.body = Mock()
        mock_resp_2.body.tasks = [
            AsyncTaskNode(
                id="t3",
                name="thread2_task",
                file="baz.rs",
                line=30,
                children=[],
            )
        ]

        mock_dap_client.zxdb_async_backtrace = AsyncMock(
            side_effect=[mock_resp_1, mock_resp_2]
        )

        with patch.object(
            daemon, "ensure_process_stopped", new_callable=AsyncMock
        ):
            resp = await daemon.registry.handle(
                "async-backtrace", AsyncBacktraceRequest(pid=1001)
            )
            self.assertTrue(resp.success)
            assert isinstance(resp.body, AsyncBacktraceResponse)
            self.assertEqual(mock_dap_client.zxdb_async_backtrace.call_count, 2)
            self.assertEqual(len(resp.body.tasks), 1)
            task = resp.body.tasks[0]
            self.assertEqual(task.name, "thread2_task")

    @patch("daemon.daemon.FfxCmd")
    @patch("daemon.daemon.package_server")
    async def test_start_dap_server_uses_ephemeral_repository_name(
        self, mock_package_server: Mock, mock_ffx_cmd: Mock
    ) -> None:
        mock_package_server.is_running = AsyncMock(return_value=False)
        mock_package_server.ephemeral_repository_name = Mock(
            return_value="zxdb-deadbeef12345678"
        )
        mock_package_server.start = AsyncMock(return_value=Mock())
        mock_ffx_cmd.return_value.start = AsyncMock(
            side_effect=RuntimeError("stop before signal wait")
        )

        daemon = Daemon(port=15678)
        resp = await daemon._start_dap_server()

        self.assertIsNotNone(resp)
        assert resp is not None
        self.assertFalse(resp.success)
        self.assertEqual(daemon.repo_name, "zxdb-deadbeef12345678")
        mock_package_server.ephemeral_repository_name.assert_called_once_with(
            "zxdb"
        )
        mock_package_server.start.assert_awaited_once_with(
            "zxdb-deadbeef12345678"
        )

    @patch("daemon.daemon.package_server")
    async def test_start_dap_server_does_not_set_repo_name_on_start_failure(
        self, mock_package_server: Mock
    ) -> None:
        mock_package_server.is_running = AsyncMock(return_value=False)
        mock_package_server.ephemeral_repository_name = Mock(
            return_value="zxdb-deadbeef12345678"
        )
        mock_package_server.start = AsyncMock(
            side_effect=RuntimeError("package server failed")
        )

        daemon = Daemon(port=15678)
        resp = await daemon._start_dap_server()

        self.assertIsNotNone(resp)
        assert resp is not None
        self.assertFalse(resp.success)
        self.assertIsNone(daemon.repo_name)
        self.assertIsNone(daemon.package_server_proc)

    async def test_handle_watch_add_and_delete(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        thread = daemon.get_or_create_thread(1, name="t1", process_id=1001)
        thread.is_stopped = True

        with (
            patch.object(
                daemon.dap_client, "stack_trace", new_callable=AsyncMock
            ) as mock_stack,
            patch.object(
                daemon.dap_client,
                "data_breakpoint_info",
                new_callable=AsyncMock,
            ) as mock_info,
            patch.object(
                daemon.dap_client,
                "set_data_breakpoints",
                new_callable=AsyncMock,
            ) as mock_set,
            patch.object(
                daemon.dap_client, "zxdb_threads", new_callable=AsyncMock
            ) as mock_threads,
        ):
            mock_stack.return_value = StackTraceResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                body=StackTraceResponseBody(
                    stack_frames=[
                        StackFrame(id=42, name="main", line=10, column=1)
                    ]
                ),
            )
            mock_info.return_value = DataBreakpointInfoResponse(
                seq=2,
                type="response",
                request_seq=2,
                success=True,
                body=DataBreakpointInfoResponseBody(
                    data_id="1001:0x1234:4",
                    description="my_var (4 bytes @ 0x1234)",
                    access_types=["write", "readWrite"],
                ),
            )
            mock_set.return_value = SetDataBreakpointsResponse(
                seq=3,
                type="response",
                request_seq=3,
                success=True,
                body=SetDataBreakpointsResponseBody(
                    breakpoints=[
                        Breakpoint(
                            id=1,
                            verified=True,
                            instruction_reference="0x1234",
                        )
                    ]
                ),
            )

            add_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(
                    name="my_var",
                    thread_id=1,
                    frame_index=0,
                    access_type="write",
                ),
            )
            self.assertTrue(add_resp.success)
            assert add_resp.body is not None
            self.assertEqual(add_resp.body["dataId"], "1001:0x1234:4")
            self.assertIn("1001:0x1234:4", daemon.active_watchpoints)
            self.assertEqual(
                daemon.active_watchpoints["1001:0x1234:4"],
                WatchpointInfo(
                    data_id="1001:0x1234:4",
                    expression="my_var",
                    thread_id=1,
                    frame_index=0,
                    access_type="write",
                    verified=True,
                    message=None,
                ),
            )
            self.assertEqual(
                daemon.watchpoint_ids_by_bp_id, {1: "1001:0x1234:4"}
            )

            # Verify get-state surfaces active watchpoints
            mock_threads.return_value = Mock(body=Mock(threads=[]))
            state_resp = await daemon.registry.handle(
                "get-state", GetStateRequest()
            )
            self.assertTrue(state_resp.success)
            assert isinstance(state_resp.body, GetStateResponse)
            self.assertEqual(
                state_resp.body.watchpoints,
                {
                    "1001:0x1234:4": WatchpointInfo(
                        data_id="1001:0x1234:4",
                        expression="my_var",
                        thread_id=1,
                        frame_index=0,
                        access_type="write",
                        verified=True,
                        message=None,
                    )
                },
            )

            # Delete watchpoint by data_id without re-querying dataBreakpointInfo
            mock_info.reset_mock()
            mock_set.return_value = SetDataBreakpointsResponse(
                seq=4,
                type="response",
                request_seq=4,
                success=True,
                body=SetDataBreakpointsResponseBody(breakpoints=[]),
            )
            del_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(name="1001:0x1234:4", delete=True),
            )
            self.assertTrue(del_resp.success)
            mock_info.assert_not_called()
            self.assertEqual(len(daemon.active_watchpoints), 0)

    async def test_handle_watch_edge_cases(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        proc = daemon.get_or_create_process(1001, name="test_proc")
        t1 = daemon.get_or_create_thread(1, process_id=1001)
        t1.is_stopped = True
        proc.threads[1] = t1

        with (
            patch.object(
                daemon.dap_client, "stack_trace", new_callable=AsyncMock
            ) as mock_stack,
            patch.object(
                daemon.dap_client,
                "data_breakpoint_info",
                new_callable=AsyncMock,
            ) as mock_info,
            patch.object(
                daemon.dap_client,
                "set_data_breakpoints",
                new_callable=AsyncMock,
            ) as mock_set,
        ):
            mock_stack.return_value = StackTraceResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                body=StackTraceResponseBody(
                    stack_frames=[
                        StackFrame(id=42, name="main", line=10, column=1)
                    ]
                ),
            )

            # 0. Omitting thread_id or access_type when setting is rejected
            missing_tid_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(name="my_var", access_type="write"),
            )
            self.assertFalse(missing_tid_resp.success)
            self.assertIn("thread_id", missing_tid_resp.message or "")

            missing_access_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(name="my_var", thread_id=1),
            )
            self.assertFalse(missing_access_resp.success)
            self.assertIn("access_type", missing_access_resp.message or "")
            mock_info.assert_not_called()

            # 1. Address dereference expression delegates to data_breakpoint_info
            mock_info.return_value = DataBreakpointInfoResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                body=DataBreakpointInfoResponseBody(
                    data_id="1001:0x2000:4",
                    description="*(uint32_t*)0x2000 (0x2000, 4 bytes)",
                ),
            )
            mock_set.return_value = SetDataBreakpointsResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                body=SetDataBreakpointsResponseBody(
                    breakpoints=[Breakpoint(id=1, verified=True)]
                ),
            )
            addr_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(
                    name="*(uint32_t*)0x2000",
                    thread_id=1,
                    access_type="write",
                ),
            )
            self.assertTrue(addr_resp.success)
            mock_info.assert_called_once()
            self.assertIn("1001:0x2000:4", daemon.active_watchpoints)

            # 2. Unverified watchpoint is recorded with verified=False. zxdb
            # recreates every data breakpoint on each setDataBreakpoints call,
            # so the existing watchpoint also gets a new id.
            mock_info.return_value = DataBreakpointInfoResponse(
                seq=2,
                type="response",
                request_seq=2,
                success=True,
                body=DataBreakpointInfoResponseBody(
                    data_id="1001:0x3000:4",
                    description="bad_var (0x3000, 4 bytes)",
                ),
            )
            mock_set.return_value = SetDataBreakpointsResponse(
                seq=3,
                type="response",
                request_seq=3,
                success=True,
                body=SetDataBreakpointsResponseBody(
                    breakpoints=[
                        Breakpoint(id=2, verified=True),
                        Breakpoint(
                            id=3,
                            verified=False,
                            message="No free hardware watchpoint slots.",
                        ),
                    ]
                ),
            )
            bad_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(
                    name="bad_var", thread_id=1, access_type="readWrite"
                ),
            )
            self.assertTrue(bad_resp.success)
            self.assertIn("1001:0x3000:4", daemon.active_watchpoints)
            bad_wp = daemon.active_watchpoints["1001:0x3000:4"]
            self.assertFalse(bad_wp.verified)
            self.assertEqual(
                bad_wp.message, "No free hardware watchpoint slots."
            )
            self.assertEqual(
                daemon.watchpoint_ids_by_bp_id,
                {2: "1001:0x2000:4", 3: "1001:0x3000:4"},
            )

            # 3. Deleting a watchpoint refreshes the remaining ones.
            mock_set.return_value = SetDataBreakpointsResponse(
                seq=4,
                type="response",
                request_seq=4,
                success=True,
                body=SetDataBreakpointsResponseBody(
                    breakpoints=[
                        Breakpoint(id=4, verified=False, message="lost slot")
                    ]
                ),
            )
            del_resp = await daemon.registry.handle(
                "watch", WatchRequest(name="1001:0x3000:4", delete=True)
            )
            self.assertTrue(del_resp.success)
            self.assertEqual(list(daemon.active_watchpoints), ["1001:0x2000:4"])
            remaining = daemon.active_watchpoints["1001:0x2000:4"]
            self.assertFalse(remaining.verified)
            self.assertEqual(remaining.message, "lost slot")
            self.assertEqual(
                daemon.watchpoint_ids_by_bp_id, {4: "1001:0x2000:4"}
            )

            # 4. Process cleanup purges watchpoints belonging to that process
            daemon.clear_process(1001)
            self.assertEqual(len(daemon.active_watchpoints), 0)
            self.assertEqual(daemon.watchpoint_ids_by_bp_id, {})

    def test_breakpoint_changed_event_updates_watchpoint(self) -> None:
        daemon = Daemon(port=15678)
        daemon.set_watchpoints(
            {
                "1001:0x2000:4": WatchpointInfo(
                    data_id="1001:0x2000:4",
                    expression="my_var",
                    thread_id=1,
                    frame_index=0,
                    access_type="write",
                )
            },
            [Breakpoint(id=7, verified=True)],
        )

        # Events for other breakpoint ids or other reasons are ignored.
        daemon._handle_breakpoint_changed(
            {"reason": "changed", "breakpoint": {"id": 8, "verified": False}}
        )
        daemon._handle_breakpoint_changed(
            {"reason": "new", "breakpoint": {"id": 7, "verified": False}}
        )
        self.assertTrue(daemon.active_watchpoints["1001:0x2000:4"].verified)

        # An async install failure marks the matching watchpoint unverified.
        daemon._handle_breakpoint_changed(
            {
                "reason": "changed",
                "breakpoint": {
                    "id": 7,
                    "verified": False,
                    "message": "Failed to install watchpoint",
                },
            }
        )
        wp = daemon.active_watchpoints["1001:0x2000:4"]
        self.assertFalse(wp.verified)
        self.assertEqual(wp.message, "Failed to install watchpoint")

    async def test_handle_watch_dap_error_reports_operation(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1, process_id=1001).is_stopped = True

        with (
            patch.object(
                daemon.dap_client, "stack_trace", new_callable=AsyncMock
            ) as mock_stack,
            patch.object(
                daemon.dap_client,
                "data_breakpoint_info",
                new_callable=AsyncMock,
            ) as mock_info,
            patch.object(
                daemon.dap_client,
                "set_data_breakpoints",
                new_callable=AsyncMock,
            ) as mock_set,
        ):
            mock_stack.return_value = StackTraceResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                body=StackTraceResponseBody(
                    stack_frames=[
                        StackFrame(id=42, name="main", line=10, column=1)
                    ]
                ),
            )
            # DapClient raises DapError for adapter rejections (success=false)
            # and timeouts; its message already names the request.
            mock_info.side_effect = DapError(
                "DAP request dataBreakpointInfo failed: no such variable"
            )
            set_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(name="my_var", thread_id=1, access_type="write"),
            )
            self.assertFalse(set_resp.success)
            self.assertEqual(
                set_resp.message,
                "Failed to set watchpoint on 'my_var': "
                "DAP request dataBreakpointInfo failed: no such variable",
            )
            mock_set.assert_not_called()
            self.assertEqual(daemon.active_watchpoints, {})

            mock_set.side_effect = DapError(
                "DAP request setDataBreakpoints failed: connection closed"
            )
            clear_resp = await daemon.registry.handle(
                "watch", WatchRequest(clear=True)
            )
            self.assertFalse(clear_resp.success)
            self.assertEqual(
                clear_resp.message,
                "Failed to clear watchpoints: "
                "DAP request setDataBreakpoints failed: connection closed",
            )

            del_resp = await daemon.registry.handle(
                "watch", WatchRequest(name="1001:0x9999:4", delete=True)
            )
            self.assertFalse(del_resp.success)
            self.assertIn("not found", del_resp.message or "")
            self.assertIn("active data_ids: none", del_resp.message or "")

    async def test_handle_watch_dap_error_preserves_state(self) -> None:
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_thread(1, process_id=1001).is_stopped = True
        existing = WatchpointInfo(
            data_id="1001:0x1000:4",
            expression="a",
            thread_id=1,
            frame_index=0,
            access_type="write",
        )
        daemon.set_watchpoints(
            {existing.data_id: existing}, [Breakpoint(id=1, verified=True)]
        )

        def assert_state_unchanged() -> None:
            self.assertEqual(
                list(daemon.active_watchpoints), [existing.data_id]
            )
            self.assertEqual(
                daemon.watchpoint_ids_by_bp_id, {1: existing.data_id}
            )

        with (
            patch.object(
                daemon.dap_client, "stack_trace", new_callable=AsyncMock
            ) as mock_stack,
            patch.object(
                daemon.dap_client,
                "data_breakpoint_info",
                new_callable=AsyncMock,
            ) as mock_info,
            patch.object(
                daemon.dap_client,
                "set_data_breakpoints",
                new_callable=AsyncMock,
            ) as mock_set,
        ):
            mock_stack.return_value = StackTraceResponse(
                seq=1,
                type="response",
                request_seq=1,
                success=True,
                body=StackTraceResponseBody(
                    stack_frames=[
                        StackFrame(id=42, name="main", line=10, column=1)
                    ]
                ),
            )
            mock_info.return_value = DataBreakpointInfoResponse(
                seq=2,
                type="response",
                request_seq=2,
                success=True,
                body=DataBreakpointInfoResponseBody(
                    data_id="1001:0x2000:4",
                    description="b (0x2000, 4 bytes)",
                ),
            )
            mock_set.side_effect = DapError(
                "DAP request setDataBreakpoints failed: no free slots"
            )

            # 1. setDataBreakpoints fails while adding a watchpoint.
            set_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(name="b", thread_id=1, access_type="write"),
            )
            self.assertFalse(set_resp.success)
            self.assertEqual(
                set_resp.message,
                "Failed to set watchpoint on 'b': "
                "DAP request setDataBreakpoints failed: no free slots",
            )
            assert_state_unchanged()

            # 2. setDataBreakpoints fails while deleting a watchpoint.
            del_resp = await daemon.registry.handle(
                "watch", WatchRequest(name=existing.data_id, delete=True)
            )
            self.assertFalse(del_resp.success)
            self.assertEqual(
                del_resp.message,
                "Failed to delete watchpoint: "
                "DAP request setDataBreakpoints failed: no free slots",
            )
            assert_state_unchanged()

            # 3. setDataBreakpoints fails while clearing watchpoints.
            clear_resp = await daemon.registry.handle(
                "watch", WatchRequest(clear=True)
            )
            self.assertFalse(clear_resp.success)
            assert_state_unchanged()

            # 4. stackTrace fails before dataBreakpointInfo is sent.
            mock_info.reset_mock()
            mock_stack.side_effect = DapError(
                "DAP request stackTrace failed: thread gone"
            )
            stack_resp = await daemon.registry.handle(
                "watch",
                WatchRequest(name="b", thread_id=1, access_type="write"),
            )
            self.assertFalse(stack_resp.success)
            self.assertEqual(
                stack_resp.message,
                "Failed to set watchpoint on 'b': "
                "DAP request stackTrace failed: thread gone",
            )
            mock_info.assert_not_called()
            assert_state_unchanged()

            # 5. Pausing the thread fails.
            mock_stack.reset_mock()
            with patch.object(
                daemon,
                "ensure_stopped",
                new_callable=AsyncMock,
                side_effect=DapError("DAP request pause failed: busy"),
            ):
                pause_resp = await daemon.registry.handle(
                    "watch",
                    WatchRequest(name="b", thread_id=1, access_type="write"),
                )
            self.assertFalse(pause_resp.success)
            self.assertEqual(
                pause_resp.message,
                "Failed to set watchpoint on 'b': "
                "pause thread 1 failed: DAP request pause failed: busy",
            )
            mock_stack.assert_not_called()
            assert_state_unchanged()

    def test_update_resumed_threads_clears_orphan_threads(self) -> None:
        """Verifies whole-system resume clears stopped state on orphan threads."""
        daemon = Daemon(port=15678)
        bound_thread = daemon.get_or_create_thread(
            101, process_id=1001, is_stopped=True
        )
        orphan_thread = daemon.get_or_create_thread(
            102, process_id=None, is_stopped=True
        )
        self.assertTrue(bound_thread.is_stopped)
        self.assertTrue(orphan_thread.is_stopped)

        daemon.update_resumed_threads(thread_id=101, single_thread=False)

        self.assertFalse(bound_thread.is_stopped)
        self.assertFalse(orphan_thread.is_stopped)


class TestDaemonStart(unittest.IsolatedAsyncioTestCase):
    @patch("daemon.handlers.start.portpicker.pick_unused_port")
    async def test_start_handler_port_allocation(self, mock_pick: Mock) -> None:
        mock_pick.return_value = 54321
        daemon = Daemon(port=None)

        # Select an unused port dynamically when starting a new session without
        # an explicit port.
        req1 = StartRequest(connect=False)
        with patch.object(
            daemon, "_check_already_running", return_value=None
        ), patch.object(
            daemon, "_start_dap_server", new_callable=AsyncMock
        ) as mock_start_server, patch.object(
            daemon, "_connect_to_dap", new_callable=AsyncMock, return_value=True
        ):
            resp1 = await start.handle(daemon, req1)
            self.assertTrue(resp1.success)
            self.assertEqual(daemon.port, 54321)

        # Fall back to DEFAULT_DAP_PORT when attaching without an explicit port.
        daemon2 = Daemon(port=None)
        req2 = StartRequest(connect=True)
        with patch.object(
            daemon2, "_check_already_running", return_value=None
        ), patch.object(
            daemon2,
            "_connect_to_dap",
            new_callable=AsyncMock,
            return_value=True,
        ):
            resp2 = await start.handle(daemon2, req2)
            self.assertTrue(resp2.success)
            self.assertEqual(daemon2.port, 15678)

        # Honor the user-specified custom port when connecting to an
        # existing DAP server.
        daemon3 = Daemon(port=None)
        req3 = StartRequest(connect=True, port=9999)
        with patch.object(
            daemon3, "_check_already_running", return_value=None
        ), patch.object(
            daemon3,
            "_connect_to_dap",
            new_callable=AsyncMock,
            return_value=True,
        ):
            resp3 = await start.handle(daemon3, req3)
            self.assertTrue(resp3.success)
            self.assertEqual(daemon3.port, 9999)

        # Respect explicitly provided port configuration.
        daemon4 = Daemon(port=None)
        req4 = StartRequest(connect=False, port=9999)
        with patch.object(
            daemon4, "_check_already_running", return_value=None
        ), patch.object(
            daemon4, "_start_dap_server", new_callable=AsyncMock
        ) as mock_start_server, patch.object(
            daemon4,
            "_connect_to_dap",
            new_callable=AsyncMock,
            return_value=True,
        ):
            resp4 = await start.handle(daemon4, req4)
            self.assertTrue(resp4.success)
            self.assertEqual(daemon4.port, 9999)

    @patch("daemon.daemon.FfxCmd")
    @patch("daemon.daemon.package_server")
    async def test_start_dap_server_early_exit(
        self, mock_package_server: Mock, mock_ffx_cmd: Mock
    ) -> None:
        mock_package_server.is_running = AsyncMock(return_value=False)
        mock_package_server.ephemeral_repository_name = Mock(
            return_value="zxdb-123"
        )
        mock_package_server.start = AsyncMock()

        # Simulate early exit by having run_to_completion() return quickly
        # without sending SIGUSR1.
        mock_dap_proc = AsyncMock()
        mock_output = Mock()
        mock_output.return_code = 123
        mock_dap_proc.run_to_completion = AsyncMock(return_value=mock_output)
        mock_ffx_cmd.return_value.start = AsyncMock(return_value=mock_dap_proc)

        daemon = Daemon(port=15678)

        resp = await daemon._start_dap_server()

        self.assertIsNotNone(resp)
        assert resp is not None
        self.assertFalse(resp.success)
        self.assertEqual(resp.message, "zxdb exited early with code 123")

    @patch("daemon.daemon.ZxdbDapClient")
    async def test_handle_m2_process_inspection_commands(
        self, mock_dap_client_class: Mock
    ) -> None:
        """Verifies zero-pause process inspection handlers (`modules`, `aspace`, `handles`, `mem-read`)."""
        mock_dap_client = mock_dap_client_class.return_value
        daemon = Daemon(port=15678)
        daemon.zxdb_writer = Mock()
        daemon.get_or_create_process(48291, name="driver_host:pdev")

        executed_commands: list[str] = []

        async def console_side_effect(args: Any) -> Mock:
            cmd = args.command
            executed_commands.append(cmd)
            resp = Mock(success=True)
            if cmd == "process 1 libs":
                resp.body = Mock(
                    output=(
                        "  Load address  Name\n"
                        " 0x40050123000  libxhci.so\n"
                        " 0x40080200000  libdriver_runtime.so\n"
                    )
                )
            elif cmd == "process 2 libs":
                resp.body = Mock(
                    output=(
                        "  Load address  Name\n" " 0x50010000000  libother.so\n"
                    )
                )
            elif cmd == "process 1 sym-stat":
                resp.body = Mock(
                    output=(
                        "Symbol index status\n\n"
                        "Process 1 symbol status\n\n"
                        "  libxhci.so\n"
                        "    Base: 0x40050123000\n"
                        "    Build ID: 4a8f19c2\n"
                        "    Symbols loaded: Yes\n"
                        "    Symbol file: /build-id/4a/8f19c2.debug\n"
                        "    Source files indexed: 42\n"
                        "    Symbols indexed: 318\n\n"
                        "  libdriver_runtime.so\n"
                        "    Base: 0x40080200000\n"
                        "    Build ID: 99aabbcc\n"
                        "    Symbols loaded: No\n\n"
                        '  👉 Use "libs" to refresh the module list\n'
                    )
                )
            elif cmd == "process 2 sym-stat":
                resp.body = Mock(
                    output=(
                        "Symbol index status\n\n"
                        "Process 2 symbol status\n\n"
                        "  libother.so\n"
                        "    Base: 0x50010000000\n"
                        "    Build ID: deadbeef\n"
                        "    Symbols loaded: Yes\n"
                    )
                )
            elif cmd in (
                "process 1 aspace",
                "process 1 aspace -- 0x954e4000",
            ):
                resp.body = Mock(
                    output=(
                        "          Start              End  Prot   Size    "
                        " Koid  Offset  Cmt.Pgs  Name\n"
                        "      0x200000    0x80000000000  ---   128T      "
                        "                       proc-vmar\n"
                        "    0x954e4000       0x954e8000  rw-    16K    "
                        " 4242     0x0        4      mmio-xhci\n"
                        "\n"
                        "              Page size: 4096\n"
                        "     Total mapped bytes: 16384\n"
                        "  Total committed pages: 4 = 16384 bytes\n"
                    )
                )
            elif cmd == "process 2 aspace -- 0x50010000000 - 0x1000":
                resp.body = Mock(
                    output=(
                        "          Start              End  Prot   Size    "
                        " Koid  Offset  Cmt.Pgs  Name\n"
                        " 0x5000ffff000    0x50010000000  r--     4K    "
                        " 7777     0x0        1      other-vmo\n"
                    )
                )
            elif cmd == "process 1 handle -x":
                resp.body = Mock(
                    output=(
                        "  Handle  Type                        Koid\n"
                        "  0x1003  ZX_OBJ_TYPE_INTERRUPT       0x292e\n"
                        "  <none>  ZX_OBJ_TYPE_VMO             0x1092\n"
                        "  0x1007  <unknown (33)>              0x3000\n"
                    )
                )
            elif cmd == "process 2 handle -x":
                resp.body = Mock(
                    output=(
                        "  Handle  Type                        Koid\n"
                        "  0x2001  ZX_OBJ_TYPE_CHANNEL         0x8888\n"
                    )
                )
            elif cmd in (
                "process 1 handle -x 0x1003",
                "process 1 handle -k -x 10542",
            ):
                resp.body = Mock(
                    output=(
                        "                 Handle  0x1003\n"
                        "                   Type  ZX_OBJ_TYPE_INTERRUPT\n"
                        "                   Koid  0x292e\n"
                        "                 Rights  ZX_RIGHT_DUPLICATE\n"
                        "                         ZX_RIGHT_WAIT\n"
                        "           Related koid  0x0\n"
                    )
                )
            elif cmd == "process 1 handle -k -x 4242":
                resp.body = Mock(
                    output=(
                        "                 Handle  <none>\n"
                        "                   Type  ZX_OBJ_TYPE_VMO\n"
                        "                   Koid  0x1092\n"
                        "                   Name  mmio-xhci\n"
                        "      VMO size in bytes  0x4000\n"
                        "           Cache policy "
                        " ZX_CACHE_POLICY_UNCACHED_DEVICE\n"
                    )
                )
            elif cmd.startswith("process 1 mem-read --size=16 -- "):
                resp.body = Mock(
                    output=(
                        "0x7ffee1234000:  01 02 03 04 05 06 07 08-09 0a 0b 0c"
                        " 0d 0e 0f 10  |................\n"
                    )
                )
            elif cmd == "process 2 mem-read --size=8 -- $rsp - 8":
                resp.body = Mock(
                    output="0x7ffee9990000:  aa bb cc dd ee ff 00 11  |........\n"
                )
            else:
                resp.body = Mock(output="ok")
            return resp

        mock_dap_client.zxdb_console_command = AsyncMock(
            side_effect=console_side_effect
        )

        # 1. modules
        mod_resp = await daemon.registry.handle(
            "modules", ModulesRequest(filter="xhci")
        )
        self.assertTrue(mod_resp.success)
        assert isinstance(mod_resp.body, ModulesResponse)
        self.assertEqual(mod_resp.body.pid, 48291)
        self.assertEqual(len(mod_resp.body.modules), 1)
        m0 = mod_resp.body.modules[0]
        self.assertEqual(m0.name, "libxhci.so")
        self.assertEqual(m0.base, "0x40050123000")
        self.assertEqual(m0.build_id, "4a8f19c2")
        self.assertTrue(m0.symbols_loaded)
        self.assertEqual(m0.files_indexed, 42)
        self.assertEqual(m0.functions_indexed, 318)

        # 2. aspace
        as_resp = await daemon.registry.handle(
            "aspace", AspaceRequest(filter="mmio")
        )
        self.assertTrue(as_resp.success)
        assert isinstance(as_resp.body, AspaceResponse)
        self.assertEqual(as_resp.body.page_size, 4096)
        self.assertEqual(len(as_resp.body.regions), 1)
        r0 = as_resp.body.regions[0]
        self.assertEqual(r0.name, "mmio-xhci")
        self.assertEqual(r0.vmo_koid, 4242)
        self.assertEqual(r0.committed_pages, 4)

        # 3. handles with type filter and detail=True
        h_resp = await daemon.registry.handle(
            "handles", HandlesRequest(type="interrupt", detail=True)
        )
        self.assertTrue(h_resp.success)
        assert isinstance(h_resp.body, HandlesResponse)
        self.assertEqual(len(h_resp.body.handles), 1)
        h0 = h_resp.body.handles[0]
        self.assertEqual(h0.handle_value, "0x1003")
        self.assertEqual(h0.type, "ZX_OBJ_TYPE_INTERRUPT")
        self.assertEqual(h0.koid, 0x292E)
        self.assertEqual(h0.rights, ["ZX_RIGHT_DUPLICATE", "ZX_RIGHT_WAIT"])

        # 3b. handles with unknown type and mapped VMO enrichment via handle -k
        h_all_resp = await daemon.registry.handle(
            "handles", HandlesRequest(detail=True)
        )
        self.assertTrue(h_all_resp.success)
        assert isinstance(h_all_resp.body, HandlesResponse)
        self.assertEqual(len(h_all_resp.body.handles), 3)
        self.assertEqual(h_all_resp.body.handles[1].vmo_name, "mmio-xhci")
        self.assertEqual(
            h_all_resp.body.handles[1].cache_policy,
            "ZX_CACHE_POLICY_UNCACHED_DEVICE",
        )
        self.assertEqual(h_all_resp.body.handles[2].type, "<unknown (33)>")

        # 4. mem-read without pausing any thread
        mr_resp = await daemon.registry.handle(
            "mem-read", MemReadRequest(address="0x7ffee1234000", size=16)
        )
        self.assertTrue(mr_resp.success)
        assert isinstance(mr_resp.body, MemReadResponse)
        self.assertEqual(mr_resp.body.address, "0x7ffee1234000")
        self.assertEqual(mr_resp.body.size, 16)
        self.assertEqual(
            mr_resp.body.u32_le,
            ["0x04030201", "0x08070605", "0x0c0b0a09", "0x100f0e0d"],
        )
        self.assertEqual(
            mr_resp.body.u64_le,
            ["0x0807060504030201", "0x100f0e0d0c0b0a09"],
        )

        # 5. Multiple processes without --pid returns error
        daemon.get_or_create_process(99999, name="driver_host:other")
        for cmd_name, req_obj in (
            ("modules", ModulesRequest()),
            ("aspace", AspaceRequest()),
            ("handles", HandlesRequest()),
            ("mem-read", MemReadRequest(address="0x1000", size=16)),
        ):
            multi_resp = await daemon.registry.handle(cmd_name, req_obj)
            self.assertFalse(multi_resp.success)
            self.assertIn("Multiple processes active", multi_resp.message or "")

        # 6. Multi-process --pid selection scopes commands to `process 2`
        executed_commands.clear()
        mod2_resp = await daemon.registry.handle(
            "modules", ModulesRequest(pid=99999)
        )
        self.assertTrue(mod2_resp.success)
        assert isinstance(mod2_resp.body, ModulesResponse)
        self.assertEqual(mod2_resp.body.pid, 99999)
        self.assertEqual(len(mod2_resp.body.modules), 1)
        self.assertEqual(mod2_resp.body.modules[0].name, "libother.so")
        self.assertEqual(mod2_resp.body.modules[0].build_id, "deadbeef")

        as2_resp = await daemon.registry.handle(
            "aspace",
            AspaceRequest(pid=99999, address="0x50010000000 - 0x1000"),
        )
        self.assertTrue(as2_resp.success)
        assert isinstance(as2_resp.body, AspaceResponse)
        self.assertEqual(as2_resp.body.pid, 99999)
        self.assertEqual(len(as2_resp.body.regions), 1)
        self.assertEqual(as2_resp.body.regions[0].name, "other-vmo")

        h2_resp = await daemon.registry.handle(
            "handles", HandlesRequest(pid=99999)
        )
        self.assertTrue(h2_resp.success)
        assert isinstance(h2_resp.body, HandlesResponse)
        self.assertEqual(h2_resp.body.pid, 99999)
        self.assertEqual(len(h2_resp.body.handles), 1)
        self.assertEqual(h2_resp.body.handles[0].koid, 0x8888)

        mr2_resp = await daemon.registry.handle(
            "mem-read",
            MemReadRequest(pid=99999, address="$rsp - 8", size=8),
        )
        self.assertTrue(mr2_resp.success)
        assert isinstance(mr2_resp.body, MemReadResponse)
        self.assertEqual(mr2_resp.body.address, "0x7ffee9990000")
        self.assertEqual(
            executed_commands,
            [
                "process 2 libs",
                "process 2 sym-stat",
                "process 2 aspace -- 0x50010000000 - 0x1000",
                "process 2 handle -x",
                "process 2 mem-read --size=8 -- $rsp - 8",
            ],
        )

        # 7. Unknown --pid is rejected across all four commands
        for cmd_name, req_obj in (
            ("modules", ModulesRequest(pid=11111)),
            ("aspace", AspaceRequest(pid=11111)),
            ("handles", HandlesRequest(pid=11111)),
            ("mem-read", MemReadRequest(pid=11111, address="0x1000", size=16)),
        ):
            unknown_resp = await daemon.registry.handle(cmd_name, req_obj)
            self.assertFalse(unknown_resp.success)
            self.assertEqual(unknown_resp.message, "Process 11111 not found")

        # 8. Non-zero frame_index without thread_id in mem-read is rejected
        bad_frame_resp = await daemon.registry.handle(
            "mem-read",
            MemReadRequest(pid=48291, address="0x1000", size=16, frame_index=1),
        )
        self.assertFalse(bad_frame_resp.success)
        self.assertEqual(
            bad_frame_resp.message,
            "thread_id is required when specifying a non-zero frame_index",
        )


if __name__ == "__main__":
    unittest.main()
