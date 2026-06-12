# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from shared.protocol import Response
from shared.protocol.threads import ThreadsRequest

if TYPE_CHECKING:
    from daemon.daemon import Daemon

COMMAND_NAME = "threads"


async def handle(daemon: Daemon, _req: ThreadsRequest) -> Response:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    try:
        resp = await daemon.dap_client.threads(daemon.zxdb_writer)
        body = resp.body.model_dump(by_alias=True) if resp.body else None
        return Response(
            success=True,
            body=body,
        )
    except Exception as e:
        return Response(success=False, message=f"Failed to get threads: {e}")
