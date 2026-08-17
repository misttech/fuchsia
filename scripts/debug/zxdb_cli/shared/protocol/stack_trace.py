# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Literal

from pydantic import BaseModel, ConfigDict, Field, model_validator
from pydap.dap_types import StackFrame
from shared.protocol.base import BaseRequest


class ThreadStackTraceResponse(BaseModel):
    """Stack trace for a single thread in a process."""

    model_config = ConfigDict(populate_by_name=True)

    id: int
    # NOTE: It is a deliberate choice to depend on the DAP StackFrame definition here
    # to avoid duplicating identical definitions, despite coupling the protocols.
    stack_frames: list[StackFrame] = Field(alias="stackFrames")
    total_frames: int = Field(alias="totalFrames")


class ProcessStackTraceResponse(BaseModel):
    """Response containing stack traces for all threads in a process."""

    model_config = ConfigDict(populate_by_name=True)
    process_id: int = Field(alias="processId", title="ProcessId")
    stacks: list[ThreadStackTraceResponse]


class StackTraceRequest(BaseRequest):
    """Request stack trace for a thread or all threads in a process."""

    command: Literal["stackTrace"] = "stackTrace"
    thread_id: int | None = None
    pid: int | None = None
    raw: bool = False

    @model_validator(mode="after")
    def validate_targets(self) -> "StackTraceRequest":
        if self.thread_id is not None and self.pid is not None:
            raise ValueError("Cannot specify both thread_id and pid")
        if self.thread_id is None and self.pid is None:
            raise ValueError("Must specify either thread_id or pid")
        return self
