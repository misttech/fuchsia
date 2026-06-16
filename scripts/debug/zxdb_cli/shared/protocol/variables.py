# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Literal

from shared.protocol.base import BaseRequest


class VariablesRequest(BaseRequest):
    """Request variables for a frame in a thread."""

    command: Literal["variables"] = "variables"
    thread_id: int
    frame_index: int = 0
