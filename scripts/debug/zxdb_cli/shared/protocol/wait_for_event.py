# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Literal

from shared.protocol.base import BaseRequest


class WaitForEventRequest(BaseRequest):
    """Request to wait for a debug adapter event."""

    command: Literal["wait-for-event"] = "wait-for-event"
    last_seen_seq: int  # Overridden to be required
    timeout: int | None = None
