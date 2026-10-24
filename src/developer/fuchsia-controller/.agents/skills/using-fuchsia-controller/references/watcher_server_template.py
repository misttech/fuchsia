#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Host-side FIDL callback / watcher server template using fuchsia-controller."""

import asyncio

import fidl_fuchsia_memorypressure as f_mp
from fuchsia_controller_py import Channel, Context


class MemoryWatcherImpl(f_mp.WatcherServer):
    def __init__(
        self, server_ch: Channel, level_queue: asyncio.Queue[int]
    ) -> None:
        super().__init__(server_ch)
        self._level_queue = level_queue

    def on_level_changed(
        self, request: f_mp.WatcherOnLevelChangedRequest
    ) -> None:
        self._level_queue.put_nowait(request.level)


async def observe_initial_level(ctx: Context) -> int:
    provider = f_mp.ProviderClient(
        ctx.connect_device_proxy(
            "core/memory_pressure_signaler", f_mp.ProviderMarker
        )
    )
    client_ch, server_ch = ctx.channel_create()
    level_queue: asyncio.Queue[int] = asyncio.Queue()
    watcher_server = MemoryWatcherImpl(server_ch, level_queue)
    # Always retain a reference to the background server task!
    server_task = asyncio.create_task(watcher_server.serve())
    try:
        provider.register_watcher(watcher=client_ch.take())
        level = await asyncio.wait_for(level_queue.get(), timeout=10.0)
        return level
    finally:
        server_task.cancel()
