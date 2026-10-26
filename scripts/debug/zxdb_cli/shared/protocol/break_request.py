# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal, Self

from pydantic import model_validator
from shared.protocol.base import BaseRequest

COMMAND_NAME: Final = "break"


class BreakRequest(BaseRequest[dict[str, Any]]):
    """Request to set or delete a source or function breakpoint.

    Exactly one target mode must be specified:
    1. Source breakpoint: both `file` and `line` are set.
    2. Function breakpoint: `function` is set.
    """

    command: Literal["break"] = COMMAND_NAME
    file: str | None = None
    line: int | None = None
    function: str | None = None
    condition: str | None = None
    delete: bool = False
    response_type: ClassVar[Any] = dict[str, Any]

    @model_validator(mode="after")
    def validate_target_mode(self) -> Self:
        """Ensures exactly one breakpoint target mode is specified."""
        has_file = self.file is not None
        has_line = self.line is not None
        if has_file != has_line:
            raise ValueError(
                "Both 'file' and 'line' must be provided together for a source"
                " breakpoint"
            )

        has_source = has_file and has_line
        has_function = self.function is not None
        if sum([has_source, has_function]) != 1:
            raise ValueError(
                "Must specify either ('file' and 'line') or 'function'"
            )
        return self
