# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from cli.commands.base import BaseCommand


class Command(BaseCommand):
    COMMAND_NAME = "wait-for-event"

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        wait_parser = subparsers.add_parser(
            "wait-for-event",
            aliases=["wait_for_event", "waitForEvent"],
            help="Wait for event (default shows all events)",
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
