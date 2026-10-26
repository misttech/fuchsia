# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import sys
from typing import Any

from cli.commands.base import BaseCommand
from shared.protocol.mem_read import COMMAND_NAME


class Command(BaseCommand):
    """CLI command definition for `mem-read`."""

    COMMAND_NAME = COMMAND_NAME
    ALIASES = ["mem_read", "memRead", "x"]

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        """Registers the `mem-read` subparser and arguments."""
        mr_parser = subparsers.add_parser(
            Command.COMMAND_NAME,
            aliases=Command.ALIASES,
            help=(
                "Read memory from a process or stack frame as hex, u32, and u64"
                " arrays"
            ),
        )
        mr_parser.add_argument(
            "address",
            type=str,
            help=(
                "Address or expression to read from (e.g. '0x7ffee1234000' or"
                " '&dev->regs')"
            ),
        )
        mr_parser.add_argument(
            "-s",
            "--size",
            default=64,
            type=int,
            help="Number of bytes to read (default: 64)",
        )
        mr_parser.add_argument(
            "-p",
            "--pid",
            default=None,
            type=int,
            help="Process KOID (optional if only one process is attached)",
        )
        mr_parser.add_argument(
            "-t",
            "--thread-id",
            default=None,
            type=int,
            help=(
                "Optional thread ID when evaluating a frame-relative expression"
            ),
        )
        mr_parser.add_argument(
            "--frame-index",
            default=0,
            type=int,
            help=(
                "Optional stack frame index when --thread-id is specified"
                " (default: 0)"
            ),
        )

    @staticmethod
    async def execute(args: argparse.Namespace) -> int | None:
        """Validates `mem-read` CLI arguments before sending to the daemon."""
        if args.thread_id is None and args.frame_index != 0:
            print(
                "Error: --thread-id is required when specifying a non-zero"
                " --frame-index",
                file=sys.stderr,
            )
            return 1
        return None
