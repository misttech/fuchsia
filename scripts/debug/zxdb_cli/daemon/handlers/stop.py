# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from shared.protocol import Response
from shared.protocol.stop import StopRequest

if TYPE_CHECKING:
    from daemon.daemon import Daemon

COMMAND_NAME = "stop"


async def handle(daemon: Daemon, _req: StopRequest) -> Response:
    daemon.stop_event.set()
    return Response(success=True, message="Daemon stopping")
