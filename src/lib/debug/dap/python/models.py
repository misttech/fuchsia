# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from dataclasses import asdict, dataclass
from enum import Enum
from typing import Any, Dict, List, Optional, Tuple, Type

from pydap.dap_types import StackFrame, Thread


class MessageType(str, Enum):
    REQUEST = "request"
    RESPONSE = "response"
    EVENT = "event"


def to_camel(snake_str: str) -> str:
    components = snake_str.split("_")
    return components[0] + "".join(x.title() for x in components[1:])


def camel_case_factory(items: List[Tuple[str, Any]]) -> Dict[str, Any]:
    return {to_camel(k): v for k, v in items if v is not None}


def dataclass_to_dict(obj: Any) -> Dict[str, Any]:
    return asdict(obj, dict_factory=camel_case_factory)


def from_dict(cls: Type[Any], data: Dict[str, Any]) -> Any:
    """Creates a dataclass instance from a dictionary with camelCase keys."""
    if not hasattr(cls, "__dataclass_fields__"):
        return data
    kwargs = {}
    for field_name, field in cls.__dataclass_fields__.items():
        camel_name = to_camel(field_name)
        if camel_name in data:
            value = data[camel_name]
            origin = getattr(field.type, "__origin__", None)
            args = getattr(field.type, "__args__", None)

            if (
                origin in (list, List)
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
    arguments: Optional[Dict[str, Any]] = None


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
    command: Optional[str] = None
    message: Optional[str] = None
    body: Optional[Dict[str, Any]] = None


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
    body: Optional[Dict[str, Any]] = None


@dataclass
class InitializeArguments:
    """Arguments for `initialize` request.

    Attributes:
        adapter_id: The ID of the debug adapter.
        supports_invalidated_event: Client supports the `invalidated` event.
        supports_run_in_terminal_request: Client supports the `runInTerminal` request.
    """

    adapter_id: str
    supports_invalidated_event: Optional[bool] = None
    supports_run_in_terminal_request: Optional[bool] = None


@dataclass
class DisconnectArguments:
    """Arguments for `disconnect` request.

    Attributes:
        terminate_debuggee: Indicates whether the debuggee should be terminated when the debugger is disconnected.
    """

    terminate_debuggee: Optional[bool] = None


@dataclass
class StackTraceResponse:
    """Response to `stackTrace` request.

    Attributes:
        stack_frames: The stack frames of the thread.
    """

    stack_frames: List[StackFrame]


@dataclass
class ContinueResponseBody:
    """Response to `continue` request.

    Attributes:
        all_threads_continued: Indicates whether all threads were continued.
    """

    all_threads_continued: bool


@dataclass
class ThreadsResponse:
    """Response to `threads` request.

    Attributes:
        threads: All threads.
    """

    threads: List[Thread]


@dataclass
class StackTraceArguments:
    """Arguments for `stackTrace` request.

    Attributes:
        thread_id: Retrieve the stacktrace for this thread.
        start_frame: The index of the first frame to return; if omitted frames start at 0.
        levels: The maximum number of frames to return. If levels is not specified or 0, all frames are returned.
    """

    thread_id: int
    start_frame: Optional[int] = None
    levels: Optional[int] = None


@dataclass
class ContinueArguments:
    """Arguments for `continue` request.

    Attributes:
        thread_id: Specifies the active thread.
        single_thread: If this flag is true, execution is resumed only for the thread with given `threadId`.
    """

    thread_id: int
    single_thread: Optional[bool] = None


@dataclass
class PauseArguments:
    """Arguments for `pause` request.

    Attributes:
        thread_id: Pause execution for this thread.
    """

    thread_id: int
