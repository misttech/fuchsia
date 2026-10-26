# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal

from pydantic import BaseModel, ConfigDict, Field
from shared.protocol.base import BaseRequest

COMMAND_NAME: Final = "aspace"


class AddressRegionEntry(BaseModel):
    """Information about a mapped VMAR or VMO region in the target process."""

    model_config = ConfigDict(extra="forbid")

    start: str
    end: str
    prot: str
    size: str
    vmo_koid: int | None = None
    vmo_offset: str | None = None
    committed_pages: int | None = None
    name: str
    depth: int = 0


class AspaceResponse(BaseModel):
    """Structured response for the aspace command."""

    model_config = ConfigDict(extra="forbid")

    pid: int | None = None
    regions: list[AddressRegionEntry] = Field(default_factory=list)
    page_size: int | None = None
    total_mapped_bytes: int | None = None
    total_committed_pages: int | None = None
    total_committed_bytes: int | None = None


class AspaceRequest(BaseRequest[AspaceResponse]):
    """Request the virtual address space map for a target process."""

    command: Literal["aspace"] = COMMAND_NAME
    pid: int | None = None
    address: str | None = None
    filter: str | None = None
    response_type: ClassVar[Any] = AspaceResponse
