# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from typing import Any

from pydantic import ValidationError
from shared.protocol import (
    PROTOCOL_VERSION,
    deserialize_response,
    make_request,
)
from shared.protocol.aspace import AspaceRequest, AspaceResponse
from shared.protocol.async_backtrace import (
    AsyncBacktraceRequest,
    AsyncBacktraceResponse,
)
from shared.protocol.attach import AttachRequest
from shared.protocol.break_request import BreakRequest
from shared.protocol.console import ConsoleRequest, ConsoleResponse
from shared.protocol.continue_request import ContinueRequest
from shared.protocol.detach import DetachRequest
from shared.protocol.evaluate import EvaluateRequest, EvaluateResponse
from shared.protocol.finish import FinishRequest
from shared.protocol.get_state import GetStateRequest, GetStateResponse
from shared.protocol.handles import HandlesRequest, HandlesResponse
from shared.protocol.hello import HelloRequest
from shared.protocol.mem_read import MemReadRequest, MemReadResponse
from shared.protocol.modules import ModulesRequest, ModulesResponse
from shared.protocol.next_request import NextRequest
from shared.protocol.pause import PauseRequest
from shared.protocol.stack_trace import (
    ProcessStackTraceResponse,
    StackTraceRequest,
    ThreadStackTraceResponse,
)
from shared.protocol.start import StartRequest
from shared.protocol.step_in import StepInRequest
from shared.protocol.stop import StopRequest
from shared.protocol.threads import ThreadsRequest
from shared.protocol.variables import VariablesRequest
from shared.protocol.wait_for_event import WaitForEventRequest
from shared.protocol.watch_request import WatchRequest
from zxdb_dap import AsyncTaskNode


class TestAsyncBacktraceRequestSchema(unittest.TestCase):
    def test_valid_request_explicit_pid(self) -> None:
        req = AsyncBacktraceRequest(pid=1234)
        self.assertEqual(req.command, "async-backtrace")
        self.assertEqual(req.pid, 1234)

    def test_valid_request_no_pid(self) -> None:
        req = AsyncBacktraceRequest()
        self.assertEqual(req.command, "async-backtrace")
        self.assertIsNone(req.pid)


class TestAsyncBacktraceResponseSchema(unittest.TestCase):
    def test_response_serialization_with_tasks(self) -> None:
        child = AsyncTaskNode(
            id="task-2", name="child_task", file="bar.rs", line=20
        )
        parent = AsyncTaskNode(
            id="task-1",
            name="parent_task",
            file="foo.rs",
            line=10,
            children=[child],
        )
        resp = AsyncBacktraceResponse(
            process_id=1234,
            tasks=[parent],
        )
        dumped = resp.model_dump(by_alias=True)
        self.assertEqual(dumped["process_id"], 1234)
        self.assertEqual(len(dumped["tasks"]), 1)
        self.assertEqual(dumped["tasks"][0]["id"], "task-1")
        self.assertEqual(dumped["tasks"][0]["name"], "parent_task")
        self.assertEqual(len(dumped["tasks"][0]["children"]), 1)
        self.assertEqual(dumped["tasks"][0]["children"][0]["id"], "task-2")


class TestStackTraceRequestSchema(unittest.TestCase):
    def test_valid_request_thread_id(self) -> None:
        req = StackTraceRequest(thread_id=1)
        self.assertEqual(req.thread_id, 1)
        self.assertIsNone(req.pid)
        self.assertFalse(req.raw)
        self.assertFalse(req.force_remote_unwind)
        self.assertIsNone(req.unwinder)

    def test_valid_request_pid(self) -> None:
        req = StackTraceRequest(pid=1234, raw=True)
        self.assertIsNone(req.thread_id)
        self.assertEqual(req.pid, 1234)
        self.assertTrue(req.raw)
        self.assertFalse(req.force_remote_unwind)
        self.assertIsNone(req.unwinder)

    def test_valid_request_unwinder_and_remote_unwind(self) -> None:
        req = StackTraceRequest(
            thread_id=1, force_remote_unwind=True, unwinder="fp"
        )
        self.assertEqual(req.thread_id, 1)
        self.assertTrue(req.force_remote_unwind)
        self.assertEqual(req.unwinder, "fp")

    def test_malformed_request_invalid_unwinder(self) -> None:
        with self.assertRaises(ValidationError):
            StackTraceRequest(thread_id=1, unwinder="invalid")

    def test_malformed_request_both(self) -> None:
        with self.assertRaises(ValidationError):
            StackTraceRequest(thread_id=1, pid=1234)

    def test_malformed_request_neither(self) -> None:
        with self.assertRaises(ValidationError):
            StackTraceRequest()


