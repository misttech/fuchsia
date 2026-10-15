# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import sys
from typing import Any, get_args

from cli.commands.base import BaseCommand
from shared.protocol.watch_request import COMMAND_NAME, WatchAccessType


class Command(BaseCommand):
    COMMAND_NAME = COMMAND_NAME
    ALIASES = [
        "watchpoint",
        "data-breakpoint",
        "setDataBreakpoints",
        "set-data-breakpoints",
        "set_data_breakpoints",
    ]

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        watch_parser = subparsers.add_parser(
            Command.COMMAND_NAME,
            aliases=Command.ALIASES,
            help="Set or delete a data breakpoint (watchpoint)",
            description=(
                "Set or delete a hardware watchpoint on a variable or expression. "
                "Passing -t/--thread-id and -a/--access-type (and optionally "
                "--frame-index) is required when setting a watchpoint. To watch a "
                "memory address, pass a typed dereference expression "
                "(e.g. '*(uint32_t*)0x1234'). The expression is evaluated once "
                "and the watchpoint stays on that fixed address for the life of "
                "the process; a watchpoint on a local variable keeps firing after "
                "its function returns and the stack slot is reused. Remove it "
                "with 'watch -d <data_id>'; 'get-state' lists each active "
                "watchpoint's data_id and expression."
            ),
        )
        watch_parser.add_argument(
            "-t",
            "--thread-id",
            type=int,
            default=None,
            help="Thread ID whose stack frame and process should be used (required when setting a watchpoint)",
        )
        watch_parser.add_argument(
            "--frame-index",
            type=int,
            default=0,
            help="Frame index within the stacktrace (defaults to 0)",
        )
        watch_parser.add_argument(
            "-a",
            "--access-type",
            choices=list(get_args(WatchAccessType)),
            default=None,
            help="Access type to break on ('write' or 'readWrite'; required when setting a watchpoint)",
        )
        watch_parser.add_argument(
            "-c",
            "--condition",
            type=str,
            default=None,
            help="Optional conditional expression for the watchpoint",
        )
        watch_parser.add_argument(
            "--hit-condition",
            type=str,
            default=None,
            help="Optional hit count condition for the watchpoint",
        )
        watch_parser.add_argument(
            "-d",
            "--delete",
            action="store_true",
            help="Delete the watchpoint matching the given data_id instead of adding it",
        )
        watch_parser.add_argument(
            "--clear",
            action="store_true",
            help="Clear all active watchpoints",
        )
        watch_parser.add_argument(
            "name",
            nargs="*",
            default=[],
            help=(
                "Variable or expression to watch; with -d/--delete, the "
                "data_id of the watchpoint to remove"
            ),
        )

    @staticmethod
    async def execute(args: argparse.Namespace) -> int | None:
        if isinstance(args.name, list):
            args.name = " ".join(args.name).strip()
        if args.clear:
            return None
        if args.delete:
            if not args.name:
                print(
                    "Error: data_id is required when deleting a watchpoint.",
                    file=sys.stderr,
                )
                return 2
            return None
        if not args.name:
            print(
                "Error: expression is required unless --clear is specified.",
                file=sys.stderr,
            )
            return 2
        missing = []
        if args.thread_id is None:
            missing.append("-t/--thread-id")
        if args.access_type is None:
            missing.append("-a/--access-type")
        if missing:
            print(
                f"Error: {' and '.join(missing)} required when setting a watchpoint.",
                file=sys.stderr,
            )
            return 2
        return None
