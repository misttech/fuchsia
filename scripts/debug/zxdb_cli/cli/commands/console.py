# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import sys
from typing import Any

from cli.commands.base import BaseCommand
from shared.protocol.console import COMMAND_NAME


class Command(BaseCommand):
    COMMAND_NAME = COMMAND_NAME
    ALIASES = ["cmd", "console-command", "console_command"]

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        console_parser = subparsers.add_parser(
            Command.COMMAND_NAME,
            aliases=Command.ALIASES,
            help="Execute a raw zxdb console command.",
            description=(
                "Execute a raw zxdb console command, optionally scoped to a "
                "specific thread and stack frame."
            ),
        )
        console_parser.add_argument(
            "-t",
            "--thread-id",
            type=int,
            default=None,
            help="Optional Thread ID to scope the console command",
        )
        console_parser.add_argument(
            "--frame-index",
            type=int,
            default=0,
            help="Frame index within the stacktrace (defaults to 0)",
        )
        console_parser.add_argument(
            "input",
            nargs=argparse.REMAINDER,
            help="Console command to execute",
        )

    @staticmethod
    async def execute(args: argparse.Namespace) -> int | None:
        if isinstance(args.input, list):
            if args.input and args.input[0] == "--":
                args.input = args.input[1:]
            args.input = " ".join(args.input).strip()
        elif isinstance(args.input, str):
            args.input = args.input.strip()
        if not args.input:
            print("Error: console command input is required", file=sys.stderr)
            return 1
        if args.thread_id is None and args.frame_index != 0:
            print(
                "Error: --thread-id is required when specifying a non-zero --frame-index",
                file=sys.stderr,
            )
            return 1
        return None
