# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from pydap.models import StepOutArguments
from shared.protocol.base import Response
from shared.protocol.finish import (
    COMMAND_NAME,
    FinishRequest,
)

__all__ = ["COMMAND_NAME", "handle"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon


async def handle(daemon: Daemon, req: FinishRequest) -> Response:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    args = StepOutArguments(
        thread_id=req.thread_id, single_thread=req.single_thread
    )

    try:
        resp = await daemon.dap_client.step_out(args)
        return Response(success=True, body=resp.dump_dap())
    except Exception as e:
        return Response(success=False, message=f"Failed to finish frame: {e}")