class TestDetachRequestSchema(unittest.TestCase):
    def test_valid_request_pid(self) -> None:
        req = DetachRequest(pid=1234)
        self.assertEqual(req.pid, 1234)
        self.assertFalse(req.all)

    def test_valid_request_all(self) -> None:
        req = DetachRequest(all=True)
        self.assertIsNone(req.pid)
        self.assertTrue(req.all)

    def test_malformed_request_both(self) -> None:
        with self.assertRaises(ValidationError):
            DetachRequest(pid=1234, all=True)

    def test_malformed_request_neither(self) -> None:
        with self.assertRaises(ValidationError):
            DetachRequest()


class TestHelloRequestSchema(unittest.TestCase):
    def test_valid(self) -> None:
        req = HelloRequest(version=PROTOCOL_VERSION)
        self.assertEqual(req.version, PROTOCOL_VERSION)

    def test_missing_version(self) -> None:
        with self.assertRaises(ValidationError):
            HelloRequest()

    def test_type_coercion(self) -> None:
        # Pydantic should coerce valid integer-like strings to integers by default
        req = HelloRequest(version="5")
        self.assertEqual(req.version, 5)

        with self.assertRaises(ValidationError):
            HelloRequest(version="not-an-int")


class TestAttachRequestSchema(unittest.TestCase):
    def test_valid_int_pid(self) -> None:
        req = AttachRequest(filter=1234)
        self.assertEqual(req.filter, 1234)

    def test_valid_string_name(self) -> None:
        req = AttachRequest(filter="my_process")
        self.assertEqual(req.filter, "my_process")

    def test_missing_filter(self) -> None:
        with self.assertRaises(ValidationError):
            AttachRequest()

    def test_weak_and_exact_flags(self) -> None:
        req = AttachRequest(
            filter="xhci", weak=True, exact=True, recursive=True
        )
        self.assertTrue(req.weak)
        self.assertTrue(req.exact)
        self.assertTrue(req.recursive)

    def test_process_inspection_schemas(self) -> None:
        mod_req = ModulesRequest(pid=48291, filter="xhci", force_reload=True)
        self.assertEqual(mod_req.command, "modules")
        self.assertIsInstance(ModulesResponse(), ModulesResponse)

        as_req = AspaceRequest(pid=48291, address="0x1000", filter="mmio")
        self.assertEqual(as_req.command, "aspace")
        self.assertIsInstance(AspaceResponse(), AspaceResponse)

        h_req = HandlesRequest(pid=48291, type="interrupt", detail=True)
        self.assertEqual(h_req.command, "handles")
        self.assertIsInstance(HandlesResponse(), HandlesResponse)

        mr_req = MemReadRequest(address="0x7ffee1234000", size=32, pid=48291)
        self.assertEqual(mr_req.command, "mem-read")
        self.assertIsInstance(
            MemReadResponse(address="0x7ffee1234000", size=32, hex_bytes=""),
            MemReadResponse,
        )


class TestWaitForEventRequestSchema(unittest.TestCase):
    def test_valid(self) -> None:
        req = WaitForEventRequest(last_seen_seq=10, timeout=5)
        self.assertEqual(req.last_seen_seq, 10)
        self.assertEqual(req.timeout, 5)

    def test_optional_timeout(self) -> None:
        req = WaitForEventRequest(last_seen_seq=10)
        self.assertEqual(req.last_seen_seq, 10)
        self.assertIsNone(req.timeout)

    def test_missing_last_seen_seq(self) -> None:
        with self.assertRaises(ValidationError):
            WaitForEventRequest(timeout=5)


