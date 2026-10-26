# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from cli.commands.base import BaseCommand
from shared.protocol.attach import COMMAND_NAME


class Command(BaseCommand):
    """CLI command definition for `attach`."""

    COMMAND_NAME = COMMAND_NAME

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        """Registers the `attach` subparser and arguments."""
        attach_parser = subparsers.add_parser(
            Command.COMMAND_NAME, help="Attach to a process"
        )
        attach_parser.add_argument(
            "filter", help="Process name or ID to attach to"
        )
        attach_parser.add_argument(
            "--weak",
            action="store_true",
            default=False,
            help=(
                "Attach weakly (defer symbol loading until an exception occurs;"
                " note that running `modules` transitions a weak attach to a"
                " strong attach)"
            ),
        )
        attach_parser.add_argument(
            "--exact",
            action="store_true",
            default=False,
            help=(
                "Match filter string against exact process name instead of"
                " substring"
            ),
        )
        attach_parser.add_argument(
            "-r",
            "--recursive",
            action="store_true",
            default=False,
            help="Also match child jobs when installing a filter",
        )
        attach_parser.add_argument(
            "--job-only",
            action="store_true",
            default=False,
            help="Only attach to matching job filters",
        )
