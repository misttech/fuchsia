# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Request, response, and event message models for the Debug Adapter Protocol."""

from enum import Enum
from typing import Any, Literal

from pydantic import Field, model_serializer

from .dap_types import (
    Breakpoint,
    DapBaseModel,
    DataBreakpoint,
    DataBreakpointAccessType,
    FunctionBreakpoint,
    Scope,
    Source,
    SourceBreakpoint,
    StackFrame,
    SteppingGranularity,
    Thread,
    Variable,
)


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


class ThreadEventBody(DapBaseModel):
    """Body of the standard thread event.

    Attributes:
        reason: The reason for the event.
        thread_id: The identifier of the thread.
    """

    reason: Literal["started", "exited"] | str
    thread_id: int


class ThreadEvent(Event):
    """Standard thread event.

    Attributes:
        type: Message type ('event').
        event: Event name ('thread').
        body: Thread event payload.
    """

    type: Literal["event"] = "event"
    event: Literal["thread"] = "thread"
    body: ThreadEventBody


class InitializeArguments(DapBaseModel):
    """Arguments for `initialize` request.

    Attributes:
        adapter_id: The ID of the debug adapter.
        supports_invalidated_event: Client supports the `invalidated` event.
        supports_run_in_terminal_request: Client supports the `runInTerminal`
            request.
    """

    adapter_id: str = Field(alias="adapterID")
    supports_invalidated_event: bool | None = None
    supports_run_in_terminal_request: bool | None = None


class DisconnectArguments(DapBaseModel):
    """Arguments for `disconnect` request.

    Attributes:
        terminate_debuggee: Indicates whether the debuggee should be terminated
            when the debugger is disconnected.
    """

    terminate_debuggee: bool | None = None


class StackTraceResponseBody(DapBaseModel):
    """Body of response to `stackTrace` request.

    Attributes:
        stack_frames: The frames of the stackframe. If the array has length
            zero, there are no stackframes available.
        total_frames: The total number of frames available in the stack.
    """

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

    According to the DAP specification semantics, `allThreadsContinued` is
    optional and defaults to True when omitted by a server. This client-side
    fallback value has no effect on the DAP server's choices or behavior when
    generating responses.

    Attributes:
        all_threads_continued: Indicates whether all threads were continued.
    """

    all_threads_continued: bool = True


class ContinueResponse(Response):
    """Response to `continue` request.

    Attributes:
        body: The continue response body.
    """

    body: ContinueResponseBody = Field(default_factory=ContinueResponseBody)


class ThreadsResponseBody(DapBaseModel):
    """Body of response to `threads` request.

    Attributes:
        threads: All threads.
    """

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
        start_frame: The index of the first frame to return; if omitted frames
            start at 0.
        levels: The maximum number of frames to return. If levels is not
            specified or 0, all frames are returned.
    """

    thread_id: int
    start_frame: int | None = None
    levels: int | None = None


class ContinueArguments(DapBaseModel):
    """Arguments for `continue` request.

    Attributes:
        thread_id: Specifies the active thread.
        single_thread: If this flag is true, execution is resumed only for the
            thread with given `thread_id`.
    """

    thread_id: int
    single_thread: bool | None = None


class PauseArguments(DapBaseModel):
    """Arguments for `pause` request.

    Attributes:
        thread_id: Pause execution for this thread.
    """

    thread_id: int


class StepOutArguments(DapBaseModel):
    """Arguments for `stepOut` request.

    Attributes:
        thread_id: Specifies the thread for which to step out.
        single_thread: If this flag is true, execution is resumed only for the
            thread with given `thread_id`.
        granularity: Stepping granularity ('statement' | 'line' |
            'instruction').
    """

    thread_id: int
    single_thread: bool | None = None
    granularity: SteppingGranularity | None = None


class NextArguments(DapBaseModel):
    """Arguments for `next` request.

    Attributes:
        thread_id: Specifies the thread for which to step over.
        single_thread: If this flag is true, execution is resumed only for the
            thread with given `thread_id`.
        granularity: Stepping granularity ('statement' | 'line' |
            'instruction').
    """

    thread_id: int
    single_thread: bool | None = None
    granularity: SteppingGranularity | None = None


class StepInArguments(DapBaseModel):
    """Arguments for `stepIn` request.

    Attributes:
        thread_id: Specifies the thread for which to step in.
        single_thread: If this flag is true, execution is resumed only for the
            thread with given `thread_id`.
        target_id: Id of the target frame to step into.
        granularity: Stepping granularity ('statement' | 'line' |
            'instruction').
    """

    thread_id: int
    single_thread: bool | None = None
    target_id: int | None = None
    granularity: SteppingGranularity | None = None


class LaunchArguments(DapBaseModel):
    """Arguments for `launch` request.

    Attributes:
        process: Target process name or URL to launch.
        launch_command: Optional shell command used to launch the target.
    """

    process: str
    launch_command: str = Field(default="", alias="launchCommand")


