# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Literal

from shared.protocol.base import BaseRequest


class ContinueRequest(BaseRequest):
    """Request to resume execution of a thread."""

    command: Literal["continue"] = "continue"
    thread_id: int
    single_thread: bool | None = None
