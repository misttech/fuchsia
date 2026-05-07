# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from dataclasses import asdict, dataclass
from enum import Enum
from typing import Any

from pydap.dap_types import StackFrame, Thread


class MessageType(str, Enum):
    REQUEST = "request"
    RESPONSE = "response"
    EVENT = "event"


def verbatim_factory(items: list[tuple[str, Any]]) -> dict[str, Any]:
    return {k: v for k, v in items if v is not None}


def dataclass_to_dict(obj: Any) -> dict[str, Any]:
    return asdict(obj, dict_factory=verbatim_factory)


def from_dict(cls: type[Any], data: dict[str, Any]) -> Any:
    """Creates a dataclass instance from a dictionary with spec-cased keys."""
    if not hasattr(cls, "__dataclass_fields__"):
        return data
    kwargs = {}
    for field_name, field in cls.__dataclass_fields__.items():
        if field_name in data:
            value = data[field_name]
            origin = getattr(field.type, "__origin__", None)
            args = getattr(field.type, "__args__", None)

            if (
                origin is list
                and args
                and hasattr(args[0], "__dataclass_fields__")
            ):
                kwargs[field_name] = [from_dict(args[0], v) for v in value]
            elif hasattr(field.type, "__dataclass_fields__"):
                kwargs[field_name] = from_dict(field.type, value)
            else:
                kwargs[field_name] = value
    return cls(**kwargs)


@dataclass
class ProtocolMessage:
    """Base class of all requests, responses, and events.

    Attributes:
        seq: Sequence number (strictly increasing).
        type: Message type.
    """

    seq: int
    type: str


@dataclass
class Request:
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


@dataclass
class Response:
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
    request_seq: int
    success: bool
    command: str | None = None
    message: str | None = None
    body: dict[str, Any] | None = None


@dataclass
class Event:
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


@dataclass
class InitializeArguments:
    """Arguments for `initialize` request.

    Attributes:
        adapterID: The ID of the debug adapter.
        supportsInvalidatedEvent: Client supports the `invalidated` event.
        supportsRunInTerminalRequest: Client supports the `runInTerminal` request.
    """

    adapterID: str
    supportsInvalidatedEvent: bool | None = None
    supportsRunInTerminalRequest: bool | None = None


@dataclass
class DisconnectArguments:
    """Arguments for `disconnect` request.

    Attributes:
        terminateDebuggee: Indicates whether the debuggee should be terminated when the debugger is disconnected.
    """

    terminateDebuggee: bool | None = None


@dataclass
class StackTraceResponse:
    """Response to `stackTrace` request.

    Attributes:
        stackFrames: The stack frames of the thread.
    """

    stackFrames: list[StackFrame]


@dataclass
class ContinueResponseBody:
    """Response to `continue` request.

    Attributes:
        allThreadsContinued: Indicates whether all threads were continued.
    """

    allThreadsContinued: bool


@dataclass
class ThreadsResponse:
    """Response to `threads` request.

    Attributes:
        threads: All threads.
    """

    threads: list[Thread]


@dataclass
class StackTraceArguments:
    """Arguments for `stackTrace` request.

    Attributes:
        threadId: Retrieve the stacktrace for this thread.
        startFrame: The index of the first frame to return; if omitted frames start at 0.
        levels: The maximum number of frames to return. If levels is not specified or 0, all frames are returned.
    """

    threadId: int
    startFrame: int | None = None
    levels: int | None = None


@dataclass
class ContinueArguments:
    """Arguments for `continue` request.

    Attributes:
        threadId: Specifies the active thread.
        singleThread: If this flag is true, execution is resumed only for the thread with given `threadId`.
    """

    threadId: int
    singleThread: bool | None = None


@dataclass
class PauseArguments:
    """Arguments for `pause` request.

    Attributes:
        threadId: Pause execution for this thread.
    """

    threadId: int


@dataclass
class AttachRequestArguments:
    """Arguments for `attach` request.

    Attributes:
        _restart: Arbitrary data from the previous, restarted session.
        extra_fields: Additional implementation specific attributes.
    """

    _restart: Any | None = None
    extra_fields: dict[str, Any] | None = None
