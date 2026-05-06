# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import dataclasses
import json
from typing import Any


@dataclasses.dataclass(kw_only=True)
class BaseRequest:
    command: str


@dataclasses.dataclass(kw_only=True)
class StartRequest(BaseRequest):
    port: int | None = None
    command: str = "start"


@dataclasses.dataclass(kw_only=True)
class StopRequest(BaseRequest):
    command: str = "stop"


@dataclasses.dataclass(kw_only=True)
class GetStateRequest(BaseRequest):
    command: str = "get-state"


@dataclasses.dataclass(kw_only=True)
class AttachRequest(BaseRequest):
    filter: str | int
    command: str = "attach"


@dataclasses.dataclass(kw_only=True)
class ThreadsRequest(BaseRequest):
    command: str = "threads"


# TODO(https://fxbug.dev/509557630): Implement process-wide continue.
@dataclasses.dataclass(kw_only=True)
class ContinueRequest(BaseRequest):
    thread_id: int
    single_thread: bool | None = None
    command: str = "continue"


# TODO(https://fxbug.dev/509557630): Implement process-wide pause.
@dataclasses.dataclass(kw_only=True)
class PauseRequest(BaseRequest):
    thread_id: int
    command: str = "pause"


@dataclasses.dataclass(kw_only=True)
class StackTraceRequest(BaseRequest):
    thread_id: int
    command: str = "stackTrace"


@dataclasses.dataclass
class ThreadInfo:
    id: int
    name: str


@dataclasses.dataclass
class GetStateResponse:
    threads: list[ThreadInfo]


@dataclasses.dataclass
class Response:
    success: bool
    message: str | None = None
    body: dict[str, Any] | None = None


def serialize(obj: BaseRequest | Response) -> str:
    assert dataclasses.is_dataclass(obj)
    return json.dumps(dataclasses.asdict(obj)) + "\n"


def make_request(data: dict[str, Any]) -> BaseRequest:
    command = data.get("command")
    if command == "start":
        return StartRequest(port=data.get("port"))
    elif command == "stop":
        return StopRequest()
    elif command == "get-state":
        return GetStateRequest()
    elif command == "attach":
        filter = data.get("filter")
        if filter is None:
            raise ValueError("Filter must be specified for attach")
        return AttachRequest(filter=filter)
    elif command == "threads":
        return ThreadsRequest()
    elif command == "pause":
        thread_id = data.get("thread_id")
        if thread_id is None:
            raise ValueError("Thread ID must be specified for pause")
        return PauseRequest(thread_id=thread_id)
    elif command == "continue":
        thread_id = data.get("thread_id")
        if thread_id is None:
            raise ValueError("Thread ID must be specified for continue")
        return ContinueRequest(
            thread_id=thread_id, single_thread=data.get("single_thread")
        )
    elif command == "stackTrace":
        thread_id = data.get("thread_id")
        if thread_id is None:
            raise ValueError("Thread ID must be specified for stackTrace")
        return StackTraceRequest(thread_id=thread_id)
    else:
        raise ValueError("Unknown command")


def deserialize_request(line: str) -> BaseRequest:
    data = json.loads(line.strip())
    return make_request(data)
