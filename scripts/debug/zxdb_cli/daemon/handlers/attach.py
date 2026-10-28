# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import TYPE_CHECKING, Any

from daemon.driver_utils import (
    build_driver_host_warnings,
    fetch_driver_hosts,
    match_driver_host,
)
from pydap.models import AttachRequestArguments
from shared.protocol.attach import (
    COMMAND_NAME,
    AttachRequest,
)
from shared.protocol.base import Response
from shared.protocol.driver_list import DriverHostEntry

__all__ = ["COMMAND_NAME", "handle"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon


async def _attach_to_matched_driver_host(
    daemon: Daemon,
    matched_host: DriverHostEntry,
    matched_driver: str | None,
    *,
    weak: bool,
) -> Response[dict[str, Any]]:
    """Attaches zxdb to `matched_host` and builds the driver attach response.

    Note:
        `matched_driver` is the matched driver URL when attaching via
        `--driver`, or the host's first (primary) driver URL when attaching via
        `--node` or PID.
    """
    extra_fields: dict[str, Any] = {
        "process": str(matched_host.host_koid),
    }
    if weak:
        extra_fields["weak"] = True

    try:
        await daemon.dap_client.attach(
            AttachRequestArguments(restart=None, extra_fields=extra_fields)
        )
        daemon.get_or_create_process(
            matched_host.host_koid, name=matched_host.host_name
        )
        hazard_warning, colocated_warning = build_driver_host_warnings(
            matched_host
        )
        return Response(
            success=True,
            body={
                "driver_host_koid": matched_host.host_koid,
                "driver_host_name": matched_host.host_name,
                "matched_driver": matched_driver,
                "colocated_drivers": matched_host.drivers,
                "devices": matched_host.devices,
                "attach_mode": "weak" if weak else "strong",
                "transport_hazard": matched_host.transport_hazard,
                "hazard_warning": hazard_warning,
                "colocated_warning": colocated_warning,
                "filter_pattern": None,
            },
        )
    except Exception as e:
        return Response(
            success=False,
            message=(
                f"Failed to attach to driver_host {matched_host.host_koid}: {e}"
            ),
        )


async def _install_driver_attach_filter(
    daemon: Daemon, req: AttachRequest
) -> Response[dict[str, Any]]:
    """Installs a deferred process filter in zxdb when `--wait` is requested."""
    filter_pattern = "driver_host"
    extra_fields: dict[str, Any] = {"process": filter_pattern}
    if req.weak:
        extra_fields["weak"] = True
    if req.recursive:
        extra_fields["recursive"] = True
    if req.job_only:
        extra_fields["job_only"] = True

    requested_target = req.driver or req.node
    filter_warning = (
        f"zxdb cannot match driver or device node '{requested_target}' "
        f"directly to a driver_host process; installed filter '{filter_pattern}' "
        "will attach to every newly started driver_host. Recommend using --weak "
        "to avoid pausing threads unexpectedly."
    )

    try:
        await daemon.dap_client.attach(
            AttachRequestArguments(restart=None, extra_fields=extra_fields)
        )
        return Response(
            success=True,
            body={
                "driver_host_koid": None,
                "driver_host_name": None,
                "matched_driver": None,
                "colocated_drivers": [],
                "devices": [],
                "attach_mode": "filter",
                "transport_hazard": False,
                "hazard_warning": None,
                "colocated_warning": None,
                "filter_pattern": filter_pattern,
                "requested_target": requested_target,
                "filter_warning": filter_warning,
            },
        )
    except Exception as e:
        return Response(
            success=False,
            message=(
                f"Failed to install attach filter '{filter_pattern}': {e}"
            ),
        )


async def _handle_driver_or_node_attach(
    daemon: Daemon, req: AttachRequest
) -> Response[dict[str, Any]]:
    """Resolves a `--driver` or `--node` attach request via `ffx driver`."""
    if req.exact:
        return Response(
            success=False,
            message="--exact cannot be used with --driver or --node",
        )

    hosts: list[DriverHostEntry] = []
    ffx_error: str | None = None
    try:
        hosts = await fetch_driver_hosts(daemon)
    except Exception as e:
        ffx_error = str(e)

    matched_host, matched_driver = match_driver_host(
        hosts, driver=req.driver, node=req.node
    )

    if matched_host is not None:
        return await _attach_to_matched_driver_host(
            daemon, matched_host, matched_driver, weak=req.weak
        )

    if req.wait:
        return await _install_driver_attach_filter(daemon, req)

    target_desc = req.driver or req.node or ""
    detail = f" (ffx error: {ffx_error})" if ffx_error else ""
    return Response(
        success=False,
        message=(
            f"No running driver_host matched '{target_desc}'{detail}. "
            "Use --wait to install a filter for future driver starts."
        ),
    )


async def handle(daemon: Daemon, req: AttachRequest) -> Response[Any]:
    """Handles an `attach` request by forwarding zxdb attach filters via DAP."""
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    if req.driver is not None or req.node is not None:
        return await _handle_driver_or_node_attach(daemon, req)

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
