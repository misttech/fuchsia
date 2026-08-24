# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Final, Literal

from shared.protocol.base import BaseRequest

COMMAND_NAME: Final = "get-state"


class GetStateRequest(BaseRequest):
    """Request current state of threads."""

    command: Literal["get-state"] = COMMAND_NAME
