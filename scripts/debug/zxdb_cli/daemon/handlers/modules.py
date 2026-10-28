# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import re
from typing import TYPE_CHECKING, Final

from shared.protocol.base import Response
from shared.protocol.modules import (
    COMMAND_NAME,
    ModuleEntry,
    ModulesRequest,
    ModulesResponse,
)
from zxdb_dap import ZxdbConsoleCommandArguments

__all__ = [
    "COMMAND_NAME",
    "handle",
    "parse_libs_output",
    "parse_sym_stat_output",
]

if TYPE_CHECKING:
    from daemon.daemon import Daemon

# In `sym-stat`, module names under `Process <N> symbol status` are indented by
# 2 spaces, while per-module key-value attributes are indented by 4 spaces.
_SYM_MODULE_INDENT: Final[str] = "  "
_SYM_ATTR_INDENT: Final[str] = "    "

_LIBS_ROW_RE = re.compile(
    r"""
    ^\s*
    (?P<base>0x[0-9a-fA-F]+)      # Module load address in hex
    \s+
    (?P<name>\S.*?)               # Module or shared library name
    \s*$
    """,
    re.VERBOSE,
)

_PROCESS_SYM_HEADER_RE = re.compile(r"^\s*Process\s+\d+\s+symbol status\s*$")
_SYM_BASE_RE = re.compile(r"^\s*Base:\s*(?P<base>0x[0-9a-fA-F]+)\s*$")
_SYM_BUILD_ID_RE = re.compile(
    r"""
    ^\s*Build[ ]ID:\s*
    (?P<build_id>\S+?)                  # ELF GNU build-id hex string
    (?:\s+\(Downloading\.\.\.\))?       # Optional symbol server download indicator
    \s*$
    """,
    re.VERBOSE,
)
_SYM_LOADED_RE = re.compile(
    r"^\s*Symbols loaded:\s*(?P<loaded>Yes|No)\s*$", re.IGNORECASE
)
_SYM_FILE_RE = re.compile(r"^\s*Symbol file:\s*(?P<symbol_file>.+?)\s*$")
_SYM_FILES_INDEXED_RE = re.compile(
    r"^\s*Source files indexed:\s*(?P<count>\d+)\s*$"
)
_SYM_FUNCS_INDEXED_RE = re.compile(r"^\s*Symbols indexed:\s*(?P<count>\d+)\s*$")


def parse_libs_output(text: str) -> list[ModuleEntry]:
    """Parses `zxdb` `libs` table output into `ModuleEntry` items."""
    entries: list[ModuleEntry] = []
    for line in text.splitlines():
        m = _LIBS_ROW_RE.match(line)
        if m:
            entries.append(
                ModuleEntry(
                    name=m.group("name").strip(),
                    base=m.group("base").lower(),
                )
            )
    return entries


def _build_module_entry(
    name: str, fields: dict[str, str | int | bool]
) -> ModuleEntry:
    """Constructs a `ModuleEntry` from parsed `sym-stat` key-value fields."""
    build_id = fields.get("build_id")
    symbols_loaded = fields.get("symbols_loaded")
    symbol_file = fields.get("symbol_file")
    files_indexed = fields.get("files_indexed")
    functions_indexed = fields.get("functions_indexed")
    return ModuleEntry(
        name=name,
        base=str(fields["base"]),
        build_id=str(build_id) if build_id is not None else None,
        symbols_loaded=(
            bool(symbols_loaded) if symbols_loaded is not None else None
        ),
        symbol_file=str(symbol_file) if symbol_file is not None else None,
        files_indexed=(
            int(files_indexed) if isinstance(files_indexed, int) else None
        ),
        functions_indexed=(
            int(functions_indexed)
            if isinstance(functions_indexed, int)
            else None
        ),
    )


