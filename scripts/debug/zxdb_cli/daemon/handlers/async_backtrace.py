# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import TYPE_CHECKING

from pydap.client import DapError
from shared.protocol import Response
from shared.protocol.async_backtrace import (
    COMMAND_NAME,
    AsyncBacktraceRequest,
    AsyncBacktraceResponse,
)
from zxdb_dap import AsyncTaskNode, ZxdbAsyncBacktraceArguments

__all__ = ["COMMAND_NAME", "handle"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon


async def handle(
    daemon: Daemon, req: AsyncBacktraceRequest
) -> Response[AsyncBacktraceResponse]:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    try:
        target_proc = None
        if req.pid is not None:
            target_proc = daemon.processes.get(req.pid)
            if not target_proc:
                return Response(
                    success=False,
                    message=f"Process {req.pid} not found",
                )
        else:
            if len(daemon.processes) == 1:
                target_proc = next(iter(daemon.processes.values()))
            elif len(daemon.processes) == 0:
                return Response(
                    success=False,
                    message="No active processes found",
                )
            else:
                return Response(
                    success=False,
                    message="Multiple processes active; please specify a PID with --pid / -p",
                )

        target_pid = target_proc.id
        await daemon.ensure_process_stopped(target_pid)

        target_proc = daemon.processes.get(target_pid)
        if not target_proc:
            return Response(
                success=False,
                message=f"Process {target_pid} not found after stop",
            )

        if not target_proc.threads:
            try:
                threads_resp = await daemon.dap_client.zxdb_threads()
                if threads_resp.body and threads_resp.body.threads:
                    daemon.update_thread_cache(threads_resp.body.threads)
            except Exception:
                pass
            target_proc = daemon.processes.get(target_pid)

        if not target_proc or not target_proc.threads:
            return Response(
                success=True,
                body=AsyncBacktraceResponse(
                    process_id=target_pid,
                    tasks=[],
                ),
            )

        # Note: we assume that any thread that can return async tasks will return the entire global
        # set of tasks.
        #
        # This isn't correct in the general case, but should cover the vast majority of processes
        # and components in practice which will not be running more than one executor in one
        # process. It will either be a single threaded executor or a multithreaded program with some worker
        # threads associated with the executor and possibly other threads that are wholly unrelated
        # to any executor. In the multithreaded executor case, we assume that all threads
        # associated with the executor share the same pool of global tasks, and that any worker
        # thread can access that global state.
        #
        # Based on those assumptions, this loop terminates precisely at the first thread that
        # returns a non-empty list of tasks, which will always correspond to the global tree of
        # tasks that are known to the executor in both single and multithreaded environments.
        tasks: list[AsyncTaskNode] = []
        for thread in list(target_proc.threads.values()):
            try:
                abt_resp = await daemon.dap_client.zxdb_async_backtrace(
                    ZxdbAsyncBacktraceArguments(thread_id=thread.id)
                )
                if (
                    abt_resp.success
                    and abt_resp.body
                    and abt_resp.body.tasks is not None
                ):
                    tasks = abt_resp.body.tasks
                    if tasks:
                        break
            except DapError:
                continue

        return Response(
            success=True,
            body=AsyncBacktraceResponse(
                process_id=target_proc.id,
                tasks=tasks,
            ),
        )
    except Exception as e:
        return Response(
            success=False,
            message=f"Failed to get async backtrace: {e}",
        )
