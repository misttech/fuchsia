# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import abc
import argparse
import configparser
import shlex
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import IO, Callable, Dict, List, Optional, Tuple, Type


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
        self.repository_root: Path = Path(self.polyfill_options.repository_root)
        self.log_file: Optional[str] = self.polyfill_options.log_file

        self._log_file_path = Path(self.log_file) if self.log_file else None

        # Global git arguments
        global_git_parser = _create_global_git_args_parser()
        self.global_git_options, _ = global_git_parser.parse_known_args(
            args.global_git_args
        )

    def write_log(self, level: str, message: str) -> None:
        if self._log_file_path:
            with self._log_file_path.open("a") as f:
                f.write(f"[{level}] {message}\n")

    def print(self, message: str, file: Optional[IO[str]] = sys.stdout) -> None:
        """Prints a message and logs it."""
        print(message, file=file)
        self.write_log("PRINT", message)

    def error(self, message: str) -> None:
        """Prints an error message to stderr and logs it."""
        print(message, file=sys.stderr)
        self.write_log("ERROR", message)

    def fatal(self, message: str) -> None:
        """Prints a fatal error message to stderr and logs it."""
        print(f"fatal: {message}", file=sys.stderr)
        self.write_log("FATAL", message)

    def log_info(self, message: str) -> None:
        """Logs an info message to the log."""
        self.write_log("INFO", message)

    def output(self, message: str, end: str = "") -> None:
        """Prints output to stdout and logs it."""
        sys.stdout.write(message)
        if end:
            sys.stdout.write(end)
        self.write_log("OUTPUT", message.strip())

    def run_real_git(self, args: List[str], cwd: Optional[str] = None) -> str:
        """Runs the real git command with the given arguments."""
        full_command = [self.real_git] + args
        self.write_log("EXEC", shlex.join(full_command))

        result = subprocess.run(
            full_command,
            capture_output=True,
            text=True,
            cwd=cwd,
            check=True,
        )

        return result.stdout

    def get_relative_path(self) -> str:
        """Returns the relative path to the repository root that we should be using based on the args."""
        invoker_cwd = Path(self.invoker_cwd)
        args = self.global_git_options

        if args.C and args.git_dir:
            raise ValueError("Cannot use both -C and --git-dir")

        path: Path
        if args.C:
            path = Path(args.C)
        elif args.git_dir:
            path = Path(args.git_dir)
            if path.name != ".git":
                raise ValueError(f"git_dir must end in .git, got {path}")
            path = path.parent
        else:
            path = invoker_cwd

        if not path.is_absolute():
            path = invoker_cwd / path

        return str(path.relative_to(self.repository_root))


def get_workspace_id_and_snapshot_version(
    context: Context,
) -> Tuple[str, int]:
    """Returns the workspace id and snapshot version for the given repository root.

    Args:
        context: The execution context.

    Returns:
        A tuple of the workspace id and snapshot version.
    """
    citc_dir = context.repository_root.parent / ".citc"
    try:
        workspace_id = (citc_dir / "workspace_id").read_text().strip()
        snapshot_version = (citc_dir / "snapshot_version").read_text().strip()
    except Exception as e:
        context.fatal(f"could not read citc metadata: {e}")
        return "", 0
    return workspace_id, int(snapshot_version)


def get_submodule_paths(repository_path: Path) -> List[str]:
    """Returns a list of submodule paths from the .gitmodules file.

    This function returns a list of submodule paths from the .gitmodules file.

    Args:
        repository_path: The path to the repository. This path should be an
        absolute path.

    Returns:
        A list of submodule paths.
    """

    gitmodules_path = repository_path / ".gitmodules"
    paths: List[str] = []
    if not gitmodules_path.exists():
        return paths

    config = configparser.ConfigParser()
    try:
        config.read(gitmodules_path)
        for section in config.sections():
            if "path" in config[section]:
                paths.append(config[section]["path"])
    except configparser.Error:
        # If we can't parse the file, we assume there are no submodules.
        pass
    return paths


def get_target_repository_at_path(
    relative_path: str, repository_root: Path
) -> str:
    """Returns the relative path to the repository root for the target path.

    Args:
        relative_path: The relative path to the root directory.
        repository_root: The root of the repository.

    Returns:
        The relative path to the submodule from repository_root, or "" if it is the main repository.
    """
    current_path = repository_root / relative_path

    # Ensure we are inside the repository root.
    try:
        current_path.relative_to(repository_root)
    except ValueError:
        # If the path is outside the repo root, assume main repo.
        return ""

    # Collect all ancestors that have .gitmodules entries
    repos: List[Path] = [repository_root]

    path_to_check = current_path
    while True:
        submodules = get_submodule_paths(path_to_check)
        if submodules:
            repos.extend(
                [path_to_check / submodule for submodule in submodules]
            )

        if path_to_check == repository_root:
            break

        path_to_check = path_to_check.parent

        if not str(path_to_check).startswith(str(repository_root)):
            break

    # Sort the repositories by length so that we check the deepest ones first.
    repos.sort(key=lambda p: len(str(p)), reverse=True)

    for repo in repos:
        try:
            current_path.relative_to(repo)
            if repo == repository_root:
                return ""
            return str(repo.relative_to(repository_root))
        except ValueError:
            continue

    return ""


