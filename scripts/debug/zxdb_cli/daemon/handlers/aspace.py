# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import re
from typing import TYPE_CHECKING, Final

from shared.protocol.aspace import (
    COMMAND_NAME,
    AddressRegionEntry,
    AspaceRequest,
    AspaceResponse,
)
from shared.protocol.base import Response
from zxdb_dap import ZxdbConsoleCommandArguments

__all__ = ["COMMAND_NAME", "handle", "parse_aspace_output"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon

# `zxdb` indents nested VMAR/mapping names by 2 spaces per hierarchy level.
_INDENT_SPACES_PER_DEPTH: Final[int] = 2

_ASPACE_PREFIX_RE = re.compile(
    r"""
    ^\s*
    (?P<start>0x[0-9a-fA-F]+)       # Start virtual address
    \s+
    (?P<end>0x[0-9a-fA-F]+)         # End virtual address
    \s+
    (?P<prot>[r-][w-][x-][sp]?)     # Protection flags (e.g. rwx, rw-, r-xs)
    \s+
    (?P<size>\d+[BKMGT])            # Human-readable region size (e.g. 4K, 2M)
    (?P<rest>.*)$                   # Remaining columns (VMO/Linux columns + name)
    """,
    re.VERBOSE,
)

_VMO_COLS_RE = re.compile(
    r"""
    ^\s*
    (?P<koid>\d+)                   # Backing VMO KOID (Fuchsia aspace output)
    \s+
    (?P<offset>0x[0-9a-fA-F]+)      # Byte offset into VMO
    \s+
    (?P<cmt_pgs>\d+)                # Committed page count
    \s(?P<name_part>.*)$            # Indented VMAR/VMO region name
    """,
    re.VERBOSE,
)

_LINUX_COLS_RE = re.compile(
    r"""
    ^\s*
    (?P<offset>0x[0-9a-fA-F]+)      # File mapping offset (Linux /proc/maps format)
    \s(?P<name_part>.*)$            # Mapped file path or region name
    """,
    re.VERBOSE,
)

_PAGE_SIZE_RE = re.compile(r"^\s*Page size:\s*(?P<page_size>\d+)\s*$")
_TOTAL_MAPPED_RE = re.compile(r"^\s*Total mapped bytes:\s*(?P<mapped>\d+)\s*$")
_TOTAL_COMMITTED_RE = re.compile(
    r"""
    ^\s*Total[ ]committed[ ]pages:\s*
    (?P<pages>\d+)                  # Committed page count
    \s*=\s*
    (?P<bytes>\d+)                  # Committed byte total
    \s+bytes\s*$
    """,
    re.VERBOSE,
)


def _parse_aspace_region_line(
    line: str, name_col_idx: int | None
) -> AddressRegionEntry | None:
    """Parses a single `aspace` table row into an `AddressRegionEntry`."""
    m = _ASPACE_PREFIX_RE.match(line)
    if not m:
        return None

    rest = m.group("rest")
    vmo_koid: int | None = None
    vmo_offset: str | None = None
    committed_pages: int | None = None

    if m_vmo := _VMO_COLS_RE.match(rest):
        vmo_koid = int(m_vmo.group("koid"))
        vmo_offset = m_vmo.group("offset").lower()
        committed_pages = int(m_vmo.group("cmt_pgs"))
        raw_name = m_vmo.group("name_part")
    elif m_linux := _LINUX_COLS_RE.match(rest):
        vmo_offset = m_linux.group("offset").lower()
        raw_name = m_linux.group("name_part")
    else:
        raw_name = rest

    name = raw_name.strip()
    depth = 0
    if name_col_idx is not None and len(line) > name_col_idx and name:
        name_pos = line.rfind(name)
        if name_pos >= name_col_idx:
            depth = max(
                0, (name_pos - name_col_idx) // _INDENT_SPACES_PER_DEPTH
            )

    return AddressRegionEntry(
        start=m.group("start").lower(),
        end=m.group("end").lower(),
        prot=m.group("prot"),
        size=m.group("size"),
        vmo_koid=vmo_koid,
        vmo_offset=vmo_offset,
        committed_pages=committed_pages,
        name=name,
        depth=depth,
    )


def parse_aspace_output(text: str, pid: int | None = None) -> AspaceResponse:
    """Parses `zxdb` `aspace` output into a structured `AspaceResponse`."""
    regions: list[AddressRegionEntry] = []
    page_size: int | None = None
    total_mapped_bytes: int | None = None
    total_committed_pages: int | None = None
    total_committed_bytes: int | None = None
    name_col_idx: int | None = None

    for line in text.splitlines():
        if name_col_idx is None and "Start" in line and "Name" in line:
            name_col_idx = line.find("Name")
            continue

        if m_ps := _PAGE_SIZE_RE.match(line):
            page_size = int(m_ps.group("page_size"))
            continue
        if m_tm := _TOTAL_MAPPED_RE.match(line):
            total_mapped_bytes = int(m_tm.group("mapped"))
            continue
        if m_tc := _TOTAL_COMMITTED_RE.match(line):
            total_committed_pages = int(m_tc.group("pages"))
            total_committed_bytes = int(m_tc.group("bytes"))
            continue

        if region := _parse_aspace_region_line(line, name_col_idx):
            regions.append(region)

    return AspaceResponse(
        pid=pid,
        regions=regions,
        page_size=page_size,
        total_mapped_bytes=total_mapped_bytes,
        total_committed_pages=total_committed_pages,
        total_committed_bytes=total_committed_bytes,
    )


async def handle(
    daemon: Daemon, req: AspaceRequest
) -> Response[AspaceResponse]:
    """Handles an `aspace` request by running `aspace` via `zxdb.ConsoleCommand`."""
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    pid, err = daemon.resolve_process_id(req.pid)
    if err is not None:
        return Response(success=False, message=err)

    aspace_cmd = daemon.scope_console_command(
        f"aspace -- {req.address}" if req.address else "aspace", pid
    )

    try:
        cmd_resp = await daemon.dap_client.zxdb_console_command(
            ZxdbConsoleCommandArguments(
                command=aspace_cmd,
            )
        )
        if not cmd_resp.success or cmd_resp.body is None:
            return Response(
                success=False,
                message=cmd_resp.message or "Failed to query address space",
            )

        raw_text = cmd_resp.body.output or ""
        if "Region not mapped." in raw_text:
            return Response(
                success=True,
                body=AspaceResponse(pid=pid, regions=[]),
                message="Region not mapped.",
            )

        parsed = parse_aspace_output(raw_text, pid=pid)
        if (
            not parsed.regions
            and parsed.page_size is None
            and "Start" not in raw_text
            and raw_text.strip()
        ):
            return Response(success=False, message=raw_text.strip())

        if req.filter:
            needle = req.filter.lower()
            parsed.regions = [
                r
                for r in parsed.regions
                if needle in r.name.lower()
                or needle in r.prot.lower()
                or (r.vmo_koid is not None and needle in str(r.vmo_koid))
            ]

        return Response(success=True, body=parsed)
    except Exception as e:
        return Response(
            success=False, message=f"Failed to query address space: {e}"
        )