def parse_sym_stat_output(text: str) -> list[ModuleEntry]:
    """Parses the `Process <N> symbol status` section of `sym-stat` output."""
    entries: list[ModuleEntry] = []
    in_process_section = False
    current_name: str | None = None
    current_fields: dict[str, str | int | bool] = {}

    def flush_current() -> None:
        nonlocal current_name, current_fields
        if current_name is not None and "base" in current_fields:
            entries.append(_build_module_entry(current_name, current_fields))
        current_name = None
        current_fields = {}

    for line in text.splitlines():
        if _PROCESS_SYM_HEADER_RE.match(line):
            in_process_section = True
            continue
        if not in_process_section:
            continue
        if 'Use "libs" to refresh' in line or "No known modules." in line:
            flush_current()
            break

        if m_base := _SYM_BASE_RE.match(line):
            current_fields["base"] = m_base.group("base").lower()
            continue
        if m_bid := _SYM_BUILD_ID_RE.match(line):
            current_fields["build_id"] = m_bid.group("build_id")
            continue
        if m_loaded := _SYM_LOADED_RE.match(line):
            current_fields["symbols_loaded"] = (
                m_loaded.group("loaded").lower() == "yes"
            )
            continue
        if m_file := _SYM_FILE_RE.match(line):
            current_fields["symbol_file"] = m_file.group("symbol_file")
            continue
        if m_files := _SYM_FILES_INDEXED_RE.match(line):
            current_fields["files_indexed"] = int(m_files.group("count"))
            continue
        if m_funcs := _SYM_FUNCS_INDEXED_RE.match(line):
            current_fields["functions_indexed"] = int(m_funcs.group("count"))
            continue

        stripped = line.strip()
        if (
            stripped
            and line.startswith(_SYM_MODULE_INDENT)
            and not line.startswith(_SYM_ATTR_INDENT)
        ):
            flush_current()
            current_name = stripped

    flush_current()
    return entries


def _merge_module_entries(
    libs_entries: list[ModuleEntry], sym_entries: list[ModuleEntry]
) -> list[ModuleEntry]:
    """Merges `libs` and `sym-stat` module entries by base address."""
    by_base: dict[str, ModuleEntry] = {e.base.lower(): e for e in libs_entries}
    for sym in sym_entries:
        key = sym.base.lower()
        if key in by_base:
            existing = by_base[key]
            existing.build_id = sym.build_id
            existing.symbols_loaded = sym.symbols_loaded
            existing.symbol_file = sym.symbol_file
            existing.files_indexed = sym.files_indexed
            existing.functions_indexed = sym.functions_indexed
        else:
            by_base[key] = sym

    return sorted(
        by_base.values(),
        key=lambda m: int(m.base, 16) if m.base.startswith("0x") else 0,
    )


async def handle(
    daemon: Daemon, req: ModulesRequest
) -> Response[ModulesResponse]:
    """Handles a `modules` request by combining `libs` and `sym-stat` output."""
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    pid, err = daemon.resolve_process_id(req.pid)
    if err is not None:
        return Response(success=False, message=err)

    libs_cmd = daemon.scope_console_command(
        "libs --reload" if req.force_reload else "libs", pid
    )

    try:
        libs_resp = await daemon.dap_client.zxdb_console_command(
            ZxdbConsoleCommandArguments(
                command=libs_cmd,
            )
        )
        if not libs_resp.success or libs_resp.body is None:
            return Response(
                success=False,
                message=libs_resp.message or "Failed to query modules via libs",
            )

        libs_text = libs_resp.body.output or ""
        libs_entries = parse_libs_output(libs_text)
        if (
            not libs_entries
            and "Load address" not in libs_text
            and libs_text.strip()
        ):
            return Response(success=False, message=libs_text.strip())

        sym_resp = await daemon.dap_client.zxdb_console_command(
            ZxdbConsoleCommandArguments(
                command=daemon.scope_console_command("sym-stat", pid),
            )
        )
        sym_entries = (
            parse_sym_stat_output(sym_resp.body.output or "")
            if sym_resp.success and sym_resp.body is not None
            else []
        )

        modules = _merge_module_entries(libs_entries, sym_entries)
        if req.filter:
            needle = req.filter.lower()
            modules = [
                m
                for m in modules
                if needle in m.name.lower()
                or (m.build_id is not None and needle in m.build_id.lower())
                or (
                    m.symbol_file is not None
                    and needle in m.symbol_file.lower()
                )
            ]

        return Response(
            success=True,
            body=ModulesResponse(pid=pid, modules=modules),
        )
    except Exception as e:
        return Response(success=False, message=f"Failed to query modules: {e}")
