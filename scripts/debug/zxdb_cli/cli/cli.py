# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import asyncio
import json
import sys
from typing import Final

from daemon_manager.manager import UDS_PATH, DaemonManager, DaemonManagerError
from pydantic import ValidationError
from shared.protocol import (
    BaseRequest,
    StartRequest,
    StopRequest,
    deserialize_request,
    make_request,
    serialize,
)

# The maximum number of seconds that we will wait for the daemon to start. In particular, this is
# how long we will wait for the daemon to write into the pipe FD that we pass to it when we start
# the new process.
DAEMON_STARTUP_TIMEOUT_SECS: Final[float] = 10.0


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
    start_parser.add_argument(
        "--connect",
        action="store_true",
        help="Connect to existing DAP server",
    )
    subparsers.add_parser("stop", help="Stop the daemon")
    subparsers.add_parser("get-state", help="Get state of session")
    detach_parser = subparsers.add_parser(
        "detach", help="Detach from a process"
    )
    detach_parser.add_argument(
        "pid", type=int, nargs="?", help="PID of process to detach from"
    )
    detach_parser.add_argument(
        "--all", action="store_true", help="Detach from all processes"
    )

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
        print(
            "Error: --json and command are mutually exclusive", file=sys.stderr
        )
        return 1

    if not parsed_args.json and not parsed_args.command:
        print(
            "Error: Either --json or a command must be provided",
            file=sys.stderr,
        )
        return 1

    # Process the parsed arguments and dispatch to commands.
    req: BaseRequest | None = None
    if parsed_args.json:
        try:
            req = deserialize_request(parsed_args.json)
            if isinstance(req, StartRequest):
                return await start_daemon(req.port, req.connect)
            elif isinstance(req, StopRequest):
                return await stop_daemon()
        except (ValueError, ValidationError) as e:
            print(f"Error: {e}", file=sys.stderr)
            return 1
    elif parsed_args.command == "start":
        return await start_daemon(parsed_args.port, parsed_args.connect)
    elif parsed_args.command == "stop":
        return await stop_daemon()
    elif parsed_args.command:
        try:
            args_dict = vars(parsed_args)
            req = make_request(args_dict)
        except (ValueError, ValidationError) as e:
            print(f"Error: {e}", file=sys.stderr)
            return 1

    assert req is not None
    # This assertion should theoretically be impossible to trigger because
    # parsed_args.command dispatches to make_request(), which uses Pydantic's
    # TypeAdapter.validate_python() to guarantee that 'req' is a valid subclass
    # of BaseRequest. Actual validation failures are caught and surfaced as
    # ValueError/ValidationError in shared/protocol.py.
    # We preserve this as a defensive runtime check.
    assert isinstance(req, BaseRequest)
    if parsed_args.ack_seq is not None:
        req.ack_seq = parsed_args.ack_seq
    return await send_command(req)


async def start_daemon(
    port: int | None, connect_to_existing: bool = False
) -> int:
    """Spawns the daemon process and waits for it to be ready."""
    manager = DaemonManager(
        socket_path=UDS_PATH,
        port=port,
        connect_to_existing=connect_to_existing,
        startup_timeout=DAEMON_STARTUP_TIMEOUT_SECS,
    )
    try:
        proc = await manager.start()
        if proc is None:
            print(
                json.dumps(
                    {
                        "success": True,
                        "message": "Connected to existing daemon",
                    }
                )
            )
        else:
            print(
                json.dumps(
                    {
                        "success": True,
                        "message": "Daemon started successfully",
                    }
                )
            )
        return 0
    except DaemonManagerError as e:
        print(
            json.dumps(
                {
                    "success": False,
                    "message": str(e),
                }
            ),
            file=sys.stderr,
        )
        return 1
    except Exception as e:
        print(
            json.dumps(
                {
                    "success": False,
                    "message": f"Failed to start daemon: {e}",
                }
            ),
            file=sys.stderr,
        )
        return 1


async def stop_daemon() -> int:
    """Stops the daemon using DaemonManager.

    Note: We do not wait for the process directly because the CLI is a
    short-lived invocation and we do not persist the DaemonManager's process
    handle across commands. Instead, `manager.stop()` gracefully requests the
    daemon stop via UDS, which drains the socket connection before shutdown.
    """
    manager = DaemonManager(socket_path=UDS_PATH)
    try:
        await manager.stop()
        return 0
    except Exception as e:
        print(f"Error stopping daemon: {e}", file=sys.stderr)
        return 1


async def send_command(req: BaseRequest) -> int:
    if not UDS_PATH.exists():
        print(
            f"Daemon socket not found at {UDS_PATH}. Is it running?",
            file=sys.stderr,
        )
        return 1

    try:
        reader, writer = await asyncio.open_unix_connection(UDS_PATH)
    except Exception as e:
        print(f"Error communicating with daemon: {e}", file=sys.stderr)
        return 1

    try:
        writer.write(serialize(req).encode("utf-8"))
        await writer.drain()

        try:
            response_line = await asyncio.wait_for(
                reader.readline(), timeout=5.0
            )
        except asyncio.TimeoutError:
            print(
                "Timed out waiting for response from daemon.", file=sys.stderr
            )
            return 1

        if response_line:
            print(response_line.decode("utf-8").strip())
        else:
            print("No response received from daemon.", file=sys.stderr)
        return 0
    except Exception as e:
        print(f"Error communicating with daemon: {e}", file=sys.stderr)
        return 1
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except Exception:
            pass


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
