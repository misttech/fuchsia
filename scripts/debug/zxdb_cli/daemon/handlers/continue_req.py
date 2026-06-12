# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from pydap.models import ContinueArguments
from shared.protocol import Response
from shared.protocol.continue_request import ContinueRequest

if TYPE_CHECKING:
    from daemon.daemon import Daemon

COMMAND_NAME = "continue"


async def handle(daemon: Daemon, req: ContinueRequest) -> Response:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    args = ContinueArguments(
        threadId=req.thread_id, singleThread=req.single_thread
    )

    try:
        resp = await daemon.dap_client.continue_thread(daemon.zxdb_writer, args)
        return Response(success=True, body=resp.dump_dap())
    except Exception as e:
        return Response(success=False, message=f"Failed to continue: {e}")
