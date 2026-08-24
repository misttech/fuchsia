# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Final, Literal

from pydap.dap_types import SteppingGranularity
from shared.protocol.base import BaseRequest

COMMAND_NAME: Final = "step-in"


class StepInRequest(BaseRequest):
    """Request to step into a function call."""

    command: Literal["step-in"] = COMMAND_NAME
    thread_id: int
    single_thread: bool | None = None
    target_id: int | None = None
    granularity: SteppingGranularity | None = None
