# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from cli.commands.base import BaseCommand


class Command(BaseCommand):
    @staticmethod
    def register_cli(subparsers: Any) -> None:
        continue_parser = subparsers.add_parser(
            "continue", help="Resume execution"
        )
        continue_parser.add_argument(
            "thread_id", type=int, help="Thread ID to resume"
        )
        continue_parser.add_argument(
            "--single-thread",
            action="store_true",
            default=None,
            help="Resume only the specified thread",
        )
