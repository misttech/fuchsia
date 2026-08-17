# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from pydantic import ValidationError
from shared.protocol import PROTOCOL_VERSION, make_request
from shared.protocol.attach import AttachRequest
from shared.protocol.detach import DetachRequest
from shared.protocol.finish import FinishRequest
from shared.protocol.hello import HelloRequest
from shared.protocol.next_request import NextRequest
from shared.protocol.stack_trace import StackTraceRequest
from shared.protocol.start import StartRequest
from shared.protocol.step_in import StepInRequest
from shared.protocol.stop import StopRequest
from shared.protocol.wait_for_event import WaitForEventRequest


class TestStackTraceRequestSchema(unittest.TestCase):
    def test_valid_request_thread_id(self) -> None:
        req = StackTraceRequest(thread_id=1)
        self.assertEqual(req.thread_id, 1)
        self.assertIsNone(req.pid)
        self.assertFalse(req.raw)

    def test_valid_request_pid(self) -> None:
        req = StackTraceRequest(pid=1234, raw=True)
        self.assertIsNone(req.thread_id)
        self.assertEqual(req.pid, 1234)
        self.assertTrue(req.raw)

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
            "command": "step_in",
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
            "command": "step_in",
            "thread_id": 1,
            "granularity": "invalid_granularity",
        }
        with self.assertRaises(ValidationError):
            make_request(data)

    def test_parse_unknown_command(self) -> None:
        data = {"command": "unknown-cmd"}
        with self.assertRaises(ValidationError):
            make_request(data)


if __name__ == "__main__":
    unittest.main()
