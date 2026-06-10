# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Literal

from pydantic import model_validator
from shared.protocol.base import BaseRequest


class DetachRequest(BaseRequest):
    """Request to detach from a process."""

    command: Literal["detach"] = "detach"
    pid: int | None = None
    all: bool = False

    @model_validator(mode="after")
    def validate(self) -> "DetachRequest":
        if self.all and self.pid is not None:
            raise ValueError("Cannot specify both PID and all")
        if not self.all and self.pid is None:
            raise ValueError("PID is required when all is not specified")
        return self
