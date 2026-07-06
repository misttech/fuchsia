# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from pydap.dap_types import Source, SourceBreakpoint
from pydap.models import SetBreakpointsArguments
from shared.protocol import Response
from shared.protocol.break_request import BreakRequest

if TYPE_CHECKING:
    from daemon.daemon import Daemon

COMMAND_NAME = "break"


async def handle(daemon: Daemon, req: BreakRequest) -> Response:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    # TODO(https://fxbug.dev/530560621): Use a per-file lock wrapped around the
    # breakpoint set to prevent race conditions during concurrent updates.
    lines = set(daemon.active_breakpoints.get(req.file, set()))

    if req.delete:
        if req.line in lines:
            lines.remove(req.line)
    else:
        lines.add(req.line)

    try:
        args = SetBreakpointsArguments(
            source=Source(path=req.file),
            breakpoints=[SourceBreakpoint(line=line) for line in sorted(lines)],
        )
        resp = await daemon.dap_client.set_breakpoints(daemon.zxdb_writer, args)
        if resp.success:
            daemon.active_breakpoints[req.file] = lines
            return Response(success=True, body=resp.body.dump_dap())
        else:
            return Response(success=False, message=resp.message)
    except Exception as e:
        return Response(
            success=False, message=f"Failed to set breakpoints: {e}"
        )
