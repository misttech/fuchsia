# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import asyncio
import subprocess
import sys
import time
from pathlib import Path
from typing import Final

from fx_cmd.lib import FxCmd
from shared.protocol import BaseRequest, make_request, serialize

# TODO(https://fxbug.dev/504962182): Replace this with something more appropriate.
UDS_PATH: Final[Path] = Path("/tmp/fx-debug-daemon.sock")


async def main(args: list[str]) -> int:
    parser = argparse.ArgumentParser(description="fx debug cli")
    subparsers = parser.add_subparsers(dest="command", required=True)

    start_parser = subparsers.add_parser("start", help="Start the daemon")
    start_parser.add_argument(
        "--port", type=int, help="Port for DAP server", default=None
    )
    subparsers.add_parser("stop", help="Stop the daemon")
    subparsers.add_parser("get-state", help="Get state of session")
    attach_parser = subparsers.add_parser("attach", help="Attach to a process")
    attach_parser.add_argument("filter", help="Process name or ID to attach to")

    parsed_args = parser.parse_args(args)

    if parsed_args.command == "start":
        return await start_daemon(parsed_args.port)
    elif parsed_args.command:
        try:
            # Convert parsed_args to a dict
            args_dict = vars(parsed_args)
            req = make_request(args_dict)
            return await send_command(req)
        except ValueError as e:
            print(f"Error: {e}")
            return 1

    return 0


async def start_daemon(port: int | None) -> int:
    # Check if a daemon is already running. If the socket file exists, attempt
    # to connect to it to verify if it is active. If the connection is refused,
    # the socket is stale (e.g., from a crash or rapid restart) and can be safely removed.
    if UDS_PATH.exists():
        try:
            reader, writer = await asyncio.open_unix_connection(UDS_PATH)
            writer.close()
            await writer.wait_closed()
            print(f"Daemon socket already exists at {UDS_PATH}")
            return 1
        except (ConnectionRefusedError, FileNotFoundError):
            UDS_PATH.unlink(missing_ok=True)

    fx_cmd = FxCmd()
    args = ["zxdb-daemon"]
    if port is not None:
        args.extend(["--port", str(port)])

    command_line = fx_cmd.command_line(*args)

    try:
        # Spawn daemon process in background
        subprocess.Popen(
            command_line,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        print("Spawning daemon...")

        # Wait for socket to appear
        for _ in range(5):
            if UDS_PATH.exists():
                print("Daemon is ready.")
                return 0
            time.sleep(1)

        print("Daemon started but socket not found yet.")
        return 1
    except Exception as e:
        print(f"Failed to start daemon: {e}")
        return 1


async def send_command(req: BaseRequest) -> int:
    if not UDS_PATH.exists():
        print(f"Daemon socket not found at {UDS_PATH}. Is it running?")
        return 1

    try:
        reader, writer = await asyncio.open_unix_connection(UDS_PATH)
        writer.write(serialize(req).encode("utf-8"))
        await writer.drain()

        response_line = await reader.readline()
        if response_line:
            print(f"Response: {response_line.decode('utf-8').strip()}")
        else:
            print("No response received from daemon.")

        writer.close()
        await writer.wait_closed()
        return 0
    except Exception as e:
        print(f"Error communicating with daemon: {e}")
        return 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
