#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Standalone reproduction script template using fuchsia-controller."""

import argparse
import asyncio
import json
import pathlib
import subprocess
from typing import Any

import fidl_fuchsia_buildinfo as f_buildinfo
import fidl_fuchsia_developer_remotecontrol as f_remotecontrol
from fuchsia_controller_py import Context


def create_device_context(target: str | None = None) -> Context:
    """Creates a Fuchsia Controller Context preserving user ffx connectivity config."""
    user_cfg_path = (
        pathlib.Path.home()
        / ".local/share/Fuchsia/ffx/config/.ffx_user_config.json"
    )
    config: dict[str, Any] = (
        json.loads(user_cfg_path.read_text()) if user_cfg_path.exists() else {}
    )
    config.pop("target.default", None)
    if isinstance(config.get("target"), dict):
        config["target"].pop("default", None)
        if not config["target"]:
            config.pop("target", None)
    else:
        config.pop("target", None)

    if not target:
        out = subprocess.check_output(
            ["ffx", "target", "list", "--format", "json"], text=True
        )
        targets = json.loads(out)
        defaults = [t for t in targets if t.get("is_default")]
        chosen = defaults[0] if defaults else (targets[0] if targets else None)
        if not chosen:
            raise RuntimeError("No Fuchsia target found via `ffx target list`")
        target = chosen["nodename"]

    return Context(target=target, config=config)


async def run_reproduction(target: str | None) -> None:
    ctx = create_device_context(target)

    # 1. Connect to Remote Control Service (RCS)
    rcs = f_remotecontrol.RemoteControlClient(
        ctx.connect_remote_control_proxy()
    )
    ident = (await rcs.identify_host()).unwrap()
    print(f"Connected to node: {ident.response.nodename}")

    # 2. Connect directly to a component via moniker and Marker
    build_info = f_buildinfo.ProviderClient(
        ctx.connect_device_proxy("core/build-info", f_buildinfo.ProviderMarker)
    )
    info = await build_info.get_build_info()
    print(
        f"Product: {info.build_info.product_config}, "
        f"Board: {info.build_info.board_config}"
    )


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Fuchsia Controller repro script"
    )
    parser.add_argument(
        "--target", "-t", default=None, help="Target nodename or address"
    )
    args = parser.parse_args()
    asyncio.run(run_reproduction(args.target))


if __name__ == "__main__":
    main()
