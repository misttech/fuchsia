# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import re
from typing import TYPE_CHECKING

from shared.protocol.base import Response
from shared.protocol.handles import (
    COMMAND_NAME,
    HandleEntry,
    HandlesRequest,
    HandlesResponse,
)
from zxdb_dap import ZxdbConsoleCommandArguments

__all__ = [
    "COMMAND_NAME",
    "handle",
    "parse_handle_detail_output",
    "parse_handles_table_output",
]

if TYPE_CHECKING:
    from daemon.daemon import Daemon

_HANDLE_ROW_RE = re.compile(
    r"""
    ^\s*
    # Handle value, or '<none>' for unmapped VMOs.
    (?P<handle_val><none>|0x[0-9a-fA-F]+|\d+)
    \s+
    # Zircon object type name or raw numeric ID.
    (?P<type>ZX_OBJ_TYPE_\S+|<unknown\s+\(\d+\)>|\d+)
    \s+
    # Kernel Object ID (KOID) in hex or decimal.
    (?P<koid>0x[0-9a-fA-F]+|\d+)
    \s*$
    """,
    re.VERBOSE,
)

_DETAIL_KNOWN_KEYS = (
    "Handle",
    "Type",
    "Koid",
    "Rights",
    "Related koid",
    "Peer-owner koid",
    "Name",
    "VMO size in bytes",
    "Parent koid",
    "# children",
    "# mappings",
    "Share count",
    "Flags",
    "Committed bytes",
    "Cache policy",
    "Metadata bytes",
    "Committed change events",
)

# `zxdb` aligns detail key-value columns with at least two spaces between the
# right-aligned key name and its value.
_DETAIL_KEY_VALUE_RE = re.compile(
    r"^\s*(?P<key>"
    + "|".join(re.escape(k) for k in _DETAIL_KNOWN_KEYS)
    + r")\s\s+(?P<val>.*?)\s*$"
)


def _parse_int_auto(val: str) -> int:
    """Parses a hex (`0x...`) or decimal integer string."""
    val = val.strip()
    if val.lower().startswith("0x"):
        return int(val, 16)
    return int(val, 10)


def _try_parse_int(val: str) -> int | None:
    """Attempts to parse an integer string, returning `None` on failure."""
    try:
        return _parse_int_auto(val)
    except ValueError:
        return None


def parse_handles_table_output(text: str) -> list[HandleEntry]:
    """Parses `zxdb` `handle` table output into `HandleEntry` items."""
    entries: list[HandleEntry] = []
    for line in text.splitlines():
        m = _HANDLE_ROW_RE.match(line)
        if not m:
            continue
        entries.append(
            HandleEntry(
                handle_value=m.group("handle_val"),
                type=m.group("type"),
                koid=_parse_int_auto(m.group("koid")),
            )
        )
    return entries


def parse_handle_detail_output(text: str) -> HandleEntry | None:
    """Parses `zxdb` `handle -k <koid>` detail output into a `HandleEntry`."""
    if (
        not text.strip()
        or "No object with koid" in text
        or "No handle with value" in text
    ):
        return None

    handle_value: str = "<none>"
    obj_type: str = "ZX_OBJ_TYPE_NONE"
    koid: int | None = None
    related_koid: int | None = None
    peer_owner_koid: int | None = None
    rights: list[str] = []
    vmo_name: str | None = None
    vmo_size_bytes: int | None = None
    cache_policy: str | None = None
    current_list_key: str | None = None

    for line in text.splitlines():
        stripped = line.strip()
        if not stripped:
            continue

        if m := _DETAIL_KEY_VALUE_RE.match(line):
            matched_key = m.group("key")
            matched_val = m.group("val")
            current_list_key = matched_key
            if matched_key == "Handle":
                handle_value = matched_val
            elif matched_key == "Type":
                obj_type = matched_val
            elif matched_key == "Koid":
                koid = _try_parse_int(matched_val)
            elif matched_key == "Rights":
                if matched_val:
                    rights.append(matched_val)
            elif matched_key == "Related koid":
                related_koid = _try_parse_int(matched_val)
            elif matched_key == "Peer-owner koid":
                peer_owner_koid = _try_parse_int(matched_val)
            elif matched_key == "Name":
                vmo_name = matched_val
            elif matched_key == "VMO size in bytes":
                vmo_size_bytes = _try_parse_int(matched_val)
            elif matched_key == "Cache policy":
                cache_policy = matched_val
        elif current_list_key == "Rights" and stripped.startswith("ZX_RIGHT_"):
            rights.append(stripped)

    if koid is None:
        return None

    return HandleEntry(
        handle_value=handle_value,
        type=obj_type,
        koid=koid,
        related_koid=related_koid,
        peer_owner_koid=peer_owner_koid,
        rights=rights,
        vmo_name=vmo_name,
        vmo_size_bytes=vmo_size_bytes,
        cache_policy=cache_policy,
    )


