# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Daemon handler for listing driver_host processes and bound device nodes."""

from typing import TYPE_CHECKING

from daemon.driver_utils import (
    fetch_driver_hosts,
    matches_driver_host_filter,
)
from shared.protocol.base import Response
from shared.protocol.driver_list import (
    COMMAND_NAME,
    DriverListRequest,
    DriverListResponse,
)

__all__ = ["COMMAND_NAME", "handle"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon


async def handle(
    daemon: Daemon, req: DriverListRequest
) -> Response[DriverListResponse]:
    """Handles a `driver-list` request and optionally filters the results."""
    try:
        hosts = await fetch_driver_hosts(daemon)
        if req.filter:
            needle = req.filter.lower()
            hosts = [h for h in hosts if matches_driver_host_filter(h, needle)]
        return Response(
            success=True,
            body=DriverListResponse(driver_hosts=hosts),
        )
    except Exception as e:
        return Response(
            success=False, message=f"Failed to list driver hosts: {e}"
        )
