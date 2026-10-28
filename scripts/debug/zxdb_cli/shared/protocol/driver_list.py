# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal

from pydantic import BaseModel, ConfigDict, Field
from shared.protocol.base import BaseRequest

COMMAND_NAME: Final = "driver-list"


class DriverHostEntry(BaseModel):
    """Information about a driver_host process and its loaded drivers."""

    model_config = ConfigDict(extra="forbid")

    host_koid: int
    host_name: str = ""
    drivers: list[str] = Field(default_factory=list)
    devices: list[str] = Field(default_factory=list)
    colocated: bool = False
    attached: bool = False
    transport_hazard: bool = False
    transport_sensitive: list[str] = Field(default_factory=list)


class DriverListResponse(BaseModel):
    """Response for driver-list command."""

    model_config = ConfigDict(extra="forbid")

    driver_hosts: list[DriverHostEntry]


class DriverListRequest(BaseRequest[DriverListResponse]):
    """Request to list running driver_hosts, drivers, and device nodes."""

    command: Literal["driver-list"] = COMMAND_NAME
    filter: str | None = None
    response_type: ClassVar[Any] = DriverListResponse
