# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import dataclasses
import json
from typing import Any

PROTOCOL_VERSION = 3


@dataclasses.dataclass(kw_only=True)
class BaseRequest:
    """Base class for all requests containing the command name."""

    command: str
    last_seen_seq: int | None = None
    ack_seq: int | None = None


@dataclasses.dataclass(kw_only=True)
class StartRequest(BaseRequest):
    """Request to start the debugging session."""

    port: int | None = None

    # Connect to an existing debug adapter on |port|.
    connect: bool = False
    command: str = "start"


@dataclasses.dataclass(kw_only=True)
class HelloRequest(BaseRequest):
    """Initial handshake request to verify protocol version."""

    version: int
    command: str = "hello"


@dataclasses.dataclass(kw_only=True)
class StopRequest(BaseRequest):
    """Request to stop the daemon and session."""

    command: str = "stop"


@dataclasses.dataclass(kw_only=True)
class GetStateRequest(BaseRequest):
    """Request current state of threads."""

    command: str = "get-state"


@dataclasses.dataclass(kw_only=True)
class WaitForEventRequest(BaseRequest):
    timeout: int | None = None
    command: str = "wait-for-event"


@dataclasses.dataclass(kw_only=True)
class AttachRequest(BaseRequest):
    """Request to attach to a process."""

    filter: str | int
    command: str = "attach"


@dataclasses.dataclass(kw_only=True)
class ThreadsRequest(BaseRequest):
    """Request list of threads."""

    command: str = "threads"


# TODO(https://fxbug.dev/509557630): Implement process-wide continue.
@dataclasses.dataclass(kw_only=True)
class ContinueRequest(BaseRequest):
    """Request to resume execution of a thread."""

    thread_id: int
    single_thread: bool | None = None
    command: str = "continue"


# TODO(https://fxbug.dev/509557630): Implement process-wide pause.
@dataclasses.dataclass(kw_only=True)
class PauseRequest(BaseRequest):
    """Request to pause execution of a thread."""

    thread_id: int
    command: str = "pause"


@dataclasses.dataclass(kw_only=True)
class StackTraceRequest(BaseRequest):
    """Request stack trace for a thread."""

    thread_id: int
    command: str = "stackTrace"


@dataclasses.dataclass
class ThreadInfo:
    """Information about a single thread."""

    id: int
    name: str


@dataclasses.dataclass
class GetStateResponse:
    """Response for get-state command containing thread list."""

    threads: list[ThreadInfo]


@dataclasses.dataclass
class Response:
    """Standard response wrapper."""

    success: bool
    message: str | None = None
    body: dict[str, Any] | None = None
    events: list[dict[str, Any]] | None = None


def serialize(obj: BaseRequest | Response) -> str:
    assert dataclasses.is_dataclass(obj)
    return json.dumps(dataclasses.asdict(obj)) + "\n"


def make_request(data: dict[str, Any]) -> BaseRequest:
    """Dispatches raw dictionary data into appropriate request objects."""

    command = data.get("command")
    req: BaseRequest
    match command:
        case "start":
            req = StartRequest(
                port=data.get("port"),
                connect=data.get("connect", False),
            )
        case "hello":
            version = data.get("version")
            if version is None:
                raise ValueError("Version must be specified for hello")
            req = HelloRequest(version=version)
        case "stop":
            req = StopRequest()
        case "get-state":
            req = GetStateRequest()
        case "attach":
            process_filter = data.get("filter")
            if process_filter is None:
                raise ValueError("Filter must be specified for attach")
            req = AttachRequest(filter=process_filter)
        case "threads":
            req = ThreadsRequest()
        case "pause":
            thread_id = data.get("thread_id")
            if thread_id is None:
                raise ValueError("Thread ID must be specified for pause")
            req = PauseRequest(thread_id=thread_id)
        case "continue":
            thread_id = data.get("thread_id")
            if thread_id is None:
                raise ValueError("Thread ID must be specified for continue")
            req = ContinueRequest(
                thread_id=thread_id, single_thread=data.get("single_thread")
            )
        case "stackTrace":
            thread_id = data.get("thread_id")
            if thread_id is None:
                raise ValueError("Thread ID must be specified for stackTrace")
            req = StackTraceRequest(thread_id=thread_id)
        case "wait-for-event":
            last_seen = data.get("last_seen_seq")
            timeout = data.get("timeout")
            if last_seen is not None:
                try:
                    last_seen_seq = int(last_seen)
                except ValueError:
                    raise ValueError("last_seen_seq must be an integer")
            else:
                last_seen_seq = None

            try:
                timeout_val = int(timeout) if timeout is not None else None
            except ValueError:
                raise ValueError("timeout must be an integer")

            req = WaitForEventRequest(
                last_seen_seq=last_seen_seq, timeout=timeout_val
            )
        case _:
            raise ValueError(f"Unknown command: {command}")

    ack_seq = data.get("ack_seq")
    if ack_seq is not None:
        try:
            req.ack_seq = int(ack_seq)
        except ValueError:
            raise ValueError("ack_seq must be an integer")

    return req


def deserialize_request(line: str) -> BaseRequest:
    data = json.loads(line.strip())
    return make_request(data)
