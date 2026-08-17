# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import typing
from typing import Annotated

from pydantic import Field, TypeAdapter
from shared.protocol.attach import AttachRequest
from shared.protocol.base import (
    PROTOCOL_VERSION,
    BaseRequest,
    GetStateResponse,
    ProtocolRegistry,
    Response,
    ThreadInfo,
    deserialize_request,
    get_schema,
    make_request,
    serialize,
)
from shared.protocol.break_request import BreakRequest
from shared.protocol.continue_request import ContinueRequest
from shared.protocol.detach import DetachRequest
from shared.protocol.evaluate import EvaluateRequest, EvaluateResponse
from shared.protocol.finish import FinishRequest
from shared.protocol.get_state import GetStateRequest
from shared.protocol.hello import HelloRequest
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

RequestType = Annotated[
    AttachRequest
    | BreakRequest
    | ContinueRequest
    | DetachRequest
    | EvaluateRequest
    | FinishRequest
    | GetStateRequest
    | HelloRequest
    | NextRequest
    | PauseRequest
    | StackTraceRequest
    | StartRequest
    | StepInRequest
    | StopRequest
    | ThreadsRequest
    | VariablesRequest
    | WaitForEventRequest,
    Field(discriminator="command"),
]

__all__ = [
    "BaseRequest",
    "Response",
    "ThreadInfo",
    "GetStateResponse",
    "ProcessStackTraceResponse",
    "ThreadStackTraceResponse",
    "PROTOCOL_VERSION",
    "serialize",
    "make_request",
    "deserialize_request",
    "get_schema",
    "RequestType",
    "AttachRequest",
    "BreakRequest",
    "ContinueRequest",
    "DetachRequest",
    "EvaluateRequest",
    "EvaluateResponse",
    "FinishRequest",
    "GetStateRequest",
    "HelloRequest",
    "NextRequest",
    "PauseRequest",
    "StackTraceRequest",
    "StartRequest",
    "StepInRequest",
    "StopRequest",
    "ThreadsRequest",
    "VariablesRequest",
    "WaitForEventRequest",
]

_request_adapter = TypeAdapter(RequestType)
ProtocolRegistry.request_adapter = _request_adapter
