# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import asyncio
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Final

from fx_cmd.lib import FxCmd
from shared.protocol import (
    PROTOCOL_VERSION,
    BaseRequest,
    HelloRequest,
    StartRequest,
    deserialize_request,
    make_request,
    serialize,
)

# The maximum number of seconds that we will wait for the daemon to start. In particular, this is
# how long we will wait for the daemon to write into the pipe FD that we pass to it when we start
# the new process.
DAEMON_STARTUP_TIMEOUT_SECS: Final[float] = 10.0
# TODO(https://fxbug.dev/504962182): Replace this with something more appropriate.
UDS_PATH: Final[Path] = Path("/tmp/fx-debug-daemon.sock")


async def main(args: list[str]) -> int:
    # Define argument parser for the CLI.
    # It supports a raw JSON input or specific subcommands.
    parser = argparse.ArgumentParser(description="fx debug cli")
    parser.add_argument("--json", help="JSON request string")
    parser.add_argument(
        "--ack-seq",
        type=int,
        help="Acknowledge events up to this sequence number",
    )
    subparsers = parser.add_subparsers(dest="command", required=False)

    start_parser = subparsers.add_parser("start", help="Start the daemon")
    start_parser.add_argument(
        "--port", type=int, help="Port for DAP server", default=None
    )
    subparsers.add_parser("stop", help="Stop the daemon")
    subparsers.add_parser("get-state", help="Get state of session")
    attach_parser = subparsers.add_parser("attach", help="Attach to a process")
    attach_parser.add_argument("filter", help="Process name or ID to attach to")
    subparsers.add_parser("threads", help="Get list of threads")

    continue_parser = subparsers.add_parser("continue", help="Resume execution")
    continue_parser.add_argument(
        "thread_id", type=int, help="Thread ID to resume"
    )
    continue_parser.add_argument(
        "--single-thread",
        action="store_true",
        default=None,
        help="Resume only the specified thread",
    )

    pause_parser = subparsers.add_parser("pause", help="Interrupt execution")
    pause_parser.add_argument("thread_id", type=int, help="Thread ID to pause")

    stack_trace_parser = subparsers.add_parser(
        "stackTrace",
        help="Get stack trace of a thread. This will automatically pause the given thread when called.",
    )
    stack_trace_parser.add_argument(
        "thread_id", type=int, help="Thread ID to get stack trace for"
    )

    wait_parser = subparsers.add_parser(
        "wait-for-event", help="Wait for event (default shows all events)"
    )
    wait_parser.add_argument(
        "--last-seen-seq",
        type=int,
        default=0,
        help="Last seen sequence number",
    )
    wait_parser.add_argument(
        "--timeout",
        type=int,
        default=10,
        help="Timeout in seconds (default=10 seconds)",
    )

    parsed_args = parser.parse_args(args)

    if parsed_args.json and parsed_args.command:
        print("Error: --json and command are mutually exclusive")
        return 1

    if not parsed_args.json and not parsed_args.command:
        print("Error: Either --json or a command must be provided")
        return 1

    # Process the parsed arguments and dispatch to commands.
    req: BaseRequest | None = None
    if parsed_args.json:
        try:
            req = deserialize_request(parsed_args.json)
            if isinstance(req, StartRequest):
                return await start_daemon(req.port)
        except json.JSONDecodeError as e:
            print(f"Error: Invalid JSON: {e}")
            return 1
        except ValueError as e:
            print(f"Error: {e}")
            return 1
    elif parsed_args.command == "start":
        return await start_daemon(parsed_args.port)
    elif parsed_args.command:
        try:
            args_dict = vars(parsed_args)
            req = make_request(args_dict)
            return await send_command(req)
        except ValueError as e:
            print(f"Error: {e}")
            return 1

    assert req is not None
    assert isinstance(req, BaseRequest)
    return await send_command(req)


