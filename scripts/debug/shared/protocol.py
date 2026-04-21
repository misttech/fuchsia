# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import dataclasses
import json
from typing import Any


@dataclasses.dataclass
class BaseRequest:
    command: str


@dataclasses.dataclass
class StopRequest(BaseRequest):
    command: str = "stop"


@dataclasses.dataclass
class GetStateRequest(BaseRequest):
    command: str = "get-state"


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
    if command == "stop":
        return StopRequest()
    elif command == "get-state":
        return GetStateRequest()
    else:
        raise ValueError("Unknown command")


def deserialize_request(line: str) -> BaseRequest:
    data = json.loads(line.strip())
    return make_request(data)
