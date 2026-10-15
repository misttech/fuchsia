# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal

from shared.protocol.base import BaseRequest

COMMAND_NAME: Final = "watch"

# zxdb does not support read-only watchpoints, so "read" is intentionally
# excluded from the DAP `DataBreakpointAccessType` values.
WatchAccessType = Literal["write", "readWrite"]


class WatchRequest(BaseRequest[dict[str, Any]]):
    """Request to set or delete a data breakpoint (watchpoint) on a variable,
    expression, or memory address.

    `name` is the variable or expression to watch when setting a watchpoint,
    or the `data_id` of the watchpoint to remove when `delete=True`. Active
    watchpoints, including both their `data_id` and `expression`, are listed
    in `get-state`.

    When setting a watchpoint, `thread_id` and `access_type` are required
    (`frame_index` is optional, defaulting to 0) to identify the target process
    and stack frame for evaluation. If the thread is running, the daemon
    automatically pauses it first.

    To watch a memory address, pass a dereferenced pointer expression
    (e.g., `*(uint32_t*)0x1234`).
    To clear all active watchpoints, set `clear=True`.
    """

    command: Literal["watch"] = COMMAND_NAME
    name: str = ""
    thread_id: int | None = None
    frame_index: int = 0
    access_type: WatchAccessType | None = None
    condition: str | None = None
    hit_condition: str | None = None
    delete: bool = False
    clear: bool = False
    response_type: ClassVar[Any] = dict[str, Any]
