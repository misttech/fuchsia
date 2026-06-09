# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Annotated, Any, Literal

from pydantic import BaseModel, ConfigDict, Field, TypeAdapter, model_validator

PROTOCOL_VERSION = 4


class BaseRequest(BaseModel):
    """Base class for all requests, enforcing keyword-only instantiation."""

    model_config = ConfigDict(kw_only=True)

    command: str
    last_seen_seq: int | None = None
    ack_seq: int | None = None


class StartRequest(BaseRequest):
    """Request to start the debugging session."""

    command: Literal["start"] = "start"
    port: int | None = None
    connect: bool = False


class HelloRequest(BaseRequest):
    """Initial handshake request to verify protocol version."""

    command: Literal["hello"] = "hello"
    version: int


class StopRequest(BaseRequest):
    """Request to stop the daemon and session."""

    command: Literal["stop"] = "stop"


class DetachRequest(BaseRequest):
    """Request to detach from a process."""

    command: Literal["detach"] = "detach"
    pid: int | None = None
    all: bool = False

    @model_validator(mode="after")
    def validate(self) -> "DetachRequest":
        if self.all and self.pid is not None:
            raise ValueError("Cannot specify both PID and all")
        if not self.all and self.pid is None:
            raise ValueError("PID is required when all is not specified")
        return self


class GetStateRequest(BaseRequest):
    """Request current state of threads."""

    command: Literal["get-state"] = "get-state"


class WaitForEventRequest(BaseRequest):
    """Request to wait for a debug adapter event."""

    command: Literal["wait-for-event"] = "wait-for-event"
    last_seen_seq: int  # Overridden to be required
    timeout: int | None = None


class AttachRequest(BaseRequest):
    """Request to attach to a process."""

    command: Literal["attach"] = "attach"
    # Place 'int' first in the Union to avoid Pydantic standard coercion of PIDs to strings.
    filter: int | str


class ThreadsRequest(BaseRequest):
    """Request list of threads."""

    command: Literal["threads"] = "threads"


class ContinueRequest(BaseRequest):
    """Request to resume execution of a thread."""

    command: Literal["continue"] = "continue"
    thread_id: int
    single_thread: bool | None = None


class PauseRequest(BaseRequest):
    """Request to pause execution of a thread."""

    command: Literal["pause"] = "pause"
    thread_id: int


class StackTraceRequest(BaseRequest):
    """Request stack trace for a thread."""

    command: Literal["stackTrace"] = "stackTrace"
    thread_id: int


class ThreadInfo(BaseModel):
    """Information about a single thread."""

    id: int
    name: str


class GetStateResponse(BaseModel):
    """Response for get-state command containing thread list and active processes."""

    threads: list[ThreadInfo]
    processes: dict[int, str] | None = None


class Response(BaseModel):
    """Standard response wrapper."""

    success: bool
    message: str | None = None
    body: GetStateResponse | dict[str, Any] | None = None
    events: list[dict[str, Any]] | None = None


# RequestType is a polymorphic union of all request models, using the "command" field
# as a discriminator. This allows Pydantic to automatically select and validate
# the correct subclass during parsing.
RequestType = Annotated[
    StartRequest
    | HelloRequest
    | StopRequest
    | DetachRequest
    | GetStateRequest
    | WaitForEventRequest
    | AttachRequest
    | ThreadsRequest
    | ContinueRequest
    | PauseRequest
    | StackTraceRequest,
    Field(discriminator="command"),
]

# TypeAdapter is used to validate python dicts or JSON payloads against the polymorphic RequestType union.
_request_adapter = TypeAdapter(RequestType)


def serialize(obj: BaseModel) -> str:
    return obj.model_dump_json() + "\n"


def make_request(data: dict[str, Any]) -> BaseRequest:
    return _request_adapter.validate_python(data)


def deserialize_request(line: str) -> BaseRequest:
    return _request_adapter.validate_json(line.strip())


def get_schema() -> dict[str, Any]:
    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "zxdb-cli Protocol Schema",
        "description": "JSON schema for requests and responses in the zxdb-cli UDS protocol",
        "version": PROTOCOL_VERSION,
        "requests": _request_adapter.json_schema(),
        "responses": Response.model_json_schema(),
    }
