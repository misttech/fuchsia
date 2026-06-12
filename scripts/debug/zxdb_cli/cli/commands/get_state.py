# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from cli.commands.base import BaseCommand


class Command(BaseCommand):
    @staticmethod
    def register_cli(subparsers: Any) -> None:
        subparsers.add_parser("get-state", help="Get state of session")
