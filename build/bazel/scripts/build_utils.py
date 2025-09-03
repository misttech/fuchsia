# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Common utilities supporting Bazel query and build actions.

This module will be imported directly from other scripts, including some in
this directory, as well as ones in //build/api/. Its purpose is to provide
utility functions to perform Bazel operations in the Fuchsia workspace. It should
not depend on any other non-standard module, and the logic to create that workspace
should stay in workspace_utils.bzl instead.
"""

import dataclasses
import errno
import hashlib
import json
import os
import shlex
import subprocess
import sys
import time
import typing as T
from pathlib import Path

# A type that describes either a path string of a Path instance.
FilePath: T.TypeAlias = str | os.PathLike[T.Any]


def get_host_platform() -> str:
    """Return host platform name, following Fuchsia conventions."""
    if sys.platform == "linux":
        return "linux"
    elif sys.platform == "darwin":
        return "mac"
    else:
        return os.uname().sysname


def get_host_arch() -> str:
    """Return host CPU architecture, following Fuchsia conventions."""
    host_arch = os.uname().machine
    if host_arch == "x86_64":
        return "x64"
    elif host_arch.startswith(("armv8", "aarch64")):
        return "arm64"
    else:
        return host_arch


def get_host_tag() -> str:
    """Return host tag, following Fuchsia conventions."""
    return "%s-%s" % (get_host_platform(), get_host_arch())


def find_fuchsia_dir(from_path: T.Optional[FilePath] = None) -> Path:
    """Find the Fuchsia checkout from a specific path.

    Args:
        from_path: Optional starting path for search. Defaults to the current directory.
    Returns:
        Path to the Fuchsia checkout directory (absolute).
    Raises:
        ValueError if the path could not be found.
    """
    start_path = Path(from_path).resolve() if from_path else Path.cwd()
    cur_path = start_path
    while True:
        if (cur_path / ".jiri_manifest").exists():
            return cur_path
        prev_path = cur_path
        cur_path = cur_path.parent
        if cur_path == prev_path:
            raise ValueError(
                f"Could not find Fuchsia checkout directory from: {start_path}"
            )


def find_fx_build_dir(fuchsia_dir: FilePath) -> T.Optional[Path]:
    """Find the build directory set through 'fx set' or 'fx use'.

    Args:
       fuchsia_dir: Path to Fuchsia checkout directory.
    Returns:
       Path to build directory if found, of None if none
       is available (e.g. fresh checkout or infra build).
    """
    fuchsia_dir_path = Path(fuchsia_dir)
    fx_build_dir_file = fuchsia_dir_path / ".fx-build-dir"
    if fx_build_dir_file.exists():
        build_dir_relative = fx_build_dir_file.read_text().strip()
        if build_dir_relative:
            build_dir = fuchsia_dir_path / build_dir_relative
            if build_dir.exists():
                return build_dir
    return None


def find_host_binary_path(program: str) -> T.Optional[Path]:
    """Find the absolute path of a given program. Like the UNIX `which` command.

    Args:
        program: Program name.
    Returns:
        program's absolute path, found by parsing the content of $PATH.
        Or None if nothing is found.
    """
    for path in os.environ.get("PATH", "").split(":"):
        # According to Posix, an empty path component is equivalent to ´.'.
        if path == "" or path == ".":
            path = os.getcwd()
        candidate = os.path.realpath(os.path.join(path, program))
        if os.path.isfile(candidate) and os.access(
            candidate, os.R_OK | os.X_OK
        ):
            return Path(candidate)

    return None


def get_bazel_relative_topdir(fuchsia_dir: FilePath) -> tuple[str, set[Path]]:
    """Return Bazel topdir, relative to Ninja output dir.

    Args:
        fuchsia_dir: Fuchsia source directory path.
    Returns:
        A (topdir, input_files) pair, where input_files is a set of Path
        values corresponding to the file(s) read by this function.
    """
    input_file = Path(fuchsia_dir) / "build/bazel/config/bazel_top_dir"
    assert input_file.exists(), f"Missing input file: {input_file}"
    return input_file.read_text().strip(), {input_file}


def find_bazel_launcher_path(
    fuchsia_dir: FilePath, build_dir: FilePath
) -> T.Optional[Path]:
    """Find the path of the Bazel launcher script.

    Args:
        fuchsia_dir: Path to Fuchsia checkout directory.
        build_dir: Path to Fuchsia build directory.

    Returns:
        Path to bazel launcher script, or empty Path() value if the file
        does not exist.
    """
    bazel_topdir, _ = get_bazel_relative_topdir(fuchsia_dir)
    result = Path(build_dir) / bazel_topdir / "bazel"
    return result if result.exists() else None


def find_bazel_workspace_path(
    fuchsia_dir: FilePath, build_dir: FilePath
) -> T.Optional[Path]:
    """Find the path of the Bazel workspace.

    Args:
        fuchsia_dir: Path to Fuchsia checkout directory.
        build_dir: Path to Fuchsia build directory.

    Returns:
        Path to bazel workspace, or None if the directory does not exists.
    """
    bazel_topdir, _ = get_bazel_relative_topdir(fuchsia_dir)
    result = Path(build_dir) / bazel_topdir / "workspace"
    return result if result.exists() else None


def force_symlink(dst_path: FilePath, target_path: FilePath) -> None:
    """Create a symlink at |dst_path| that points to |target_path|.

    The generated symlink target will always be a relative path.

    Args:
        dst_path: path to symlink file to write or update.
        target_path: path to actual symlink target.
    """
    dst_dir = os.path.dirname(dst_path)
    target_path = os.path.relpath(target_path, dst_dir)
    return force_raw_symlink(dst_path, target_path)


def force_raw_symlink(dst_path: FilePath, target_path: FilePath) -> None:
    """Create a symlink at |dst_path| that points to |target_path|.

    The generated symlink target will always be the unmodified |target_path|
    value, which can be absolute or relative, and/or point to a
    non-existing file.

    Args:
        dst_path: path to symlink file to write or update.
        target_path: raw symlink target path.
    """
    dst_dir = os.path.dirname(dst_path)
    os.makedirs(dst_dir, exist_ok=True)
    try:
        os.symlink(target_path, dst_path)
    except OSError as e:
        if e.errno == errno.EEXIST:
            os.remove(dst_path)
            os.symlink(target_path, dst_path)
        else:
            raise


_HEXADECIMAL_SET = set("0123456789ABCDEFabcdef")


def is_hexadecimal_string(s: str) -> bool:
    """Return True if input string only contains hexadecimal characters."""
    return bool(s) and all([c in _HEXADECIMAL_SET for c in s])


_BUILD_ID_PREFIX = ".build-id/"


def is_likely_build_id_path(path: str) -> bool:
    """Return True if path is a .build-id/xx/yyyy* file name."""
    # Look for .build-id/XX/ where XX is an hexadecimal string.
    pos = path.find(_BUILD_ID_PREFIX)
    if pos < 0:
        return False

    if pos > 0 and path[pos - 1] != "/":
        return False

    path = path[pos + len(_BUILD_ID_PREFIX) :]
    if len(path) < 3 or path[2] != "/":
        return False

    return is_hexadecimal_string(path[0:2])


def is_likely_content_hash_path(path: str) -> bool:
    """Return True if file path is likely based on a content hash.

    Args:
        path: File path
    Returns:
        True if the path is a likely content-based file path, False otherwise.
    """
    # Look for .build-id/XX/ where XX is an hexadecimal string.
    if is_likely_build_id_path(path):
        return True

    # Look for a long hexadecimal sequence filename.
    filename = os.path.basename(path)
    return len(filename) >= 16 and is_hexadecimal_string(filename)


assert is_likely_content_hash_path("/src/.build-id/ae/23094.so")
assert not is_likely_content_hash_path("/src/.build-id/log.txt")


# Callable object that takes log messages as input.
LogFunc = T.Callable[[str], None]


class TimeProfile(object):
    """Track duration of generation/build steps' start and end times.

    Usage is:
      1) Create instance.

      2) Call start() when starting a new step. Repeat as many times as needed.

      3) Optionally call stop() when a step has completed. Useful if some
         unrelated work needs to happen after the next start() call.

      4) Call print() to print a table detailing the timings of all
         steps over a given threshold.
    """

    def __init__(
        self,
        log: None | LogFunc = None,
        now: None | T.Callable[[], float] = None,
    ) -> None:
        """Constructor.

        Args:
            log: An optional callable that can be used to print step descriptions
               when start() is called.
            now: An optional callable that can be used to return the current time
               in seconds. Default is to use time.time. Only used for tests.
        """
        self._now = now if now else time.time
        self._start_time = self._now()
        self._steps: list[tuple[float, float, str]] = []
        self._log = log

    def start(self, name: str, description: str = "") -> None:
        """Start a new regeneration step (and stop the current one if any)

        Args:
            name: Step name (used in final print() output)
            description: Optional step description. Will be sent to the log
               if one was provided in the constructor.
        """
        if description and self._log:
            self._log(description)
        cur_time = self._close_last_step()
        self._steps.append((cur_time, 0, name))

    def stop(self) -> None:
        """Stop the current step (record its end time)."""
        self._close_last_step()

    def _close_last_step(self) -> float:
        cur_time = self._now()
        if self._steps:
            start_time, end_time, name = self._steps[-1]
            if end_time == 0:
                end_time = cur_time
                self._steps[-1] = (start_time, end_time, name)
        return cur_time

    def to_json_timings(self) -> dict[str, float]:
        """Generate a JSON object detailing the durtaion of each step.

        Returns:
           A dictionary mapping step names to durations in seconds.
           Keys are ordered according to step execution.
        """
        self._close_last_step()
        return {
            name: end_time - start_time
            for start_time, end_time, name in self._steps
        }

    def print(self, short_step_threshold: float = 0.0) -> None:
        """Print timings results for all recorded steps.

        Args:
            short_step_threshold: A threshold in seconds. Any step
                that was faster than this will be omitted from the
                output.
        """
        self._close_last_step()
        if short_step_threshold:
            print(
                "Timing results for regeneration steps slower than %.1f seconds:"
                % short_step_threshold
            )
        else:
            print("Timing results for all regeneration steps:")
        for step in self._steps:
            start_time, end_time, name = step
            duration = end_time - start_time
            if duration < short_step_threshold:
                continue
            print("%5.2fs   %s" % (end_time - start_time, name))


def log_stderr(msg: str) -> None:
    """A LogFunc implementation that prints messages to stderr."""
    print(msg, file=sys.stderr)


def cmd_args_to_string(cmd_args: list[FilePath]) -> str:
    """Convert a list of command arguments to a printable string.

    Args:
        cmd_args: A list of either strings or file paths.
    Returns:
        A single string representing the shell-quoted command.
    """
    return " ".join(shlex.quote(str(c)) for c in cmd_args)


@dataclasses.dataclass
class CommandResult:
    """The result of invoking CommandLauncher.run_command().

    This is similar to subprocess.CompletedProcess[str], but it doesn't
    hold file descriptors open, and can be trivially instantiated for tests
    or mock CommandLauncher instances.
    """

    returncode: int = 0
    stdout: str = ""
    stderr: str = ""
    args: list[str] = dataclasses.field(default=list)  # type: ignore


class CommandLauncher(object):
    """Convenience class to launch commands.

    A small wrapper around subprocess.run(), which allows logging invocations
    and errors. It also allows mock implementations for tests to override the
    run_command() method in derived classes.
    """

    def __init__(
        self,
        log: None | LogFunc = None,
        log_err: None | LogFunc = log_stderr,
    ) -> None:
        """Create instance.

        Args:
            log: Optional LogFunc to send runtime logs to.
            log_err: Optional LogFunc to send runtime error logs to.
        """
        self.log = log
        self.log_err = log_err

    def run_command_internal(
        self, cmd_args: list[FilePath], print_stdout: bool, print_stderr: bool
    ) -> CommandResult:
        """Internal implementation for run_command().

        Mock implementations can override this to avoid calling
        external commands during unit-tests.
        """
        ret = subprocess.run(
            [str(c) for c in cmd_args],
            stdout=None if print_stdout else subprocess.PIPE,
            stderr=None if print_stderr else subprocess.PIPE,
            text=True,
        )
        return CommandResult(ret.returncode, ret.stdout, ret.stderr, ret.args)

    def run_command(
        self,
        cmd_args: list[FilePath],
        print_stdout: bool = False,
        print_stderr: bool = False,
    ) -> CommandResult:
        """Run a command.

        By default, this captures both stdout and stderr, unless
        any of print_stdout or print_stderr is used.

        Args:
            cmd_args: List of command-line arguments, each can be either a string
               or a Path instance for convenience.
            print_stdout: Optional flag, set to True to not capture the command's stdout
               and send it to the caller's standard output stream instead.
            print_stderr: Optional flag, set to True to not capture the command's stderr
               and send it to the caller's error output stream instead.
        Returns:
            A BazelCommandResult value.
        """
        if self.log:
            self.log("CMD: " + cmd_args_to_string(cmd_args))

        ret = self.run_command_internal(
            cmd_args, print_stdout=print_stdout, print_stderr=print_stderr
        )

        if ret.returncode != 0 and self.log_err:
            self.log_err(
                "Error when invoking command: %s\n%s\n%s\n"
                % (cmd_args_to_string(cmd_args), ret.stderr, ret.stdout)
            )

        return ret


class BazelLauncher(CommandLauncher):
    """Convenience class to launch Bazel invocations.

    A small wrapper around subprocess.run(), which allows tests to override
    the run_command() method in derived classes.
    """

    def __init__(
        self,
        bazel_launcher: FilePath,
        log: None | LogFunc = None,
        log_err: None | LogFunc = log_stderr,
    ) -> None:
        """Create instance.

        Args:
            log: Optional LogFunc to send runtime logs to.
            log_err: Optional LogFunc to send runtime error logs to.
        """
        super().__init__(log, log_err)
        self._bazel_launcher = bazel_launcher

    def run_bazel_command(
        self,
        bazel_args: list[FilePath],
        print_stdout: bool = False,
        print_stderr: bool = False,
    ) -> CommandResult:
        """Run a Bazel command.

        Args:
            bazel_args: Bazel command-line arguments.
            print_stderr: Optional flag, set to True to not capture stderr.
        Returns:
            A CommandResult value.
        """
        return self.run_command(
            [self._bazel_launcher] + bazel_args,
            print_stdout=print_stdout,
            print_stderr=print_stderr,
        )

    def run_query(
        self, query_type: str, query_args: list[str], ignore_errors: bool
    ) -> CommandResult:
        """Run a Bazel query, potentially ignoring errors.

        Args:
            query_type: Type of query ("query", "cquery" or "aquery").
            query_args: Other query arguments.
            ignore_errors: Set to True to allow queries that ignore errors.
               This adds "--keep_going" to the launched command.
        Returns:
            A BazelCommandResult value.
        """
        query_cmd: list[FilePath] = []
        query_cmd.append(query_type)
        query_cmd.extend(query_args)
        if ignore_errors:
            query_cmd += ["--keep_going"]

        return self.run_bazel_command(query_cmd)


class BazelQueryCache(object):
    def __init__(
        self,
        cache_dir: os.PathLike[str],
    ) -> None:
        self._cache_dir = cache_dir

    def get_query_output(
        self,
        query_type: str,
        query_args: list[str],
        launcher: BazelLauncher,
        log: None | LogFunc = None,
    ) -> T.Optional[list[str]]:
        """Run a bazel query and return its output as a series of lines.

        Args:
            query_type: One of 'query', 'cquery' or 'aquery'
            query_args: Extra query arguments.
            launcher: A BazelLauncher instance.
            log: Optional LogFunc value. If not provided, uses launcher.log.

        Returns:
            On success, a list of output lines. On failure return None.
        """
        # The result of queries does not change between incremental builds,
        # as their outputs only depend on the shape of the Bazel graph, not
        # the content of the artifacts. Due to this, it is possible to cache
        # the outputs to save several seconds per bazel_action() invocation.
        #
        # The data is simply stored under $WORKSPACE/fuchsia_build_generated/bazel_query_cache/
        # which will be removed by each regenerator call, since it may change the Bazel
        # graph dependencies, and thus the query results.

        # Reuse launcher.log value if none is specified explicitly.
        if not log:
            log = launcher.log

        cache_key, cache_key_args = self.compute_cache_key_and_args(
            query_type, query_args
        )
        cache_file = os.path.join(self._cache_dir, f"{cache_key}.json")
        if os.path.exists(cache_file):
            try:
                with open(cache_file, "rt") as f:
                    cache_value = json.load(f)
                assert cache_value["key_args"] == cache_key_args
                if log:
                    log(
                        f"Found cached values for query {cache_key}: {cache_key_args}"
                    )
                return cache_value["output_lines"]
            except Exception as e:
                print(
                    f"WARNING: Error when reading cached values for query {cache_key}: {cache_key_args}:\n{e}",
                    file=sys.stderr,
                )

        if log:
            query_start_time = time.time()

        ret = launcher.run_query(query_type, query_args, ignore_errors=False)
        if ret.returncode != 0:
            return None

        result = ret.stdout.splitlines()

        # Write the result to the cache.
        new_cache_value = {
            "key_args": cache_key_args,
            "output_lines": result,
        }
        if log:
            log(
                "Query took %.1f seconds for query %s"
                % (time.time() - query_start_time, cache_key_args)
            )
            log(f"Writing query values to cache for query {cache_key}\n")

        os.makedirs(os.path.dirname(cache_file), exist_ok=True)
        with open(cache_file, "wt") as f:
            json.dump(new_cache_value, f)

        return result

    @staticmethod
    def compute_cache_key_and_args(
        query_type: str, query_args: list[str]
    ) -> tuple[str, list[str]]:
        """Compute the cache key and arguments. Exposed for tests.

        Args:
            query_type: query type (e.g. "query", "cquery" or "aquery")
            query_args: query arguments.
        Returns:
            a (cache_key, cache_key_args), where cache_key is a unique
            hexadecimal cache key value, and cache_key_args encode the
            input query in a human readable way. This will be stored in
            the cache value for debugging.
        """
        cache_key_args = [query_type] + query_args
        cache_key_inputs = cache_key_args[:]

        # If "--starlark:file=FILE" or "--starlark:file FILE" is used, add the
        # content of FILE to compute the cache key. This ensure stale cache
        # entries are not reused when only this file changes during development.
        starlark_file_option = "--starlark:file"
        for n, arg in enumerate(query_args):
            input_path = None
            if arg == starlark_file_option and n + 1 < len(query_args):
                # For --starlark:file FILE
                input_path = query_args[n + 1]
            elif arg.startswith(f"{starlark_file_option}="):
                # For --starlark:file=FILE
                input_path = arg[len(starlark_file_option) + 1 :]
            if input_path:
                with open(input_path, "rt") as f:
                    cache_key_inputs.append(f.read())

        cache_key = hashlib.sha256(
            repr(cache_key_inputs).encode("utf-8")
        ).hexdigest()

        return (cache_key, cache_key_args)


class BazelCommand(object):
    """Convenience class to wrap the Bazel launcher script invocations."""

    def __init__(self, bazel_launcher: Path) -> None:
        self._command_start = [
            str(bazel_launcher),
        ]
        self._common_args = [
            "--config=quiet",
            "--platforms=//build/bazel/platforms:host",  # For now, only supports host targets.
        ]

    @classmethod
    def from_dirs(
        cls,
        fuchsia_dir: T.Optional[Path] = None,
        build_dir: T.Optional[Path] = None,
    ) -> "BazelCommand":
        if fuchsia_dir:
            fuchsia_dir = fuchsia_dir.resolve()
        else:
            fuchsia_dir = find_fuchsia_dir(os.path.dirname(__file__))

        if build_dir:
            build_dir = build_dir.resolve()
        else:
            build_dir = find_fx_build_dir(fuchsia_dir)
            if not build_dir:
                raise Exception(
                    "Could not find Fuchsia build directory, please specify build dir."
                )

        bazel_launcher = find_bazel_launcher_path(fuchsia_dir, build_dir)
        if not bazel_launcher:
            raise Exception(
                f"Could not find Bazel launcher script! fuchsia dir={fuchsia_dir} build dir={build_dir}"
            )
        return BazelCommand(bazel_launcher)

    def run(self, command: str, args: T.Sequence[str] = []) -> str:
        """Run a specific Bazel command with optional args, return output as string."""
        return subprocess.check_output(
            self._command_start + [command] + self._common_args + list(args),
            text=True,
        ).strip()

    def get_execroot(self) -> Path:
        """Return the absolute path to the Bazel execroot directory."""
        execroot = self.run("info", ["execution_root"])
        return Path(execroot)

    def get_output_base(self) -> Path:
        """Return the absolute path to the Bazel execroot directory."""
        execroot = self.run("info", ["output_base"])
        return Path(execroot)
