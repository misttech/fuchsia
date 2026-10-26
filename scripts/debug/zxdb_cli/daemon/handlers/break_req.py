# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import TYPE_CHECKING, Any

from pydap.dap_types import (
    FunctionBreakpoint,
    Source,
    SourceBreakpoint,
)
from pydap.models import (
    SetBreakpointsArguments,
    SetFunctionBreakpointsArguments,
)
from shared.protocol.base import Response
from shared.protocol.break_request import (
    COMMAND_NAME,
    BreakRequest,
)

__all__ = [
    "COMMAND_NAME",
    "handle",
    "sync_function_breakpoints",
    "sync_source_breakpoints",
]

if TYPE_CHECKING:
    from daemon.daemon import Daemon


async def sync_source_breakpoints(daemon: Daemon, file: str) -> Any:
    """Synchronizes source breakpoints for `file` with zxdb."""
    lines = daemon.active_breakpoints.get(file, set())
    bps: list[SourceBreakpoint] = []

    for line in sorted(lines):
        cond = daemon.breakpoint_conditions.get((file, line))
        bps.append(
            SourceBreakpoint(line=line, condition=cond)
            if cond is not None
            else SourceBreakpoint(line=line)
        )

    args = SetBreakpointsArguments(
        source=Source(path=file),
        breakpoints=bps,
    )
    return await daemon.dap_client.set_breakpoints(args)


async def sync_function_breakpoints(daemon: Daemon) -> Any:
    """Synchronizes function breakpoints with zxdb."""
    fn_bps: list[FunctionBreakpoint] = []

    for fn_name, cond in daemon.active_function_breakpoints.items():
        fn_bps.append(
            FunctionBreakpoint(name=fn_name, condition=cond)
            if cond is not None
            else FunctionBreakpoint(name=fn_name)
        )

    args = SetFunctionBreakpointsArguments(
        breakpoints=fn_bps,
    )
    return await daemon.dap_client.set_function_breakpoints(args)


async def _handle_function_breakpoint(
    daemon: Daemon, req: BreakRequest
) -> Response[Any]:
    """Sets or deletes a symbol breakpoint via `setFunctionBreakpoints`."""
    assert req.function is not None
    prev_fns = dict(daemon.active_function_breakpoints)

    try:
        if req.delete:
            daemon.active_function_breakpoints.pop(req.function, None)
        else:
            daemon.active_function_breakpoints[req.function] = req.condition

        resp = await sync_function_breakpoints(daemon)
        if resp.success:
            return Response(success=True, body=resp.body.dump_dap())

        daemon.active_function_breakpoints = prev_fns
        return Response(success=False, message=resp.message)
    except Exception:
        daemon.active_function_breakpoints = prev_fns
        raise


async def _handle_source_breakpoint(
    daemon: Daemon, req: BreakRequest
) -> Response[Any]:
    """Sets or deletes a source file/line breakpoint via `setBreakpoints`."""
    assert req.file is not None and req.line is not None
    prev_lines = set(daemon.active_breakpoints.get(req.file, set()))
    prev_cond = daemon.breakpoint_conditions.get((req.file, req.line))
    lines = set(prev_lines)

    def restore_previous_state() -> None:
        assert req.file is not None and req.line is not None
        daemon.active_breakpoints[req.file] = prev_lines
        if prev_cond is not None:
            daemon.breakpoint_conditions[(req.file, req.line)] = prev_cond
        else:
            daemon.breakpoint_conditions.pop((req.file, req.line), None)

    try:
        if req.delete:
            lines.discard(req.line)
            daemon.breakpoint_conditions.pop((req.file, req.line), None)
        else:
            lines.add(req.line)
            if req.condition is not None:
                daemon.breakpoint_conditions[
                    (req.file, req.line)
                ] = req.condition
            else:
                daemon.breakpoint_conditions.pop((req.file, req.line), None)

        daemon.active_breakpoints[req.file] = lines
        resp = await sync_source_breakpoints(daemon, req.file)
        if resp.success:
            return Response(success=True, body=resp.body.dump_dap())

        restore_previous_state()
        return Response(success=False, message=resp.message)
    except Exception:
        restore_previous_state()
        raise


async def handle(daemon: Daemon, req: BreakRequest) -> Response[Any]:
    """Dispatches a `break` request to source or function breakpoint handlers."""
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    try:
        if req.function is not None:
            return await _handle_function_breakpoint(daemon, req)
        return await _handle_source_breakpoint(daemon, req)
    except Exception as e:
        return Response(
            success=False, message=f"Failed to set breakpoints: {e}"
        )
