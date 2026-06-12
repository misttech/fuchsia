# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from cli.commands.base import BaseCommand


class Command(BaseCommand):
    @staticmethod
    def register_cli(subparsers: Any) -> None:
        attach_parser = subparsers.add_parser(
            "attach", help="Attach to a process"
        )
        attach_parser.add_argument(
            "filter", help="Process name or ID to attach to"
        )
