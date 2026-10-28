# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal, Self

from pydantic import model_validator
from shared.protocol.base import BaseRequest

COMMAND_NAME: Final = "attach"


class AttachRequest(BaseRequest[dict[str, Any]]):
    """Request to attach to a process or driver_host."""

    command: Literal["attach"] = COMMAND_NAME
    # Place 'int' first in the Union to avoid Pydantic standard coercion of
    # PIDs to strings.
    filter: int | str | None = None
    driver: str | None = None
    node: str | None = None
    weak: bool = False
    exact: bool = False
    recursive: bool = False
    job_only: bool = False
    wait: bool = False
    response_type: ClassVar[Any] = dict[str, Any]

    @model_validator(mode="after")
    def validate_target_provided(self) -> Self:
        """Validates that exactly one of 'filter', 'driver', or 'node' is specified."""
        provided = [
            x is not None for x in (self.filter, self.driver, self.node)
        ]
        if sum(provided) != 1:
            raise ValueError(
                "Must specify exactly one of 'filter', 'driver', or 'node'"
            )
        if self.exact and (self.driver is not None or self.node is not None):
            raise ValueError("--exact cannot be used with --driver or --node")
        return self