async def _try_connect_and_handshake() -> bool | None:
    """Attempts to connect to the UDS and perform handshake.

    Returns:
        True if handshake succeeded.
        False if handshake failed (version mismatch or error response).
        None if connection failed (socket not ready yet).
    """
    try:
        reader, writer = await asyncio.open_unix_connection(UDS_PATH)
    except (ConnectionRefusedError, FileNotFoundError):
        return None
    except Exception as e:
        print(
            json.dumps(
                {
                    "success": False,
                    "message": f"Error connecting to daemon: {e}",
                }
            )
        )
        return False

    try:
        req = HelloRequest(version=PROTOCOL_VERSION)
        writer.write(serialize(req).encode("utf-8"))
        await writer.drain()

        response_line = await reader.readline()
        writer.close()
        await writer.wait_closed()

        if not response_line:
            print(
                json.dumps(
                    {
                        "success": False,
                        "message": "No response received during handshake",
                    }
                )
            )
            return False

        resp_dict = json.loads(response_line.decode("utf-8"))
        if not resp_dict.get("success"):
            print(
                json.dumps(
                    {
                        "success": False,
                        "message": f"Handshake failed: {resp_dict.get('message')}",
                    }
                )
            )
            return False

        body = resp_dict.get("body", {})
        daemon_version = body.get("protocol_version")
        if daemon_version != PROTOCOL_VERSION:
            print(
                json.dumps(
                    {
                        "success": False,
                        "message": f"Protocol version mismatch. CLI: {PROTOCOL_VERSION}, Daemon: {daemon_version}",
                    }
                )
            )
            return False

        return True
    except Exception as e:
        print(
            json.dumps(
                {
                    "success": False,
                    "message": f"Error during handshake: {e}",
                }
            )
        )
        return False


async def start_daemon(port: int | None) -> int:
    """Spawns the daemon process and waits for it to be ready."""
    # Check if a daemon is already running. If the socket file exists, attempt
    # to connect to it to verify if it is active. If the connection is refused,
    # the socket is stale (e.g., from a crash or rapid restart) and can be safely removed.
    if UDS_PATH.exists():
        try:
            reader, writer = await asyncio.open_unix_connection(UDS_PATH)
            writer.close()
            await writer.wait_closed()
            print(
                json.dumps(
                    {
                        "success": False,
                        "message": f"Daemon socket already exists at {UDS_PATH}",
                    }
                )
            )
            return 1
        except (ConnectionRefusedError, FileNotFoundError):
            UDS_PATH.unlink(missing_ok=True)

    fx_cmd = FxCmd()
    args = ["zxdb-daemon"]
    if port is not None:
        args.extend(["--port", str(port)])

    # Create a pipe for synchronization
    read_fd, write_fd = os.pipe()
    os.set_inheritable(write_fd, True)
    args.append(f"--ready-fd={write_fd}")

    command_line = fx_cmd.command_line(*args)

    try:
        # Spawn daemon process in background
        subprocess.Popen(
            command_line,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
            pass_fds=[write_fd],  # Ensure FD is passed to child
        )
        print("Spawning daemon...")

        # Close write end in parent
        os.close(write_fd)

        # Wait for signal on the pipe with timeout
        loop = asyncio.get_running_loop()
        try:
            # Read 1 byte from the pipe
            await asyncio.wait_for(
                loop.run_in_executor(None, os.read, read_fd, 1),
                timeout=DAEMON_STARTUP_TIMEOUT_SECS,
            )
        except asyncio.TimeoutError:
            print(
                json.dumps(
                    {
                        "success": False,
                        "message": "Timed out waiting for daemon to signal readiness.",
                    }
                )
            )
            os.close(read_fd)
            return 1
        except Exception as e:
            print(
                json.dumps(
                    {
                        "success": False,
                        "message": f"Error reading from pipe: {e}",
                    }
                )
            )
            os.close(read_fd)
            return 1
        finally:
            try:
                os.close(read_fd)
            except OSError:
                pass

        # Now that daemon signaled readiness, perform handshake
        result = await _try_connect_and_handshake()
        if result is True:
            print(
                json.dumps(
                    {
                        "success": True,
                        "protocol_version": PROTOCOL_VERSION,
                    }
                )
            )
            return 0
        elif result is None:
            print(
                json.dumps(
                    {
                        "success": False,
                        "message": "Daemon started but failed to respond to handshake in time.",
                    }
                )
            )
            return 1
        else:
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
            print(response_line.decode("utf-8").strip())
        else:
            print("No response received from daemon.")

        if req.command == "stop":
            # Wait for daemon to close connection (EOF)
            try:
                await asyncio.wait_for(reader.read(), timeout=5.0)
            except asyncio.TimeoutError:
                print(
                    "Warning: Timed out waiting for daemon to close connection."
                )

        writer.close()
        await writer.wait_closed()
        return 0
    except Exception as e:
        print(f"Error communicating with daemon: {e}")
        return 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
