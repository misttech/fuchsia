# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal

from pydantic import BaseModel, ConfigDict
from shared.protocol.base import BaseRequest

COMMAND_NAME: Final = "console"


class ConsoleResponse(BaseModel):
    """Response containing the console command output."""

    model_config = ConfigDict(extra="forbid")

    output: str


class ConsoleRequest(BaseRequest[ConsoleResponse]):
    """Request execution of a raw zxdb console command."""

    command: Literal["console"] = COMMAND_NAME
    input: str
    thread_id: int | None = None
    frame_index: int = 0
    response_type: ClassVar[Any] = ConsoleResponse
