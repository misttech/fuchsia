# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from cli.commands.base import BaseCommand
from shared.protocol.handles import COMMAND_NAME


class Command(BaseCommand):
    """CLI command definition for `handles`."""

    COMMAND_NAME = COMMAND_NAME
    ALIASES = ["handle"]

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        """Registers the `handles` subparser and arguments."""
        h_parser = subparsers.add_parser(
            Command.COMMAND_NAME,
            aliases=Command.ALIASES,
            help="Show open Zircon handles and mapped VMOs for a process",
        )
        h_parser.add_argument(
            "-p",
            "--pid",
            default=None,
            type=int,
            help="Process KOID (optional if only one process is attached)",
        )
        h_parser.add_argument(
            "--type",
            default=None,
            type=str,
            help=(
                "Filter handles by type substring (e.g. 'interrupt', 'vmo',"
                " 'bti', 'channel', 'port')"
            ),
        )
        h_parser.add_argument(
            "-k",
            "--koid",
            default=None,
            type=int,
            help="Inspect a specific object by KOID",
        )
        h_parser.add_argument(
            "--detail",
            action="store_true",
            default=False,
            help=(
                "Fetch detailed rights, related KOID, and VMO attributes for"
                " matching handles (makes N + 1 sequential LoadInfoHandleTable"
                " IPC calls; combine with --type to limit round-trips)"
            ),
        )
