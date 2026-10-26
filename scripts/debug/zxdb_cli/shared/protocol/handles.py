# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal

from pydantic import BaseModel, ConfigDict, Field
from shared.protocol.base import BaseRequest

COMMAND_NAME: Final = "handles"


class HandleEntry(BaseModel):
    """Information about an open Zircon handle or mapped VMO."""

    model_config = ConfigDict(extra="forbid")

    handle_value: str
    type: str
    koid: int
    related_koid: int | None = None
    peer_owner_koid: int | None = None
    rights: list[str] = Field(default_factory=list)
    vmo_name: str | None = None
    vmo_size_bytes: int | None = None
    cache_policy: str | None = None


class HandlesResponse(BaseModel):
    """Structured response for the handles command."""

    model_config = ConfigDict(extra="forbid")

    pid: int | None = None
    handles: list[HandleEntry] = Field(default_factory=list)


class HandlesRequest(BaseRequest[HandlesResponse]):
    """Request the Zircon handle table for a target process."""

    command: Literal["handles"] = COMMAND_NAME
    pid: int | None = None
    type: str | None = None
    koid: int | None = None
    hex: bool = True
    detail: bool = False
    response_type: ClassVar[Any] = HandlesResponse
