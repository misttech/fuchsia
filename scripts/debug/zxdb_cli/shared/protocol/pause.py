# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Literal

from pydantic import model_validator
from shared.protocol.base import BaseRequest


class PauseRequest(BaseRequest):
    """Request to pause execution of a thread or process."""

    command: Literal["pause"] = "pause"
    thread_id: int | None = None
    pid: int | None = None

    @model_validator(mode="after")
    def validate_target(self) -> "PauseRequest":
        if self.thread_id is None and self.pid is None:
            raise ValueError("Must specify either thread_id or pid")
        if self.thread_id is not None and self.pid is not None:
            raise ValueError("Cannot specify both thread_id and pid")
        return self
