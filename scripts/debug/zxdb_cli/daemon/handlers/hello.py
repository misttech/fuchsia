# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from shared.protocol import PROTOCOL_VERSION, Response
from shared.protocol.hello import HelloRequest

if TYPE_CHECKING:
    from daemon.daemon import Daemon

COMMAND_NAME = "hello"


async def handle(daemon: Daemon, req: HelloRequest) -> Response:
    """Handles the hello handshake request.

    Verifies the protocol version.
    """
    if req.version != PROTOCOL_VERSION:
        return Response(
            success=False,
            message=(
                f"Protocol version mismatch. CLI version: {req.version}, "
                f"Daemon version: {PROTOCOL_VERSION}"
            ),
        )

    return Response(success=True, body={"protocol_version": PROTOCOL_VERSION})