class TestPolymorphicParsing(unittest.TestCase):
    def test_parse_start(self) -> None:
        data = {"command": "start", "port": 15678, "connect": True}
        req = make_request(data)
        self.assertTrue(isinstance(req, StartRequest))
        self.assertEqual(req.port, 15678)
        self.assertTrue(req.connect)

    def test_parse_stop(self) -> None:
        data = {"command": "stop", "ack_seq": 10}
        req = make_request(data)
        self.assertTrue(isinstance(req, StopRequest))
        self.assertEqual(req.ack_seq, 10)

    def test_parse_finish(self) -> None:
        data = {"command": "finish", "thread_id": 1, "single_thread": True}
        req = make_request(data)
        self.assertTrue(isinstance(req, FinishRequest))
        self.assertEqual(req.thread_id, 1)
        self.assertTrue(req.single_thread)

    def test_parse_next(self) -> None:
        data = {
            "command": "next",
            "thread_id": 1,
            "single_thread": True,
            "granularity": "line",
        }
        req = make_request(data)
        self.assertTrue(isinstance(req, NextRequest))
        self.assertEqual(req.thread_id, 1)
        self.assertTrue(req.single_thread)
        self.assertEqual(req.granularity, "line")

    def test_parse_next_invalid_granularity(self) -> None:
        data = {
            "command": "next",
            "thread_id": 1,
            "granularity": "invalid_granularity",
        }
        with self.assertRaises(ValidationError):
            make_request(data)

    def test_parse_step_in(self) -> None:
        data = {
            "command": "step-in",
            "thread_id": 1,
            "single_thread": True,
            "target_id": 0,
            "granularity": "line",
        }
        req = make_request(data)
        self.assertTrue(isinstance(req, StepInRequest))
        self.assertEqual(req.thread_id, 1)
        self.assertTrue(req.single_thread)
        self.assertEqual(req.target_id, 0)
        self.assertEqual(req.granularity, "line")

    def test_parse_step_in_invalid_granularity(self) -> None:
        data = {
            "command": "step-in",
            "thread_id": 1,
            "granularity": "invalid_granularity",
        }
        with self.assertRaises(ValidationError):
            make_request(data)

    def test_parse_async_backtrace(self) -> None:
        data = {"command": "async-backtrace", "pid": 1234}
        req = make_request(data)
        self.assertTrue(isinstance(req, AsyncBacktraceRequest))
        self.assertEqual(req.pid, 1234)

    def test_parse_console(self) -> None:
        data = {"command": "console", "input": "sym-stat"}
        req = make_request(data)
        self.assertTrue(isinstance(req, ConsoleRequest))
        self.assertEqual(req.input, "sym-stat")
        self.assertIsNone(req.thread_id)
        self.assertEqual(req.frame_index, 0)

    def test_parse_console_with_thread_and_frame(self) -> None:
        data = {
            "command": "console",
            "input": "regs",
            "thread_id": 1001,
            "frame_index": 2,
        }
        req = make_request(data)
        self.assertTrue(isinstance(req, ConsoleRequest))
        self.assertEqual(req.input, "regs")
        self.assertEqual(req.thread_id, 1001)
        self.assertEqual(req.frame_index, 2)

    def test_parse_unknown_command(self) -> None:
        data = {"command": "unknown-cmd"}
        with self.assertRaises(ValidationError):
            make_request(data)


