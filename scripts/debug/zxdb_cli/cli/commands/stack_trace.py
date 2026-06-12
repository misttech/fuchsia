# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from cli.commands.base import BaseCommand


class Command(BaseCommand):
    @staticmethod
    def register_cli(subparsers: Any) -> None:
        stack_trace_parser = subparsers.add_parser(
            "stackTrace",
            help="Get stack trace of a thread. This will automatically pause the given thread when called.",
        )
        stack_trace_parser.add_argument(
            "thread_id", type=int, help="Thread ID to get stack trace for"
        )
