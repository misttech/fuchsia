# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from cli.commands.base import BaseCommand
from shared.protocol.modules import COMMAND_NAME


class Command(BaseCommand):
    """CLI command definition for `modules`."""

    COMMAND_NAME = COMMAND_NAME
    ALIASES = ["libs", "sym-stat", "sym_stat"]

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        """Registers the `modules` subparser and arguments."""
        mod_parser = subparsers.add_parser(
            Command.COMMAND_NAME,
            aliases=Command.ALIASES,
            help=(
                "List loaded .so modules, base addresses, Build IDs, and DWARF"
                " symbol status"
            ),
        )
        mod_parser.add_argument(
            "-p",
            "--pid",
            default=None,
            type=int,
            help="Process KOID (optional if only one process is attached)",
        )
        mod_parser.add_argument(
            "-f",
            "--filter",
            default=None,
            type=str,
            help=(
                "Optional substring filter on module name, build ID, or symbol"
                " file"
            ),
        )
        mod_parser.add_argument(
            "-r",
            "--reload",
            dest="force_reload",
            action="store_true",
            default=False,
            help=(
                "Force reload symbol information for all libraries in the"
                " process"
            ),
        )
