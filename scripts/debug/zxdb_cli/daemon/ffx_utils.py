# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utilities for running `ffx` commands with machine-readable JSON output."""

import json
from typing import Any

from ffx_cmd.lib import FfxCmd, FfxOutputFormat

__all__ = ["run_ffx_json"]


async def run_ffx_json(*args: str) -> Any:
    """Executes an `ffx --machine json` subcommand and parses its JSON stdout.

    Args:
        *args: Positional arguments forwarded to `ffx`.

    Returns:
        The decoded JSON payload returned by `ffx`.

    Raises:
        RuntimeError: If `ffx` exits with a non-zero return code or empty
            stdout.
    """
    ffx = FfxCmd(output_format=FfxOutputFormat.JSON)
    out = await ffx.sync(*args)
    if out.return_code != 0 or not out.stdout.strip():
        raise RuntimeError(
            out.stderr.strip()
            or f"ffx {' '.join(args)} failed with code {out.return_code}"
        )
    return json.loads(out.stdout)
