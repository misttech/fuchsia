# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import abc
import argparse
import shlex
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, List, Optional, Tuple, Type


class Context:
    """Holds the context for the git polyfill execution."""

    def __init__(self, args: "ArgsCollection"):
        self.args = args
        self.git_subcommand_args: Optional[argparse.Namespace] = None

        # Polyfill arguments
        polyfill_parser = _create_polyfill_parser()
        self.polyfill_options = polyfill_parser.parse_args(args.polyfill_args)

        self.real_git: str = self.polyfill_options.real_git
        self.invoker_cwd: str = self.polyfill_options.invoker_cwd
        self.log_file: Optional[str] = self.polyfill_options.log_file

        self._log_file_path = Path(self.log_file) if self.log_file else None

        # Global git arguments
        global_git_parser = _create_global_git_args_parser()
        self.global_git_options, _ = global_git_parser.parse_known_args(
            args.global_git_args
        )

    def write_log(self, level: str, message: str):
        if self._log_file_path:
            with self._log_file_path.open("a") as f:
                f.write(f"[{level}] {message}\n")

    def print(self, message: str, file=sys.stdout):
        """Prints a message and logs it."""
        print(message, file=file)
        self.write_log("PRINT", message)

    def error(self, message: str):
        """Prints an error message to stderr and logs it."""
        print(message, file=sys.stderr)
        self.write_log("ERROR", message)

    def fatal(self, message: str):
        """Prints a fatal error message to stderr and logs it."""
        print(f"fatal: {message}", file=sys.stderr)
        self.write_log("FATAL", message)

    def log_info(self, message: str):
        """Logs an info message to the log."""
        self.write_log("INFO", message)

    def output(self, message: str, end: str = ""):
        """Prints output to stdout and logs it."""
        sys.stdout.write(message)
        if end:
            sys.stdout.write(end)
        self.write_log("OUTPUT", message.strip())


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
    context: Context,
    repository_root: Path,
) -> Tuple[str, int]:
    """Returns the workspace id and snapshot version for the given repository root.

    Args:
        context: The execution context.
        repository_root: The repository root to get the workspace id and snapshot version for.

    Returns:
        A tuple of the workspace id and snapshot version.
    """
    citc_dir = repository_root.parent / ".citc"
    try:
        workspace_id = (citc_dir / "workspace_id").read_text().strip()
        snapshot_version = (citc_dir / "snapshot_version").read_text().strip()
    except Exception as e:
        context.fatal(f"could not read citc metadata: {e}")
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


class GitSubCommand(abc.ABC):
    """Abstract base class for git subcommands."""

    def add_arguments(self, parser: argparse.ArgumentParser) -> None:
        """Override to add subcommand-specific arguments."""

    @abc.abstractmethod
    def execute(self, context: Context) -> int:
        """Executes the command.

        Args:
            context: The execution context.

        Returns:
            The exit code (0 for success, non-zero for failure).
        """

    def run(self, context: Context) -> int:
        """Parses arguments and executes the command."""
        parser = argparse.ArgumentParser(
            prog=f"git {getattr(self, '_command_name')}"
        )
        self.add_arguments(parser)
        context.git_subcommand_args = parser.parse_args(
            context.args.remaining_args
        )
        return self.execute(context)


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

    def execute(self, context: Context) -> int:
        args = context.git_subcommand_args
        # We only support HEAD for now
        if args.rev != "HEAD":
            context.error(
                "cog workspaces only support 'HEAD' revisions at this time"
            )
            return 1

        repository_root = get_repository_root()
        if not repository_root:
            context.error("Not in a cog workspace")
            return 1

        workspace_id, snapshot_version = get_workspace_id_and_snapshot_version(
            context, repository_root
        )
        if not workspace_id or not snapshot_version:
            context.error("Not in a cog workspace")
            return 1

        # Determine repo_root
        repo_root = "fuchsia"
        relative_git_dir = get_relative_git_dir(
            context.global_git_options, repository_root
        )
        if relative_git_dir:
            repo_root = f"fuchsia/{relative_git_dir}"

        request = f'request_base {{ workspace_id: "{workspace_id}" base_snapshot_version: {snapshot_version}}} repo_root: "{repo_root}"'

        try:
            cmd = [
                context.real_git,
                "citc",
                "api.call",
                "GetDrafts",
                request,
            ]
            result = subprocess.run(cmd, capture_output=True, text=True)

            if result.returncode != 0:
                context.error(result.stderr)
                return result.returncode

            for line in result.stdout.splitlines():
                if "commit_hash:" in line:
                    parts = line.split('"')
                    if len(parts) >= 2:
                        context.print(parts[1])
                        return 0

            return 1
        except Exception as e:
            context.fatal(f"{e}")
            return 1


