# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import re
from typing import TYPE_CHECKING, Final

from pydap.models import StackTraceArguments
from shared.protocol.base import Response
from shared.protocol.mem_read import (
    COMMAND_NAME,
    MemoryRowEntry,
    MemReadRequest,
    MemReadResponse,
)
from zxdb_dap import ZxdbConsoleCommandArguments

__all__ = ["COMMAND_NAME", "handle", "parse_mem_read_output"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon

_BITS_PER_BYTE: Final[int] = 8
_U32_BYTE_WIDTH: Final[int] = 4
_U64_BYTE_WIDTH: Final[int] = 8

_MEM_ROW_RE = re.compile(
    r"""
    ^\s*
    # Row base address followed by ':'.
    (?P<addr>0x[0-9a-fA-F]+):
    \s+
    # Space- or hyphen-separated byte tokens ('??' = unmapped).
    (?P<hex_part>(?:(?:[0-9a-fA-F]{2}|\?\?)(?:[\s-]|$))+)
    """,
    re.VERBOSE,
)

_HEX_TOKEN_RE = re.compile(
    r"""
    [0-9a-fA-F]{2}    # Valid two-nybble hex byte
    |
    \?\?              # Unmapped/unreadable byte placeholder
    """,
    re.VERBOSE,
)


def _pack_little_endian_words(
    byte_vals: list[int | None], word_bytes: int
) -> list[str]:
    """Packs contiguous mapped bytes into little-endian hex words."""
    hex_width = word_bytes * 2
    words: list[str] = []

    for i in range(0, len(byte_vals) - (word_bytes - 1), word_bytes):
        chunk = byte_vals[i : i + word_bytes]
        if any(b is None for b in chunk):
            continue

        word_val = 0
        for idx, b in enumerate(chunk):
            assert b is not None
            word_val |= b << (_BITS_PER_BYTE * idx)
        words.append(f"0x{word_val:0{hex_width}x}")

    return words


def parse_mem_read_output(
    text: str, requested_address: str, requested_size: int
) -> MemReadResponse:
    """Parses `zxdb` `mem-read` output into a structured `MemReadResponse`."""
    rows: list[MemoryRowEntry] = []
    all_byte_vals: list[int | None] = []
    all_hex_parts: list[str] = []
    resolved_address = requested_address

    for line in text.splitlines():
        if "|" in line:
            left_part, ascii_part = line.split("|", 1)
        else:
            left_part, ascii_part = line, ""

        m = _MEM_ROW_RE.match(left_part)
        if not m:
            continue

        row_addr = m.group("addr").lower()
        if not rows:
            resolved_address = row_addr

        tokens = _HEX_TOKEN_RE.findall(m.group("hex_part"))
        if not tokens:
            continue

        row_hex = "".join(t.lower() for t in tokens)
        all_hex_parts.append(row_hex)
        for tok in tokens:
            if tok == "??":
                all_byte_vals.append(None)
            else:
                all_byte_vals.append(int(tok, 16))

        rows.append(
            MemoryRowEntry(
                address=row_addr,
                bytes_hex=row_hex,
                ascii=ascii_part.rstrip("\r\n"),
            )
        )

    u32_le = _pack_little_endian_words(
        all_byte_vals, word_bytes=_U32_BYTE_WIDTH
    )
    u64_le = _pack_little_endian_words(
        all_byte_vals, word_bytes=_U64_BYTE_WIDTH
    )

    return MemReadResponse(
        address=resolved_address,
        size=len(all_byte_vals) if all_byte_vals else requested_size,
        hex_bytes="".join(all_hex_parts),
        u32_le=u32_le,
        u64_le=u64_le,
        rows=rows,
    )


async def handle(
    daemon: Daemon, req: MemReadRequest
) -> Response[MemReadResponse]:
    """Handles a `mem-read` request by evaluating `mem-read` in the zxdb REPL."""
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    if req.size <= 0:
        return Response(
            success=False, message="Memory read size must be a positive integer"
        )

    if req.thread_id is None and req.frame_index != 0:
        return Response(
            success=False,
            message=(
                "thread_id is required when specifying a non-zero frame_index"
            ),
        )

    pid: int | None = None
    if req.pid is not None or req.thread_id is None:
        pid, err = daemon.resolve_process_id(req.pid)
        if err is not None:
            return Response(success=False, message=err)

    if req.thread_id is not None and pid is not None:
        thread = daemon.threads.get(req.thread_id)
        if thread is not None and thread.process is not None:
            if thread.process.id != pid:
                return Response(
                    success=False,
                    message=(
                        f"Thread {req.thread_id} does not belong to process"
                        f" {pid}"
                    ),
                )

    try:
        frame_id: int | None = None
        if req.thread_id is not None:
            await daemon.ensure_stopped(req.thread_id)

            stack_resp = await daemon.dap_client.stack_trace(
                StackTraceArguments(thread_id=req.thread_id)
            )
            if not stack_resp.body or not stack_resp.body.stack_frames:
                return Response(success=False, message="No stack frames found")

            frames = stack_resp.body.stack_frames
            if req.frame_index < 0 or req.frame_index >= len(frames):
                return Response(
                    success=False,
                    message=(
                        f"Frame index {req.frame_index} out of range (stack has"
                        f" {len(frames)} frames)"
                    ),
                )
            frame_id = frames[req.frame_index].id

        base_cmd = f"mem-read --size={req.size} -- {req.address}"
        mem_cmd = (
            base_cmd
            if frame_id is not None
            else daemon.scope_console_command(base_cmd, pid)
        )
        cmd_resp = await daemon.dap_client.zxdb_console_command(
            ZxdbConsoleCommandArguments(
                command=mem_cmd,
                frame_id=frame_id,
            )
        )
        if not cmd_resp.success or cmd_resp.body is None:
            return Response(
                success=False,
                message=cmd_resp.message or "Failed to read memory",
            )

        raw_text = cmd_resp.body.output or ""
        parsed = parse_mem_read_output(raw_text, req.address, req.size)
        if not parsed.rows and raw_text.strip():
            return Response(
                success=False,
                message=raw_text.strip(),
            )

        return Response(success=True, body=parsed)
    except Exception as e:
        return Response(success=False, message=f"Failed to read memory: {e}")
