# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import sys
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
            Command.COMMAND_NAME,
            help="Attach to a process or driver_host",
        )
        attach_parser.add_argument(
            "filter",
            nargs="?",
            default=None,
            help="Process name or ID to attach to",
        )
        attach_parser.add_argument(
            "-d",
            "--driver",
            default=None,
            help=(
                "Driver component/module name or URL substring (e.g. 'xhci' or"
                " 'xhci.cm')"
            ),
        )
        attach_parser.add_argument(
            "-n",
            "--node",
            default=None,
            help="Device topology node moniker (e.g. 'dev.sys.platform.xhci')",
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
                " substring (cannot be used with --driver or --node)"
            ),
        )
        attach_parser.add_argument(
            "-r",
            "--recursive",
            action="store_true",
            default=False,
            help=(
                "Also match child jobs when installing a filter"
                " (with --driver/--node, applies only to --wait filter)"
            ),
        )
        attach_parser.add_argument(
            "--job-only",
            action="store_true",
            default=False,
            help=(
                "Only attach to matching job filters (with --driver/--node,"
                " applies only to --wait filter)"
            ),
        )
        attach_parser.add_argument(
            "--wait",
            action="store_true",
            default=False,
            help=(
                "When using --driver or --node, install a deferred"
                " 'driver_host' process filter if no matching host is currently"
                " running"
            ),
        )

    @staticmethod
    async def execute(args: argparse.Namespace) -> int | None:
        """Validates that exactly one target selector argument is provided."""
        provided = [
            getattr(args, field, None) is not None
            for field in ("filter", "driver", "node")
        ]
        if sum(provided) != 1:
            print(
                "Error: Must specify exactly one of positional <filter>,"
                " --driver, or --node.",
                file=sys.stderr,
            )
            return 1
        if args.exact and (args.driver is not None or args.node is not None):
            print(
                "Error: --exact cannot be used with --driver or --node.",
                file=sys.stderr,
            )
            return 1
        return None
