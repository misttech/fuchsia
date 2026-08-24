# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from pydap.models import NextArguments
from shared.protocol.base import Response
from shared.protocol.next_request import (
    COMMAND_NAME,
    NextRequest,
)

__all__ = ["COMMAND_NAME", "handle"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon


async def handle(daemon: Daemon, req: NextRequest) -> Response:
    """Handles a next (step over) command request.

    Args:
        daemon: Daemon instance holding active DAP client session.
        req: NextRequest containing parameters.

    Returns:
        Response object with success status and optional body or error message.
    """
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    args = NextArguments(
        thread_id=req.thread_id,
        single_thread=req.single_thread,
        granularity=req.granularity,
    )

    try:
        resp = await daemon.dap_client.next(args)
        return Response(success=True, body=resp.dump_dap())
    except Exception as e:
        return Response(success=False, message=f"Failed to step over: {e}")