async def _query_single_koid(
    daemon: Daemon, pid: int | None, koid: int, hex_flag: str
) -> Response[HandlesResponse]:
    """Queries detailed handle metadata for a single KOID."""
    detail_resp = await daemon.dap_client.zxdb_console_command(
        ZxdbConsoleCommandArguments(
            command=daemon.scope_console_command(
                f"handle -k{hex_flag} {koid}", pid
            ),
        )
    )
    if not detail_resp.success or detail_resp.body is None:
        return Response(
            success=False,
            message=detail_resp.message
            or f"Failed to query handle for koid {koid}",
        )

    raw_detail = detail_resp.body.output or ""
    entry = parse_handle_detail_output(raw_detail)
    if entry is None:
        if "No object with koid" in raw_detail:
            return Response(
                success=True,
                body=HandlesResponse(pid=pid, handles=[]),
                message=raw_detail.strip(),
            )
        if raw_detail.strip():
            return Response(success=False, message=raw_detail.strip())

    return Response(
        success=True,
        body=HandlesResponse(
            pid=pid, handles=[entry] if entry is not None else []
        ),
    )


async def _enrich_handles_with_detail(
    daemon: Daemon,
    entries: list[HandleEntry],
    hex_flag: str,
    pid: int | None = None,
) -> list[HandleEntry]:
    """Queries per-handle detail for each entry in `entries`."""
    enriched: list[HandleEntry] = []
    for entry in entries:
        detail_cmd = (
            f"handle{hex_flag} {entry.handle_value}"
            if entry.handle_value != "<none>"
            else f"handle -k{hex_flag} {entry.koid}"
        )
        d_resp = await daemon.dap_client.zxdb_console_command(
            ZxdbConsoleCommandArguments(
                command=daemon.scope_console_command(detail_cmd, pid),
            )
        )
        if d_resp.success and d_resp.body is not None:
            detailed = parse_handle_detail_output(d_resp.body.output or "")
            if detailed is not None:
                enriched.append(detailed)
                continue
        enriched.append(entry)
    return enriched


async def handle(
    daemon: Daemon, req: HandlesRequest
) -> Response[HandlesResponse]:
    """Handles a `handles` request by querying and parsing zxdb `handle` output."""
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    pid, err = daemon.resolve_process_id(req.pid)
    if err is not None:
        return Response(success=False, message=err)

    hex_flag = " -x" if req.hex else ""

    try:
        if req.koid is not None:
            return await _query_single_koid(daemon, pid, req.koid, hex_flag)

        table_resp = await daemon.dap_client.zxdb_console_command(
            ZxdbConsoleCommandArguments(
                command=daemon.scope_console_command(f"handle{hex_flag}", pid),
            )
        )
        if not table_resp.success or table_resp.body is None:
            return Response(
                success=False,
                message=table_resp.message or "Failed to query handles",
            )

        raw_table = table_resp.body.output or ""
        entries = parse_handles_table_output(raw_table)
        if (
            not entries
            and "Handle" not in raw_table
            and "No handles." not in raw_table
            and raw_table.strip()
        ):
            return Response(success=False, message=raw_table.strip())

        if req.type:
            needle = req.type.lower()
            entries = [e for e in entries if needle in e.type.lower()]

        if req.detail:
            entries = await _enrich_handles_with_detail(
                daemon, entries, hex_flag, pid=pid
            )

        return Response(
            success=True,
            body=HandlesResponse(pid=pid, handles=entries),
        )
    except Exception as e:
        return Response(success=False, message=f"Failed to query handles: {e}")
