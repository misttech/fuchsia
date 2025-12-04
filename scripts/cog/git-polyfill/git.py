# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import abc
import argparse
import subprocess
import sys
from pathlib import Path
from typing import Dict, List, Optional, Tuple, Type


def get_workspace_root() -> Optional[Path]:
    cwd = Path.cwd()
    workspace_root = None
    for ancestor in [cwd] + list(cwd.parents):
        if (ancestor / ".citc").is_dir():
            workspace_root = ancestor
            break
    return workspace_root


def get_workspace_id_and_snapshot_version(
    workspace_root: Path,
) -> Tuple[str, int]:
    citc_dir = workspace_root / ".citc"
    try:
        workspace_id = (citc_dir / "workspace_id").read_text().strip()
        snapshot_version = (citc_dir / "snapshot_version").read_text().strip()
    except Exception as e:
        print(f"fatal: could not read citc metadata: {e}", file=sys.stderr)
        return "", 0
    return workspace_id, int(snapshot_version)


def get_relative_git_dir(
    top_level_args: argparse.Namespace, workspace_root: Path
) -> Optional[Path]:
    if top_level_args.git_dir and top_level_args.C:
        raise ValueError("--git_dir and -C cannot be used together")

    path = None
    if top_level_args.git_dir:
        git_dir = Path(top_level_args.git_dir).expanduser()
        if git_dir.name != ".git":
            raise ValueError("git_dir must end in .git")

        # remove the .git suffix
        path = git_dir.parent

    if top_level_args.C:
        path = Path(top_level_args.C).expanduser()

    if path and path.is_absolute():
        path = path.relative_to(workspace_root)

    # If the path is equal to the workspace root we want to return None which is the same as the
    # user not specifying a git_dir or -C
    if path == Path("."):
        return None

    return path


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


@register_command("rev-parse")
class RevParseCommand(GitSubCommand):
    def add_arguments(self, parser: argparse.ArgumentParser) -> None:
        parser.add_argument("rev", help="The revision to parse")

    def execute(
        self, top_level_args: argparse.Namespace, args: argparse.Namespace
    ) -> int:
        # We only support HEAD for now
        if args.rev != "HEAD":
            print("cog workspaces only support 'HEAD' revisions at this time")
            return 1

        workspace_root = get_workspace_root()
        if not workspace_root:
            print("Not in a cog workspace")
            return 1

        workspace_id, snapshot_version = get_workspace_id_and_snapshot_version(
            workspace_root
        )
        if not workspace_id or not snapshot_version:
            print("Not in a cog workspace")
            return 1

        # Determine repo_root
        repo_root = "fuchsia"
        relative_git_dir = get_relative_git_dir(top_level_args, workspace_root)
        if relative_git_dir:
            repo_root = f"fuchsia/{relative_git_dir}"

        request = f'request_base {{ workspace_id: "{workspace_id}" base_snapshot_version: {snapshot_version}}} repo_root: "{repo_root}"'

        try:
            cmd = [
                top_level_args.real_git,
                "citc",
                "api.call",
                "GetDrafts",
                request,
            ]
            result = subprocess.run(cmd, capture_output=True, text=True)

            if result.returncode != 0:
                print(result.stderr, file=sys.stderr)
                return result.returncode

            for line in result.stdout.splitlines():
                if "commit_hash:" in line:
                    parts = line.split('"')
                    if len(parts) >= 2:
                        print(parts[1])
                        return 0

            return 1
        except Exception as e:
            print(f"fatal: {e}", file=sys.stderr)
            return 1


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
        "--real-git",
        type=str,
        help="Path to the real git binary",
        required=True,
    )
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
        "--git-dir",
        type=str,
        metavar="path",
        help="Use <path> as the path to the .git directory",
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
