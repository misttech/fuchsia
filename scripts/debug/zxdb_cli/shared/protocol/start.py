# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Literal

from shared.protocol.base import BaseRequest


class StartRequest(BaseRequest):
    """Request to start the debugging session."""

    command: Literal["start"] = "start"
    port: int | None = None
    connect: bool = False
