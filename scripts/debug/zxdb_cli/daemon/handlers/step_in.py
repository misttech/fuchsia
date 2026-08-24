# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from pydap.models import StepInArguments
from shared.protocol import Response
from shared.protocol.step_in import StepInRequest

if TYPE_CHECKING:
    from daemon.daemon import Daemon

COMMAND_NAME = "step-in"


async def handle(daemon: Daemon, req: StepInRequest) -> Response:
    """Handles a step_in (step into) command request.

    Args:
        daemon: Daemon instance holding active DAP client session.
        req: StepInRequest containing parameters.

    Returns:
        Response object with success status and optional body or error message.
    """
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    args = StepInArguments(
        thread_id=req.thread_id,
        single_thread=req.single_thread,
        target_id=req.target_id,
        granularity=req.granularity,
    )

    try:
        resp = await daemon.dap_client.step_in(args)
        return Response(success=True, body=resp.dump_dap())
    except Exception as e:
        return Response(success=False, message=f"Failed to step in: {e}")