def get_repo_root_for_repo(repo_path: str) -> str:
    """Returns the repo root string expected by the backend.

    Args:
        repo_path: The relative path of the repo (e.g. "" or "sub/module").

    Returns:
        The full repo root string (e.g. "fuchsia" or "fuchsia/sub/module").
    """
    if not repo_path:
        return "fuchsia"
    return f"fuchsia/{repo_path}"


class GitSubCommand(abc.ABC):
    """Abstract base class for git subcommands."""

    _command_name: str

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


def register_command(
    name: str,
) -> Callable[[Type[GitSubCommand]], Type[GitSubCommand]]:
    """Decorator to register a GitSubCommand implementation."""

    def decorator(cls: Type[GitSubCommand]) -> Type[GitSubCommand]:
        _COMMANDS[name] = cls
        cls._command_name = name
        return cls

    return decorator


@register_command("rev-parse")
class RevParseCommand(GitSubCommand):
    def add_arguments(self, parser: argparse.ArgumentParser) -> None:
        parser.add_argument("rev", nargs="*", help="The revision to parse")
        parser.add_argument(
            "--show-toplevel",
            action="store_true",
            help="Show the absolute path of the top-level directory",
        )

    def execute(self, context: Context) -> int:
        repository_root = context.repository_root

        rev_cache: Dict[str, str] = {}

        target_repo = get_target_repository_at_path(
            context.get_relative_path(), repository_root
        )
        repo_root = get_repo_root_for_repo(target_repo)

        def _get_rev(rev: str) -> Optional[str]:
            if rev in rev_cache:
                return rev_cache[rev]

            (
                workspace_id,
                snapshot_version,
            ) = get_workspace_id_and_snapshot_version(context)
            if not workspace_id or not snapshot_version:
                context.error("Not in a cog workspace")
                return None

            request = f'request_base {{ workspace_id: "{workspace_id}" base_snapshot_version: {snapshot_version}}} repo_root: "{repo_root}"'

            try:
                args = ["citc", "api.call", "GetDrafts", request]
                stdout = context.run_real_git(args)

                for line in stdout.splitlines():
                    if "commit_hash:" in line:
                        parts = line.split('"')
                        if len(parts) >= 2:
                            fetched_rev = parts[1]
                            rev_cache[rev] = fetched_rev
                            return fetched_rev
            except subprocess.CalledProcessError as e:
                context.error(e.stderr)
            except Exception as e:
                context.fatal(f"{e}")
            return None

        # Iterate over arguments in the order they were provided. This is important because
        # the order of arguments determines the order of the output. For example, if the
        # arguments are ["--show-toplevel", "HEAD"], the output should be the repository root
        # followed by the head revision.
        for arg in context.args.remaining_args:
            if arg == "--show-toplevel":
                # TODO(469510407) Make sure this is using the active repo.
                context.print(str(repository_root))
            elif arg.startswith("-"):
                pass
            else:
                if arg != "HEAD":
                    context.error(
                        "cog workspaces only support 'HEAD' revisions at this time"
                    )
                    return 1

                if rev := _get_rev(arg):
                    context.print(rev)

        return 0


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
            git_args = []
            git_args.extend(context.args.global_git_args)
            git_args.append("ls-files")
            git_args.extend(context.args.remaining_args)

            stdout = context.run_real_git(git_args, cwd=context.invoker_cwd)

            end = "\0" if args and args.z else "\n"
            context.output(stdout, end=end)
            return 0
        except subprocess.CalledProcessError as e:
            context.error(e.stderr)
            return e.returncode
        except Exception as e:
            context.fatal(f"{e}")
            return 1


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
        "--repository-root",
        type=str,
        help="Path to the repository root. This is the directory that would contain the .git directory for the root repository.",
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


def _verify_repository_root_is_cog(repository_root: Path) -> bool:
    return (repository_root.parent / ".citc").is_dir()


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

    if not _verify_repository_root_is_cog(context.repository_root):
        context.error("Not in a cog workspace.")
        return 1

    command_class = _COMMANDS[args_collection.command_name]
    command = command_class()
    return command.run(context)


if __name__ == "__main__":
    sys.exit(main())