@register_command("status")
class StatusCommand(GitSubCommand):
    def execute(self, context: Context) -> int:
        context.log_info(str(context.args.global_git_args))
        context.log_info(str(context.git_subcommand_args))
        context.print("not implemented yet")
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

    def execute(self, context: Context) -> int:
        args = context.git_subcommand_args
        try:
            cmd = [
                context.real_git,
            ]
            cmd.extend(context.args.global_git_args)
            cmd.append("ls-files")
            cmd.extend(context.args.remaining_args)

            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                cwd=context.invoker_cwd,
            )

            if result.returncode != 0:
                context.error(result.stderr)
                return result.returncode

            end = "\0" if args.z else "\n"
            context.output(result.stdout, end=end)
            return 0
        except Exception as e:
            context.fatal(f"{e}")
            return 10


def _find_command_name_and_position(
    args: List[str],
) -> Tuple[Optional[str], int]:
    for i, arg in enumerate(args):
        if arg in _COMMANDS:
            return arg, i
    return None, -1


def _split_args(args: List[str]) -> Tuple[List[str], List[str]]:
    """Splits args into two lists, everything before '--' and everything after."""
    i = 0
    while i < len(args):
        if args[i] == "--":
            return args[:i], args[i + 1 :]
        i += 1
    return args, []


def _create_polyfill_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="git-polyfill", add_help=False)
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
        "--log-file",
        type=str,
        help="Path to a file to append logs to.",
    )
    return parser


def _create_global_git_args_parser() -> argparse.ArgumentParser:
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
        "--git-dir",
        type=str,
        metavar="path",
        help="Use <path> as the path to the .git directory",
    )
    parser.add_argument(
        "--version", action="version", version="git version 2.x (fuchsia-cog)"
    )
    return parser


@dataclass
class ArgsCollection:
    polyfill_args: List[str]
    global_git_args: List[str]
    command_name: str
    remaining_args: List[str]

    def __init__(self, args: List[str]):
        if "--" not in args:
            raise ValueError(
                "Arguments must contain '--' to separate polyfill args from git args."
            )

        # We split on the first '--' found. This is important because git commands
        # can also use '--' to separate flags from positional arguments (e.g. file paths).
        # We want to ensure that the first '--' is used to separate the polyfill arguments
        # from the git command and its arguments.
        #
        # Example: git.py <polyfill-args> -- <global-git-args> <command> <command-args> -- <files>
        self.polyfill_args, git_args = _split_args(args)

        command_name, command_index = _find_command_name_and_position(git_args)

        if not command_name:
            raise ValueError("No git command found.")

        self.global_git_args = git_args[:command_index]
        self.command_name = command_name
        self.remaining_args = git_args[command_index + 1 :]


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: git <command> [<args>]", file=sys.stderr)
        return 1

    provided_args = sys.argv[1:]
    try:
        args_collection = ArgsCollection(provided_args)
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1

    context = Context(args_collection)
    context.write_log("START", f"{shlex.join(sys.argv)}\n")

    command_class = _COMMANDS[args_collection.command_name]
    command = command_class()
    return command.run(context)


if __name__ == "__main__":
    sys.exit(main())
