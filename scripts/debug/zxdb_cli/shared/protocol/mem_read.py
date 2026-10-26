# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal

from pydantic import BaseModel, ConfigDict, Field
from shared.protocol.base import BaseRequest

COMMAND_NAME: Final = "mem-read"


class MemoryRowEntry(BaseModel):
    """A 16-byte row from a memory dump."""

    model_config = ConfigDict(extra="forbid")

    address: str
    bytes_hex: str
    ascii: str


class MemReadResponse(BaseModel):
    """Structured response for the mem-read command."""

    model_config = ConfigDict(extra="forbid")

    address: str
    size: int
    hex_bytes: str
    u32_le: list[str] = Field(default_factory=list)
    u64_le: list[str] = Field(default_factory=list)
    rows: list[MemoryRowEntry] = Field(default_factory=list)


class MemReadRequest(BaseRequest[MemReadResponse]):
    """Read memory from a target process or stack frame."""

    command: Literal["mem-read"] = COMMAND_NAME
    address: str
    size: int = 64
    pid: int | None = None
    thread_id: int | None = None
    frame_index: int = 0
    response_type: ClassVar[Any] = MemReadResponse
