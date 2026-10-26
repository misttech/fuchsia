# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal

from pydantic import BaseModel, ConfigDict, Field
from shared.protocol.base import BaseRequest

COMMAND_NAME: Final = "modules"


class ModuleEntry(BaseModel):
    """Information about a loaded module (.so / binary) and symbol status."""

    model_config = ConfigDict(extra="forbid")

    name: str
    base: str
    build_id: str | None = None
    symbols_loaded: bool | None = None
    symbol_file: str | None = None
    files_indexed: int | None = None
    functions_indexed: int | None = None


class ModulesResponse(BaseModel):
    """Structured response for the modules command."""

    model_config = ConfigDict(extra="forbid")

    pid: int | None = None
    modules: list[ModuleEntry] = Field(default_factory=list)


class ModulesRequest(BaseRequest[ModulesResponse]):
    """Request loaded modules and symbol status for a target process."""

    command: Literal["modules"] = COMMAND_NAME
    pid: int | None = None
    filter: str | None = None
    force_reload: bool = False
    response_type: ClassVar[Any] = ModulesResponse
