# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import sys
from typing import Any

from cli.commands.base import BaseCommand
from shared.protocol.driver_list import COMMAND_NAME


class Command(BaseCommand):
    """CLI command to list running driver_host processes and loaded drivers."""

    COMMAND_NAME = COMMAND_NAME
    ALIASES = ["driver-hosts", "driver_list", "driverList"]

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        """Registers the `driver-list` subcommand and its filter arguments."""
        list_parser = subparsers.add_parser(
            Command.COMMAND_NAME,
            aliases=Command.ALIASES,
            help=(
                "List running driver_host processes, loaded drivers, and bound"
                " device nodes"
            ),
        )
        list_parser.add_argument(
            "filter",
            nargs="?",
            default=None,
            help=(
                "Optional substring filter for driver URL, device moniker, or"
                " host KOID"
            ),
        )
        list_parser.add_argument(
            "-f",
            "--filter",
            dest="filter_flag",
            default=None,
            help=(
                "Optional substring filter for driver URL, device moniker, or"
                " host KOID"
            ),
        )

    @staticmethod
    async def execute(args: argparse.Namespace) -> int | None:
        """Validates and normalizes positional and flag filter arguments."""
        if not hasattr(args, "filter_flag"):
            return None

        if args.filter_flag is not None:
            if args.filter is not None:
                print(
                    "Error: Cannot specify both positional filter and"
                    " --filter.",
                    file=sys.stderr,
                )
                return 1
            args.filter = args.filter_flag

        delattr(args, "filter_flag")
        return None
