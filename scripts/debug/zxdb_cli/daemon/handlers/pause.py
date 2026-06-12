# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from shared.protocol import Response
from shared.protocol.pause import PauseRequest

if TYPE_CHECKING:
    from daemon.daemon import Daemon

COMMAND_NAME = "pause"


async def handle(daemon: Daemon, req: PauseRequest) -> Response:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    try:
        await daemon.ensure_stopped(req.thread_id)
        return Response(success=True)
    except Exception as e:
        return Response(success=False, message=f"Failed to pause: {e}")
