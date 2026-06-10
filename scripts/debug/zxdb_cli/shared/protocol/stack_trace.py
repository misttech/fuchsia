# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Literal

from shared.protocol.base import BaseRequest


class StackTraceRequest(BaseRequest):
    """Request stack trace for a thread."""

    command: Literal["stackTrace"] = "stackTrace"
    thread_id: int
