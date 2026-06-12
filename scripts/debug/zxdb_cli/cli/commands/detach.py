# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from cli.commands.base import BaseCommand


class Command(BaseCommand):
    @staticmethod
    def register_cli(subparsers: Any) -> None:
        detach_parser = subparsers.add_parser(
            "detach", help="Detach from a process"
        )
        detach_parser.add_argument(
            "pid", type=int, nargs="?", help="PID of process to detach from"
        )
        detach_parser.add_argument(
            "--all", action="store_true", help="Detach from all processes"
        )
