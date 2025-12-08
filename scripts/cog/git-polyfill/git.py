# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import abc
import argparse
import subprocess
import sys
from pathlib import Path
from typing import Dict, List, Optional, Tuple, Type


def get_repository_root() -> Optional[Path]:
    """Returns the repository root for the current directory.

    The repository root is the directory that would contain the .git directory.
    """
    cwd = Path.cwd()
    repository_root = None
    for ancestor in [cwd] + list(cwd.parents):
        if (ancestor.parent / ".citc").is_dir():
            repository_root = ancestor
            break
    return repository_root


def get_workspace_id_and_snapshot_version(
    repository_root: Path,
) -> Tuple[str, int]:
    """Returns the workspace id and snapshot version for the given repository root.

    Args:
        repository_root: The repository root to get the workspace id and snapshot version for.

    Returns:
        A tuple of the workspace id and snapshot version.
    """
    citc_dir = repository_root.parent / ".citc"
    try:
        workspace_id = (citc_dir / "workspace_id").read_text().strip()
        snapshot_version = (citc_dir / "snapshot_version").read_text().strip()
    except Exception as e:
        print(f"fatal: could not read citc metadata: {e}", file=sys.stderr)
        return "", 0
    return workspace_id, int(snapshot_version)


def get_relative_git_dir(
    top_level_args: argparse.Namespace, repository_root: Path
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
        path = path.relative_to(repository_root)

    # If the path is equal to the workspace root we want to return None which is the same as the
    # user not specifying a git_dir or -C
    if path == Path("."):
        return None

    return path


def namespace_to_args(
    namespace: argparse.Namespace, ignored_flags: List[str] = []
) -> List[str]:
    """Converts an argparse.Namespace object back into a list of command-line arguments."""
    args: List[str] = []
    for key, value in vars(namespace).items():
        if value is None:
            continue

        flag = ""
        if len(key) == 1:
            # Assume single character keys are for short options (e.g. -C)
            flag = f"-{key}"
        else:
            flag = f"--{key.replace('_', '-')}"

        if flag in ignored_flags:
            continue

        if isinstance(value, bool):
            if value:
                args.append(flag)
        elif isinstance(value, list):
            for item in value:
                args.append(flag)
                args.append(str(item))
        else:
            args.append(flag)
            args.append(str(value))

    return args


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

        repository_root = get_repository_root()
        if not repository_root:
            print("Not in a cog workspace")
            return 1

        workspace_id, snapshot_version = get_workspace_id_and_snapshot_version(
            repository_root
        )
        if not workspace_id or not snapshot_version:
            print("Not in a cog workspace")
            return 1

        # Determine repo_root
        repo_root = "fuchsia"
        relative_git_dir = get_relative_git_dir(top_level_args, repository_root)
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


@register_command("ls-files")
class LsFilesCommand(GitSubCommand):
    def add_arguments(self, parser: argparse.ArgumentParser) -> None:
        parser.add_argument(
            "-z",
            action="store_true",
            help="\\0 line termination on output and do not quote filenames.",
        )
        parser.add_argument(
            "-c",
            "--cached",
            action="store_true",
            help="Show all files cached in Git’s index, i.e. all tracked files.",
        )
        parser.add_argument(
            "-d",
            "--deleted",
            action="store_true",
            help="Show files with an unstaged deletion.",
        )
        parser.add_argument(
            "-o",
            "--others",
            action="store_true",
            help="Show other (i.e. untracked) files in the output.",
        )
        parser.add_argument(
            "-i",
            "--ignored",
            action="store_true",
            help="Show only ignored files in the output.",
        )
        parser.add_argument(
            "-s",
            "--stage",
            action="store_true",
            help="Show staged contents' mode bits, object name and stage number in the output.",
        )
        parser.add_argument(
            "-u",
            "--unmerged",
            action="store_true",
            help="Show information about unmerged files in the output.",
        )
        parser.add_argument(
            "-m",
            "--modified",
            action="store_true",
            help="Show files with an unstaged modification.",
        )
        parser.add_argument(
            "-x",
            "--exclude",
            type=str,
            metavar="<pattern>",
            nargs="*",
            help="Skip untracked files matching pattern.",
        )
        parser.add_argument(
            "--format",
            type=str,
            metavar="<format>",
            help="A string that interpolates %(fieldname) from the result being shown.",
        )
        parser.add_argument(
            "--exclude-standard",
            action="store_true",
            help="Exclude the standard git files.",
        )
        parser.add_argument("file", nargs="*", help="Files to show.")

    def execute(
        self, top_level_args: argparse.Namespace, args: argparse.Namespace
    ) -> int:
        try:
            cmd = [
                top_level_args.real_git,
            ]
            cmd.extend(
                namespace_to_args(
                    top_level_args,
                    ignored_flags=["--real-git", "--invoker-cwd"],
                )
            )
            cmd.append("ls-files")

            # The namespace_to_args function does not know how to handle positional
            # arguments, it will convert all positional arguments to flags and so we need to
            # handle it here.
            cmd.extend(namespace_to_args(args, ignored_flags=["--file"]))

            if args.file:
                cmd.extend(args.file)

            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                cwd=top_level_args.invoker_cwd,
            )

            if result.returncode != 0:
                print(result.stderr, file=sys.stderr)
                return result.returncode

            end = "\0" if args.z else "\n"
            print(result.stdout, end=end)
            return 0
        except Exception as e:
            print(f"fatal: {e}", file=sys.stderr)
            return 10


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
        "--invoker-cwd",
        type=str,
        help="Path that git was invoked from",
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
