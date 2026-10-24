# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, get_args

from cli.commands.base import BaseCommand
from shared.protocol.stack_trace import COMMAND_NAME
from zxdb_dap import UnwinderType


class Command(BaseCommand):
    COMMAND_NAME = COMMAND_NAME
    ALIASES = ["stack_trace", "stackTrace", "bt", "backtrace"]

    @staticmethod
    def register_cli(subparsers: Any) -> None:
        stack_trace_parser = subparsers.add_parser(
            Command.COMMAND_NAME,
            aliases=Command.ALIASES,
            help="Get stack trace of a thread or all threads in a process. This will automatically pause threads when called.",
        )
        group = stack_trace_parser.add_mutually_exclusive_group(required=True)
        group.add_argument(
            "-t",
            "--thread-id",
            type=int,
            default=None,
            help="Thread ID to get stack trace for",
        )
        group.add_argument(
            "-p",
            "--pid",
            type=int,
            default=None,
            help="Process ID to get stack traces for all threads in the process",
        )
        stack_trace_parser.add_argument(
            "-r",
            "--raw",
            action="store_true",
            help="Display raw stack trace without eliding subtle frames",
        )
        stack_trace_parser.add_argument(
            "--force-remote-unwind",
            action="store_true",
            help="Force unwinding to happen on the target device",
        )
        stack_trace_parser.add_argument(
            "--unwinder",
            choices=list(get_args(UnwinderType)),
            default=None,
            help="Force the unwinder to use only the specified implementation",
        )