class EvaluateArguments(DapBaseModel):
    """Arguments for `evaluate` request.

    Attributes:
        expression: The expression or zxdb console verb to evaluate.
        context: The context in which the evaluate request is run (e.g. 'repl').
        frame_id: Evaluate the expression in the scope of this stack frame.
    """

    expression: str
    context: str = Field(default="repl")
    frame_id: int | None = None


class EvaluateResponseBody(DapBaseModel):
    """Body of response to `evaluate` request.

    Attributes:
        result: The string representation of the evaluation result.
        type: The optional type of the evaluate result.
        variables_reference: If greater than 0, the evaluate result is
            structured and its children can be retrieved.
    """

    # TODO(https://fxbug.dev/529329366): Support `type` and `variablesReference`
    # in the zxdb backend.
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
        """Flattens `extra_fields` into the top-level serialized payload."""
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


class SetBreakpointsArguments(DapBaseModel):
    """Arguments for `setBreakpoints` request.

    Attributes:
        source: The source location of the breakpoints.
        breakpoints: The code locations of the breakpoints.
        source_modified: Deprecated, client should not use this.
    """

    source: Source
    breakpoints: list[SourceBreakpoint] | None = None
    source_modified: bool | None = None


class SetBreakpointsResponseBody(DapBaseModel):
    """Body of response to `setBreakpoints` request.

    Attributes:
        breakpoints: Information about the breakpoints.
    """

    breakpoints: list[Breakpoint]


class SetBreakpointsResponse(Response):
    """Response to `setBreakpoints` request.

    Attributes:
        body: The setBreakpoints response body.
    """

    body: SetBreakpointsResponseBody


class DataBreakpointInfoArguments(DapBaseModel):
    """Arguments for `dataBreakpointInfo` request.

    Attributes:
        name: The name of the variable's child to obtain data breakpoint
            information for. If `variables_reference` isn't specified, this can
            be an expression, or an address if `as_address` is also True.
        variables_reference: Reference to the variable container if the data
            breakpoint is requested for a child of the container.
        frame_id: When `name` is an expression, evaluate it in the scope of
            this stack frame.
        bytes_count: If specified, return information for the range of memory
            extending `bytes_count` number of bytes from the address or
            variable. Serialized as `bytes` on the wire.
        as_address: If True, `name` is a memory address.
        mode: The mode of the desired breakpoint.
    """

    name: str
    variables_reference: int | None = None
    frame_id: int | None = None
    bytes_count: int | None = Field(default=None, alias="bytes")
    as_address: bool | None = None
    mode: str | None = None


class DataBreakpointInfoResponseBody(DapBaseModel):
    """Body of response to `dataBreakpointInfo` request.

    Attributes:
        data_id: An identifier for the data on which a data breakpoint can be
            registered with the `setDataBreakpoints` request, or None if no
            data breakpoint is available.
        description: UI string that describes on what data the breakpoint is
            set on or why a data breakpoint is not available.
        access_types: Attribute lists the available access types for a potential
            data breakpoint.
        can_persist: Attribute indicates that a potential data breakpoint could
            be persisted across sessions.
    """

    data_id: str | None
    description: str
    access_types: list[DataBreakpointAccessType] | None = None
    can_persist: bool | None = None


class DataBreakpointInfoResponse(Response):
    """Response to `dataBreakpointInfo` request.

    Attributes:
        body: The dataBreakpointInfo response body.
    """

    body: DataBreakpointInfoResponseBody


class SetDataBreakpointsArguments(DapBaseModel):
    """Arguments for `setDataBreakpoints` request.

    Attributes:
        breakpoints: The contents of this array replaces all existing data
            breakpoints. An empty array clears all data breakpoints.
    """

    breakpoints: list[DataBreakpoint]


class SetDataBreakpointsResponseBody(DapBaseModel):
    """Body of response to `setDataBreakpoints` request.

    Attributes:
        breakpoints: Information about the data breakpoints.
    """

    breakpoints: list[Breakpoint]


class SetDataBreakpointsResponse(Response):
    """Response to `setDataBreakpoints` request.

    Attributes:
        body: The setDataBreakpoints response body.
    """

    body: SetDataBreakpointsResponseBody


class SetFunctionBreakpointsArguments(DapBaseModel):
    """Arguments for `setFunctionBreakpoints` request.

    Attributes:
        breakpoints: The function names of the breakpoints.
    """

    breakpoints: list[FunctionBreakpoint]


class SetFunctionBreakpointsResponseBody(DapBaseModel):
    """Body of response to `setFunctionBreakpoints` request.

    Attributes:
        breakpoints: Information about the breakpoints.
    """

    breakpoints: list[Breakpoint]


class SetFunctionBreakpointsResponse(Response):
    """Response to `setFunctionBreakpoints` request.

    Attributes:
        body: The setFunctionBreakpoints response body.
    """

    body: SetFunctionBreakpointsResponseBody
