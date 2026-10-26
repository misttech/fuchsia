# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from cli.commands.base import BaseCommand
from shared.protocol.aspace import COMMAND_NAME


class Command(BaseCommand):
    """CLI command definition for `aspace`."""

    COMMAND_NAME = COMMAND_NAME
    ALIASES = ["as"]

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        """Registers the `aspace` subparser and arguments."""
        as_parser = subparsers.add_parser(
            Command.COMMAND_NAME,
            aliases=Command.ALIASES,
            help=(
                "Show virtual address space mappings (VMARs/VMOs/MMIO) for a"
                " process"
            ),
        )
        as_parser.add_argument(
            "address",
            nargs="?",
            default=None,
            type=str,
            help=(
                "Optional address to filter containing regions (e.g."
                " '0x530b010dc000')"
            ),
        )
        as_parser.add_argument(
            "-p",
            "--pid",
            default=None,
            type=int,
            help="Process KOID (optional if only one process is attached)",
        )
        as_parser.add_argument(
            "-f",
            "--filter",
            default=None,
            type=str,
            help=(
                "Optional substring filter on region name, permissions, or VMO"
                " KOID"
            ),
        )
