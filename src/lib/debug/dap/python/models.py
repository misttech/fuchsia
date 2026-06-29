# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from enum import Enum
from typing import Any

from pydantic import Field, model_serializer

from .dap_types import DapBaseModel, Scope, StackFrame, Thread, Variable


class MessageType(str, Enum):
    """Defines the types of DAP messages."""

    REQUEST = "request"
    RESPONSE = "response"
    EVENT = "event"


class ProtocolMessage(DapBaseModel):
    """Base class of all requests, responses, and events.

    Attributes:
        seq: Sequence number (strictly increasing).
        type: Message type.
    """

    seq: int
    type: str


class Request(DapBaseModel):
    """A client request.

    Attributes:
        seq: Sequence number (strictly increasing).
        type: Message type.
        command: The command to execute.
        arguments: Object containing arguments for the command.
    """

    seq: int
    type: str
    command: str
    arguments: dict[str, Any] | None = None


class Response(DapBaseModel):
    """Response for a request.

    Attributes:
        seq: Sequence number (strictly increasing).
        type: Message type.
        request_seq: Sequence number of the corresponding request.
        success: Outcome of the request.
        command: The command requested.
        message: Contains the error message if `success` is false.
        body: The body of the response. The detail depends on the command.
    """

    seq: int
    type: str
    request_seq: int = Field(alias="request_seq")
    success: bool
    command: str | None = None
    message: str | None = None
    body: dict[str, Any] | None = None


class Event(DapBaseModel):
    """A server event.

    Attributes:
        seq: Sequence number (strictly increasing).
        type: Message type.
        event: Type of event.
        body: Event-specific information.
    """

    seq: int
    type: str
    event: str
    body: dict[str, Any] | None = None


class InitializeArguments(DapBaseModel):
    """Arguments for `initialize` request.

    Attributes:
        adapter_id: The ID of the debug adapter.
        supports_invalidated_event: Client supports the `invalidated` event.
        supports_run_in_terminal_request: Client supports the `runInTerminal` request.
    """

    adapter_id: str = Field(alias="adapterID")
    supports_invalidated_event: bool | None = None
    supports_run_in_terminal_request: bool | None = None


class DisconnectArguments(DapBaseModel):
    """Arguments for `disconnect` request.

    Attributes:
        terminate_debuggee: Indicates whether the debuggee should be terminated when the debugger is disconnected.
    """

    terminate_debuggee: bool | None = None


class StackTraceResponseBody(DapBaseModel):
    """Body of response to `stackTrace` request."""

    stack_frames: list[StackFrame]
    total_frames: int | None = None


class StackTraceResponse(Response):
    """Response to `stackTrace` request.

    Attributes:
        body: The stack trace response body.
    """

    body: StackTraceResponseBody


class ContinueResponseBody(DapBaseModel):
    """Response to `continue` request.

    Attributes:
        all_threads_continued: Indicates whether all threads were continued.
    """

    all_threads_continued: bool


class ThreadsResponseBody(DapBaseModel):
    """Body of response to `threads` request."""

    threads: list[Thread]


class ThreadsResponse(Response):
    """Response to `threads` request.

    Attributes:
        body: The threads response body.
    """

    body: ThreadsResponseBody


class StackTraceArguments(DapBaseModel):
    """Arguments for `stackTrace` request.

    Attributes:
        thread_id: Retrieve the stacktrace for this thread.
        start_frame: The index of the first frame to return; if omitted frames start at 0.
        levels: The maximum number of frames to return. If levels is not specified or 0, all frames are returned.
    """

    thread_id: int
    start_frame: int | None = None
    levels: int | None = None


class ContinueArguments(DapBaseModel):
    """Arguments for `continue` request.

    Attributes:
        thread_id: Specifies the active thread.
        single_thread: If this flag is true, execution is resumed only for the thread with given `thread_id`.
    """

    thread_id: int
    single_thread: bool | None = None


class PauseArguments(DapBaseModel):
    """Arguments for `pause` request.

    Attributes:
        thread_id: Pause execution for this thread.
    """

    thread_id: int


class LaunchArguments(DapBaseModel):
    """Arguments for `launch` request."""

    process: str
    launch_command: str = Field(default="", alias="launchCommand")


class EvaluateArguments(DapBaseModel):
    """Arguments for `evaluate` request."""

    expression: str
    context: str = Field(default="repl")
    frame_id: int | None = None


class EvaluateResponseBody(DapBaseModel):
    """Body of response to `evaluate` request."""

    # TODO(https://fxbug.dev/529329366): Support `type` and `variablesReference` in the zxdb
    # backend.
    result: str
    type: str | None = None
    variables_reference: int


class EvaluateResponse(Response):
    """Response to `evaluate` request.

    Attributes:
        body: The evaluate response body.
    """

    body: EvaluateResponseBody


class AttachRequestArguments(DapBaseModel):
    """Arguments for `attach` request.

    Attributes:
        restart: Arbitrary data from the previous, restarted session.
        extra_fields: Additional implementation specific attributes.
    """

    restart: Any | None = Field(default=None, alias="__restart")
    extra_fields: dict[str, Any] | None = Field(
        default=None, alias="extra_fields"
    )

    @model_serializer(mode="wrap")
    def _serialize(self, handler: Any) -> dict[str, Any]:
        data = handler(self)
        extra_fields = data.pop("extra_fields", None)
        if extra_fields:
            data.update(extra_fields)
        return data


class ScopesArguments(DapBaseModel):
    """Arguments for `scopes` request.

    Attributes:
        frame_id: Retrieve the scopes for this stack frame.
    """

    frame_id: int


class ScopesResponseBody(DapBaseModel):
    """Body of response to `scopes` request.

    Attributes:
        scopes: The scopes in the frame.
    """

    scopes: list[Scope]


class ScopesResponse(Response):
    """Response to `scopes` request.

    Attributes:
        body: The scopes response body.
    """

    body: ScopesResponseBody


class VariablesArguments(DapBaseModel):
    """Arguments for `variables` request.

    Attributes:
        variables_reference: Retrieve the variables for this reference.
        start: The index of the first variable to return.
        count: The number of variables to return.
    """

    variables_reference: int
    start: int | None = None
    count: int | None = None


class VariablesResponseBody(DapBaseModel):
    """Body of response to `variables` request.

    Attributes:
        variables: The variables.
    """

    variables: list[Variable]


class VariablesResponse(Response):
    """Response to `variables` request.

    Attributes:
        body: The variables response body.
    """

    body: VariablesResponseBody