class TestResponseTypeAndDeserialization(unittest.TestCase):
    def test_response_types_defined(self) -> None:
        self.assertEqual(
            AsyncBacktraceRequest.response_type, AsyncBacktraceResponse
        )
        self.assertEqual(GetStateRequest.response_type, GetStateResponse)
        self.assertEqual(ConsoleRequest.response_type, ConsoleResponse)
        self.assertEqual(EvaluateRequest.response_type, EvaluateResponse)
        self.assertEqual(
            StackTraceRequest.response_type,
            ThreadStackTraceResponse | ProcessStackTraceResponse,
        )
        self.assertEqual(AttachRequest.response_type, dict[str, Any])
        self.assertEqual(BreakRequest.response_type, dict[str, Any])
        self.assertEqual(ContinueRequest.response_type, dict[str, Any])
        self.assertEqual(DetachRequest.response_type, dict[str, Any])
        self.assertEqual(FinishRequest.response_type, dict[str, Any])
        self.assertEqual(HelloRequest.response_type, dict[str, Any])
        self.assertEqual(NextRequest.response_type, dict[str, Any])
        self.assertEqual(PauseRequest.response_type, dict[str, Any])
        self.assertEqual(StartRequest.response_type, dict[str, Any])
        self.assertEqual(StepInRequest.response_type, dict[str, Any])
        self.assertIsNone(StopRequest.response_type)
        self.assertEqual(ThreadsRequest.response_type, dict[str, Any])
        self.assertEqual(VariablesRequest.response_type, dict[str, Any])
        self.assertIsNone(WaitForEventRequest.response_type)

    def test_deserialize_async_backtrace_response(self) -> None:
        req = AsyncBacktraceRequest(pid=1234)
        json_line = (
            '{"success": true, "message": null, "events": null, "body": '
            '{"process_id": 1234, "tasks": []}}'
        )
        resp = deserialize_response(json_line, req)
        self.assertTrue(resp.success)
        self.assertIsInstance(resp.body, AsyncBacktraceResponse)
        assert resp.body is not None
        self.assertEqual(resp.body.process_id, 1234)
        self.assertEqual(resp.body.tasks, [])

    def test_deserialize_typed_response(self) -> None:
        req = GetStateRequest()
        json_line = (
            '{"success": true, "message": null, "events": null, "body": '
            '{"threads": [{"id": 1, "name": "t1"}], "processes": null, "breakpoints": null}}'
        )
        resp = deserialize_response(json_line, req)
        self.assertTrue(resp.success)
        self.assertIsInstance(resp.body, GetStateResponse)
        assert resp.body is not None
        self.assertEqual(len(resp.body.threads), 1)
        self.assertEqual(resp.body.threads[0].name, "t1")

    def test_deserialize_dict_response(self) -> None:
        req = ThreadsRequest()
        json_line = (
            '{"success": true, "message": null, "events": null, "body": '
            '{"threads": [{"id": 1, "name": "t1"}]}}'
        )
        resp = deserialize_response(json_line, req)
        self.assertTrue(resp.success)
        self.assertIsInstance(resp.body, dict)
        self.assertEqual(resp.body, {"threads": [{"id": 1, "name": "t1"}]})

    def test_deserialize_none_response(self) -> None:
        req = StopRequest()
        json_line = '{"success": true, "message": "stopped", "events": null, "body": null}'
        resp = deserialize_response(json_line, req)
        self.assertTrue(resp.success)
        self.assertIsNone(resp.body)
        self.assertEqual(resp.message, "stopped")

    def test_deserialize_union_response_thread_and_process(self) -> None:
        req = StackTraceRequest(thread_id=1)
        thread_line = (
            '{"success": true, "message": null, "events": null, "body": '
            '{"thread_id": 1, "stack_frames": [{"frame_index": 0, "name": "foo", "line": 10, "column": 1}], "total_frames": 1}}'
        )
        resp_thread = deserialize_response(thread_line, req)
        self.assertTrue(resp_thread.success)
        self.assertIsInstance(resp_thread.body, ThreadStackTraceResponse)

        process_line = (
            '{"success": true, "message": null, "events": null, "body": '
            '{"process_id": 1234, "stacks": [{"thread_id": 1, "stack_frames": [], "total_frames": 0}]}}'
        )
        resp_proc = deserialize_response(process_line, req)
        self.assertTrue(resp_proc.success)
        self.assertIsInstance(resp_proc.body, ProcessStackTraceResponse)

    def test_deserialize_error_response_typed_request(self) -> None:
        req = GetStateRequest()
        json_line = '{"success": false, "message": "Handler failed", "events": null, "body": null}'
        resp = deserialize_response(json_line, req)
        self.assertFalse(resp.success)
        self.assertIsNone(resp.body)
        self.assertEqual(resp.message, "Handler failed")

    def test_deserialize_evaluate_forbid_extra(self) -> None:
        req = EvaluateRequest(thread_id=1, expression="x")
        json_line = (
            '{"success": true, "message": null, "events": null, "body": '
            '{"result": "123", "type": "int", "unknown_field": 42}}'
        )
        with self.assertRaises(ValidationError):
            deserialize_response(json_line, req)

    def test_deserialize_console_response(self) -> None:
        req = ConsoleRequest(input="sym-stat")
        json_line = (
            '{"success": true, "message": null, "events": null, "body": '
            '{"output": "Process 1 symbol status\\n"}}'
        )
        resp = deserialize_response(json_line, req)
        self.assertTrue(resp.success)
        self.assertIsInstance(resp.body, ConsoleResponse)
        assert resp.body is not None
        self.assertEqual(resp.body.output, "Process 1 symbol status\n")

    def test_deserialize_console_forbid_extra(self) -> None:
        req = ConsoleRequest(input="sym-stat")
        json_line = (
            '{"success": true, "message": null, "events": null, "body": '
            '{"output": "ok", "extra": 1}}'
        )
        with self.assertRaises(ValidationError):
            deserialize_response(json_line, req)

    def test_make_watch_request(self) -> None:
        req = make_request(
            {
                "command": "watch",
                "name": "my_var",
                "thread_id": 1,
                "frame_index": 2,
                "access_type": "readWrite",
            }
        )
        self.assertIsInstance(req, WatchRequest)
        assert isinstance(req, WatchRequest)
        self.assertEqual(req.name, "my_var")
        self.assertEqual(req.thread_id, 1)
        self.assertEqual(req.frame_index, 2)
        self.assertEqual(req.access_type, "readWrite")
        self.assertFalse(req.delete)

        with self.assertRaises(ValidationError):
            make_request(
                {
                    "command": "watch",
                    "name": "my_var",
                    "thread_id": 1,
                    "access_type": "read",
                }
            )

        del_req = make_request(
            {
                "command": "watch",
                "name": "1001:0x1234:4",
                "delete": True,
            }
        )
        self.assertIsInstance(del_req, WatchRequest)
        assert isinstance(del_req, WatchRequest)
        self.assertEqual(del_req.name, "1001:0x1234:4")
        self.assertTrue(del_req.delete)

    def test_deserialize_get_state_with_watchpoints(self) -> None:
        req = GetStateRequest()
        json_line = (
            '{"success": true, "message": null, "events": null, "body": '
            '{"threads": [{"id": 1, "name": "t1"}], "processes": null, '
            '"breakpoints": null, "watchpoints": {"1001:0x1234:4": '
            '{"data_id": "1001:0x1234:4", "expression": "my_var", '
            '"thread_id": 1, "frame_index": 0, "access_type": "write", '
            '"verified": true, "message": null}}}}'
        )
        resp = deserialize_response(json_line, req)
        self.assertTrue(resp.success)
        self.assertIsInstance(resp.body, GetStateResponse)
        assert resp.body is not None
        self.assertIsNotNone(resp.body.watchpoints)
        assert resp.body.watchpoints is not None
        wp = resp.body.watchpoints["1001:0x1234:4"]
        self.assertEqual(wp.data_id, "1001:0x1234:4")
        self.assertEqual(wp.expression, "my_var")
        self.assertEqual(wp.thread_id, 1)
        self.assertEqual(wp.frame_index, 0)
        self.assertEqual(wp.access_type, "write")
        self.assertTrue(wp.verified)
        self.assertIsNone(wp.message)


if __name__ == "__main__":
    unittest.main()
