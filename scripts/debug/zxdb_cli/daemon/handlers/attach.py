# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import TYPE_CHECKING, Any

from pydap.models import AttachRequestArguments
from shared.protocol.attach import (
    COMMAND_NAME,
    AttachRequest,
)
from shared.protocol.base import Response

__all__ = ["COMMAND_NAME", "handle"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon


async def handle(daemon: Daemon, req: AttachRequest) -> Response[Any]:
    """Handles an `attach` request by forwarding zxdb attach filters via DAP."""
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    # AttachRequestArguments is a generic DAP model. Zxdb-specific arguments
    # (like "process", "weak", "exact", "recursive", "job_only") must be passed
    # via extra_fields, which are flattened during serialization.
    extra_fields: dict[str, Any] = {"process": req.filter}
    if req.weak:
        extra_fields["weak"] = True
    if req.exact:
        extra_fields["exact"] = True
    if req.recursive:
        extra_fields["recursive"] = True
    if req.job_only:
        extra_fields["job_only"] = True

    attach_args = AttachRequestArguments(
        restart=None, extra_fields=extra_fields
    )

    try:
        resp = await daemon.dap_client.attach(attach_args)
        return Response(success=True, body=resp.dump_dap())
    except Exception as e:
        return Response(success=False, message=f"Failed to attach: {e}")
