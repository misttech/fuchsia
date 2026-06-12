# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from shared.protocol import Response
from shared.protocol.detach import DetachRequest
from zxdb_dap import ZxdbDetachArguments

if TYPE_CHECKING:
    from daemon.daemon import Daemon

COMMAND_NAME = "detach"


async def handle(daemon: Daemon, req: DetachRequest) -> Response:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    try:
        if req.all:
            args = ZxdbDetachArguments(all=True)
        else:
            args = ZxdbDetachArguments(pid=req.pid)
        resp = await daemon.dap_client.zxdb_detach(daemon.zxdb_writer, args)
        if not resp.get("success"):
            return Response(
                success=False,
                message=resp.get("message", "Failed to detach from process"),
            )
        if req.all:
            daemon.active_processes.clear()
        elif req.pid is not None and req.pid in daemon.active_processes:
            del daemon.active_processes[req.pid]

        # Synthesize and enqueue detached event
        await daemon.event_queue.put(
            {"event": "detached", "body": {"pid": req.pid, "all": req.all}}
        )
        return Response(success=True)
    except Exception as e:
        return Response(success=False, message=f"Failed to detach: {e}")
