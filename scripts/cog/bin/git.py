# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import abc
import argparse
import sys
from typing import Dict, List, Optional, Tuple, Type


class GitSubCommand(abc.ABC):
    """Abstract base class for git subcommands."""

    def add_arguments(self, parser: argparse.ArgumentParser) -> None:
        """Override to add subcommand-specific arguments."""

    @abc.abstractmethod
    def execute(
        self, top_level_args: argparse.Namespace, args: argparse.Namespace
    ) -> int:
        """Executes the command.

        Args:
            top_level_args: The parsed top-level arguments.
            args: The parsed subcommand arguments.

        Returns:
            The exit code (0 for success, non-zero for failure).
        """

    def run(
        self, top_level_args: argparse.Namespace, raw_args: List[str]
    ) -> int:
        """Parses arguments and executes the command."""
        parser = argparse.ArgumentParser(
            prog=f"git {getattr(self, '_command_name')}"
        )
        self.add_arguments(parser)
        args = parser.parse_args(raw_args)
        return self.execute(top_level_args, args)


_COMMANDS: Dict[str, Type[GitSubCommand]] = {}


def register_command(name: str):
    """Decorator to register a GitSubCommand implementation."""

    def decorator(cls: Type[GitSubCommand]):
        _COMMANDS[name] = cls
        cls._command_name = name
        return cls

    return decorator


@register_command("status")
class StatusCommand(GitSubCommand):
    def execute(
        self, top_level_args: argparse.Namespace, args: argparse.Namespace
    ) -> int:
        print(top_level_args)
        print(args)
        print("not implemented yet")
        return 0


def _find_command_name_and_position(
    args: List[str],
) -> Tuple[Optional[str], int]:
    for i, arg in enumerate(args):
        if arg in _COMMANDS:
            return arg, i
    return None, -1


def _create_top_level_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="git", add_help=False)
    parser.add_argument(
        "-C",
        type=str,
        metavar="path",
        help="Run as if git was started in <path>",
    )
    parser.add_argument(
        "--no-optional-locks",
        action="store_true",
        help="Do not perform optional operations that require locks",
    )
    parser.add_argument(
        "--version", action="version", version="git version 2.x (fuchsia-cog)"
    )
    parser.add_argument(
        "--help", action="help", help="show this help message and exit"
    )
    return parser


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: git <command> [<args>]", file=sys.stderr)
        return 1

    provided_args = sys.argv[1:]
    command_name, command_index = _find_command_name_and_position(provided_args)

    if command_name is None:
        # If no registered command is found, we fail.
        # We try to identify what the user intended to run for a better error message.
        print("This command is not yet implemented.", file=sys.stderr)
        return 1

    top_level_parser = _create_top_level_parser()

    top_level_args = provided_args[:command_index]
    remaining_args = provided_args[command_index + 1 :]

    top_level_args = top_level_parser.parse_args(top_level_args)

    command_class = _COMMANDS[command_name]
    command = command_class()
    return command.run(top_level_args, remaining_args)


if __name__ == "__main__":
    sys.exit(main())
