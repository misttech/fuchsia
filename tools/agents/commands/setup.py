# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Setup subcommand handler for configuring AI coding agent permissions."""

from __future__ import annotations

import argparse
import pathlib

from agents.lib import config


def register_subcommand(
    subparsers: argparse._SubParsersAction[argparse.ArgumentParser],
) -> argparse.ArgumentParser:
    """Register the setup subcommand with the top-level argument parser."""
    parser = subparsers.add_parser(
        "setup",
        help="Configure AI coding agent permissions and profiles.",
        description="Configure AI coding agent permissions and profiles.",
    )
    add_arguments(parser)
    parser.set_defaults(func=run)
    return parser


def add_arguments(parser: argparse.ArgumentParser) -> None:
    """Add arguments for setup command to parser."""
    parser.add_argument(
        "-a",
        "--allow",
        action="append",
        default=[],
        help="Additional grant rule to ALLOW",
    )
    parser.add_argument(
        "-d",
        "--deny",
        action="append",
        default=[],
        help="Additional grant rule to DENY",
    )
    parser.add_argument(
        "-k",
        "--ask",
        action="append",
        default=[],
        help="Additional grant rule to ASK",
    )
    parser.add_argument(
        "--config",
        type=pathlib.Path,
        default=None,
        help="Custom path to output Gemini configuration file (default: ~/.gemini/config/config.json)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print changes without modifying config or services.",
    )


def run(args: argparse.Namespace) -> int:
    """Execute setup with parsed arguments."""
    config_path = args.config or config.DEFAULT_CONFIG_PATH
    success = config.apply_grants(
        config_path=config_path,
        allow=args.allow or [],
        deny=args.deny or [],
        ask=args.ask or [],
        dry_run=args.dry_run,
    )
    return 0 if success else 1
