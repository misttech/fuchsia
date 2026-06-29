# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
from typing import Any, Protocol

from pydantic import Field, model_validator
from pydap.dap_types import DapBaseModel
from pydap.models import StackTraceArguments, StackTraceResponse


class ZxdbStackTraceArguments(StackTraceArguments):
    """Arguments for zxdb `stackTrace` request.

    Attributes:
        remote_unwind: Force remote unwind on the target.
    """

    remote_unwind: bool | None = None


class ZxdbDetachArguments(DapBaseModel):
    """Arguments for `zxdb.Detach` request.

    Attributes:
        pid: Process ID to detach from.
        detach_all: Whether to detach from all processes.
    """

    pid: int | None = None
    detach_all: bool | None = Field(alias="all", default=None)

    @model_validator(mode="after")
    def validate_exclusive_args(self) -> "ZxdbDetachArguments":
        if self.detach_all and self.pid is not None:
            raise ValueError("Cannot specify pid when detach_all is True")
        if not self.detach_all and self.pid is None:
            raise ValueError("Must specify either pid or detach_all")
        return self


class SupportsSendRequest(Protocol):
    async def _send_request(
        self,
        writer: asyncio.StreamWriter,
        command: str,
        arguments: DapBaseModel | None = None,
        timeout: float = 5.0,
    ) -> dict[str, Any]:
        ...


class ZxdbDapMixin:
    """Mixin for zxdb-specific DAP extensions."""

    async def zxdb_detach(
        self: SupportsSendRequest,
        writer: asyncio.StreamWriter,
        args: ZxdbDetachArguments,
    ) -> dict[str, Any]:
        """Sends a custom zxdb detach request."""
        return await self._send_request(writer, "zxdb.Detach", args)

    async def zxdb_stack_trace(
        self: SupportsSendRequest,
        writer: asyncio.StreamWriter,
        args: ZxdbStackTraceArguments,
    ) -> StackTraceResponse:
        """Sends a custom zxdb stackTrace request."""
        resp = await self._send_request(writer, "stackTrace", args)
        return StackTraceResponse.model_validate(resp)
